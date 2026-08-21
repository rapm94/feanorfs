use crate::conflict_artifacts::{
    is_cloud_deleted_sentinel, is_sentinel_content, resolve_artifact, write_conflict_triple,
    write_new_durable, ArtifactRole,
};
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::fs_util::{apply_executable_mode, atomic_write_visible};
use crate::local::ClientDb;
use crate::paths::conflicts_dir;
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    conflict_candidate_paths, detect_concurrent_edits, is_safe_rel_path, ConcurrentEdit,
    ConflictKind, FileState, SyncRequest, SyncResponse,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;

fn file_changed_since(base: Option<&FileState>, current: &FileState) -> bool {
    match base {
        None => true,
        Some(base) => {
            current.hash != base.hash
                || current.deleted != base.deleted
                || current.mode != base.mode
        }
    }
}

fn live_state(state: Option<&FileState>) -> Option<&FileState> {
    state.filter(|state| !state.deleted)
}

fn same_content(left: Option<&FileState>, right: Option<&FileState>) -> bool {
    match (live_state(left), live_state(right)) {
        (Some(left), Some(right)) => left.hash == right.hash && left.mode == right.mode,
        (None, None) => true,
        _ => false,
    }
}

async fn load_server_view_with_head(
    ctx: &SyncCtx<'_>,
) -> Result<(Option<String>, HashMap<String, FileState>)> {
    if ctx.format_version() >= 3 {
        let head = ctx.api.get_head(ctx.workspace_id()).await?;
        let files = match head.as_deref() {
            Some(head) => {
                crate::snapshot::SnapshotEngine::new(ctx)
                    .load_files(head)
                    .await?
            }
            None => HashMap::new(),
        };
        return Ok((head, files));
    }
    let response = ctx
        .api
        .peek_sync(&SyncRequest {
            workspace_id: ctx.workspace_id().to_string(),
            files: Vec::new(),
        })
        .await?;
    Ok((
        None,
        response
            .download_required
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect(),
    ))
}

/// Load the complete active server view without relying on LWW delta direction.
pub async fn load_server_view(ctx: &SyncCtx<'_>) -> Result<HashMap<String, FileState>> {
    Ok(load_server_view_with_head(ctx).await?.1)
}

pub async fn load_last_synced_snapshot(ctx: &SyncCtx<'_>) -> Result<HashMap<String, FileState>> {
    crate::snapshot::SnapshotEngine::new(ctx)
        .load_last_synced()
        .await
}

pub async fn pending_conflict_paths(db: &ClientDb) -> Result<HashSet<String>> {
    Ok(db
        .list_pending_conflict_paths()
        .await?
        .into_iter()
        .collect())
}

pub fn conflicts_pending(pending_paths: Option<&HashSet<String>>) -> bool {
    pending_paths.is_some_and(|p| !p.is_empty())
}

pub async fn detect_workspace_conflicts(
    ctx: &SyncCtx<'_>,
    last_synced: &HashMap<String, FileState>,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
    already_pending: &HashSet<String>,
) -> Result<Vec<(ConcurrentEdit, ConflictKind)>> {
    let server_files = load_server_view(ctx).await?;
    detect_workspace_conflicts_with_server_view(
        last_synced,
        local_files,
        response,
        already_pending,
        &server_files,
    )
}

fn detect_workspace_conflicts_with_server_view(
    last_synced: &HashMap<String, FileState>,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
    already_pending: &HashSet<String>,
    server_files: &HashMap<String, FileState>,
) -> Result<Vec<(ConcurrentEdit, ConflictKind)>> {
    let their_changed: HashMap<String, FileState> = server_files
        .iter()
        .filter(|(path, remote)| !same_content(Some(remote), last_synced.get(*path)))
        .map(|(path, remote)| (path.clone(), remote.clone()))
        .collect();
    let their_deleted: HashSet<String> = last_synced
        .iter()
        .filter(|(path, base)| !base.deleted && !server_files.contains_key(*path))
        .map(|(path, _)| path.clone())
        .collect();

    let candidates = conflict_candidate_paths(response, already_pending)
        .into_iter()
        .chain(their_changed.keys().cloned())
        .chain(their_deleted.iter().cloned());
    let mut edits = detect_concurrent_edits(
        last_synced,
        local_files,
        &their_changed,
        &their_deleted,
        candidates,
        already_pending,
    );
    edits.retain(|(c, _)| is_safe_rel_path(&c.path));
    Ok(edits)
}

pub async fn register_and_write_conflicts(
    ctx: &SyncCtx<'_>,
    items: &[(ConcurrentEdit, ConflictKind)],
    ours_base: Option<&Path>,
) -> Result<(PathBuf, HashSet<String>)> {
    let ts = chrono::Utc::now().timestamp_millis();
    let dir = conflicts_dir(ctx.base)?.join(ts.to_string());
    fs::create_dir_all(&dir).await?;

    let local_root = ours_base.unwrap_or(ctx.base);
    let ours_reader = crate::workspace_read::WorkspaceReadRoot::open(local_root)?;
    for (edit, kind) in items {
        let ours_label = ours_missing_label(kind);
        write_conflict_triple(&dir, edit, ctx, Some(&ours_reader), ours_label).await?;
    }

    let paths: Vec<String> = items.iter().map(|(c, _)| c.path.clone()).collect();
    fs::write(dir.join("manifest.json"), serde_json::to_string(&paths)?).await?;

    let mut out = HashSet::new();
    let head = if ctx.format_version() >= 3 {
        ctx.api.get_head(ctx.workspace_id()).await?
    } else {
        None
    };
    let tree_root = match head.as_deref() {
        Some(head) => crate::snapshot::SnapshotEngine::new(ctx)
            .load_snapshot(head)
            .await
            .ok()
            .map(|snapshot| snapshot.root),
        None => None,
    };
    for (c, kind) in items {
        let dir_string = dir.to_string_lossy().to_string();
        match (&head, &tree_root) {
            (Some(head), Some(tree_root)) => {
                let identity = crate::conflict_artifacts::conflict_identity_from_edit(
                    ctx.workspace_id(),
                    head,
                    head,
                    tree_root,
                    c,
                    *kind,
                    &crate::conflict_artifacts::IdentityBinding::default(),
                );
                let fingerprint = feanorfs_common::compute_conflict_identity_fingerprint(&identity);
                ctx.db
                    .upsert_conflict_fingerprinted(
                        &c.path,
                        kind,
                        &dir_string,
                        ts,
                        &identity,
                        &fingerprint,
                    )
                    .await?;
            }
            _ => {
                ctx.db
                    .upsert_conflict(
                        &c.path,
                        kind,
                        &dir_string,
                        ts,
                        crate::state::ConflictRecordStatus::Pending,
                    )
                    .await?;
            }
        }
        out.insert(c.path.clone());
    }

    Ok((dir, out))
}

