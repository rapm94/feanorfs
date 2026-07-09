//! Tray dashboard aggregation — shells no duplicate sync logic.

use crate::agent::check_agent;
use crate::agent::list_agents;
use crate::api::ApiClient;
use crate::commands::{do_status, MirrorState};
use crate::conflict_artifacts::{is_binary_content, resolve_artifact, ArtifactRole};
use crate::local::{load_config, ClientDb};
use crate::lock::try_acquire_sync_lock;
use crate::tray_state::{is_paused, is_syncing, is_watching};
use anyhow::Result;
use feanorfs_common::tray_contract::{
    TrayAgentEntry, TrayAgentsSummary, TrayConflictEntry, TrayStatusResult,
};
use feanorfs_common::{ConflictKind, ConflictRecord};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

const AGENT_CACHE_FILE: &str = "tray-agent-cache.json";
const AGENT_CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAgents {
    cached_at_ms: i64,
    summary: TrayAgentsSummary,
}

fn agent_cache_path(current_dir: &Path) -> PathBuf {
    current_dir.join(".feanorfs").join(AGENT_CACHE_FILE)
}

fn cache_agents(current_dir: &Path, summary: &TrayAgentsSummary) {
    let entry = CachedAgents {
        cached_at_ms: chrono::Utc::now().timestamp_millis(),
        summary: summary.clone(),
    };
    if let Ok(json) = serde_json::to_string(&entry) {
        let _ = std::fs::write(agent_cache_path(current_dir), json);
    }
}

fn cached_agents(current_dir: &Path) -> Option<TrayAgentsSummary> {
    let content = std::fs::read_to_string(agent_cache_path(current_dir)).ok()?;
    let entry: CachedAgents = serde_json::from_str(&content).ok()?;
    let age_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(entry.cached_at_ms);
    if age_ms < i64::try_from(AGENT_CACHE_TTL.as_millis()).unwrap_or(i64::MAX) {
        Some(entry.summary)
    } else {
        None
    }
}

/// Drop cached agent summary after land/keep so the next tray status is fresh.
pub fn invalidate_agent_cache(current_dir: &Path) {
    let _ = std::fs::remove_file(agent_cache_path(current_dir));
}

fn conflict_kind_str(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::EditEdit => "edit_edit",
        ConflictKind::EditDelete => "edit_delete",
        ConflictKind::DeleteEdit => "delete_edit",
    }
}

fn conflict_choices(kind: ConflictKind) -> Vec<String> {
    match kind {
        ConflictKind::EditEdit => vec!["local".into(), "cloud".into(), "both".into()],
        ConflictKind::EditDelete | ConflictKind::DeleteEdit => {
            vec!["local".into(), "cloud".into()]
        }
    }
}

fn conflict_label(record: &ConflictRecord) -> String {
    match record.kind {
        ConflictKind::EditEdit => "You and the cloud both changed this file".into(),
        ConflictKind::EditDelete => "You changed it; the cloud deleted it".into(),
        ConflictKind::DeleteEdit => "You deleted it; the cloud changed it".into(),
    }
}

fn mirror_state_str(state: MirrorState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "idle".into())
}

fn workspace_label(current_dir: &Path) -> String {
    current_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace")
        .to_string()
}

