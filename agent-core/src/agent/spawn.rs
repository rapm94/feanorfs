use anyhow::{bail, Result};
use feanorfs_common::normalize_path;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;

use crate::api::ApiClient;
use crate::ctx::SyncCtx;
use crate::local::{build_workspace_walker, ClientDb};
use crate::lock::SyncLock;
use crate::paths::{agent_dir, validate_name};
use crate::snapshot::SnapshotEngine;

struct SpawnCleanupGuard {
    target: PathBuf,
    restore_from: Option<PathBuf>,
    armed: bool,
}

impl Drop for SpawnCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let _ = std::fs::remove_dir_all(&self.target);
        if let Some(backup) = &self.restore_from {
            if std::fs::rename(backup, &self.target).is_err() {
                let _ = restore_directory(backup, &self.target);
                let _ = std::fs::remove_dir_all(backup);
            }
        }
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

fn reflink_or_copy(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if reflink::reflink(source, destination).is_err() {
        std::fs::copy(source, destination)?;
    }
    Ok(())
}

fn restore_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let child = destination.join(entry.file_name());
        if ty.is_dir() {
            restore_directory(&entry.path(), &child)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), child)?;
        }
    }
    Ok(())
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
    let config = if base.join(".feanorfs/config.json").exists() {
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
    let target = agent_dir(ctx.base, name);
    if target.exists() {
        if replace {
            // Preserve the original agent tree until the new copy is committed.
        } else {
            bail!(
                "Agent workspace '{name}' already exists. Run `feanorfs agent clean {name}` or use `--replace`."
            );
        }
    }

    let _sync_guard = SyncLock::acquire(ctx.base)?;

    if no_sync {
        let local = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
        let last = crate::conflicts::load_last_synced_snapshot(ctx).await?;
        let dirty = local
            .iter()
            .filter(|(path, state)| {
                !last.get(*path).is_some_and(|last_state| {
                    last_state.hash == state.hash && last_state.deleted == state.deleted
                })
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        if !dirty.is_empty() {
            bail!(
                "Folder is not in sync with last agreed state. Dirty paths: {}",
                dirty.join(", ")
            );
        }
    }

    let pending = crate::conflicts::pending_conflict_paths(ctx.db).await?;
    if !pending.is_empty() {
        bail!(
            "Your folder needs attention before an agent can copy it. Conflicts: {}",
            pending.into_iter().collect::<Vec<_>>().join(", ")
        );
    }

    crate::sync_pass::do_sync(
        ctx.api,
        ctx.db,
        ctx.base,
        ctx.workspace_id(),
        ctx.password(),
        false,
    )
    .await?;

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

    let server_files = crate::conflicts::load_server_view(ctx).await?;
    let base_snapshot = SnapshotEngine::new(ctx)
        .publish_server_view(&server_files, "folder")
        .await?;

    let restore_from = if replace && target.exists() {
        let backup = replacement_backup_path(&target);
        if fs::try_exists(&backup).await? {
            fs::remove_dir_all(&backup).await?;
        }
        fs::rename(&target, &backup).await?;
        Some(backup)
    } else {
        None
    };

    let mut guard = SpawnCleanupGuard {
        target: target.clone(),
        restore_from,
        armed: true,
    };

    fs::create_dir_all(&target).await?;

    let result: Result<usize> = async {
        inject_spawn_failure(ctx.base, name, "after-stage").await?;

        let mut copied = 0;
        for entry in build_workspace_walker(ctx.base, false)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        {
            let Ok(relative) = entry.path().strip_prefix(ctx.base) else {
                continue;
            };
            let Some(relative) = relative.to_str() else {
                continue;
            };

            let normalized = normalize_path(relative);
            if !feanorfs_common::is_safe_rel_path(&normalized) {
                continue;
            }

            reflink_or_copy(entry.path(), &target.join(normalized))?;
            copied += 1;
        }

        SnapshotEngine::new(ctx)
            .write_agent_base(name, &base_snapshot)
            .await?;
        if let Some(backup) = guard.restore_from.take() {
            let _ = fs::remove_dir_all(backup).await;
        }
        Ok(copied)
    }
    .await;

    if result.is_err() {
        if let Some(backup) = guard.restore_from.take() {
            let _ = fs::remove_dir_all(&target).await;
            let _ = restore_directory(&backup, &target);
            let _ = fs::remove_dir_all(backup).await;
        }
    }

    guard.armed = false;
    result
}

async fn inject_spawn_failure(base: &Path, name: &str, point: &str) -> Result<()> {
    let path = base
        .join(".feanorfs")
        .join(format!("test-spawn-failpoint-{name}"));
    if fs::read_to_string(&path).await.ok().as_deref() == Some(point) {
        fs::remove_file(path).await?;
        bail!("injected agent spawn failure at {point}");
    }
    Ok(())
}