fn ours_missing_label(kind: &ConflictKind) -> &'static str {
    match kind {
        ConflictKind::EditDelete => "deleted-locally",
        ConflictKind::DeleteEdit => "deleted-locally",
        ConflictKind::EditEdit => "no-local-snapshot",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKeep {
    Local,
    Cloud,
    Both,
    File,
}

fn conflict_artifact_paths(conflict_dir: &Path, path: &str) -> [PathBuf; 3] {
    [
        resolve_artifact(conflict_dir, path, ArtifactRole::Original),
        resolve_artifact(conflict_dir, path, ArtifactRole::Local),
        resolve_artifact(conflict_dir, path, ArtifactRole::Cloud),
    ]
}

async fn remove_path_artifacts(conflict_dir: &Path, path: &str) -> Result<()> {
    for artifact in conflict_artifact_paths(conflict_dir, path) {
        if artifact.is_file() {
            fs::remove_file(&artifact).await?;
        }
    }
    Ok(())
}

/// Forget a conflict for a path that transport policy now excludes. Local
/// working bytes are deliberately untouched.
pub async fn discard_excluded_conflict(db: &ClientDb, path: &str) -> Result<()> {
    let Some(record) = db.get_conflict_record(path).await? else {
        return Ok(());
    };
    let conflict_dir = PathBuf::from(&record.conflict_dir);
    remove_path_artifacts(&conflict_dir, path).await?;
    db.resolve_conflict_path(path).await?;
    if db.count_pending_in_dir(&record.conflict_dir).await? == 0 && conflict_dir.is_dir() {
        if let Err(error) = fs::remove_dir_all(&conflict_dir).await {
            tracing::warn!(
                "Could not remove resolved conflict directory {}: {error}",
                conflict_dir.display()
            );
        }
    }
    Ok(())
}

async fn current_head_state(ctx: &SyncCtx<'_>) -> Result<Option<crate::objects::LoadedTree>> {
    if ctx.format_version() < 3 {
        return Ok(None);
    }
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Ok(None);
    };
    Ok(Some(
        crate::snapshot::SnapshotEngine::new(ctx)
            .load_state(&head)
            .await?,
    ))
}

fn cloud_mode(state: Option<&crate::objects::LoadedTree>, path: &str) -> Option<u32> {
    let state = state?;
    if let Some(conflict) = state
        .conflicts
        .iter()
        .find(|conflict| conflict.path == path)
    {
        return conflict
            .theirs
            .as_ref()
            .filter(|leg| !leg.deleted)
            .map(|leg| leg.mode);
    }
    state.files.get(path).map(|file| file.mode)
}

async fn portable_disk_mode(path: &Path) -> Result<u32> {
    let metadata = fs::metadata(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(if metadata.permissions().mode() & 0o111 != 0 {
            feanorfs_common::EXECUTABLE_MODE
        } else {
            0
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(0)
    }
}

pub async fn resolve_conflict(
    ctx: &SyncCtx<'_>,
    path: &str,
    keep: ResolveKeep,
    file_source: Option<&Path>,
) -> Result<()> {
    if !is_safe_rel_path(path) {
        return Err(crate::agent::continuous::unsafe_path_failure(format!(
            "unsafe path: {path}"
        )));
    }
    let record = match ctx.db.get_conflict_record(path).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return Err(crate::agent::continuous::conflict_failure(format!(
                "no pending conflict for {path}"
            )))
        }
        Err(error) => return Err(error),
    };
    let before = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    crate::snapshot::SnapshotEngine::new(ctx)
        .snapshot_local_view(&before, "you")
        .await?;
    let head_state = current_head_state(ctx).await?;
    let conflict_dir = PathBuf::from(&record.conflict_dir);
    let mut additional_paths = Vec::new();
    let mut selected_modes = HashMap::new();

    match keep {
        ResolveKeep::File => {
            let src = file_source.with_context(|| "conflicts keep --file requires a path")?;
            let content = fs::read(src).await?;
            let mode = portable_disk_mode(src).await?;
            atomic_write_visible(ctx.base, path, &content).await?;
            apply_executable_mode(&ctx.base.join(path), mode).await?;
            upload_sealed(
                ctx,
                path,
                &content,
                chrono::Utc::now().timestamp_millis(),
                mode,
            )
            .await?;
            selected_modes.insert(path.to_string(), mode);
        }
        ResolveKeep::Local => {
            if let Some(state) = before.get(path).filter(|state| !state.deleted) {
                let plain = crate::sync_pass::read_upload_source(&read_root, path, state).await?;
                upload_sealed(ctx, path, &plain, state.mtime, state.mode).await?;
                selected_modes.insert(path.to_string(), state.mode);
            } else {
                upload_tombstone_for(ctx, path).await?;
            }
        }
        ResolveKeep::Cloud => {
            let mode = cloud_mode(head_state.as_ref(), path).unwrap_or(0);
            let theirs_file = resolve_artifact(&conflict_dir, path, ArtifactRole::Cloud);
            if theirs_file.exists() {
                let content = fs::read(&theirs_file).await?;
                if is_cloud_deleted_sentinel(&content) {
                    let ours_path = ctx.base.join(path);
                    if ours_path.exists() {
                        fs::remove_file(&ours_path).await?;
                    }
                    upload_tombstone_for(ctx, path).await?;
                } else if is_sentinel_content(&content) {
                    bail!("theirs version unavailable on disk; re-run sync while online");
                } else {
                    atomic_write_visible(ctx.base, path, &content).await?;
                    apply_executable_mode(&ctx.base.join(path), mode).await?;
                    upload_sealed(
                        ctx,
                        path,
                        &content,
                        chrono::Utc::now().timestamp_millis(),
                        mode,
                    )
                    .await?;
                    selected_modes.insert(path.to_string(), mode);
                }
            } else {
                bail!("cloud version artifact missing for {path}");
            }
        }
        ResolveKeep::Both => {
            let theirs_file = resolve_artifact(&conflict_dir, path, ArtifactRole::Cloud);
            let mut occupied: HashSet<String> = before.keys().cloned().collect();
            occupied.extend(additional_paths.iter().cloned());
            occupied.insert(path.to_string());
            let alt_path = allocate_conflict_copy_name(ctx.base, path, &occupied)?;
            if let Some(state) = before.get(path).filter(|state| !state.deleted) {
                let content = crate::sync_pass::read_upload_source(&read_root, path, state).await?;
                upload_sealed(ctx, path, &content, state.mtime, state.mode).await?;
                selected_modes.insert(path.to_string(), state.mode);
            }
            if theirs_file.exists() {
                let content = fs::read(&theirs_file).await?;
                if !is_sentinel_content(&content) {
                    let mode = cloud_mode(head_state.as_ref(), path).unwrap_or(0);
                    // Create-new (O_EXCL, no-follow): a concurrent
                    // materializer that already claimed the allocated copy
                    // name must never have its copy replaced.
                    write_new_durable(&ctx.base.join(&alt_path), &content).await?;
                    apply_executable_mode(&ctx.base.join(&alt_path), mode).await?;
                    upload_sealed(
                        ctx,
                        &alt_path,
                        &content,
                        chrono::Utc::now().timestamp_millis(),
                        mode,
                    )
                    .await?;
                    selected_modes.insert(alt_path.clone(), mode);
                    additional_paths.push(alt_path);
                }
            }
        }
    }

    let mut resolved_files = if ctx.format_version() >= 3 {
        crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?
    } else {
        load_server_view(ctx).await?
    };
    for (selected_path, mode) in selected_modes {
        if let Some(state) = resolved_files.get_mut(&selected_path) {
            state.mode = mode;
        }
    }
    let resolver = std::env::var("FEANORFS_AGENT").unwrap_or_else(|_| "human".into());
    let fingerprinted = ctx.db.is_conflict_fingerprinted(path).await?;
    if fingerprinted && ctx.format_version() >= 3 {
        // Guarded publication for fingerprinted records: revalidate the exact
        // identity and CAS exactly once per validated plan. On ANY failure —
        // including a lost CAS — the conflict and its registry record are
        // preserved for manual action; there is NO fallback to the legacy
        // path-removal publication, which could silently publish a stale
        // choice against a changed conflict.
        publish_manual_resolution(ctx, path, &resolved_files, &additional_paths, &resolver).await?;
    } else {
        // Legacy (unfingerprinted) records keep the legacy path-removal
        // publication: no identity to revalidate, so it retries on head
        // movement exactly like the pre-fingerprint behavior.
        crate::snapshot::SnapshotEngine::new(ctx)
            .resolve_conflict(path, &resolved_files, &additional_paths, &resolver)
            .await?;
    }

    ctx.db.resolve_conflict_path(path).await?;
    remove_path_artifacts(&conflict_dir, path).await?;

    let method = match keep {
        ResolveKeep::Local => crate::state::ResolutionMethod::Local,
        ResolveKeep::Cloud => crate::state::ResolutionMethod::Cloud,
        ResolveKeep::Both => crate::state::ResolutionMethod::Both,
        ResolveKeep::File => crate::state::ResolutionMethod::File,
    };
    let source_hash = file_source
        .and_then(|p| std::fs::read(p).ok())
        .map(|b| feanorfs_common::hash_bytes(&b));
    ctx.db
        .record_conflict_resolution(path, method, source_hash.as_deref(), &resolver)
        .await?;

    if ctx.db.count_pending_in_dir(&record.conflict_dir).await? == 0 && conflict_dir.is_dir() {
        if let Err(e) = fs::remove_dir_all(&conflict_dir).await {
            tracing::warn!(
                "failed to clean conflict dir {}: {e}",
                conflict_dir.display()
            );
        }
    }
    Ok(())
}