async fn load_agents_summary(
    current_dir: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<TrayAgentsSummary> {
    if let Some(cached) = cached_agents(current_dir) {
        return Ok(cached);
    }

    let names = list_agents(current_dir, db).await?;
    let mut entries = Vec::new();
    let mut working = 0u32;
    let mut need_attention = 0u32;

    for name in &names {
        match check_agent(current_dir, db, api, workspace_id, name, password).await {
            Ok(check) => {
                let conflict_count = check.conflicts.len() as u32;
                let change_count = check.our_changes.len() as u32;
                let state = if conflict_count > 0 {
                    need_attention += 1;
                    working += 1;
                    "conflicts"
                } else if change_count > 0 {
                    working += 1;
                    "changes"
                } else {
                    "clean"
                };
                entries.push(TrayAgentEntry {
                    name: name.clone(),
                    state: state.into(),
                    change_count,
                    conflict_count,
                });
            }
            Err(_) => {
                entries.push(TrayAgentEntry {
                    name: name.clone(),
                    state: "offline".into(),
                    change_count: 0,
                    conflict_count: 0,
                });
            }
        }
    }

    let summary = TrayAgentsSummary {
        working,
        need_attention,
        entries,
    };
    cache_agents(current_dir, &summary);
    Ok(summary)
}

async fn cheap_tray_status(
    current_dir: &Path,
    config: &crate::local::Config,
    db: &ClientDb,
) -> Result<TrayStatusResult> {
    let records = db.list_conflict_records().await?;
    let pending_conflicts: Vec<TrayConflictEntry> = records
        .iter()
        .map(|r| TrayConflictEntry {
            path: r.path.clone(),
            kind: conflict_kind_str(r.kind).into(),
            label: conflict_label(r),
            choices: conflict_choices(r.kind),
        })
        .collect();

    let agents = cached_agents(current_dir).unwrap_or(TrayAgentsSummary {
        working: 0,
        need_attention: 0,
        entries: vec![],
    });

    let mirror = if pending_conflicts.is_empty() {
        MirrorState::Syncing
    } else {
        MirrorState::Conflict
    };

    Ok(TrayStatusResult {
        mirror_state: mirror_state_str(mirror),
        paused: is_paused(current_dir),
        watching: is_watching(current_dir),
        workspace_path: current_dir.to_string_lossy().into_owned(),
        workspace_id: config.workspace_id.clone(),
        workspace_label: workspace_label(current_dir),
        pending_conflicts,
        agents,
    })
}

pub async fn do_tray_status(current_dir: &Path) -> Result<TrayStatusResult> {
    let config = load_config(current_dir)?;
    let db = ClientDb::new(current_dir.join(".feanorfs")).await?;

    if is_syncing(current_dir) {
        return cheap_tray_status(current_dir, &config, &db).await;
    }

    let lock_wait = try_acquire_sync_lock(current_dir, Duration::from_millis(400)).await;
    if lock_wait.is_err() {
        return cheap_tray_status(current_dir, &config, &db).await;
    }
    let _sync_guard = lock_wait?;

    let api = ApiClient::from_config(current_dir, &config).await?;
    let password = config.encryption_password.as_deref();

    let status = do_status(&api, &db, current_dir, &config.workspace_id, password).await?;

    let paused = is_paused(current_dir);
    let watching = is_watching(current_dir);

    let records = db.list_conflict_records().await?;
    let pending_conflicts: Vec<TrayConflictEntry> = records
        .iter()
        .map(|r| TrayConflictEntry {
            path: r.path.clone(),
            kind: conflict_kind_str(r.kind).into(),
            label: conflict_label(r),
            choices: conflict_choices(r.kind),
        })
        .collect();

    let agents =
        load_agents_summary(current_dir, &db, &api, &config.workspace_id, password).await?;

    Ok(TrayStatusResult {
        mirror_state: mirror_state_str(status.mirror_state),
        paused,
        watching,
        workspace_path: current_dir.to_string_lossy().into_owned(),
        workspace_id: config.workspace_id,
        workspace_label: workspace_label(current_dir),
        pending_conflicts,
        agents,
    })
}

pub async fn build_conflict_show(
    db: &ClientDb,
    path: &str,
) -> Result<feanorfs_common::ConflictShowResult> {
    use feanorfs_common::ConflictShowResult;

    let record = db
        .get_conflict_record(path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no pending conflict for {path}"))?;
    let dir = Path::new(&record.conflict_dir);
    let local = resolve_artifact(dir, path, ArtifactRole::Local);
    let cloud = resolve_artifact(dir, path, ArtifactRole::Cloud);
    let original = resolve_artifact(dir, path, ArtifactRole::Original);
    let local_bytes = std::fs::read(&local).unwrap_or_default();
    let cloud_bytes = std::fs::read(&cloud).unwrap_or_default();
    let binary = is_binary_content(&local_bytes) || is_binary_content(&cloud_bytes);
    let diff = if binary {
        None
    } else {
        let local_s = String::from_utf8_lossy(&local_bytes);
        let cloud_s = String::from_utf8_lossy(&cloud_bytes);
        Some(diffy::create_patch(local_s.as_ref(), cloud_s.as_ref()).to_string())
    };
    Ok(ConflictShowResult {
        path: path.into(),
        kind: conflict_kind_str(record.kind).into(),
        local_path: local.to_string_lossy().into_owned(),
        cloud_path: cloud.to_string_lossy().into_owned(),
        original_path: original.to_string_lossy().into_owned(),
        is_binary: binary,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_choices_by_kind() {
        assert_eq!(
            conflict_choices(ConflictKind::EditEdit),
            vec!["local", "cloud", "both"]
        );
        assert_eq!(
            conflict_choices(ConflictKind::EditDelete),
            vec!["local", "cloud"]
        );
        assert_eq!(
            conflict_choices(ConflictKind::DeleteEdit),
            vec!["local", "cloud"]
        );
    }

    #[test]
    fn conflict_labels_are_plain_language() {
        use feanorfs_common::ConflictRecord;

        let edit_edit = ConflictRecord {
            path: "a.txt".into(),
            kind: ConflictKind::EditEdit,
            conflict_dir: "/tmp/c".into(),
            opened_at: 0,
            status: "pending".into(),
        };
        assert!(conflict_label(&edit_edit).contains("both changed"));

        let edit_delete = ConflictRecord {
            path: "b.txt".into(),
            kind: ConflictKind::EditDelete,
            conflict_dir: "/tmp/c".into(),
            opened_at: 0,
            status: "pending".into(),
        };
        assert!(conflict_label(&edit_delete).contains("cloud deleted"));
    }
}
