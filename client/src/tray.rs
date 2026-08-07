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
    TrayAgentEntry, TrayAgentsSummary, TrayConflictEntry, TrayStatusResult, WorkerStatusSnapshot,
};
use feanorfs_common::{ConflictKind, ConflictRecord};
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

const AGENT_CACHE_FILE: &str = "tray-agent-cache.json";
const AGENT_CACHE_TTL: Duration = Duration::from_secs(30);
const WORKER_STATUS_FILE: &str = "worker-status.json";
const MAX_TRAY_CONFLICT_ENTRIES: usize = 20;
const MAX_TRAY_AGENT_ENTRIES: usize = 20;
const MAX_TRAY_STATE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAgents {
    cached_at_ms: i64,
    summary: TrayAgentsSummary,
}

fn agent_cache_path(current_dir: &Path) -> Option<PathBuf> {
    feanorfs_agent_core::ensure_workspace_state(current_dir)
        .ok()
        .map(|state| state.join(AGENT_CACHE_FILE))
}

fn worker_status_path(current_dir: &Path) -> Option<PathBuf> {
    feanorfs_agent_core::ensure_workspace_state(current_dir)
        .ok()
        .map(|state| state.join(WORKER_STATUS_FILE))
}

/// Publishes the bounded secret-free status snapshot after one sync pass.
///
/// Called by the managed worker after every successful sync so routine tray
/// refreshes never need to scan the project or take the sync lock.
pub async fn publish_worker_status(
    current_dir: &Path,
    mirror_state: &crate::commands::MirrorState,
    db: &ClientDb,
) -> anyhow::Result<()> {
    let records = db.list_conflict_records().await?;
    let snapshot = WorkerStatusSnapshot {
        mirror_state: mirror_state_str(*mirror_state),
        pending_conflict_count: conflict_count(records.len()),
        pending_conflicts: records
            .iter()
            .take(MAX_TRAY_CONFLICT_ENTRIES)
            .map(|r| TrayConflictEntry {
                path: r.path.clone(),
                kind: conflict_kind_str(r.kind).into(),
                label: conflict_label(r),
                choices: conflict_choices(r.kind),
            })
            .collect(),
        published_at_ms: chrono::Utc::now().timestamp_millis(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    if worker_status_path(current_dir).is_none() {
        return Ok(());
    }
    let state = feanorfs_agent_core::ensure_workspace_state(current_dir)?;
    feanorfs_agent_core::fs_util::atomic_write(
        &state,
        WORKER_STATUS_FILE,
        serde_json::to_vec(&snapshot)?.as_slice(),
    )
    .await
}

/// Drops stale worker state after a local control action.
///
/// Routine polling remains constant-cost and reports a transitional state
/// until the worker republishes. Only `tray status --fresh` performs a scan.
pub fn invalidate_worker_status(current_dir: &Path) {
    if let Some(path) = worker_status_path(current_dir) {
        let _ = std::fs::remove_file(path);
    }
}

fn load_worker_status(current_dir: &Path) -> Option<WorkerStatusSnapshot> {
    let content = read_bounded(worker_status_path(current_dir)?.as_path())?;
    let snapshot: WorkerStatusSnapshot = serde_json::from_slice(&content).ok()?;
    if snapshot.version != env!("CARGO_PKG_VERSION")
        || snapshot.pending_conflicts.len() > MAX_TRAY_CONFLICT_ENTRIES
        || snapshot.pending_conflict_count < conflict_count(snapshot.pending_conflicts.len())
        || !matches!(
            snapshot.mirror_state.as_str(),
            "idle" | "out_of_sync" | "offline" | "conflict" | "error" | "syncing"
        )
    {
        return None;
    }
    Some(snapshot)
}

fn cache_agents(current_dir: &Path, summary: &TrayAgentsSummary) {
    let entry = CachedAgents {
        cached_at_ms: chrono::Utc::now().timestamp_millis(),
        summary: summary.clone(),
    };
    if let Ok(json) = serde_json::to_vec(&entry) {
        if let Some(path) = agent_cache_path(current_dir) {
            let _ = write_atomic(&path, &json);
        }
    }
}

fn cached_agents(current_dir: &Path) -> Option<TrayAgentsSummary> {
    let content = read_bounded(agent_cache_path(current_dir)?.as_path())?;
    let mut entry: CachedAgents = serde_json::from_slice(&content).ok()?;
    entry.summary.entries.truncate(MAX_TRAY_AGENT_ENTRIES);
    let age_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(entry.cached_at_ms);
    if age_ms < i64::try_from(AGENT_CACHE_TTL.as_millis()).unwrap_or(i64::MAX) {
        Some(entry.summary)
    } else {
        None
    }
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take((MAX_TRAY_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_TRAY_STATE_BYTES).then_some(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(bytes)?;
    file.commit()?;
    Ok(())
}

/// Drop cached agent summary after land/keep so the next tray status is fresh.
pub fn invalidate_agent_cache(current_dir: &Path) {
    if let Some(path) = agent_cache_path(current_dir) {
        let _ = std::fs::remove_file(path);
    }
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

fn conflict_count(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
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
                if entries.len() < MAX_TRAY_AGENT_ENTRIES {
                    entries.push(TrayAgentEntry {
                        name: name.clone(),
                        state: state.into(),
                        change_count,
                        conflict_count,
                    });
                }
            }
            Err(_) => {
                if entries.len() < MAX_TRAY_AGENT_ENTRIES {
                    entries.push(TrayAgentEntry {
                        name: name.clone(),
                        state: "offline".into(),
                        change_count: 0,
                        conflict_count: 0,
                    });
                }
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
        .take(MAX_TRAY_CONFLICT_ENTRIES)
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
        pending_conflict_count: conflict_count(records.len()),
        pending_conflicts,
        agents,
    })
}

pub async fn do_tray_status(current_dir: &Path) -> Result<TrayStatusResult> {
    do_tray_status_with(current_dir, false).await
}

/// Aggregate dashboard for the tray.
///
/// Routine refreshes (`fresh=false`) read the worker-published status snapshot
/// when one exists: no project scan and no sync lock, so polling stays
/// constant-cost even in large workspaces. `fresh=true` forces the explicit
/// fresh-status path (bounded lock wait plus a real scan).
pub async fn do_tray_status_with(current_dir: &Path, fresh: bool) -> Result<TrayStatusResult> {
    let config = load_config(current_dir)?;

    if !fresh {
        if let Some(snapshot) = load_worker_status(current_dir) {
            return Ok(snapshot_tray_status(
                current_dir,
                &config,
                &snapshot,
                is_syncing(current_dir),
            ));
        }
        return Ok(missing_snapshot_tray_status(current_dir, &config));
    }

    let db = crate::open_client_db(current_dir).await?;

    if is_syncing(current_dir) {
        return cheap_tray_status(current_dir, &config, &db).await;
    }

    let lock_wait = try_acquire_sync_lock(current_dir, Duration::from_millis(400)).await;
    if lock_wait.is_err() {
        return cheap_tray_status(current_dir, &config, &db).await;
    }
    let _sync_guard = lock_wait?;

    let api = crate::open_api_client(current_dir, &config).await?;
    let password = config.encryption_password.as_deref();

    let status = do_status(&api, &db, current_dir, &config.workspace_id, password).await?;

    let paused = is_paused(current_dir);
    let watching = is_watching(current_dir);

    let (pending_conflict_count, pending_conflicts) = conflict_entries(&db).await?;

    let agents =
        load_agents_summary(current_dir, &db, &api, &config.workspace_id, password).await?;

    Ok(TrayStatusResult {
        mirror_state: mirror_state_str(status.mirror_state),
        paused,
        watching,
        workspace_path: current_dir.to_string_lossy().into_owned(),
        workspace_id: config.workspace_id,
        workspace_label: workspace_label(current_dir),
        pending_conflict_count,
        pending_conflicts,
        agents,
    })
}

/// Builds a tray result from the worker snapshot plus local tray state only.
fn snapshot_tray_status(
    current_dir: &Path,
    config: &crate::local::Config,
    snapshot: &WorkerStatusSnapshot,
    syncing: bool,
) -> TrayStatusResult {
    let agents = cached_agents(current_dir).unwrap_or(TrayAgentsSummary {
        working: 0,
        need_attention: 0,
        entries: vec![],
    });
    TrayStatusResult {
        mirror_state: if syncing {
            "syncing".into()
        } else {
            snapshot.mirror_state.clone()
        },
        paused: is_paused(current_dir),
        watching: is_watching(current_dir),
        workspace_path: current_dir.to_string_lossy().into_owned(),
        workspace_id: config.workspace_id.clone(),
        workspace_label: workspace_label(current_dir),
        pending_conflict_count: snapshot.pending_conflict_count,
        pending_conflicts: snapshot.pending_conflicts.clone(),
        agents,
    }
}

fn missing_snapshot_tray_status(
    current_dir: &Path,
    config: &crate::local::Config,
) -> TrayStatusResult {
    TrayStatusResult {
        mirror_state: "syncing".into(),
        paused: is_paused(current_dir),
        watching: is_watching(current_dir),
        workspace_path: current_dir.to_string_lossy().into_owned(),
        workspace_id: config.workspace_id.clone(),
        workspace_label: workspace_label(current_dir),
        pending_conflict_count: 0,
        pending_conflicts: Vec::new(),
        agents: cached_agents(current_dir).unwrap_or(TrayAgentsSummary {
            working: 0,
            need_attention: 0,
            entries: Vec::new(),
        }),
    }
}

async fn conflict_entries(db: &ClientDb) -> Result<(u32, Vec<TrayConflictEntry>)> {
    let records = db.list_conflict_records().await?;
    Ok((
        conflict_count(records.len()),
        records
            .iter()
            .take(MAX_TRAY_CONFLICT_ENTRIES)
            .map(|r| TrayConflictEntry {
                path: r.path.clone(),
                kind: conflict_kind_str(r.kind).into(),
                label: conflict_label(r),
                choices: conflict_choices(r.kind),
            })
            .collect(),
    ))
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
    fn worker_status_roundtrips_and_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No state dir yet: publish is a no-op, load is None.
        assert!(load_worker_status(root).is_none());
        invalidate_worker_status(root);

        let snapshot = WorkerStatusSnapshot {
            mirror_state: "conflict".into(),
            pending_conflict_count: 25,
            pending_conflicts: vec![TrayConflictEntry {
                path: "a.txt".into(),
                kind: "edit_edit".into(),
                label: "Both sides changed a.txt".into(),
                choices: vec!["local".into(), "cloud".into()],
            }],
            published_at_ms: 42,
            version: env!("CARGO_PKG_VERSION").into(),
        };
        let path = worker_status_path(root).expect("state dir must exist after publish");
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let loaded = load_worker_status(root).expect("snapshot must load");
        assert_eq!(loaded.mirror_state, "conflict");
        assert_eq!(loaded.pending_conflict_count, 25);
        assert_eq!(loaded.pending_conflicts.len(), 1);
        assert_eq!(loaded.pending_conflicts[0].path, "a.txt");
        assert_eq!(loaded.published_at_ms, 42);
        assert_eq!(loaded.version, env!("CARGO_PKG_VERSION"));

        invalidate_worker_status(root);
        assert!(load_worker_status(root).is_none());
        assert!(!path.exists());
    }

    #[test]
    fn malformed_worker_status_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = worker_status_path(root).expect("state dir must be created");
        std::fs::write(&path, b"not json").unwrap();
        assert!(load_worker_status(root).is_none());
    }

    #[test]
    fn worker_status_rejects_unbounded_or_stale_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = worker_status_path(root).expect("state dir must be created");
        let conflict = TrayConflictEntry {
            path: "a.txt".into(),
            kind: "edit_edit".into(),
            label: "Both sides changed a.txt".into(),
            choices: vec!["local".into(), "cloud".into()],
        };
        let oversized = WorkerStatusSnapshot {
            mirror_state: "conflict".into(),
            pending_conflict_count: conflict_count(MAX_TRAY_CONFLICT_ENTRIES + 1),
            pending_conflicts: vec![conflict; MAX_TRAY_CONFLICT_ENTRIES + 1],
            published_at_ms: 42,
            version: env!("CARGO_PKG_VERSION").into(),
        };
        std::fs::write(&path, serde_json::to_vec(&oversized).unwrap()).unwrap();
        assert!(load_worker_status(root).is_none());

        let stale = WorkerStatusSnapshot {
            mirror_state: "idle".into(),
            pending_conflict_count: 0,
            pending_conflicts: Vec::new(),
            published_at_ms: 42,
            version: "0.0.0".into(),
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(load_worker_status(root).is_none());

        std::fs::write(&path, vec![b'x'; MAX_TRAY_STATE_BYTES + 1]).unwrap();
        assert!(load_worker_status(root).is_none());
    }

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
