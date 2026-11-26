use anyhow::{bail, Context as _, Result};
use feanorfs_common::FileState;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;

use crate::api::ApiClient;
use crate::ctx::SyncCtx;
use crate::local::{build_workspace_walker, portable_rel_path, ClientDb};
use crate::lock::SyncLock;
use crate::paths::{agent_dir, agent_root, validate_name};
use crate::snapshot::SnapshotEngine;

struct SpawnCleanupGuard {
    target: PathBuf,
    restore_from: Option<PathBuf>,
    published: bool,
    armed: bool,
}

impl Drop for SpawnCleanupGuard {
    fn drop(&mut self) {
        if !self.armed || self.published {
            return;
        }

        if let Err(error) = remove_directory_if_present(&self.target) {
            tracing::error!(
                "failed to remove incomplete agent root {} during rollback: {error}; preserving backup at {}",
                self.target.display(),
                self.restore_from
                    .as_deref()
                    .map_or_else(|| "<none>".into(), |path| path.display().to_string())
            );
            return;
        }
        if let Some(backup) = &self.restore_from {
            if let Err(error) = std::fs::rename(backup, &self.target) {
                tracing::error!(
                    "failed to restore agent root {} during rollback: {error}; backup preserved at {}",
                    self.target.display(),
                    backup.display()
                );
            }
        }
    }
}

impl SpawnCleanupGuard {
    async fn rollback(&mut self) -> Result<()> {
        if !self.armed || self.published {
            return Ok(());
        }

        if let Err(error) = remove_directory_if_present_async(&self.target).await {
            self.armed = false;
            let backup = self
                .restore_from
                .as_deref()
                .map_or_else(|| "<none>".into(), |path| path.display().to_string());
            bail!(
                "rollback failed while removing incomplete agent root {}: {error}; original backup preserved at {backup}",
                self.target.display()
            );
        }
        if let Some(backup) = self.restore_from.as_deref() {
            if let Err(error) = fs::rename(backup, &self.target).await {
                self.armed = false;
                bail!(
                    "rollback failed while restoring agent root {}: {error}; original backup preserved at {}",
                    self.target.display(),
                    backup.display()
                );
            }
            self.restore_from = None;
        }
        self.armed = false;
        Ok(())
    }

    async fn finish_publication(&mut self) -> Result<()> {
        self.published = true;
        if let Some(backup) = self.restore_from.as_deref() {
            fs::remove_dir_all(backup).await.with_context(|| {
                format!(
                    "agent replacement was published, but the old backup could not be removed; preserved remainder at {}",
                    backup.display()
                )
            })?;
            self.restore_from = None;
        }
        self.armed = false;
        Ok(())
    }
}

fn replacement_backup_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "agent".into());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    target.with_file_name(format!("{file_name}.replace-backup-{stamp}"))
}

/// Copy one opened workspace file into the new worktree.
///
/// Each copy is only handed to the device here. [`flush_worktree`] makes the
/// whole worktree durable at once, before the base ref is published.
fn copy_opened_source(
    mut source: std::fs::File,
    destination: &Path,
    logical_path: &str,
) -> Result<()> {
    let before = source.metadata()?;
    if let Some(parent) = destination.parent().filter(|parent| !parent.is_dir()) {
        std::fs::create_dir_all(parent)?;
    }
    let (output, copied) = match clone_opened_source(&source, &before, destination)
        .with_context(|| format!("open agent clone for {logical_path}"))?
    {
        Some(clone) => {
            let length = clone.metadata()?.len();
            (clone, length)
        }
        None => {
            let mut output = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)
                .with_context(|| format!("create agent copy for {logical_path}"))?;
            let copied = std::io::copy(&mut source, &mut output)?;
            (output, copied)
        }
    };
    let after = source.metadata()?;
    if copied != before.len()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err(super::continuous::retryable_volatility_failure(format!(
            "workspace file {logical_path} changed while spawning agent"
        )));
    }
    output.set_permissions(before.permissions())?;
    sync_to_device(&output)?;
    Ok(())
}

