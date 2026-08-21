use crate::ctx::SyncCtx;
use anyhow::{bail, Context as _, Result};
use feanorfs_common::{ConcurrentEdit, ConflictKind, FileState};
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use tokio::fs;

/// Prefix for placeholder bytes written when a version cannot be materialized.
pub const SENTINEL_PREFIX: &str = "<feanorfs-sentinel:";

pub const SUFFIX_ORIGINAL: &str = ".original";
pub const SUFFIX_LOCAL: &str = ".local";
pub const SUFFIX_CLOUD: &str = ".cloud";

// Legacy suffixes (read compat)
const SUFFIX_BASE: &str = ".base";
const SUFFIX_OURS: &str = ".ours";
const SUFFIX_THEIRS: &str = ".theirs";

/// Sidecar file-name prefix for one conflict's exact identity/fingerprint.
/// There is ONE fingerprint-keyed sidecar per conflict
/// (`identity-<first-32-chars-of-fingerprint>.json`), so distinct conflicts
/// in the same directory never share or clobber a sidecar.
const IDENTITY_FILE_PREFIX: &str = "identity-";
const IDENTITY_FILE_SUFFIX: &str = ".json";
/// Fingerprint characters carried in the fingerprint-keyed sidecar file
/// name; the full fingerprint is persisted inside the file and verified on
/// read.
pub const IDENTITY_FINGERPRINT_NAME_PREFIX_CHARS: usize = 32;
const MAX_IDENTITY_SIDECAR_BYTES: usize = 128 * 1024;
const BINARY_PREFIX_BYTES: usize = 8 * 1024;

/// Materialization failure policy for the sole canonical triple writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationPolicy {
    /// Any leg read/upload failure aborts materialization (integrator needs).
    Strict,
    /// Leg read failures degrade to a bounded sentinel artifact while the
    /// rest of the triple still materializes (sync-gate needs).
    BestEffort,
}

pub fn is_sentinel_content(content: &[u8]) -> bool {
    content.starts_with(SENTINEL_PREFIX.as_bytes())
}

/// Label inside `<feanorfs-sentinel:{label}>\n`, if present.
#[must_use]
pub fn sentinel_label(content: &[u8]) -> Option<&str> {
    if !is_sentinel_content(content) {
        return None;
    }
    let s = std::str::from_utf8(content).ok()?;
    let rest = s.strip_prefix(SENTINEL_PREFIX)?;
    rest.strip_suffix(">\n").or_else(|| rest.strip_suffix('>'))
}

/// Cloud-side deletion artifact (edit/delete or delete/edit conflicts).
#[must_use]
pub fn is_cloud_deleted_sentinel(content: &[u8]) -> bool {
    sentinel_label(content) == Some("deleted")
}

pub fn is_binary_content(content: &[u8]) -> bool {
    content.is_empty() || content.contains(&0)
}

pub(crate) fn sentinel(label: &str) -> Vec<u8> {
    format!("{SENTINEL_PREFIX}{label}>\n").into_bytes()
}

#[must_use]
pub fn artifact_path(conflict_dir: &Path, rel_path: &str, suffix: &str) -> PathBuf {
    conflict_dir.join(format!("{rel_path}{suffix}"))
}

/// Reads at most `max` bytes from an artifact to classify binary content
/// without a whole-file re-read. Returns `None` when the file is missing.
#[must_use]
pub fn read_binary_prefix(path: &Path, max: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut prefix = Vec::with_capacity(max);
    std::io::Read::by_ref(&mut file)
        .take(max as u64 + 1)
        .read_to_end(&mut prefix)
        .ok()?;
    Some(prefix)
}

/// Versioned identity sidecar persisted beside a materialized triple.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdentitySidecarV1 {
    schema_version: u32,
    fingerprint: String,
    identity: feanorfs_common::ConflictIdentity,
}

const IDENTITY_SIDECAR_SCHEMA_VERSION: u32 = 1;

