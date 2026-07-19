mod materialize;
mod publish;

use anyhow::{bail, Result};
use feanorfs_common::{AgentLandResult, ConcurrentEdit};
use std::path::{Path, PathBuf};

use super::clean_agent;
use super::diff::{build_land_candidate, compute_agent_diff};
use super::proposal::write_proposal_if_clean;
use crate::api::ApiClient;
use crate::conflict_artifacts::{enrich_conflict_edit, enrich_conflict_edit_preview};
use crate::conflicts::{
    negotiate_sync_with_conflict_gate, pending_conflict_paths, register_and_write_conflicts,
};
use crate::ctx::SyncCtx;
use crate::local::ClientDb;
use crate::lock::{LandLock, SyncLock};
use crate::paths::agent_dir;
use crate::snapshot::SnapshotEngine;
use materialize::{materialize_land, MaterializeInput};
use publish::{inject_land_failure, publish_land, PublishInput};

#[allow(clippy::too_many_arguments)]
pub async fn land_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    clean_after: bool,
    propose: bool,
) -> Result<AgentLandResult> {
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    land_agent_with_ctx(&ctx, name, clean_after, propose).await
}

async fn land_agent_with_ctx(
    ctx: &SyncCtx<'_>,
    name: &str,
    clean_after: bool,
    propose: bool,
) -> Result<AgentLandResult> {
    let _land_guard = LandLock::acquire(ctx.base)?;
    let _sync_guard = SyncLock::acquire(ctx.base)?;
    let pending = pending_conflict_paths(ctx.db).await?;
    if !pending.is_empty() {
        bail!(
            "Your folder needs attention before landing agent work. Conflicts: {}",
            pending.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    let snapshots = SnapshotEngine::new(ctx);
    let agent_base = snapshots.read_agent_base(name).await?;
    let mut diff = compute_agent_diff(ctx, name).await?;
    let initial = build_land_candidate(&snapshots, &diff).await?;
    let current_root = snapshots.load_snapshot(&diff.current_head).await?.root;
    let recovering_committed_land = !diff.our_changes.is_empty() && initial.root == current_root;
    if !recovering_committed_land {
        let (_, blocked) =
            crate::sync_pass::run_sync_pass_locked(ctx, crate::sync_pass::SyncMode::Full, false)
                .await?;
        if !blocked.is_empty() {
            bail!(
                "Your folder needs attention before landing agent work. Conflicts: {}",
                blocked.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        diff = compute_agent_diff(ctx, name).await?;
    }
    let agent_path = agent_dir(ctx.base, name)?;
    let gate_local = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    if !recovering_committed_land {
        let (_, blocked) = negotiate_sync_with_conflict_gate(ctx, &gate_local, false).await?;
        if !blocked.is_empty() {
            bail!(
                "Your folder changed during land and needs attention: {}",
                blocked.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
    }
    let (diff, snapshot_id) = publish_land(
        PublishInput {
            ctx,
            name,
            agent_base: &agent_base,
            agent_path: &agent_path,
        },
        diff,
    )
    .await?;
    inject_land_failure(ctx.base, name, "after-cas").await?;
    let landed = materialize_land(
        MaterializeInput {
            ctx,
            name,
            agent_path: &agent_path,
            gate_local: &gate_local,
        },
        &diff,
    )
    .await?;
    let mut conflict_dir = None;
    let registered_count = if diff.conflicts.is_empty() {
        0
    } else {
        let (directory, paths) =
            register_and_write_conflicts(ctx, &diff.conflicts, Some(&agent_path)).await?;
        conflict_dir = Some(directory);
        paths.len()
    };
    let empty_path = PathBuf::new();
    let conflict_dir_ref = conflict_dir.as_ref().unwrap_or(&empty_path);
    let mut conflicts = diff
        .conflicts
        .iter()
        .map(|(edit, kind)| {
            if conflict_dir.is_some() {
                enrich_conflict_edit(edit.clone(), *kind, conflict_dir_ref)
            } else {
                enrich_conflict_edit_preview(edit.clone(), *kind)
            }
        })
        .collect::<Vec<ConcurrentEdit>>();
    if propose {
        for edit in &mut conflicts {
            write_proposal_if_clean(conflict_dir_ref, edit)?;
        }
    }
    if clean_after {
        clean_agent(ctx.base, ctx.db, name).await?;
    } else {
        snapshots.write_agent_base(name, &snapshot_id).await?;
    }
    let message = if landed.is_empty() && conflicts.is_empty() {
        "Nothing to land.".to_string()
    } else {
        let applied = landed
            .iter()
            .filter(|path| matches!(path.action.as_str(), "updated" | "deleted"))
            .count();
        format!(
            "Landed {} path(s){}.",
            applied,
            if registered_count > 0 {
                format!(", {registered_count} need attention")
            } else {
                String::new()
            }
        )
    };
    Ok(AgentLandResult {
        agent_name: name.to_string(),
        our_changes: diff.our_changes,
        their_changes: diff.their_changes,
        conflicts,
        landed,
        message,
        snapshot_id: Some(snapshot_id),
    })
}
