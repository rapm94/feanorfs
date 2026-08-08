use anyhow::{bail, Context as _, Result};
use std::collections::BTreeSet;
use std::io::Write as _;
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

fn copy_opened_source(
    mut source: std::fs::File,
    destination: &Path,
    logical_path: &str,
) -> Result<()> {
    let before = source.metadata()?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .with_context(|| format!("create agent copy for {logical_path}"))?;
    let copied = std::io::copy(&mut source, &mut output)?;
    output.flush()?;
    output.sync_all()?;
    let after = source.metadata()?;
    if copied != before.len()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        bail!("workspace file {logical_path} changed while spawning agent");
    }
    output.set_permissions(before.permissions())?;
    output.sync_all()?;
    Ok(())
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
        let local = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
        let last = snapshots.load_files_local(&last_synced_id).await?;
        let paths = local
            .keys()
            .chain(last.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let dirty = paths
            .into_iter()
            .filter(|path| {
                !matches!(
                    (local.get(path), last.get(path)),
                    (Some(local), Some(last))
                        if local.hash == last.hash
                            && local.deleted == last.deleted
                            && local.mode == last.mode
                )
            })
            .collect::<Vec<_>>();
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
        bail!(
            "Your folder needs attention before an agent can copy it. Conflicts: {}",
            pending.into_iter().collect::<Vec<_>>().join(", ")
        );
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

        let server_files = crate::conflicts::load_server_view(ctx).await?;
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
            let source = read_root
                .open_regular_path(relative)
                .with_context(|| format!("open agent source {normalized}"))?;
            copy_opened_source(source, &target.join(&normalized), &normalized)?;
            copied += 1;
        }

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

async fn inject_spawn_failure(base: &Path, name: &str, point: &str) -> Result<()> {
    let path = crate::workspace_layout::ensure_workspace_state(base)?
        .join(format!("test-spawn-failpoint-{name}"));
    if fs::read_to_string(&path).await.ok().as_deref() == Some(point) {
        fs::remove_file(path).await?;
        bail!("injected agent spawn failure at {point}");
    }
    Ok(())
}