pub async fn resolve_all_local_conflicts(ctx: &SyncCtx<'_>) -> Result<Vec<String>> {
    resolve_all_conflicts(ctx, ResolveKeep::Local).await
}

/// Full-validation restarts allowed after a lost CAS during guarded manual
/// publication (mirrors `RESOLUTION_MAX_APPLY_REVALIDATIONS` in
/// resolution.rs: each restart revalidates everything from the new head).
const MANUAL_PUBLISH_MAX_REVALIDATIONS: usize = 3;

/// Marker error: the fingerprinted conflict is absent from the current head.
/// Client-side registrations never publish the conflict to the head (it
/// lives in the local registry + artifacts only), so the guarded publication
/// cannot revalidate against it. The caller verifies the head still matches
/// the fingerprinted conflict's remote leg before publishing the resolved
/// state — this is never a path-only publication on a lost CAS.
#[derive(Debug)]
struct ConflictMissingFromHead {
    path: String,
}

impl std::fmt::Display for ConflictMissingFromHead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "conflict at '{}' no longer exists in the current head",
            self.path
        )
    }
}

impl std::error::Error for ConflictMissingFromHead {}

/// Guarded publication for one explicit manual choice over a fingerprinted
/// record: rebuilds the exact identity from the current head, binds the
/// already-uploaded resolved file state as the candidate, and performs one
/// revalidated CAS via [`SnapshotEngine::publish_resolution`].
///
/// On a lost CAS the plan is discarded and ALL validation restarts from the
/// NEW head: the identity/fingerprint are recomputed and must describe the
/// SAME conflict the manual choice was made against. If the conflict or
/// identity changed (or disappeared), publication fails closed with a typed
/// error and the registry record + artifacts are preserved for manual
/// action — a stale choice is never published against a changed conflict.
///
/// When the conflict is absent from the current head (client-side
/// registrations never publish the conflict to the head), the head is first
/// verified against the fingerprinted remote leg; only a matching head is
/// published — this is not a path-only fallback on any failure.
async fn publish_manual_resolution(
    ctx: &SyncCtx<'_>,
    path: &str,
    resolved_files: &HashMap<String, FileState>,
    additional_paths: &[String],
    resolver: &str,
) -> Result<()> {
    let engine = crate::snapshot::SnapshotEngine::new(ctx);
    let mut previous: Option<feanorfs_common::ConflictIdentity> = None;
    let mut last_error: Option<anyhow::Error> = None;
    for _ in 0..MANUAL_PUBLISH_MAX_REVALIDATIONS {
        let plan = match build_manual_publication_plan(
            ctx,
            path,
            resolved_files,
            additional_paths,
            resolver,
        )
        .await
        {
            Ok(plan) => plan,
            Err(error) if error.downcast_ref::<ConflictMissingFromHead>().is_some() => {
                // The conflict is absent from the current head. Client-side
                // registrations never publish the conflict to the head, so
                // this is the expected shape for a manual keep. Verify the
                // head still matches the fingerprinted conflict's remote
                // leg; only then publish the resolved state. Any other
                // failure (lost CAS, changed legs, identity mismatch, a
                // moved remote) fails closed with the conflict and registry
                // preserved.
                verify_fingerprinted_remote_unchanged(ctx, path).await?;
                return engine
                    .resolve_conflict(path, resolved_files, additional_paths, resolver)
                    .await
                    .map(|_| ());
            }
            Err(error) => return Err(error),
        };
        if let Some(previous) = &previous {
            // Revalidation from the new head: the conflict must still be the
            // exact conflict the manual choice was made against.
            if !same_conflict_identity(previous, &plan.identity) {
                return Err(crate::agent::continuous::conflict_failure(format!(
                    "conflict at '{path}' changed after the manual choice; \
                     refusing to publish — resolve the new conflict manually"
                )));
            }
        }
        previous = Some(plan.identity.clone());
        match engine.publish_resolution(plan).await {
            Ok(_) => return Ok(()),
            Err(error) if error.downcast_ref::<crate::snapshot::LostCas>().is_some() => {
                // Discard the plan and restart ALL validation from the new head.
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        crate::agent::continuous::retryable_volatility_failure(
            "workspace head changed too many times during guarded manual publication",
        )
    }))
}

/// Rebuilds one guarded-publication plan for a manual choice from the
/// CURRENT head: revalidates the conflict exists at `path`, recomputes its
/// full identity/fingerprint, and binds the already-uploaded resolved state
/// as the candidate.
async fn build_manual_publication_plan(
    ctx: &SyncCtx<'_>,
    path: &str,
    resolved_files: &HashMap<String, FileState>,
    additional_paths: &[String],
    resolver: &str,
) -> Result<crate::snapshot::ResolutionPublication> {
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Err(crate::agent::continuous::conflict_failure(format!(
            "workspace head disappeared during manual guarded publication of '{path}'"
        )));
    };
    let engine = crate::snapshot::SnapshotEngine::new(ctx);
    let snapshot = engine.load_snapshot(&head).await?;
    let state = engine.objects.get_tree_state(&snapshot.root).await?;
    let Some(conflict) = state
        .conflicts
        .iter()
        .find(|candidate| candidate.path == path)
    else {
        return Err(anyhow::Error::new(ConflictMissingFromHead {
            path: path.to_string(),
        }));
    };
    let identity = crate::conflict_artifacts::conflict_identity_from_edit(
        ctx.workspace_id(),
        &head,
        &head,
        &snapshot.root,
        conflict,
        ConflictKind::EditEdit,
        &crate::conflict_artifacts::IdentityBinding::default(),
    );
    let fingerprint = feanorfs_common::compute_conflict_identity_fingerprint(&identity);

    let state_file = resolved_files.get(path).cloned();
    let candidate = match &state_file {
        Some(file) if !file.deleted => {
            let (bytes, _) = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?
                .read_regular_stable(path, feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES)
                .await?;
            feanorfs_common::CandidateDescriptor {
                path: path.to_string(),
                hash: feanorfs_common::hash_bytes(&bytes),
                size: bytes.len() as u64,
                mode: file.mode,
                deleted: false,
            }
        }
        _ => feanorfs_common::CandidateDescriptor {
            path: path.to_string(),
            hash: String::new(),
            size: 0,
            mode: 0,
            deleted: true,
        },
    };
    let mut additional = Vec::new();
    for additional_path in additional_paths {
        if let Some(file) = resolved_files.get(additional_path) {
            additional.push((additional_path.clone(), file.clone()));
        }
    }
    Ok(crate::snapshot::ResolutionPublication {
        identity,
        fingerprint,
        candidate: Some(candidate),
        candidate_file: None,
        manual_state: state_file,
        additional,
        expected_head: head,
        author: resolver.to_string(),
    })
}