/// Automatic-resolution binding fields attached to a conflict identity.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityBinding<'a> {
    pub task_id: Option<&'a str>,
    pub intent_message_ids: &'a [String],
    pub assignment_id: Option<&'a str>,
    pub attempt: Option<u32>,
    pub designated_owner: Option<&'a str>,
    pub verification_policy: Option<&'a str>,
}

/// Converts one conflict leg `FileState` (or absence) to a canonical leg
/// descriptor.
#[must_use]
pub fn leg_descriptor(state: Option<&FileState>) -> feanorfs_common::ConflictLegDescriptor {
    match state {
        None => feanorfs_common::ConflictLegDescriptor {
            present: false,
            deleted: false,
            hash: String::new(),
            size: 0,
            mode: 0,
        },
        Some(file) => feanorfs_common::ConflictLegDescriptor {
            present: true,
            deleted: file.deleted,
            hash: file.hash.clone(),
            size: file.size,
            mode: file.mode,
        },
    }
}

/// Builds the canonical identity for one conflict edit against one inspected
/// snapshot. Automatic fields are bound only for automatic resolution.
#[must_use]
pub fn conflict_identity_from_edit(
    workspace_id: &str,
    current_snapshot: &str,
    about_snapshot: &str,
    tree_root: &str,
    edit: &ConcurrentEdit,
    kind: ConflictKind,
    binding: &IdentityBinding<'_>,
) -> feanorfs_common::ConflictIdentity {
    let mut intent_message_ids: Vec<String> = binding.intent_message_ids.to_vec();
    intent_message_ids.sort();
    intent_message_ids.dedup();
    let mut identity = feanorfs_common::ConflictIdentity {
        schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
        workspace_id: workspace_id.to_string(),
        current_snapshot: current_snapshot.to_string(),
        about_snapshot: about_snapshot.to_string(),
        tree_root: tree_root.to_string(),
        path: edit.path.clone(),
        base: leg_descriptor(edit.base.as_ref()),
        ours: leg_descriptor(edit.ours.as_ref()),
        theirs: leg_descriptor(edit.theirs.as_ref()),
        kind,
        task_id: binding.task_id.map(str::to_string),
        intent_message_ids,
        assignment_id: binding.assignment_id.map(str::to_string),
        attempt: binding.attempt,
        designated_owner: binding.designated_owner.map(str::to_string),
        verification_policy: binding.verification_policy.map(str::to_string),
    };
    // Keep the stored kind canonical regardless of caller input.
    identity.kind =
        feanorfs_common::derive_conflict_kind(&identity.base, &identity.ours, &identity.theirs);
    identity
}

/// Canonical fingerprint-keyed identity sidecar file name. The file name
/// carries only the first [`IDENTITY_FINGERPRINT_NAME_PREFIX_CHARS`] hex
/// characters as a stable per-conflict key; the full fingerprint is
/// persisted inside the file and verified on read.
#[must_use]
pub fn identity_file_name(fingerprint: &str) -> String {
    let prefix: String = fingerprint
        .chars()
        .take(IDENTITY_FINGERPRINT_NAME_PREFIX_CHARS)
        .collect();
    format!("{IDENTITY_FILE_PREFIX}{prefix}{IDENTITY_FILE_SUFFIX}")
}

/// Durably creates `dest` exclusively (O_EXCL, never follows a final-component
/// symlink/reparse/hard-link substitution) and writes `content`. The file is
/// fsynced and the parent directory synced so the new entry survives power
/// loss. An existing destination is NEVER replaced: the write fails closed,
/// which is the guarantee concurrent materializers rely on (identity
/// sidecars, conflict copies, version legs).
///
/// # Errors
/// Returns an error when the destination already exists (typed
/// [`std::io::ErrorKind::AlreadyExists`] in the error chain) or the write is
/// not durable.
pub async fn write_new_durable(dest: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    let dest = dest.to_path_buf();
    let content = content.to_vec();
    let capture_path = dest.clone();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&capture_path)?;
        output.write_all(&content)?;
        output.flush()?;
        output.sync_all()
    })
    .await
    .context("join create-new durable write task")?;
    if let Err(error) = result {
        return Err(error.into());
    }
    sync_parent_directory(&dest).await?;
    Ok(())
}

