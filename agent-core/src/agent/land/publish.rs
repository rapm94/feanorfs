use anyhow::{bail, Context, Result};
use std::path::Path;
#[cfg(debug_assertions)]
use tokio::fs;

use super::super::diff::{build_land_candidate, compute_agent_diff, AgentDiff};
use super::super::runtime::open_agent_runtime;
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::snapshot::{SnapshotEngine, SnapshotInput};
use crate::SwapHeadResult;

pub(super) struct PublishInput<'a, 'ctx> {
    pub(super) ctx: &'a SyncCtx<'ctx>,
    pub(super) name: &'a str,
    pub(super) agent_base: &'a str,
    pub(super) agent_path: &'a Path,
}

pub(super) async fn inject_land_failure(base: &Path, name: &str, point: &str) -> Result<()> {
    #[cfg(debug_assertions)]
    {
        let path = crate::workspace_layout::ensure_workspace_state(base)?
            .join(format!("test-land-failpoint-{name}"));
        if fs::read_to_string(&path).await.ok().as_deref() == Some(point) {
            fs::remove_file(path).await?;
            bail!("injected agent land failure at {point}");
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = (base, name, point);
    Ok(())
}

pub(super) async fn publish_land(
    input: PublishInput<'_, '_>,
    mut diff: AgentDiff,
) -> Result<(AgentDiff, String)> {
    let snapshots = SnapshotEngine::new(input.ctx);
    let agent_runtime = open_agent_runtime(input.ctx.base, input.name).await?;
    let agent_config = crate::local::load_config(input.ctx.base)?;
    let mut committed_snapshot = None;
    let agent_read_root = crate::workspace_read::WorkspaceReadRoot::open(input.agent_path)?;
    for _ in 0..8 {
        for change in diff.our_changes.iter().filter(|change| !change.deleted) {
            if crate::large_file::uses_chunk_transport(change.size) {
                let agent_ctx = SyncCtx::from_config_with_state_dir(
                    input.ctx.api,
                    &agent_runtime.db,
                    input.agent_path,
                    &agent_config,
                    agent_runtime.state_dir.clone(),
                )?;
                crate::large_file::upload(&agent_ctx, &change.path, &change.hash).await?;
            } else {
                let bytes =
                    crate::sync_pass::read_upload_source(&agent_read_root, &change.path, change)
                        .await?;
                let (hash, encrypted) = seal(&bytes, input.ctx.password_str(), &change.path)?;
                if hash != change.hash {
                    bail!("agent file changed while preparing land: {}", change.path);
                }
                input
                    .ctx
                    .api
                    .upload_object(input.ctx.workspace_id(), &hash, encrypted)
                    .await?;
            }
        }
        for (conflict, _) in &diff.conflicts {
            let Some(ours) = conflict.ours.as_ref().filter(|state| !state.deleted) else {
                continue;
            };
            if crate::large_file::uses_chunk_transport(ours.size) {
                let agent_ctx = SyncCtx::from_config_with_state_dir(
                    input.ctx.api,
                    &agent_runtime.db,
                    input.agent_path,
                    &agent_config,
                    agent_runtime.state_dir.clone(),
                )?;
                crate::large_file::upload(&agent_ctx, &conflict.path, &ours.hash).await?;
            } else {
                let bytes =
                    crate::sync_pass::read_upload_source(&agent_read_root, &conflict.path, ours)
                        .await?;
                let (hash, encrypted) = seal(&bytes, input.ctx.password_str(), &conflict.path)?;
                if hash != ours.hash {
                    bail!("agent file changed while preparing land: {}", conflict.path);
                }
                input
                    .ctx
                    .api
                    .upload_object(input.ctx.workspace_id(), &hash, encrypted)
                    .await?;
            }
        }
        let materialization = feanorfs_common::SyncResponse {
            upload_required: Vec::new(),
            download_required: diff
                .our_changes
                .iter()
                .filter(|state| !state.deleted)
                .cloned()
                .collect(),
            delete_local: diff
                .our_changes
                .iter()
                .filter(|state| state.deleted)
                .map(|state| state.path.clone())
                .collect(),
        };
        // Verify every landed byte through the hub/object cache and fsync a
        // same-filesystem staging copy before the head CAS. Materialization
        // after the commit point therefore cannot discover a missing/corrupt
        // object that publication should have rejected.
        crate::sync_pass::prefetch_downloads(input.ctx, &materialization).await?;
        let current_root = snapshots.load_snapshot(&diff.current_head).await?.root;
        let candidate_state = build_land_candidate(&snapshots, &diff).await?;
        if candidate_state.root == current_root {
            if input
                .ctx
                .api
                .get_head(input.ctx.workspace_id())
                .await?
                .as_deref()
                == Some(diff.current_head.as_str())
            {
                committed_snapshot = Some(diff.current_head.clone());
                break;
            }
            diff = compute_agent_diff(input.ctx, input.name).await?;
            continue;
        }
        let parents = if input.agent_base == diff.current_head {
            vec![diff.current_head.clone()]
        } else {
            vec![input.agent_base.to_string(), diff.current_head.clone()]
        };
        let candidate = snapshots
            .write(SnapshotInput {
                files: &candidate_state.files,
                conflicts: &candidate_state.conflicts,
                parents,
                author: input.name,
                message: None,
            })
            .await?;
        inject_land_failure(input.ctx.base, input.name, "after-stage").await?;
        match input
            .ctx
            .api
            .swap_head(
                input.ctx.workspace_id(),
                Some(&diff.current_head),
                &candidate,
            )
            .await?
        {
            SwapHeadResult::Swapped => {
                committed_snapshot = Some(candidate);
                break;
            }
            SwapHeadResult::Conflict(Some(_)) => {
                diff = compute_agent_diff(input.ctx, input.name).await?;
            }
            SwapHeadResult::Conflict(None) => {
                bail!("workspace head disappeared during agent land");
            }
        }
    }
    let snapshot_id =
        committed_snapshot.context("workspace head changed too many times during land")?;
    Ok((diff, snapshot_id))
}
