use anyhow::{bail, Result};
use feanorfs_common::normalize_path;
use std::path::{Path, PathBuf};
use tokio::fs;

use super::clean_agent;
use crate::api::ApiClient;
use crate::ctx::SyncCtx;
use crate::local::{build_workspace_walker, ClientDb};
use crate::lock::SyncLock;
use crate::paths::{agent_dir, validate_name};
use crate::snapshot::SnapshotEngine;

struct SpawnCleanupGuard {
    target: PathBuf,
    armed: bool,
}

impl Drop for SpawnCleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.target);
        }
    }
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
            format_version: 1,
            hub_local: false,
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
            clean_agent(ctx.base, ctx.db, name).await?;
        } else {
            bail!(
                "Agent workspace '{}' already exists. Run `feanorfs agent clean {}` or use `--replace`.",
                name,
                name
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
    } else {
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
    }
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
    fs::create_dir_all(&target).await?;
    let mut guard = SpawnCleanupGuard {
        target: target.clone(),
        armed: true,
    };
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
    guard.armed = false;
    Ok(copied)
}