/// Fsync the parent directory of `path` so a completed create-new write
/// survives power loss. Unix-only: on Windows, directory handles cannot be
/// opened with `std`, so this is a documented no-op (mirrors
/// [`crate::fs_util`] policy; see `crate::durable` module docs).
async fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let display = parent.to_path_buf();
        let sync_path = display.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::File::open(&sync_path)?.sync_all()
        })
        .await
        .map_err(|join| anyhow::anyhow!("parent directory sync task failed: {join}"))?
        .map_err(|error| crate::durable::durability_uncertain(&display, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Persists the exact identity/fingerprint beside materialized artifacts as
/// ONE fingerprint-keyed sidecar per conflict
/// (`identity-<first-32-chars-of-fingerprint>.json`, full fingerprint inside,
/// verified on read). The write is create-new (O_EXCL, no-follow) and
/// crash-durable. An existing sidecar for the SAME conflict (same
/// fingerprint and identity path) is accepted without rewriting (idempotent
/// re-registration); a sidecar belonging to a DIFFERENT conflict in the same
/// directory is never overwritten — that fails hard.
///
/// # Errors
/// Returns an error when the sidecar cannot be written or the fingerprint
/// key is already claimed by a different conflict.
pub async fn write_identity_sidecar(
    dir: &Path,
    identity: &feanorfs_common::ConflictIdentity,
    fingerprint: &str,
) -> Result<()> {
    let file_name = identity_file_name(fingerprint);
    let payload = IdentitySidecarV1 {
        schema_version: IDENTITY_SIDECAR_SCHEMA_VERSION,
        fingerprint: fingerprint.to_string(),
        identity: identity.clone(),
    };
    let bytes = serde_json::to_vec(&payload).context("serialize conflict identity sidecar")?;
    match write_new_durable(&dir.join(&file_name), &bytes).await {
        Ok(()) => Ok(()),
        Err(error) if create_new_conflict(&error) => {
            // The fingerprint key is already claimed. Only the exact same
            // conflict may reuse it; never clobber another conflict's sidecar.
            match read_identity_sidecar(dir, fingerprint) {
                Some((existing, _)) if existing.path == identity.path => Ok(()),
                Some(_) => bail!(
                    "identity sidecar {file_name} in {} belongs to a different conflict; \
                     refusing to overwrite it",
                    dir.display()
                ),
                None => bail!(
                    "identity sidecar {file_name} in {} exists but is corrupt, oversized, or \
                     fingerprint-mismatched; refusing to overwrite it",
                    dir.display()
                ),
            }
        }
        Err(error) => Err(error),
    }
}

fn create_new_conflict(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

/// Reads and validates ONE identity sidecar file (any name). Returns `None`
/// for missing, corrupt, oversized, fingerprint-mismatched sidecars, and
/// sidecars whose keyed file name does not match their stored fingerprint —
/// absence can only reduce eligibility, never grant it.
fn read_identity_sidecar_file(path: &Path) -> Option<(feanorfs_common::ConflictIdentity, String)> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() > MAX_IDENTITY_SIDECAR_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_IDENTITY_SIDECAR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_IDENTITY_SIDECAR_BYTES {
        return None;
    }
    let sidecar: IdentitySidecarV1 = serde_json::from_slice(&bytes).ok()?;
    if sidecar.schema_version != IDENTITY_SIDECAR_SCHEMA_VERSION {
        return None;
    }
    if identity_file_name(&sidecar.fingerprint)
        != path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    {
        return None;
    }
    if feanorfs_common::validate_conflict_identity(&sidecar.identity).is_err() {
        return None;
    }
    let computed = feanorfs_common::compute_conflict_identity_fingerprint(&sidecar.identity);
    if computed != sidecar.fingerprint {
        return None;
    }
    Some((sidecar.identity, sidecar.fingerprint))
}

/// Reads the fingerprint-keyed identity sidecar for `fingerprint` beside one
/// conflict directory. Returns `None` for missing, corrupt, oversized, or
/// fingerprint-mismatched sidecars (including a sidecar whose keyed file name
/// does not match its stored fingerprint) — absence can only reduce
/// eligibility, never grant it.
#[must_use]
pub fn read_identity_sidecar(
    dir: &Path,
    fingerprint: &str,
) -> Option<(feanorfs_common::ConflictIdentity, String)> {
    let (identity, stored) =
        read_identity_sidecar_file(&dir.join(identity_file_name(fingerprint)))?;
    (stored == fingerprint).then_some((identity, stored))
}

/// Reads and validates EVERY fingerprint-keyed identity sidecar in `dir`.
/// Corrupt, oversized, or fingerprint-mismatched sidecars are skipped —
/// absence can only reduce eligibility, never grant it.
#[must_use]
pub fn read_identity_sidecars_in_dir(
    dir: &Path,
) -> Vec<(feanorfs_common::ConflictIdentity, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(IDENTITY_FILE_PREFIX) && name.ends_with(IDENTITY_FILE_SUFFIX)
        })
        .filter_map(|entry| read_identity_sidecar_file(&entry.path()))
        .collect()
}