/// Clone an opened file copy-on-write, or return `None` to request a byte copy.
///
/// The source stays descriptor-anchored. A failed clone creates nothing, so the
/// byte copy can always follow and report the real error. Linux needs no
/// equivalent: `std::io::copy` already uses `copy_file_range` there.
#[cfg(target_os = "macos")]
fn clone_opened_source(
    source: &std::fs::File,
    metadata: &std::fs::Metadata,
    destination: &Path,
) -> std::io::Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd as _;
    use std::os::macos::fs::MetadataExt as _;
    use std::os::unix::ffi::OsStrExt as _;

    /// `<sys/clonefile.h>`; not exported by the `libc` crate.
    const CLONE_NOOWNERCOPY: u32 = 0x0002;

    // A clone inherits file flags. An immutable one could not be edited or
    // cleaned, and a dataless one would stay a cloud placeholder.
    if metadata.st_flags() != 0 {
        return Ok(None);
    }
    let name = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: the source descriptor and NUL-terminated path outlive the call.
    let status = unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            libc::AT_FDCWD,
            name.as_ptr(),
            CLONE_NOOWNERCOPY,
        )
    };
    if status != 0 {
        return Ok(None);
    }
    std::fs::File::open(destination).map(Some)
}

#[cfg(not(target_os = "macos"))]
fn clone_opened_source(
    _source: &std::fs::File,
    _metadata: &std::fs::Metadata,
    _destination: &Path,
) -> std::io::Result<Option<std::fs::File>> {
    Ok(None)
}

/// Hand a file to the device without draining the device write cache.
///
/// `sync_all` is `F_FULLFSYNC` on macOS and drains the whole device queue, which
/// costs milliseconds per file. Elsewhere it is an ordinary flush.
fn sync_to_device(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd as _;

        loop {
            // SAFETY: the descriptor is open for the duration of the call.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_all()
    }
}

/// Make every copied file durable before the agent base ref can name it.
///
/// On macOS one `F_FULLFSYNC` persists all data already passed to `fsync` on
/// the same device. Elsewhere [`sync_to_device`] flushed each file already.
fn flush_worktree(worktree: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(worktree)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = worktree;
        Ok(())
    }
}

fn remove_directory_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn remove_directory_if_present_async(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

// Keep the low-level async facade source-compatible; the supported blocking SDK
// groups these switches in `SpawnOptions`.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
    no_sync: bool,
    replace: bool,
) -> Result<usize> {
    let config = if crate::workspace_layout::workspace_is_configured(base) {
        crate::local::load_config(base)?
    } else {
        crate::local::Config {
            server_url: String::new(),
            workspace_id: workspace_id.to_string(),
            encryption_password: password.map(ToString::to_string),
            server_password: None,
            tls_ca_pem: None,
            format_version: 1,
            hub_local: false,
            relay: None,
            mesh: None,
        }
    };

    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    spawn_agent_with_ctx(&ctx, name, no_sync, replace).await
}