/// Whether two identities describe the SAME conflict, ignoring the
/// head-dependent snapshot/tree fields that legitimately move as the head
/// advances: same workspace, path, legs, derived kind, and automatic-binding
/// fields.
fn same_conflict_identity(
    left: &feanorfs_common::ConflictIdentity,
    right: &feanorfs_common::ConflictIdentity,
) -> bool {
    left.workspace_id == right.workspace_id
        && left.path == right.path
        && left.base == right.base
        && left.ours == right.ours
        && left.theirs == right.theirs
        && left.kind == right.kind
        && left.task_id == right.task_id
        && left.intent_message_ids == right.intent_message_ids
        && left.assignment_id == right.assignment_id
        && left.attempt == right.attempt
        && left.designated_owner == right.designated_owner
        && left.verification_policy == right.verification_policy
}

/// For a fingerprinted record whose conflict is absent from the current
/// head, verifies the head still matches the conflict's remote (theirs) leg
/// recorded in the identity sidecar — the conflict's premise is unchanged.
/// Fails closed (typed) when the workspace moved: the resolution is refused
/// and the registry record + artifacts stay for manual action.
async fn verify_fingerprinted_remote_unchanged(ctx: &SyncCtx<'_>, path: &str) -> Result<()> {
    let Some(record) = ctx.db.get_conflict_record(path).await? else {
        return Err(crate::agent::continuous::conflict_failure(format!(
            "no pending conflict for {path}"
        )));
    };
    let dir = PathBuf::from(&record.conflict_dir);
    let Some((identity, _)) = crate::conflict_artifacts::read_identity_sidecars_in_dir(&dir)
        .into_iter()
        .find(|(identity, _)| identity.path == path)
    else {
        return Err(crate::agent::continuous::conflict_failure(format!(
            "conflict at '{path}' no longer exists in the current head"
        )));
    };
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Err(crate::agent::continuous::conflict_failure(format!(
            "workspace head disappeared during manual guarded publication of '{path}'"
        )));
    };
    let state = crate::snapshot::SnapshotEngine::new(ctx)
        .load_state(&head)
        .await?;
    let head_file = state.files.get(path);
    let remote_unchanged = if identity.theirs.deleted {
        head_file.is_none() || head_file.is_some_and(|file| file.deleted)
    } else {
        head_file.is_some_and(|file| {
            file.hash == identity.theirs.hash && file.mode == identity.theirs.mode
        })
    };
    if !remote_unchanged {
        return Err(crate::agent::continuous::conflict_failure(format!(
            "conflict at '{path}' no longer exists in the current head and the workspace no \
             longer matches the fingerprinted conflict; refusing to publish — re-sync and \
             resolve the new conflict manually"
        )));
    }
    Ok(())
}

pub async fn resolve_all_cloud_conflicts(ctx: &SyncCtx<'_>) -> Result<Vec<String>> {
    resolve_all_conflicts(ctx, ResolveKeep::Cloud).await
}

async fn resolve_all_conflicts(ctx: &SyncCtx<'_>, keep: ResolveKeep) -> Result<Vec<String>> {
    if !matches!(keep, ResolveKeep::Local | ResolveKeep::Cloud) {
        bail!("bulk conflict resolution supports only local or cloud choices");
    }
    let records = ctx.db.list_conflict_records().await?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    for record in &records {
        if !is_safe_rel_path(&record.path) {
            return Err(crate::agent::continuous::unsafe_path_failure(format!(
                "unsafe path: {}",
                record.path
            )));
        }
    }

    let head_state = current_head_state(ctx).await?;
    let mut selected_modes = HashMap::new();
    let mut cloud_actions = Vec::new();
    if keep == ResolveKeep::Cloud {
        for record in &records {
            let artifact = resolve_artifact(
                Path::new(&record.conflict_dir),
                &record.path,
                ArtifactRole::Cloud,
            );
            let content = fs::read(&artifact)
                .await
                .with_context(|| format!("cloud version unavailable for {}", record.path))?;
            if is_cloud_deleted_sentinel(&content) {
                cloud_actions.push((record.path.clone(), None, 0));
            } else if is_sentinel_content(&content) {
                bail!(
                    "cloud version unavailable for {}; re-run sync while online",
                    record.path
                );
            } else {
                let mode = cloud_mode(head_state.as_ref(), &record.path).unwrap_or(0);
                cloud_actions.push((record.path.clone(), Some(content), mode));
            }
        }
    }

    let before = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    crate::snapshot::SnapshotEngine::new(ctx)
        .snapshot_local_view(&before, "you")
        .await?;

    if keep == ResolveKeep::Local {
        for record in &records {
            if let Some(state) = before.get(&record.path).filter(|state| !state.deleted) {
                let plain =
                    crate::sync_pass::read_upload_source(&read_root, &record.path, state).await?;
                upload_sealed(ctx, &record.path, &plain, state.mtime, state.mode).await?;
                selected_modes.insert(record.path.clone(), state.mode);
            } else {
                upload_tombstone_for(ctx, &record.path).await?;
            }
        }
    } else {
        for (path, content, mode) in cloud_actions {
            let destination = ctx.base.join(&path);
            match content {
                Some(content) => {
                    atomic_write_visible(ctx.base, &path, &content).await?;
                    apply_executable_mode(&destination, mode).await?;
                    selected_modes.insert(path.clone(), mode);
                }
                None => match fs::remove_file(&destination).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                },
            }
        }
    }

    let mut resolved_files = if ctx.format_version() >= 3 {
        crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?
    } else {
        load_server_view(ctx).await?
    };
    for (selected_path, mode) in selected_modes {
        if let Some(state) = resolved_files.get_mut(&selected_path) {
            state.mode = mode;
        }
    }
    let resolver = std::env::var("FEANORFS_AGENT").unwrap_or_else(|_| "human".into());
    let paths: Vec<String> = records.iter().map(|record| record.path.clone()).collect();
    crate::snapshot::SnapshotEngine::new(ctx)
        .resolve_conflicts(&paths, &resolved_files, &[], &resolver)
        .await?;
    ctx.db
        .resolve_conflict_paths_with_history(
            &paths,
            if keep == ResolveKeep::Local {
                crate::state::ResolutionMethod::Local
            } else {
                crate::state::ResolutionMethod::Cloud
            },
            &resolver,
        )
        .await?;

    let conflict_dirs: HashSet<PathBuf> = records
        .iter()
        .map(|record| PathBuf::from(&record.conflict_dir))
        .collect();
    for conflict_dir in conflict_dirs {
        if conflict_dir.is_dir() {
            if let Err(error) = fs::remove_dir_all(&conflict_dir).await {
                tracing::warn!(
                    "failed to clean conflict dir {}: {error}",
                    conflict_dir.display()
                );
            }
        }
    }
    Ok(paths)
}

async fn upload_sealed(
    ctx: &SyncCtx<'_>,
    path: &str,
    content: &[u8],
    mtime: i64,
    mode: u32,
) -> Result<()> {
    if crate::large_file::uses_chunk_transport(content.len() as u64) {
        if ctx.format_version() < 3 {
            return Err(crate::agent::continuous::unsupported_schema_failure(
                "large-file conflict resolution requires format v3; run `feanorfs migrate`",
            ));
        }
        let fingerprint = crate::large_file::fingerprint(ctx.base, ctx.password_str(), path)?;
        crate::large_file::upload(ctx, path, &fingerprint.encrypted_hash).await?;
        return Ok(());
    }
    let (hash, packed) = seal(content, ctx.password_str(), path)?;
    let file = FileState {
        path: path.to_string(),
        hash,
        size: content.len() as u64,
        mtime,
        deleted: false,
        mode,
    };
    if ctx.format_version() >= 3 {
        ctx.api
            .upload_object(ctx.workspace_id(), &file.hash, packed)
            .await
    } else {
        ctx.api.upload_file(ctx.workspace_id(), &file, packed).await
    }
}