/// Resolve artifact path preferring new suffixes, falling back to legacy.
#[must_use]
pub fn resolve_artifact(conflict_dir: &Path, rel_path: &str, role: ArtifactRole) -> PathBuf {
    let (new_suffix, old_suffix) = match role {
        ArtifactRole::Original => (SUFFIX_ORIGINAL, SUFFIX_BASE),
        ArtifactRole::Local => (SUFFIX_LOCAL, SUFFIX_OURS),
        ArtifactRole::Cloud => (SUFFIX_CLOUD, SUFFIX_THEIRS),
    };
    let new_path = artifact_path(conflict_dir, rel_path, new_suffix);
    if new_path.exists() {
        return new_path;
    }
    artifact_path(conflict_dir, rel_path, old_suffix)
}

#[derive(Debug, Clone, Copy)]
pub enum ArtifactRole {
    Original,
    Local,
    Cloud,
}

pub async fn write_version_file(
    dest: &Path,
    state: Option<&FileState>,
    ctx: &SyncCtx<'_>,
    path: &str,
) -> Result<()> {
    write_version_file_with_policy(dest, state, ctx, path, MaterializationPolicy::BestEffort).await
}

/// Writes one leg artifact, honoring the materialization policy. The leg is
/// created exclusively (O_EXCL, no-follow) and durably: an existing
/// destination — a pre-seeded file, a symlink/reparse/hard-link planted at
/// the leg path, or another concurrent materializer's output — is never
/// replaced or followed (mirrors `write_opened_local_version` safety).
pub async fn write_version_file_with_policy(
    dest: &Path,
    state: Option<&FileState>,
    ctx: &SyncCtx<'_>,
    path: &str,
    policy: MaterializationPolicy,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    match state {
        Some(f) if f.deleted => {
            write_new_durable(dest, &sentinel("deleted")).await?;
        }
        Some(f) => match crate::large_file::read_bytes(ctx, path, &f.hash, f.size).await {
            Ok(plain) => {
                write_new_durable(dest, &plain).await?;
                crate::fs_util::apply_executable_mode(dest, f.mode).await?;
            }
            Err(e) if policy == MaterializationPolicy::BestEffort => {
                write_new_durable(dest, &sentinel(&format!("materialize-failed {e}"))).await?;
            }
            Err(e) => return Err(e),
        },
        None => {
            write_new_durable(dest, &sentinel("missing")).await?;
        }
    }
    Ok(())
}