async fn spawn_agent_with_ctx(
    ctx: &SyncCtx<'_>,
    name: &str,
    no_sync: bool,
    replace: bool,
) -> Result<usize> {
    validate_name(name)?;
    let _runner_lifecycle = if replace {
        Some(super::runner::RunnerLifecycleLock::acquire_async(ctx.base).await?)
    } else {
        None
    };
    let owned_root = agent_root(ctx.base, name)?;
    let target = agent_dir(ctx.base, name)?;
    if replace && super::runner::runner_status(ctx.base)?.is_some_and(|status| status.agent == name)
    {
        bail!(
            "agent workspace '{name}' has a configured runner; runner removal is required before replacement"
        );
    }
    if target.exists() {
        if replace {
            // Preserve the original agent tree until the new copy is committed.
        } else {
            bail!(
                "Agent workspace '{name}' already exists. Run `feanorfs agent clean {name}` or use `--replace`."
            );
        }
    }

    let sync_guard = SyncLock::acquire(ctx.base)?;

    let snapshots = SnapshotEngine::new(ctx);
    let no_sync_base = if no_sync {
        let last_synced_id = snapshots
            .last_synced_id()
            .await?
            .context("Cannot spawn with --no-sync before this folder has completed a sync")?;
        let last = snapshots.load_files_local(&last_synced_id).await?;
        let dirty = paths_differing_from(ctx, &last).await?;
        if !dirty.is_empty() {
            bail!(
                "Folder is not in sync with last agreed state. Dirty paths: {}",
                dirty.join(", ")
            );
        }

        Some(last_synced_id)
    } else {
        None
    };

    let pending = crate::conflicts::pending_conflict_paths(ctx.db).await?;
    if !pending.is_empty() {
        return Err(super::continuous::conflict_failure(format!(
            "Your folder needs attention before an agent can copy it. Conflicts: {}",
            pending.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }

    let base_snapshot = if let Some(last_synced_id) = no_sync_base {
        last_synced_id
    } else {
        crate::sync_pass::do_sync_guarded(
            ctx.api,
            ctx.db,
            ctx.base,
            ctx.workspace_id(),
            ctx.password(),
            false,
            &sync_guard,
        )
        .await?;

        // The pass did not publish edits made while it ran; copying them would
        // hand the agent changes it never made.
        let server_files = crate::conflicts::load_server_view(ctx).await?;
        if !paths_differing_from(ctx, &server_files).await?.is_empty() {
            return Err(super::continuous::retryable_volatility_failure(
                "your folder changed while the agent workspace was starting; retry",
            ));
        }
        snapshots
            .publish_server_view(&server_files, "folder")
            .await?
    };

    let dehydrated = ctx
        .db
        .get_cache_entries()
        .await?
        .into_iter()
        .filter(|(_, entry)| !entry.hydrated && entry.deleted_at.is_none())
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    if !dehydrated.is_empty() {
        bail!(
            "Cannot spawn with unhydrated placeholders. Run `feanorfs hydrate` first: {}",
            dehydrated.join(", ")
        );
    }

    let restore_from = if replace && fs::try_exists(&owned_root).await? {
        // Move the complete agent-owned root so a failed replacement restores
        // its worktree, base ref, and runtime cache together. A successful
        // replacement creates a fresh root and therefore cannot reuse cache
        // metadata from the worktree it replaced.
        let backup = replacement_backup_path(&owned_root);
        if fs::try_exists(&backup).await? {
            bail!(
                "refusing to overwrite an existing agent replacement backup at {}",
                backup.display()
            );
        }
        fs::rename(&owned_root, &backup).await?;
        Some(backup)
    } else {
        None
    };

    let mut guard = SpawnCleanupGuard {
        target: owned_root.clone(),
        restore_from,
        published: false,
        armed: true,
    };

    let result: Result<usize> = async {
        fs::create_dir_all(&target).await?;
        inject_spawn_failure(ctx.base, name, "after-stage").await?;

        let mut copied = 0;
        let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
        let mut sources = crate::workspace_read::WalkCursor::new(read_root);
        for entry in build_workspace_walker(ctx.base, false).build() {
            let entry = entry.context("walk workspace while spawning agent")?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(ctx.base)
                .context("derive workspace-relative agent source path")?;
            let Some(relative_text) = relative.to_str() else {
                continue;
            };
            let Some(normalized) = portable_rel_path(relative_text) else {
                continue;
            };
            let source = sources
                .open_regular(relative)
                .with_context(|| format!("open agent source {normalized}"))?;
            copy_opened_source(source, &target.join(&normalized), &normalized)?;
            copied += 1;
        }

        flush_worktree(&target).context("flush agent worktree before publishing its base")?;
        snapshots.write_agent_base(name, &base_snapshot).await?;
        Ok(copied)
    }
    .await;

    match result {
        Ok(copied) => {
            guard.finish_publication().await?;
            Ok(copied)
        }
        Err(operation_error) => {
            if let Err(rollback_error) = guard.rollback().await {
                return Err(anyhow::anyhow!(
                    "agent spawn failed before publication: {operation_error:#}; {rollback_error:#}"
                ));
            }
            Err(operation_error)
        }
    }
}

/// Paths whose folder content differs from `expected`.
async fn paths_differing_from(
    ctx: &SyncCtx<'_>,
    expected: &HashMap<String, FileState>,
) -> Result<Vec<String>> {
    let local = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let paths = local
        .keys()
        .chain(expected.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    Ok(paths
        .into_iter()
        .filter(|path| {
            !matches!(
                (local.get(path), expected.get(path)),
                (Some(local), Some(expected))
                    if local.hash == expected.hash
                        && local.deleted == expected.deleted
                        && local.mode == expected.mode
            )
        })
        .collect())
}

async fn inject_spawn_failure(base: &Path, name: &str, point: &str) -> Result<()> {
    let path = crate::workspace_layout::ensure_workspace_state(base)?
        .join(format!("test-spawn-failpoint-{name}"));
    if fs::read_to_string(&path).await.ok().as_deref() == Some(point) {
        fs::remove_file(path).await?;
        bail!("injected agent spawn failure at {point}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(source: &Path, destination: &Path) -> Result<()> {
        copy_opened_source(std::fs::File::open(source)?, destination, "file")
    }

    #[test]
    fn copies_are_independent_of_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let (source, destination) = (dir.path().join("source"), dir.path().join("a/b/copy"));
        std::fs::write(&source, b"original").unwrap();

        copy(&source, &destination).unwrap();
        flush_worktree(dir.path()).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");

        std::fs::write(&destination, b"agent edit").unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
        std::fs::write(&source, b"human edit").unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"agent edit");
    }

    #[test]
    fn empty_files_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (source, destination) = (dir.path().join("source"), dir.path().join("copy"));
        std::fs::write(&source, b"").unwrap();
        copy(&source, &destination).unwrap();
        assert_eq!(std::fs::metadata(&destination).unwrap().len(), 0);
    }

    #[test]
    fn an_existing_destination_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (source, destination) = (dir.path().join("source"), dir.path().join("copy"));
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"keep").unwrap();
        assert!(copy(&source, &destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn permissions_follow_the_source() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        for mode in [0o755, 0o444, 0o600] {
            let source = dir.path().join(format!("source-{mode:o}"));
            let destination = dir.path().join(format!("copy-{mode:o}"));
            std::fs::write(&source, b"x").unwrap();
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode)).unwrap();
            copy(&source, &destination).unwrap();
            let copied = std::fs::metadata(&destination).unwrap().permissions();
            assert_eq!(copied.mode() & 0o7777, mode);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ordinary_files_clone_and_flagged_files_fall_back() {
        use std::os::macos::fs::MetadataExt as _;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"x").unwrap();
        let opened = std::fs::File::open(&source).unwrap();
        let clone = dir.path().join("clone");
        assert!(
            clone_opened_source(&opened, &opened.metadata().unwrap(), &clone)
                .unwrap()
                .is_some(),
            "temporary directory must be on APFS"
        );

        let name = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is NUL-terminated and outlives both calls.
        assert_eq!(
            unsafe { libc::chflags(name.as_ptr(), libc::UF_IMMUTABLE) },
            0
        );
        let destination = dir.path().join("copy");
        let result = copy(&source, &destination);
        // Clear the flag before asserting so the directory can be removed.
        assert_eq!(unsafe { libc::chflags(name.as_ptr(), 0) }, 0);
        result.unwrap();
        assert_eq!(std::fs::metadata(&destination).unwrap().st_flags(), 0);
        std::fs::write(&destination, b"editable").unwrap();
    }
}
