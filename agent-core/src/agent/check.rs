use anyhow::Result;
use feanorfs_common::{AgentCheckResult, ConcurrentEdit};
use std::path::Path;

use super::diff::compute_agent_diff;
use crate::api::ApiClient;
use crate::conflict_artifacts::enrich_conflict_edit_preview;
use crate::ctx::SyncCtx;
use crate::local::ClientDb;

pub async fn check_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
) -> Result<AgentCheckResult> {
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    let diff = compute_agent_diff(&ctx, name).await?;
    let conflicts = diff
        .conflicts
        .iter()
        .map(|(edit, kind)| enrich_conflict_edit_preview(edit.clone(), *kind))
        .collect::<Vec<ConcurrentEdit>>();
    Ok(AgentCheckResult {
        agent_name: name.to_string(),
        our_changes: diff.our_changes,
        their_changes: diff.their_changes,
        conflicts,
        conflict_risk: diff.conflict_risk,
    })
}