/// Writes the canonical `.original`/`.local`/`.cloud` triple with the sync-gate
/// policy (best effort, existing labels).
pub async fn write_conflict_triple(
    dir: &Path,
    edit: &ConcurrentEdit,
    ctx: &SyncCtx<'_>,
    ours_root: Option<&crate::workspace_read::WorkspaceReadRoot>,
    ours_missing_label: &str,
) -> Result<()> {
    write_conflict_triple_with_labels(
        dir,
        edit,
        ctx,
        ours_root,
        ours_missing_label,
        "missing",
        MaterializationPolicy::BestEffort,
    )
    .await
}

/// Sole canonical triple writer with explicit labels and policy.
///
/// Absent cloud legs use `theirs_missing_label` (the `deleted` sentinel when
/// the cloud side is a deletion); absent base legs use `missing`; absent local
/// legs use `ours_missing_label`.
pub async fn write_conflict_triple_with_labels(
    dir: &Path,
    edit: &ConcurrentEdit,
    ctx: &SyncCtx<'_>,
    ours_root: Option<&crate::workspace_read::WorkspaceReadRoot>,
    ours_missing_label: &str,
    theirs_missing_label: &str,
    policy: MaterializationPolicy,
) -> Result<()> {
    let base_dest = artifact_path(dir, &edit.path, SUFFIX_ORIGINAL);
    let ours_dest = artifact_path(dir, &edit.path, SUFFIX_LOCAL);
    let theirs_dest = artifact_path(dir, &edit.path, SUFFIX_CLOUD);

    write_version_file_with_policy(&base_dest, edit.base.as_ref(), ctx, &edit.path, policy).await?;

    match (edit.ours.as_ref(), ours_root) {
        (Some(ours), _) if ours.deleted => {
            write_new_durable(&ours_dest, &sentinel("deleted-locally")).await?;
        }
        (Some(ours), Some(root)) => {
            write_opened_local_version(&ours_dest, ours, root, ctx.password_str(), &ours.path)
                .await?;
        }
        // No worktree root: the caller materializes from the encrypted tree
        // (integrator triple), exactly like the pre-consolidation path.
        (Some(ours), None) if !ours.hash.is_empty() => {
            let plain =
                crate::large_file::read_bytes(ctx, &ours.path, &ours.hash, ours.size).await?;
            write_new_durable(&ours_dest, &plain).await?;
            crate::fs_util::apply_executable_mode(&ours_dest, ours.mode).await?;
        }
        _ => {
            write_new_durable(&ours_dest, &sentinel(ours_missing_label)).await?;
        }
    }

    match edit.theirs.as_ref() {
        Some(theirs) => {
            write_version_file_with_policy(&theirs_dest, Some(theirs), ctx, &edit.path, policy)
                .await?;
        }
        None => {
            // Single create-new write: absent legs carry the custom label
            // directly (never a double write that would clobber the leg).
            write_new_durable(&theirs_dest, &sentinel(theirs_missing_label)).await?;
        }
    }
    Ok(())
}

