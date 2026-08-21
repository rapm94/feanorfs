mod materialize;
mod publish;

use anyhow::{bail, Result};
use feanorfs_common::{AgentLandResult, ConcurrentEdit};
use std::path::{Path, PathBuf};

use super::clean_agent_with_runner_guard;
use super::diff::{build_land_candidate, compute_agent_diff};
use super::proposal::write_proposal_if_clean;
use super::runner::RunnerOperationGuard;
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
    let runner_guard = RunnerOperationGuard::acquire_async(base, name).await?;
    if clean_after && runner_guard.protects_configured_runner() {
        bail!(
            "agent workspace '{name}' has a configured runner; remove the runner before landing with cleanup"
        );
    }
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    land_agent_with_ctx(&ctx, name, clean_after, propose, &runner_guard, None).await
}

pub(super) async fn land_agent_with_ctx(
    ctx: &SyncCtx<'_>,
    name: &str,
    clean_after: bool,
    propose: bool,
    runner_guard: &RunnerOperationGuard,
    scope: Option<&feanorfs_common::WorkScope>,
) -> Result<AgentLandResult> {
    let _land_guard = LandLock::acquire(ctx.base)?;
    let _sync_guard = SyncLock::acquire(ctx.base)?;
    let pending = pending_conflict_paths(ctx.db).await?;
    if !pending.is_empty() {
        return Err(super::continuous::conflict_failure(format!(
            "Your folder needs attention before landing agent work. Conflicts: {}",
            pending.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let snapshots = SnapshotEngine::new(ctx);
    let agent_base = snapshots.read_agent_base(name).await?;
    // Canonical accepted-scope path entries (sorted, unique) that publication
    // re-applies on every CAS-retry recompute, so no retry path can publish
    // an unfiltered diff.
    let scope_paths = scope
        .map(|scope| {
            let mut entries = scope.paths.clone();
            entries.sort();
            entries.dedup();
            entries
        })
        .unwrap_or_default();
    let mut diff = compute_agent_diff(ctx, name).await?;
    if let Some(scope) = scope {
        diff = super::scope::filter_diff_by_scope(diff, scope);
    }
    let initial = build_land_candidate(&snapshots, &diff).await?;
    let current_snapshot = snapshots.load_snapshot(&diff.current_head).await?;
    let current_root = current_snapshot.root.clone();
    let recovering_committed_land = !diff.our_changes.is_empty()
        && initial.root == current_root
        && current_snapshot.author == name
        && current_snapshot
            .parents
            .iter()
            .any(|parent| parent == &agent_base);
    let recovery_gate_head = recovering_committed_land
        .then(|| current_snapshot.parents.last().cloned())
        .flatten();
    if !recovering_committed_land {
        let (_, blocked) =
            crate::sync_pass::run_sync_pass_locked(ctx, crate::sync_pass::SyncMode::Full, false)
                .await?;
        if !blocked.is_empty() {
            return Err(super::continuous::conflict_failure(format!(
                "Your folder needs attention before landing agent work. Conflicts: {}",
                blocked.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
        diff = compute_agent_diff(ctx, name).await?;
        if let Some(scope) = scope {
            diff = super::scope::filter_diff_by_scope(diff, scope);
        }
    }
    let agent_path = agent_dir(ctx.base, name)?;
    let gate_local = if let Some(previous_head) = recovery_gate_head.as_deref() {
        let mut gate = snapshots.load_state(previous_head).await?.files;
        let current = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
        // A crash may have happened either immediately after the head CAS or
        // after some/all worktree paths were already activated. Adopt only
        // paths that exactly equal the committed agent result; every other
        // current value remains compared with the pre-land head so a later
        // human edit is diverted rather than overwritten.
        for change in &diff.our_changes {
            if change.deleted {
                if !current.contains_key(&change.path) {
                    gate.remove(&change.path);
                }
            } else if let Some(actual) = current.get(&change.path).filter(|actual| {
                !actual.deleted && actual.hash == change.hash && actual.mode == change.mode
            }) {
                gate.insert(change.path.clone(), actual.clone());
            }
        }
        gate
    } else {
        crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?
    };
    if !recovering_committed_land {
        let (_, blocked, _) = negotiate_sync_with_conflict_gate(ctx, &gate_local, false).await?;
        if !blocked.is_empty() {
            return Err(super::continuous::conflict_failure(format!(
                "Your folder changed during land and needs attention: {}",
                blocked.into_iter().collect::<Vec<_>>().join(", ")
            )));
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
    crate::sync_pass::preflight_download_projection(ctx.base, &gate_local, &materialization)
        .await?;
    let (diff, snapshot_id) = publish_land(
        PublishInput {
            ctx,
            name,
            agent_base: &agent_base,
            agent_path: &agent_path,
            scope_paths,
        },
        diff,
    )
    .await?;
    inject_land_failure(ctx.base, name, "after-cas").await?;
    let landed = materialize_land(
        MaterializeInput {
            ctx,
            name,
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
        clean_agent_with_runner_guard(ctx.base, name, runner_guard).await?;
    } else {
        let base_snapshot = if diff.their_changes.is_empty() && diff.conflicts.is_empty() {
            // The committed head exactly describes the agent worktree's new
            // agreed view, so retain the ordinary fast path.
            snapshot_id.clone()
        } else {
            // The shared head may include disjoint remote paths that are not
            // in this agent worktree yet. Advancing the agent base straight
            // to that head would make those absent paths look like agent-local
            // deletions on the next pass. Record a local-only working-copy
            // base instead: old base plus the agent changes committed to the
            // head. Those paths already describe the agent worktree even when
            // shared-folder materialization was safely diverted. Ordinary
            // refresh can then see and materialize every still-inbound remote
            // path.
            let mut base_state = snapshots.load_state(&agent_base).await?;
            for change in &diff.our_changes {
                if change.deleted {
                    base_state.files.remove(&change.path);
                } else {
                    base_state.files.insert(change.path.clone(), change.clone());
                }
            }
            snapshots
                .write_local(crate::snapshot::SnapshotInput {
                    files: &base_state.files,
                    conflicts: &base_state.conflicts,
                    parents: vec![agent_base.clone()],
                    author: name,
                    message: Some("post-land working-copy base".to_string()),
                })
                .await?
        };
        snapshots.write_agent_base(name, &base_snapshot).await?;
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
