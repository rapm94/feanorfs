mod check;
mod diff;
mod land;
mod proposal;
mod refresh;
mod runner;
mod runtime;
mod spawn;

#[cfg(test)]
mod tests;

use anyhow::Result;
use std::path::Path;
use tokio::fs;

use crate::api::ApiClient;
use crate::local::ClientDb;
use crate::paths::{agent_base_ref, agent_root, agents_dir, validate_name};

pub use check::check_agent;
pub use land::land_agent;
pub use refresh::{refresh_agent, refresh_agent_guarded, refresh_agent_with_options};
pub use runner::{
    remove_configured, runner_process_metadata, runner_status, RunnerAdmission, RunnerAttention,
    RunnerConfig, RunnerExecutionMode, RunnerExecutionSession, RunnerInvocation, RunnerLaunch,
    RunnerPhase, RunnerProcessMetadata, RunnerStatus, RunnerStore,
};
pub use spawn::spawn_agent;

/// Controls whether refresh preserves or replaces agent-local edits.
#[derive(Debug, Clone, Copy, Default)]
pub struct RefreshOptions {
    /// Replace the complete agent worktree while retaining its pre-operation snapshot.
    pub replace: bool,
}

#[allow(clippy::too_many_arguments)]
pub async fn commit_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
) -> Result<feanorfs_common::AgentCommitResult> {
    let land = land_agent(base, db, api, workspace_id, name, password, false, false).await?;
    Ok(feanorfs_common::AgentCommitResult {
        agent_name: land.agent_name,
        our_changes: land.our_changes,
        their_changes: land.their_changes,
        conflicts: land.conflicts,
    })
}

pub async fn list_agents(base: &Path, _db: &ClientDb) -> Result<Vec<String>> {
    let mut visible = Vec::new();
    let mut entries = match fs::read_dir(agents_dir(base)?).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(visible),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if validate_name(&name).is_err() {
            continue;
        }
        if entry.file_type().await?.is_dir() && agent_base_ref(base, &name)?.is_file() {
            visible.push(name);
        }
    }
    visible.sort();
    Ok(visible)
}

pub async fn clean_agent(base: &Path, _db: &ClientDb, name: &str) -> Result<()> {
    validate_name(name)?;
    let _runner_lifecycle = runner::RunnerLifecycleLock::acquire_async(base).await?;
    clean_agent_locked(base, name).await
}

pub(crate) async fn clean_agent_with_runner_guard(
    base: &Path,
    name: &str,
    _runner_guard: &runner::RunnerOperationGuard,
) -> Result<()> {
    validate_name(name)?;
    clean_agent_locked(base, name).await
}

async fn clean_agent_locked(base: &Path, name: &str) -> Result<()> {
    if runner::runner_status(base)?.is_some_and(|status| status.agent == name) {
        anyhow::bail!(
            "agent workspace '{name}' has a configured runner; runner removal is required before cleanup"
        );
    }
    let target = agent_root(base, name)?;
    if target.exists() {
        fs::remove_dir_all(target).await?;
    }
    Ok(())
}