async fn write_opened_local_version(
    destination: &Path,
    expected: &FileState,
    root: &crate::workspace_read::WorkspaceReadRoot,
    password: &str,
    relative_path: &str,
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut source = root
        .open_regular(relative_path)
        .with_context(|| format!("open local conflict version {relative_path}"))?;
    let before = source.metadata()?;
    if before.len() != expected.size || !portable_mode_matches(&before, expected.mode) {
        return Err(crate::agent::continuous::retryable_volatility_failure(
            format!("local conflict version {relative_path} changed before capture"),
        ));
    }

    let destination = destination.to_path_buf();
    let password = password.to_string();
    let relative_path = relative_path.to_string();
    let expected = expected.clone();
    let capture_path = destination.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&capture_path)?;
        let copied = std::io::copy(
            &mut (&mut source).take(expected.size.saturating_add(1)),
            &mut output,
        )?;
        if copied != expected.size {
            return Err(crate::agent::continuous::retryable_volatility_failure(
                format!(
                    "local conflict version {} changed during capture",
                    relative_path
                ),
            ));
        }
        output.flush()?;
        output.sync_all()?;
        let after = source.metadata()?;
        if after.len() != before.len()
            || after.modified().ok() != before.modified().ok()
            || !portable_mode_matches(&after, expected.mode)
        {
            return Err(crate::agent::continuous::retryable_volatility_failure(
                format!(
                    "local conflict version {} changed during capture",
                    relative_path
                ),
            ));
        }

        output.seek(std::io::SeekFrom::Start(0))?;
        let observed_hash = if crate::large_file::uses_chunk_transport(expected.size) {
            crate::large_file::fingerprint_opened(&mut output, &password, &relative_path)?
                .encrypted_hash
        } else {
            let capacity = usize::try_from(expected.size)
                .context("local conflict version does not fit memory")?;
            let mut bytes = Vec::with_capacity(capacity);
            output
                .take(expected.size.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != expected.size {
                return Err(crate::agent::continuous::retryable_volatility_failure(
                    format!(
                        "local conflict version {} changed during capture",
                        relative_path
                    ),
                ));
            }
            crate::crypto::seal(&bytes, &password, &relative_path)?.0
        };
        if observed_hash != expected.hash {
            bail!(
                "local conflict version {} no longer matches the detected edit",
                relative_path
            );
        }
        Ok(())
    })
    .await
    .context("join local conflict capture task")?;
    if let Err(error) = result {
        let _ = fs::remove_file(&destination).await;
        return Err(error);
    }
    crate::fs_util::apply_executable_mode(&destination, expected.mode).await?;
    Ok(())
}

fn portable_mode_matches(metadata: &std::fs::Metadata, expected: u32) -> bool {
    #[cfg(unix)]
    {
        portable_mode(metadata) == expected
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, expected);
        true
    }
}

fn portable_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 != 0 {
            feanorfs_common::EXECUTABLE_MODE
        } else {
            0
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

#[must_use]
pub fn enrich_conflict_edit(
    mut edit: ConcurrentEdit,
    kind: ConflictKind,
    conflict_dir: &Path,
) -> ConcurrentEdit {
    let original = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Original);
    let local = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Local);
    let cloud = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Cloud);
    edit.original_file = Some(original.to_string_lossy().into_owned());
    edit.local_file = Some(local.to_string_lossy().into_owned());
    edit.cloud_file = Some(cloud.to_string_lossy().into_owned());
    if let Some(prefix) = read_binary_prefix(&local, BINARY_PREFIX_BYTES) {
        edit.local_available = !is_sentinel_content(&prefix);
        edit.is_binary = is_binary_content(&prefix);
    }
    if let Some(prefix) = read_binary_prefix(&cloud, BINARY_PREFIX_BYTES) {
        edit.cloud_available = !is_sentinel_content(&prefix);
        if !edit.is_binary {
            edit.is_binary = is_binary_content(&prefix);
        }
    }
    set_kind_hint(&mut edit, kind);
    edit
}

#[must_use]
pub fn enrich_conflict_edit_preview(
    mut edit: ConcurrentEdit,
    kind: ConflictKind,
) -> ConcurrentEdit {
    set_kind_hint(&mut edit, kind);
    edit
}

fn set_kind_hint(edit: &mut ConcurrentEdit, kind: ConflictKind) {
    edit.kind = Some(kind);
    edit.hint = Some(format!(
        "feanorfs conflicts keep {} --local | --cloud | --both | --file <reconciled>",
        edit.path
    ));
}

#[cfg(test)]
mod tests {
    use super::{
        identity_file_name, is_cloud_deleted_sentinel, is_sentinel_content, sentinel,
        sentinel_label, write_conflict_triple_with_labels, write_new_durable,
        write_opened_local_version, write_version_file_with_policy, MaterializationPolicy,
        SENTINEL_PREFIX,
    };
    use feanorfs_common::FileState;