async fn upload_tombstone_for(ctx: &SyncCtx<'_>, path: &str) -> Result<()> {
    let cached = ctx.db.get_cache_entries().await?;
    let hash = cached
        .get(path)
        .map(|c| c.encrypted_hash.clone())
        .unwrap_or_else(|| feanorfs_common::hash_bytes(b""));
    let mtime = chrono::Utc::now().timestamp_millis();
    if ctx.format_version() >= 3 {
        return Ok(());
    }
    ctx.api
        .upload_tombstone(ctx.workspace_id(), path, &hash, mtime)
        .await
}

pub async fn seed_last_synced_from_server(
    ctx: &SyncCtx<'_>,
    local_files: &HashMap<String, FileState>,
) -> Result<u32> {
    let mut synced = load_last_synced_snapshot(ctx).await?;
    let before = synced.len();
    let server_files = load_server_view(ctx).await?;
    for (path, local) in local_files {
        if !is_safe_rel_path(path) {
            continue;
        }
        if let Some(remote) = server_files.get(path) {
            if same_content(Some(local), Some(remote)) {
                synced.insert(path.clone(), remote.clone());
            }
        }
    }
    crate::snapshot::SnapshotEngine::new(ctx)
        .record_last_synced(&synced, "seed")
        .await?;
    Ok(u32::try_from(synced.len().saturating_sub(before)).unwrap_or(u32::MAX))
}

pub fn filter_blocked_paths(response: &mut SyncResponse, blocked: &HashSet<String>) {
    response.upload_required.retain(|p| !blocked.contains(p));
    response
        .download_required
        .retain(|f| !blocked.contains(&f.path));
    response.delete_local.retain(|p| !blocked.contains(p));
}

/// Paths where a lazy placeholder was written to locally (DX-10).
pub async fn detect_placeholder_corruptions(
    base_path: &Path,
    db: &ClientDb,
) -> Result<Vec<String>> {
    let cached = db.get_cache_entries().await?;
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(base_path)?;
    let mut out = Vec::new();
    for (path, entry) in &cached {
        if entry.hydrated || entry.deleted_at.is_some() {
            continue;
        }
        let Ok(file) = read_root.open_regular(path) else {
            continue;
        };
        let metadata = file.metadata()?;
        let observed_mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(0);
        #[cfg(unix)]
        let observed_mode = {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 != 0 {
                feanorfs_common::EXECUTABLE_MODE
            } else {
                0
            }
        };
        #[cfg(not(unix))]
        let observed_mode = entry.mode;
        if metadata.len() > 0
            || observed_mtime != entry.mtime
            || observed_mode != entry.mode
            || !metadata.permissions().readonly()
        {
            out.push(path.clone());
        }
    }
    Ok(out)
}

pub async fn register_placeholder_corruption(base: &Path, db: &ClientDb, path: &str) -> Result<()> {
    if db.get_conflict_record(path).await?.is_some() {
        return Ok(());
    }
    if !is_safe_rel_path(path) {
        return Err(crate::agent::continuous::unsafe_path_failure(format!(
            "unsafe placeholder path: {path}"
        )));
    }
    let (stray, _) = crate::workspace_read::WorkspaceReadRoot::open(base)?
        .read_regular_stable(path, u64::MAX - 1)
        .await?;
    let ts = chrono::Utc::now().timestamp_millis();
    let dir = conflicts_dir(base)?.join(format!("placeholder_{ts}"));
    fs::create_dir_all(&dir).await?;
    let local_dest = resolve_artifact(&dir, path, ArtifactRole::Local);
    if let Some(parent) = local_dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&local_dest, &stray).await?;
    let cloud_dest = resolve_artifact(&dir, path, ArtifactRole::Cloud);
    fs::write(
        &cloud_dest,
        format!(
            "{}hydrate-to-compare>\n",
            crate::conflict_artifacts::SENTINEL_PREFIX
        ),
    )
    .await?;
    let original_dest = resolve_artifact(&dir, path, ArtifactRole::Original);
    fs::write(
        &original_dest,
        format!(
            "{}placeholder>\n",
            crate::conflict_artifacts::SENTINEL_PREFIX
        ),
    )
    .await?;
    db.upsert_conflict(
        path,
        &ConflictKind::EditEdit,
        &dir.to_string_lossy(),
        ts,
        crate::state::ConflictRecordStatus::Pending,
    )
    .await?;
    Ok(())
}

fn paths_case_collide(a: &str, b: &str) -> bool {
    a != b && a.eq_ignore_ascii_case(b)
}

const WINDOWS_RESERVED_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

const COPY_SUFFIX_ROOM: usize = 32;

/// Sanitizes one portable path for use as a conflict-copy base name:
/// replaces control/path/reserved characters, truncates components to the
/// portable component bound, and prefixes reserved stems so the name stays
/// portable on every supported platform.
#[must_use]
fn sanitize_copy_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut component = String::new();
    let flush = |component: &mut String, out: &mut String| {
        if component.is_empty() {
            return;
        }
        let mut component = std::mem::take(component);
        while component.chars().count() > feanorfs_common::MAX_PORTABLE_COMPONENT_BYTES {
            component.pop();
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if WINDOWS_RESERVED_STEMS.contains(&stem.as_str()) {
            out.push('_');
        }
        out.push_str(&component);
    };
    for character in path.chars() {
        if character.is_control() || "/\\:|*?\"<>".contains(character) {
            flush(&mut component, &mut out);
            out.push('_');
        } else {
            component.push(character);
        }
    }
    flush(&mut component, &mut out);
    if out.is_empty() {
        out.push('_');
    }
    // Keep the final result within the portable path bound minus the suffix
    // room, char-safely.
    while out.chars().count() > feanorfs_common::MAX_PORTABLE_PATH_BYTES - COPY_SUFFIX_ROOM {
        out.pop();
    }
    out
}

/// Canonical collision key: Unicode NFC normalization followed by full
/// Unicode lowercase (the portable case-fold surface). Normalizing BOTH the
/// occupied set and the candidate this way refuses case-folded and
/// NFC-normalized collisions on every supported platform, including
/// case-insensitive filesystems.
fn normalize_copy_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfc().collect::<String>().to_lowercase()
}

/// Canonical portable conflict-copy allocator.
///
/// Deterministically chooses `{sanitized} (conflicted copy 1)`, then 2, 3,
/// ... — the first name that is portable (component/path limits, reserved
/// names), unique case-insensitively AND under Unicode NFC normalization
/// against `occupied` and the live filesystem, and never overwrites an
/// existing conflict copy. The final file write by the caller is create-new
/// (O_EXCL, no-follow), so a name claimed between allocation and write fails
/// closed instead of replacing another conflict's copy.
///
/// # Errors
/// Returns an error when no bounded name is available.
pub fn allocate_conflict_copy_name(
    base: &Path,
    path: &str,
    occupied: &HashSet<String>,
) -> Result<String> {
    let sanitized = sanitize_copy_path(path);
    let stem = format!("{sanitized} (conflicted copy");
    let occupied_normalized: HashSet<String> = occupied
        .iter()
        .map(|existing| normalize_copy_name(existing))
        .collect();
    for number in 1..=10_000u32 {
        let candidate = format!("{stem} {number})");
        if !is_safe_rel_path(&candidate)
            || candidate.len() > feanorfs_common::MAX_PORTABLE_PATH_BYTES
        {
            continue;
        }
        if occupied_normalized.contains(&normalize_copy_name(&candidate)) {
            continue;
        }
        if base.join(&candidate).exists() {
            continue;
        }
        return Ok(candidate);
    }
    bail!(
        "no portable conflict-copy name is available for {path}; \
         too many existing conflicted copies"
    )
}

