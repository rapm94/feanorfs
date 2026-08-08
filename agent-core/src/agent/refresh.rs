use anyhow::{bail, Result};
use feanorfs_common::{AgentRefreshResult, SyncResponse};
use std::path::Path;

use super::diff::compute_agent_diff;
use super::runner::{RunnerExecutionSession, RunnerOperationGuard};
use super::runtime::open_agent_runtime;
use super::RefreshOptions;
use crate::api::ApiClient;
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::local::ClientDb;
use crate::paths::agent_dir;
use crate::snapshot::{SnapshotEngine, SnapshotInput};

pub async fn refresh_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
) -> Result<AgentRefreshResult> {
    refresh_agent_with_options(
        base,
        db,
        api,
        workspace_id,
        name,
        password,
        RefreshOptions::default(),
    )
    .await
}

/// Pull current-head state into an agent using explicit refresh semantics.
///
/// # Errors
/// Returns an error when snapshots, object downloads, or worktree writes fail.
pub async fn refresh_agent_with_options(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    options: RefreshOptions,
) -> Result<AgentRefreshResult> {
    let _runner_guard = RunnerOperationGuard::acquire_async(base, name).await?;
    refresh_agent_impl(base, db, api, name, options).await
}

/// Refreshes from a trusted runner that already owns the exact agent lease.
pub async fn refresh_agent_guarded(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    runner_session: &RunnerExecutionSession<'_>,
) -> Result<AgentRefreshResult> {
    runner_session.ensure_matches(base, name)?;
    refresh_agent_impl(base, db, api, name, RefreshOptions::default()).await
}

async fn refresh_agent_impl(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    name: &str,
    options: RefreshOptions,
) -> Result<AgentRefreshResult> {
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    let _land_guard = crate::lock::LandLock::acquire(base)?;
    let _sync_guard = crate::lock::SyncLock::acquire(base)?;
    let diff = compute_agent_diff(&ctx, name).await?;
    let agent_path = agent_dir(base, name)?;
    let snapshots = SnapshotEngine::new(&ctx);
    let base_snapshot = snapshots.read_agent_base(name).await?;
    let mut refreshed_base = snapshots.load_files(&base_snapshot).await?;
    if options.replace {
        let agent_runtime = open_agent_runtime(base, name).await?;
        let agent_scan =
            crate::local::scan_local_directory(&agent_path, &agent_runtime.db, ctx.password())
                .await?;
        let agent_ctx = SyncCtx::from_config_with_state_dir(
            api,
            &agent_runtime.db,
            &agent_path,
            &config,
            agent_runtime.state_dir.clone(),
        )?;
        let agent_read_root = crate::workspace_read::WorkspaceReadRoot::open(&agent_path)?;
        for state in agent_scan.values().filter(|state| !state.deleted) {
            if crate::large_file::uses_chunk_transport(state.size) {
                crate::large_file::upload(&agent_ctx, &state.path, &state.hash).await?;
            } else {
                let bytes =
                    crate::sync_pass::read_upload_source(&agent_read_root, &state.path, state)
                        .await?;
                let (hash, encrypted) = seal(&bytes, ctx.password_str(), &state.path)?;
                if hash != state.hash {
                    bail!("agent file changed while preparing refresh: {}", state.path);
                }
                ctx.api
                    .upload_object(ctx.workspace_id(), &hash, encrypted)
                    .await?;
            }
        }
        let before_replace = snapshots
            .write(SnapshotInput {
                files: &agent_scan,
                conflicts: &[],
                parents: vec![base_snapshot],
                author: name,
                message: Some("before refresh --replace".to_string()),
            })
            .await?;
        let current = snapshots.load_state(&diff.current_head).await?;
        let response = SyncResponse {
            upload_required: Vec::new(),
            download_required: current.files.values().cloned().collect(),
            delete_local: agent_scan
                .keys()
                .filter(|path| !current.files.contains_key(*path))
                .cloned()
                .collect(),
        };
        crate::sync_pass::process_downloads(&agent_ctx, &response, &agent_scan, false).await?;
        crate::sync_pass::process_delete_local(&response, &agent_path, &agent_runtime.db).await?;
        let refreshed_snapshot = snapshots
            .write(SnapshotInput {
                files: &current.files,
                conflicts: &current.conflicts,
                parents: vec![before_replace, diff.current_head],
                author: name,
                message: Some("refresh --replace".to_string()),
            })
            .await?;
        snapshots
            .write_agent_base(name, &refreshed_snapshot)
            .await?;
        let mut refreshed = response
            .download_required
            .iter()
            .map(|file| file.path.clone())
            .chain(response.delete_local)
            .collect::<Vec<_>>();
        refreshed.sort();
        refreshed.dedup();
        return Ok(AgentRefreshResult {
            agent_name: name.to_string(),
            refreshed,
            deferred: Vec::new(),
        });
    }
    let mut refreshed = Vec::new();
    let mut deferred = Vec::new();
    let agent_runtime = open_agent_runtime(base, name).await?;
    let agent_scan =
        crate::local::scan_local_directory(&agent_path, &agent_runtime.db, ctx.password()).await?;
    let agent_ctx = SyncCtx::from_config_with_state_dir(
        api,
        &agent_runtime.db,
        &agent_path,
        &config,
        agent_runtime.state_dir.clone(),
    )?;
    let refresh_response = SyncResponse {
        upload_required: Vec::new(),
        download_required: diff
            .their_changes
            .iter()
            .filter(|state| !state.deleted)
            .cloned()
            .collect(),
        delete_local: diff
            .their_changes
            .iter()
            .filter(|state| state.deleted)
            .map(|state| state.path.clone())
            .collect(),
    };
    crate::sync_pass::process_downloads(&agent_ctx, &refresh_response, &agent_scan, false).await?;
    crate::sync_pass::process_delete_local(&refresh_response, &agent_path, &agent_runtime.db)
        .await?;
    for theirs in &diff.their_changes {
        if theirs.deleted {
            refreshed_base.remove(&theirs.path);
        } else {
            refreshed_base.insert(theirs.path.clone(), theirs.clone());
        }
        refreshed.push(theirs.path.clone());
    }
    deferred.extend(diff.conflicts.iter().map(|(edit, _)| edit.path.clone()));
    if !refreshed.is_empty() {
        let refreshed_snapshot = snapshots
            .write(SnapshotInput {
                files: &refreshed_base,
                conflicts: &[],
                parents: vec![base_snapshot],
                author: name,
                message: Some("refresh".to_string()),
            })
            .await?;
        snapshots
            .write_agent_base(name, &refreshed_snapshot)
            .await?;
    }
    Ok(AgentRefreshResult {
        agent_name: name.to_string(),
        refreshed,
        deferred,
    })
}