    fn expected(path: &str, bytes: &[u8]) -> FileState {
        FileState {
            path: path.to_string(),
            hash: crate::crypto::seal(bytes, "password", path).unwrap().0,
            size: bytes.len() as u64,
            mtime: 0,
            deleted: false,
            mode: 0,
        }
    }

    #[test]
    fn cloud_deleted_sentinel_is_recognized() {
        let deleted = format!("{SENTINEL_PREFIX}deleted>\n");
        assert!(is_sentinel_content(deleted.as_bytes()));
        assert_eq!(sentinel_label(deleted.as_bytes()), Some("deleted"));
        assert!(is_cloud_deleted_sentinel(deleted.as_bytes()));
    }

    #[test]
    fn download_failed_sentinel_is_not_cloud_deleted() {
        let failed = format!("{SENTINEL_PREFIX}download-failed offline>\n");
        assert!(is_sentinel_content(failed.as_bytes()));
        assert!(!is_cloud_deleted_sentinel(failed.as_bytes()));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn local_conflict_capture_rejects_final_symlink_substitution() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("file.txt"), b"safe").unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        let root = crate::workspace_read::WorkspaceReadRoot::open(base.path()).unwrap();
        std::fs::rename(base.path().join("file.txt"), base.path().join("held.txt")).unwrap();
        symlink(
            outside.path().join("secret.txt"),
            base.path().join("file.txt"),
        )
        .unwrap();
        let destination = base.path().join("artifact.local");

