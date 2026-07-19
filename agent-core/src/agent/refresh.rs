use anyhow::{bail, Result};
use feanorfs_common::{AgentRefreshResult, SyncResponse};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

use super::diff::compute_agent_diff;
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
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    let diff = compute_agent_diff(&ctx, name).await?;
    let agent_path = agent_dir(base, name)?;
    let snapshots = SnapshotEngine::new(&ctx);
    let base_snapshot = snapshots.read_agent_base(name).await?;
    let mut refreshed_base = snapshots.load_files(&base_snapshot).await?;
    if options.replace {
        let agent_db = ClientDb::new(crate::workspace_layout::ensure_workspace_state(
            &agent_path,
        )?)
        .await?;
        let agent_scan =
            crate::local::scan_local_directory(&agent_path, &agent_db, ctx.password()).await?;
        let agent_ctx = SyncCtx::from_config(api, &agent_db, &agent_path, &config)?;
        for state in agent_scan.values().filter(|state| !state.deleted) {
            if crate::large_file::uses_chunk_transport(state.size) {
                crate::large_file::upload(&agent_ctx, &state.path, &state.hash).await?;
            } else {
                let bytes = fs::read(agent_path.join(&state.path)).await?;
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
        crate::sync_pass::process_delete_local(&response, &agent_path, &agent_db).await?;
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
    let agent_db = ClientDb::new(crate::workspace_layout::ensure_workspace_state(
        &agent_path,
    )?)
    .await?;
    let agent_ctx = SyncCtx::from_config(api, &agent_db, &agent_path, &config)?;
    for theirs in &diff.their_changes {
        let response = if theirs.deleted {
            SyncResponse {
                upload_required: Vec::new(),
                download_required: Vec::new(),
                delete_local: vec![theirs.path.clone()],
            }
        } else {
            SyncResponse {
                upload_required: Vec::new(),
                download_required: vec![theirs.clone()],
                delete_local: Vec::new(),
            }
        };
        if theirs.deleted {
            crate::sync_pass::process_delete_local(&response, &agent_path, &agent_db).await?;
            refreshed_base.remove(&theirs.path);
        } else {
            crate::sync_pass::process_downloads(&agent_ctx, &response, &HashMap::new(), false)
                .await?;
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