/// Detect case-only path collisions during pull (DX-15).
pub fn case_conflict_paths(
    download_paths: &[FileState],
    local_paths: &HashMap<String, FileState>,
) -> Vec<String> {
    let mut out = Vec::new();
    for remote in download_paths {
        for local_path in local_paths.keys() {
            if paths_case_collide(&remote.path, local_path) {
                out.push(remote.path.clone());
                break;
            }
        }
    }
    out
}

/// Warn when server metadata regressed vs last agreed state (DX-23).
pub fn detect_server_rollback(
    last_synced: &HashMap<String, FileState>,
    server_files: &[FileState],
) -> Option<String> {
    if last_synced.is_empty() {
        return None;
    }
    let server_map: HashMap<_, _> = server_files.iter().map(|f| (f.path.clone(), f)).collect();
    let mut regressed = 0u32;
    for (path, agreed) in last_synced {
        if agreed.deleted {
            continue;
        }
        if let Some(remote) = server_map.get(path) {
            if remote.mtime < agreed.mtime && remote.hash != agreed.hash {
                regressed += 1;
            }
        }
    }
    if regressed > 0 {
        Some(format!(
            "Server looks older than this machine on {regressed} path(s); \
             run `feanorfs sync --up` to restore it instead of mass-downloading stale files."
        ))
    } else {
        None
    }
}

/// After upload, detect silent create/create collisions (DX-22).
pub async fn detect_post_upload_collisions(
    ctx: &SyncCtx<'_>,
    local_files: &HashMap<String, FileState>,
    uploaded_paths: &[String],
) -> Result<Vec<(ConcurrentEdit, ConflictKind)>> {
    if uploaded_paths.is_empty() {
        return Ok(Vec::new());
    }
    let last = load_last_synced_snapshot(ctx).await?;
    let server_map = load_server_view(ctx).await?;
    let mut out = Vec::new();
    for path in uploaded_paths {
        let Some(local) = local_files.get(path) else {
            continue;
        };
        if let Some(remote) = server_map.get(path) {
            if remote.hash != local.hash {
                let base = last.get(path).cloned();
                out.push((
                    ConcurrentEdit::new(
                        path.clone(),
                        base,
                        Some(local.clone()),
                        Some(remote.clone()),
                    ),
                    ConflictKind::EditEdit,
                ));
            }
        }
    }
    Ok(out)
}