        write_opened_local_version(
            &destination,
            &expected("file.txt", b"safe"),
            &root,
            "password",
            "file.txt",
        )
        .await
        .expect_err("final symlink substitution must fail closed");
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_conflict_capture_rejects_ancestor_symlink_substitution() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join("dir")).unwrap();
        std::fs::write(base.path().join("dir/file.txt"), b"safe").unwrap();
        std::fs::write(outside.path().join("file.txt"), b"secret").unwrap();
        let root = crate::workspace_read::WorkspaceReadRoot::open(base.path()).unwrap();
        std::fs::rename(base.path().join("dir"), base.path().join("held-dir")).unwrap();
        symlink(outside.path(), base.path().join("dir")).unwrap();
        let destination = base.path().join("artifact.local");

        write_opened_local_version(
            &destination,
            &expected("dir/file.txt", b"safe"),
            &root,
            "password",
            "dir/file.txt",
        )
        .await
        .expect_err("ancestor symlink substitution must fail closed");
        assert!(!destination.exists());
    }
    #[tokio::test]
    async fn local_conflict_capture_uses_the_local_legs_path_bound_identity() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("Foo"), b"local-case").unwrap();
        let root = crate::workspace_read::WorkspaceReadRoot::open(base.path()).unwrap();
        let destination = base.path().join("foo.local");
        let local = expected("Foo", b"local-case");

        write_opened_local_version(&destination, &local, &root, "password", &local.path)
            .await
            .unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"local-case");
    }

    /// Minimal harness for version-leg write tests: a context whose API is
    /// never exercised by the `None`/missing leg paths under test.
    struct VersionLegHarness {
        _hub_data: tempfile::TempDir,
        base: tempfile::TempDir,
        api: crate::api::ApiClient,
        db: crate::local::ClientDb,
    }

    impl VersionLegHarness {
        async fn new() -> Self {
            let hub_data = tempfile::tempdir().unwrap();
            let base = tempfile::tempdir().unwrap();
            let hub = crate::hub::LocalHub::open(hub_data.path().to_path_buf(), None)
                .await
                .unwrap();
            let api = crate::api::ApiClient::local(std::sync::Arc::clone(&hub), None);
            let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
            let db = crate::local::ClientDb::new(&state).await.unwrap();
            Self {
                _hub_data: hub_data,
                base,
                api,
                db,
            }
        }

        fn ctx(&self) -> crate::ctx::SyncCtx<'_> {
            crate::ctx::SyncCtx::with_format_version(
                &self.api,
                &self.db,
                self.base.path(),
                "workspace",
                Some("shared-key"),
                feanorfs_common::LegacyPolicy::Reject,
                3,
            )
        }
    }

    /// Version legs are created exclusively (O_EXCL, no-follow): a symlink
    /// planted at the leg path is never followed or replaced.
    #[cfg(unix)]
    #[tokio::test]
    async fn version_legs_write_create_new_and_refuse_symlink_substitution() {
        use std::os::unix::fs::symlink;

        let h = VersionLegHarness::new().await;
        let ctx = h.ctx();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, b"secret").unwrap();
        let dest = ctx.base.join("file.txt.cloud");
        symlink(&secret, &dest).unwrap();

        write_version_file_with_policy(
            &dest,
            None,
            &ctx,
            "file.txt",
            MaterializationPolicy::Strict,
        )
        .await
        .expect_err("symlink substitution at the leg path must fail closed");
        assert!(
            std::fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the planted symlink must survive untouched"
        );
        assert_eq!(
            std::fs::read(&secret).unwrap(),
            b"secret",
            "the symlink target must never be written through"
        );
    }

    /// Version legs are created exclusively: an existing destination (e.g. a
    /// concurrent materializer's leg) is never replaced.
    #[tokio::test]
    async fn version_legs_write_create_new_and_refuse_existing_destination() {
        let h = VersionLegHarness::new().await;
        let ctx = h.ctx();
        let dest = ctx.base.join("file.txt.cloud");
        std::fs::write(&dest, b"pre-existing").unwrap();

        write_version_file_with_policy(
            &dest,
            None,
            &ctx,
            "file.txt",
            MaterializationPolicy::Strict,
        )
        .await
        .expect_err("an existing leg destination must fail closed");
        assert_eq!(std::fs::read(&dest).unwrap(), b"pre-existing");
    }

    /// The create-new primitive itself: a name claimed by another writer
    /// (allocator race) is never replaced.
    #[tokio::test]
    async fn write_new_durable_create_new_race_preserves_other_writer_bytes() {
        let base = tempfile::tempdir().unwrap();
        let dest = base.path().join("copy.txt");
        std::fs::write(&dest, b"other writer").unwrap();

        let error = write_new_durable(&dest, b"replacement")
            .await
            .expect_err("create-new must refuse an existing destination");
        assert!(
            error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
            }),
            "unexpected error: {error:#}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"other writer");
    }

    /// Absent legs with a custom label are written exactly once with that
    /// label (create-new): the triple never double-writes a leg.
    #[tokio::test]
    async fn absent_theirs_leg_writes_custom_label_once() {
        let h = VersionLegHarness::new().await;
        let ctx = h.ctx();
        let dir = tempfile::tempdir().unwrap();
        let edit = feanorfs_common::ConcurrentEdit::new("a.txt".to_string(), None, None, None);

        write_conflict_triple_with_labels(
            dir.path(),
            &edit,
            &ctx,
            None,
            "deleted-locally",
            "deleted",
            MaterializationPolicy::Strict,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("a.txt.original")).unwrap(),
            sentinel("missing")
        );
        assert_eq!(
            std::fs::read(dir.path().join("a.txt.local")).unwrap(),
            sentinel("deleted-locally")
        );
        assert_eq!(
            std::fs::read(dir.path().join("a.txt.cloud")).unwrap(),
            sentinel("deleted")
        );
    }

    #[test]
    fn identity_file_name_is_fingerprint_keyed_and_stable() {
        let fingerprint = "ab".repeat(32);
        let name = identity_file_name(&fingerprint);
        assert_eq!(name, "identity-abababababababababababababababab.json");
        assert_eq!(identity_file_name(&fingerprint), name);
        // A different fingerprint maps to a different sidecar name.
        let other = format!("bb{}", &fingerprint[..62]);
        assert_ne!(identity_file_name(&other), name);
        // Short/malformed fingerprints never panic and stay bounded.
        assert_eq!(identity_file_name(""), "identity-.json");
    }
}