/// Peek server delta, detect workspace conflicts, optionally register them, and
/// return the filtered response plus blocked paths.
pub async fn negotiate_sync_with_conflict_gate(
    ctx: &SyncCtx<'_>,
    local_files: &HashMap<String, FileState>,
    register: bool,
) -> Result<(SyncResponse, HashSet<String>, Option<String>)> {
    let pending = pending_conflict_paths(ctx.db).await?;
    let (server_head, server_files) = load_server_view_with_head(ctx).await?;
    let reconciled =
        crate::tree_reconcile::reconcile(ctx, local_files, &server_files, server_head.as_deref())
            .await?;
    let last = reconciled.base;
    let mut response = reconciled.response;
    let detected = detect_workspace_conflicts_with_server_view(
        &last,
        local_files,
        &response,
        &pending,
        &server_files,
    )?;

    let mut all_detected = detected;
    for remote_path in case_conflict_paths(&response.download_required, local_files) {
        if pending.contains(&remote_path) {
            continue;
        }
        let Some(remote) = response
            .download_required
            .iter()
            .find(|f| f.path == remote_path)
            .cloned()
        else {
            continue;
        };
        let local_key = local_files
            .keys()
            .find(|p| paths_case_collide(p, &remote_path))
            .cloned();
        if let Some(local_key) = local_key {
            if let Some(local) = local_files.get(&local_key) {
                let base = last.get(&remote_path).cloned();
                all_detected.push((
                    ConcurrentEdit::new(
                        remote_path.clone(),
                        base,
                        Some(local.clone()),
                        Some(remote),
                    ),
                    ConflictKind::EditEdit,
                ));
            }
        }
    }

    let mut seen_paths: HashSet<String> =
        all_detected.iter().map(|(c, _)| c.path.clone()).collect();
    for remote in &response.download_required {
        if pending.contains(&remote.path) || !seen_paths.insert(remote.path.clone()) {
            continue;
        }
        let Some(local) = local_files.get(&remote.path) else {
            continue;
        };
        if same_content(Some(local), Some(remote)) {
            continue;
        }
        let we_changed = file_changed_since(last.get(&remote.path), local);
        let they_changed = file_changed_since(last.get(&remote.path), remote);
        if !(we_changed && they_changed) {
            continue;
        }
        let base = last
            .get(&remote.path)
            .cloned()
            .or_else(|| Some(local.clone()));
        all_detected.push((
            ConcurrentEdit::new(
                remote.path.clone(),
                base,
                Some(local.clone()),
                Some(remote.clone()),
            ),
            ConflictKind::EditEdit,
        ));
    }

    let needs_upload_collision_scan = response
        .upload_required
        .iter()
        .any(|path| local_files.contains_key(path) && !last.contains_key(path));
    if needs_upload_collision_scan {
        for path in &response.upload_required {
            if pending.contains(path) || !seen_paths.insert(path.clone()) {
                continue;
            }
            let Some(local) = local_files.get(path) else {
                continue;
            };
            let Some(remote) = server_files.get(path) else {
                continue;
            };
            if same_content(Some(local), Some(remote)) {
                continue;
            }
            let we_changed = file_changed_since(last.get(path), local);
            let they_changed = file_changed_since(last.get(path), remote);
            if !(we_changed && they_changed) {
                continue;
            }
            let base = last.get(path).cloned().or_else(|| Some(local.clone()));
            all_detected.push((
                ConcurrentEdit::new(
                    path.clone(),
                    base,
                    Some(local.clone()),
                    Some(remote.clone()),
                ),
                ConflictKind::EditEdit,
            ));
        }
    }

    let mut blocked = pending;

    if register {
        for path in detect_placeholder_corruptions(ctx.base, ctx.db).await? {
            register_placeholder_corruption(ctx.base, ctx.db, &path).await?;
            blocked.insert(path);
        }
    }

    if register && !all_detected.is_empty() {
        tracing::warn!(
            "{} concurrent workspace edit conflict(s); saved base/local/cloud versions in global FeanorFS state",
            all_detected.len()
        );
        for (c, _) in &all_detected {
            tracing::warn!("  conflict: {}", c.path);
        }
        let (_conflict_dir, new_paths) =
            register_and_write_conflicts(ctx, &all_detected, None).await?;
        blocked.extend(new_paths);
    } else {
        for (c, _) in &all_detected {
            blocked.insert(c.path.clone());
        }
    }

    filter_blocked_paths(&mut response, &blocked);
    Ok((response, blocked, server_head))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiClient;
    use crate::head::SwapHeadResult;
    use crate::snapshot::{SnapshotEngine, SnapshotInput};
    use feanorfs_common::{classify_conflict_kind, detect_concurrent_edits, hash_bytes};

    fn st(path: &str, hash: &str, deleted: bool) -> FileState {
        FileState {
            path: path.into(),
            hash: hash.into(),
            size: 1,
            mtime: 1,
            deleted,
            mode: 0,
        }
    }

    #[test]
    fn classify_edit_edit() {
        let base = st("f", "b", false);
        let ours = st("f", "o", false);
        let theirs = st("f", "t", false);
        assert_eq!(
            classify_conflict_kind(&base, Some(&ours), Some(&theirs), false),
            ConflictKind::EditEdit
        );
    }

    #[test]
    fn classify_edit_delete() {
        let base = st("f", "b", false);
        let ours = st("f", "o", false);
        assert_eq!(
            classify_conflict_kind(&base, Some(&ours), None, true),
            ConflictKind::EditDelete
        );
    }

    #[test]
    fn concurrent_delete_not_a_conflict() {
        let base = st("f", "b", false);
        let mut local = HashMap::new();
        local.insert("f".into(), st("f", "b", true));
        let mut their_deleted = HashSet::new();
        their_deleted.insert("f".into());
        let base_map = HashMap::from([("f".into(), base.clone())]);
        let edits = detect_concurrent_edits(
            &base_map,
            &local,
            &HashMap::new(),
            &their_deleted,
            vec!["f".into()],
            &HashSet::new(),
        );
        assert!(edits.is_empty());
    }

    #[test]
    fn missing_local_delete_uses_the_deleted_locally_artifact_label() {
        assert_eq!(
            ours_missing_label(&ConflictKind::DeleteEdit),
            "deleted-locally"
        );
    }

    #[test]
    fn filter_blocked_paths_strips_all_buckets() {
        let mut resp = SyncResponse {
            upload_required: vec!["a".into()],
            download_required: vec![st("b", "h", false)],
            delete_local: vec!["c".into()],
        };
        let blocked = HashSet::from(["a".into(), "b".into(), "c".into()]);
        filter_blocked_paths(&mut resp, &blocked);
        assert!(resp.upload_required.is_empty());
        assert!(resp.download_required.is_empty());
        assert!(resp.delete_local.is_empty());
    }

    #[test]
    fn conflicts_pending_uses_db_only() {
        assert!(!conflicts_pending(None));
        assert!(!conflicts_pending(Some(&HashSet::new())));
        assert!(conflicts_pending(Some(&HashSet::from(["x".into()]))));
    }

    #[test]
    fn allocator_refuses_case_folded_occupied_names() {
        let base = tempfile::tempdir().unwrap();
        let occupied = HashSet::from(["Foo (conflicted copy 1)".to_string()]);
        let name = allocate_conflict_copy_name(base.path(), "foo", &occupied).unwrap();
        assert_eq!(name, "foo (conflicted copy 2)");
    }

    #[test]
    fn allocator_refuses_ascii_case_folded_occupied_names_across_numbers() {
        let base = tempfile::tempdir().unwrap();
        let occupied = HashSet::from([
            "FOO (conflicted copy 1)".to_string(),
            "Foo (conflicted copy 2)".to_string(),
        ]);
        let name = allocate_conflict_copy_name(base.path(), "foo", &occupied).unwrap();
        assert_eq!(name, "foo (conflicted copy 3)");
    }

    #[test]
    fn allocator_refuses_nfc_normalized_occupied_names() {
        // The occupied set holds the NFD form ("Cafe\u{301}") while the
        // candidate is NFC ("Café"): both normalize-equal, so the candidate
        // must be refused even though the raw strings differ.
        let base = tempfile::tempdir().unwrap();
        let occupied = HashSet::from(["Cafe\u{301} (conflicted copy 1)".to_string()]);
        let name = allocate_conflict_copy_name(base.path(), "Café", &occupied).unwrap();
        assert_eq!(name, "Café (conflicted copy 2)");
    }

    #[test]
    fn allocator_refuses_nfc_normalized_occupied_names_reverse_direction() {
        // The occupied set holds the NFC form; the input path itself is NFD
        // and can never produce a portable (NFC-canonical) candidate, so the
        // allocator fails closed instead of emitting a normalize-equal name.
        let base = tempfile::tempdir().unwrap();
        let occupied = HashSet::from(["Café (conflicted copy 1)".to_string()]);
        let error = allocate_conflict_copy_name(base.path(), "Cafe\u{301}", &occupied)
            .expect_err("an NFD input must fail closed");
        assert!(
            error.to_string().contains("no portable conflict-copy name"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn allocator_skips_live_filesystem_claims() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("doc.txt (conflicted copy 1)"), b"claimed").unwrap();
        let occupied = HashSet::new();
        let name = allocate_conflict_copy_name(base.path(), "doc.txt", &occupied).unwrap();
        assert_eq!(name, "doc.txt (conflicted copy 2)");
    }

    #[tokio::test]
    async fn concurrent_materialization_does_not_replace_another_conflicts_copy() {
        let base = tempfile::tempdir().unwrap();
        let occupied = HashSet::new();
        let name = allocate_conflict_copy_name(base.path(), "shared.txt", &occupied).unwrap();
        assert_eq!(name, "shared.txt (conflicted copy 1)");

        // A concurrent materializer claims the allocated name first; the
        // create-new write must fail closed instead of replacing the copy.
        let claimed = base.path().join(&name);
        std::fs::write(&claimed, b"other conflict's copy").unwrap();
        let error = write_new_durable(&claimed, b"replacement")
            .await
            .expect_err("create-new write must refuse an existing copy");
        assert!(
            error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
            }),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            std::fs::read(&claimed).unwrap(),
            b"other conflict's copy",
            "the refused write must leave the concurrent copy untouched"
        );

        // The allocator now skips the claimed name on a fresh allocation.
        let next = allocate_conflict_copy_name(base.path(), "shared.txt", &occupied).unwrap();
        assert_eq!(next, "shared.txt (conflicted copy 2)");
    }

    // ---- Fingerprinted manual keep: guarded publication after CAS loss ----

    const BASE_CONTENT: &[u8] = b"base-content";
    const OURS_CONTENT: &[u8] = b"ours-content";
    const THEIRS_CONTENT: &[u8] = b"theirs-content";
    const CHANGED_THEIRS_CONTENT: &[u8] = b"changed-theirs";
    const MANUAL_PATH: &str = "conflict.txt";

    struct ManualKeepHarness {
        _hub_data: tempfile::TempDir,
        base: tempfile::TempDir,
        api: ApiClient,
        db: crate::local::ClientDb,
    }

    async fn manual_keep_harness() -> ManualKeepHarness {
        let hub_data = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let hub = crate::hub::LocalHub::open(hub_data.path().to_path_buf(), None)
            .await
            .unwrap();
        let api = ApiClient::local(std::sync::Arc::clone(&hub), None);
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(&state).await.unwrap();
        ManualKeepHarness {
            _hub_data: hub_data,
            base,
            api,
            db,
        }
    }

    impl ManualKeepHarness {
        fn ctx(&self) -> SyncCtx<'_> {
            SyncCtx::with_format_version(
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

    fn manual_keep_state(path: &str, content: &[u8], mode: u32) -> FileState {
        FileState {
            path: path.to_string(),
            hash: hash_bytes(content),
            size: content.len() as u64,
            mtime: 1,
            deleted: false,
            mode,
        }
    }

    fn manual_keep_edit(path: &str) -> ConcurrentEdit {
        ConcurrentEdit::new(
            path.to_string(),
            Some(manual_keep_state(path, BASE_CONTENT, 0)),
            Some(manual_keep_state(path, OURS_CONTENT, 0)),
            Some(manual_keep_state(path, THEIRS_CONTENT, 0)),
        )
    }

    async fn publish_manual_keep_conflict_head(ctx: &SyncCtx<'_>, edit: &ConcurrentEdit) -> String {
        let id = write_manual_keep_conflict_head(ctx, edit).await;
        let expected = ctx.api.get_head(ctx.workspace_id()).await.unwrap();
        match ctx
            .api
            .swap_head(ctx.workspace_id(), expected.as_deref(), &id)
            .await
            .unwrap()
        {
            SwapHeadResult::Swapped => id,
            _ => panic!("head must swap"),
        }
    }

    /// Writes a conflict head snapshot WITHOUT swapping it in: the lost-CAS
    /// injector swaps it into place during the CAS window.
    async fn write_manual_keep_conflict_head(ctx: &SyncCtx<'_>, edit: &ConcurrentEdit) -> String {
        let engine = SnapshotEngine::new(ctx);
        engine
            .write(SnapshotInput {
                files: &HashMap::new(),
                conflicts: std::slice::from_ref(edit),
                parents: vec![],
                author: "test",
                message: None,
            })
            .await
            .unwrap()
    }

    async fn upload_manual_keep_legs(ctx: &SyncCtx<'_>) {
        for content in [
            BASE_CONTENT,
            OURS_CONTENT,
            THEIRS_CONTENT,
            CHANGED_THEIRS_CONTENT,
        ] {
            ctx.api
                .upload_object(ctx.workspace_id(), &hash_bytes(content), content.to_vec())
                .await
                .unwrap();
        }
    }

    /// Registers the conflict at `head` as a fingerprinted pending record and
    /// materializes the cloud artifact (`keep --cloud` reads it).
    async fn register_manual_keep_conflict(
        ctx: &SyncCtx<'_>,
        head: &str,
        edit: &ConcurrentEdit,
    ) -> PathBuf {
        let engine = SnapshotEngine::new(ctx);
        let snapshot = engine.load_snapshot(head).await.unwrap();
        let identity = crate::conflict_artifacts::conflict_identity_from_edit(
            ctx.workspace_id(),
            head,
            head,
            &snapshot.root,
            edit,
            ConflictKind::EditEdit,
            &crate::conflict_artifacts::IdentityBinding::default(),
        );
        let fingerprint = feanorfs_common::compute_conflict_identity_fingerprint(&identity);
        let dir = ctx.state_dir().unwrap().join("conflicts/manual-keep");
        std::fs::create_dir_all(&dir).unwrap();
        let cloud_dest =
            crate::conflict_artifacts::resolve_artifact(&dir, &edit.path, ArtifactRole::Cloud);
        std::fs::write(&cloud_dest, THEIRS_CONTENT).unwrap();
        ctx.db
            .upsert_conflict_fingerprinted(
                &edit.path,
                &ConflictKind::EditEdit,
                &dir.to_string_lossy(),
                1,
                &identity,
                &fingerprint,
            )
            .await
            .unwrap();
        dir
    }

    /// Lost CAS with an UNCHANGED conflict: the plan is discarded, ALL
    /// validation restarts from the new head, and the manual keep publishes
    /// on the fresh pass — never degrading to path-only publication.
    #[tokio::test]
    async fn manual_keep_lost_cas_restarts_validation_and_succeeds() {
        let h = manual_keep_harness().await;
        let ctx = h.ctx();
        upload_manual_keep_legs(&ctx).await;
        let head = publish_manual_keep_conflict_head(&ctx, &manual_keep_edit(MANUAL_PATH)).await;
        register_manual_keep_conflict(&ctx, &head, &manual_keep_edit(MANUAL_PATH)).await;

        crate::snapshot::inject_lost_cas(1, None);
        resolve_conflict(&ctx, MANUAL_PATH, ResolveKeep::Cloud, None)
            .await
            .expect("manual keep must publish after a lost-CAS restart");

        // The conflict is gone from the new head and the registry.
        let new_head = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();
        assert_ne!(new_head, head);
        let state = SnapshotEngine::new(&ctx)
            .load_state(&new_head)
            .await
            .unwrap();
        assert!(state
            .conflicts
            .iter()
            .all(|conflict| conflict.path != MANUAL_PATH));
        assert!(ctx
            .db
            .get_conflict_record(MANUAL_PATH)
            .await
            .unwrap()
            .is_none());
        // The resolved bytes were written to the worktree.
        assert_eq!(
            std::fs::read(ctx.base.join(MANUAL_PATH)).unwrap(),
            THEIRS_CONTENT
        );
    }

    /// Lost CAS followed by a CHANGED conflict: revalidation from the new
    /// head detects the identity change, publication fails closed with a
    /// typed error, and the conflict + registry record survive for manual
    /// action — there is NO fallback to path-only publication.
    #[tokio::test]
    async fn manual_keep_lost_cas_then_changed_conflict_fails_closed() {
        let h = manual_keep_harness().await;
        let ctx = h.ctx();
        upload_manual_keep_legs(&ctx).await;
        let head = publish_manual_keep_conflict_head(&ctx, &manual_keep_edit(MANUAL_PATH)).await;
        register_manual_keep_conflict(&ctx, &head, &manual_keep_edit(MANUAL_PATH)).await;

        // The conflict changes DURING the CAS window (new theirs leg): the
        // changed head is written but not swapped; the lost-CAS injector
        // swaps it into place while the manual keep is publishing.
        let mut changed_edit = manual_keep_edit(MANUAL_PATH);
        changed_edit.theirs = Some(manual_keep_state(MANUAL_PATH, CHANGED_THEIRS_CONTENT, 0));
        let changed_head = write_manual_keep_conflict_head(&ctx, &changed_edit).await;
        crate::snapshot::inject_lost_cas(1, Some(changed_head.clone()));

        let error = resolve_conflict(&ctx, MANUAL_PATH, ResolveKeep::Cloud, None)
            .await
            .expect_err("a changed conflict must fail closed");
        assert!(
            error
                .to_string()
                .contains("changed after the manual choice"),
            "unexpected error: {error:#}"
        );

        // The changed conflict survives on the new head (no path-only
        // publication happened) and the registry record is preserved.
        assert_eq!(
            ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap(),
            changed_head
        );
        let state = SnapshotEngine::new(&ctx)
            .load_state(&changed_head)
            .await
            .unwrap();
        assert!(
            state
                .conflicts
                .iter()
                .any(|conflict| conflict.path == MANUAL_PATH),
            "the changed conflict must survive on the head"
        );
        let record = ctx
            .db
            .get_conflict_record(MANUAL_PATH)
            .await
            .unwrap()
            .expect("registry record must be preserved");
        assert_eq!(record.status, "pending");
        assert!(ctx.db.is_conflict_fingerprinted(MANUAL_PATH).await.unwrap());
    }

    /// Lost CAS followed by the conflict DISAPPEARING: revalidation fails
    /// closed and the registry record is preserved for manual action.
    #[tokio::test]
    async fn manual_keep_lost_cas_then_conflict_disappears_fails_closed() {
        let h = manual_keep_harness().await;
        let ctx = h.ctx();
        upload_manual_keep_legs(&ctx).await;
        let head = publish_manual_keep_conflict_head(&ctx, &manual_keep_edit(MANUAL_PATH)).await;
        register_manual_keep_conflict(&ctx, &head, &manual_keep_edit(MANUAL_PATH)).await;

        // The conflict is resolved away DURING the CAS window: a bare head
        // with no conflicts at all is written but not swapped; the lost-CAS
        // injector swaps it into place while the manual keep is publishing.
        let engine = SnapshotEngine::new(&ctx);
        let bare_head = engine
            .write(SnapshotInput {
                files: &HashMap::new(),
                conflicts: &[],
                parents: vec![],
                author: "test",
                message: None,
            })
            .await
            .unwrap();
        crate::snapshot::inject_lost_cas(1, Some(bare_head.clone()));

        let error = resolve_conflict(&ctx, MANUAL_PATH, ResolveKeep::Cloud, None)
            .await
            .expect_err("a disappeared conflict must fail closed");
        assert!(
            error.to_string().contains("no longer exists"),
            "unexpected error: {error:#}"
        );
        assert!(ctx
            .db
            .get_conflict_record(MANUAL_PATH)
            .await
            .unwrap()
            .is_some());
    }
}
