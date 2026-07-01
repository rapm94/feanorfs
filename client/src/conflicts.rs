use crate::agent::conflicts_dir;
use crate::api::ApiClient;
use crate::conflict_artifacts::{is_sentinel_content, write_conflict_triple};
use crate::fs_util::{atomic_write, file_mtime_ms};
use crate::local::ClientDb;
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    conflict_candidate_paths, detect_concurrent_edits, is_safe_rel_path, ConcurrentEdit,
    ConflictKind, FileState, SyncRequest, SyncResponse,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;

const LAST_SYNCED_KEY: &str = "last_synced_state";

pub async fn load_last_synced(db: &ClientDb) -> Result<HashMap<String, FileState>> {
    let from_table = db.load_last_synced_files().await?;
    if !from_table.is_empty() {
        return Ok(from_table);
    }
    match db.get_session_key(LAST_SYNCED_KEY).await? {
        Some(s) => match serde_json::from_str::<HashMap<String, FileState>>(&s) {
            Ok(map) => {
                if !map.is_empty() {
                    db.replace_last_synced_files(&map).await?;
                }
                Ok(map)
            }
            Err(e) => {
                tracing::warn!("Failed to parse {LAST_SYNCED_KEY}: {e}");
                Ok(HashMap::new())
            }
        },
        None => Ok(HashMap::new()),
    }
}

pub async fn commit_last_synced(
    db: &ClientDb,
    updates: &HashMap<String, FileState>,
    exclude_paths: &HashSet<String>,
) -> Result<()> {
    db.merge_last_synced_files(updates, exclude_paths).await
}

pub async fn pending_conflict_paths(db: &ClientDb) -> Result<HashSet<String>> {
    Ok(db
        .list_pending_conflict_paths()
        .await?
        .into_iter()
        .collect())
}

pub fn conflicts_pending(pending_paths: Option<&HashSet<String>>) -> bool {
    pending_paths.is_some_and(|p| !p.is_empty())
}

pub async fn detect_workspace_conflicts(
    api: &ApiClient,
    workspace_id: &str,
    last_synced: &HashMap<String, FileState>,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
    already_pending: &HashSet<String>,
) -> Result<Vec<(ConcurrentEdit, ConflictKind)>> {
    if last_synced.is_empty() {
        return Ok(Vec::new());
    }

    let base_request = SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: last_synced.values().cloned().collect(),
    };
    let base_response = api.peek_sync(&base_request).await?;
    let their_changed: HashMap<String, FileState> = base_response
        .download_required
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();
    let their_deleted: HashSet<String> = base_response.delete_local.into_iter().collect();

    let candidates = conflict_candidate_paths(response, already_pending);
    let mut edits = detect_concurrent_edits(
        last_synced,
        local_files,
        &their_changed,
        &their_deleted,
        candidates,
        already_pending,
    );
    edits.retain(|(c, _)| is_safe_rel_path(&c.path));
    Ok(edits)
}

pub async fn register_and_write_conflicts(
    base: &Path,
    api: &ApiClient,
    db: &ClientDb,
    items: &[(ConcurrentEdit, ConflictKind)],
    password: Option<&str>,
) -> Result<HashSet<String>> {
    let ts = chrono::Utc::now().timestamp_millis();
    let dir = conflicts_dir(base).join(ts.to_string());
    fs::create_dir_all(&dir).await?;

    let password_str = password.unwrap_or(feanorfs_common::LEGACY_DEFAULT_PASSWORD);
    for (edit, _) in items {
        let ours_src = edit.ours.as_ref().map(|o| base.join(&o.path));
        write_conflict_triple(
            &dir,
            edit,
            api,
            password_str,
            ours_src.as_deref(),
            "no-local-changes",
        )
        .await?;
    }

    let paths: Vec<String> = items.iter().map(|(c, _)| c.path.clone()).collect();
    fs::write(dir.join("manifest.json"), serde_json::to_string(&paths)?).await?;

    let mut out = HashSet::new();
    for (c, kind) in items {
        db.upsert_conflict(&c.path, kind, &dir.to_string_lossy(), ts, "pending")
            .await?;
        out.insert(c.path.clone());
    }

    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKeep {
    Ours,
    Theirs,
    Both,
}

fn conflict_artifact_paths(conflict_dir: &Path, path: &str) -> [PathBuf; 3] {
    [
        conflict_dir.join(format!("{path}.base")),
        conflict_dir.join(format!("{path}.ours")),
        conflict_dir.join(format!("{path}.theirs")),
    ]
}

async fn remove_path_artifacts(conflict_dir: &Path, path: &str) -> Result<()> {
    for artifact in conflict_artifact_paths(conflict_dir, path) {
        if artifact.is_file() {
            fs::remove_file(&artifact).await?;
        }
    }
    Ok(())
}

pub async fn resolve_conflict(
    base: &Path,
    api: &ApiClient,
    db: &ClientDb,
    workspace_id: &str,
    path: &str,
    keep: ResolveKeep,
    password: Option<&str>,
) -> Result<()> {
    if !is_safe_rel_path(path) {
        bail!("unsafe path: {path}");
    }
    let record = db
        .get_conflict_record(path)
        .await?
        .with_context(|| format!("no pending conflict for {path}"))?;
    let password_str = password.unwrap_or(feanorfs_common::LEGACY_DEFAULT_PASSWORD);
    let conflict_dir = PathBuf::from(&record.conflict_dir);

    match keep {
        ResolveKeep::Ours => {
            let ours_path = base.join(path);
            if ours_path.exists() {
                let plain = fs::read(&ours_path).await?;
                let packed = feanorfs_common::pack_bytes(&plain, password_str, path)?;
                let hash = feanorfs_common::hash_bytes(&packed);
                let mtime = file_mtime_ms(&ours_path).await?;
                api.upload_file(workspace_id, path, &hash, plain.len() as u64, mtime, packed)
                    .await?;
            } else {
                let cached = db.get_cache_entries().await?;
                let hash = cached
                    .get(path)
                    .map(|c| c.encrypted_hash.clone())
                    .unwrap_or_else(|| feanorfs_common::hash_bytes(b""));
                let mtime = chrono::Utc::now().timestamp_millis();
                api.upload_tombstone(workspace_id, path, &hash, mtime)
                    .await?;
            }
        }
        ResolveKeep::Theirs => {
            let theirs_file = conflict_dir.join(format!("{path}.theirs"));
            if theirs_file.exists() {
                let content = fs::read(&theirs_file).await?;
                if is_sentinel_content(&content) {
                    bail!("theirs version unavailable on disk; re-run sync while online");
                }
                atomic_write(base, path, &content).await?;
                let packed = feanorfs_common::pack_bytes(&content, password_str, path)?;
                let hash = feanorfs_common::hash_bytes(&packed);
                let mtime = chrono::Utc::now().timestamp_millis();
                api.upload_file(
                    workspace_id,
                    path,
                    &hash,
                    content.len() as u64,
                    mtime,
                    packed,
                )
                .await?;
            }
        }
        ResolveKeep::Both => {
            let theirs_file = conflict_dir.join(format!("{path}.theirs"));
            let hostname = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "other".into());
            let alt_path = format!("{path} (conflicted copy {hostname})");
            if theirs_file.exists() {
                let content = fs::read(&theirs_file).await?;
                if !is_sentinel_content(&content) {
                    atomic_write(base, &alt_path, &content).await?;
                }
            }
        }
    }

    db.resolve_conflict_path(path).await?;
    remove_path_artifacts(&conflict_dir, path).await?;

    if db.count_pending_in_dir(&record.conflict_dir).await? == 0 && conflict_dir.is_dir() {
        fs::remove_dir_all(&conflict_dir).await?;
    }
    Ok(())
}

pub async fn seed_last_synced_from_server(
    api: &ApiClient,
    db: &ClientDb,
    workspace_id: &str,
    local_files: &HashMap<String, FileState>,
) -> Result<u32> {
    let peek = api
        .peek_sync(&SyncRequest {
            workspace_id: workspace_id.to_string(),
            files: local_files.values().cloned().collect(),
        })
        .await?;
    let mut synced = load_last_synced(db).await?;
    let before = synced.len();
    for f in peek.download_required {
        if is_safe_rel_path(&f.path) {
            synced.insert(f.path.clone(), f);
        }
    }
    for (path, file) in local_files {
        if is_safe_rel_path(path) && !file.deleted {
            synced.insert(path.clone(), file.clone());
        }
    }
    db.replace_last_synced_files(&synced).await?;
    Ok(u32::try_from(synced.len().saturating_sub(before)).unwrap_or(u32::MAX))
}

pub fn filter_blocked_paths(response: &mut SyncResponse, blocked: &HashSet<String>) {
    response.upload_required.retain(|p| !blocked.contains(p));
    response
        .download_required
        .retain(|f| !blocked.contains(&f.path));
    response.delete_local.retain(|p| !blocked.contains(p));
}

/// Peek server delta, detect workspace conflicts, optionally register them, and
/// return the filtered response plus blocked paths.
pub async fn negotiate_sync_with_conflict_gate(
    api: &ApiClient,
    workspace_id: &str,
    db: &ClientDb,
    base_path: &Path,
    local_files: &HashMap<String, FileState>,
    password: Option<&str>,
    register: bool,
) -> Result<(SyncResponse, HashSet<String>)> {
    let pending = pending_conflict_paths(db).await?;
    let request = SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: local_files.values().cloned().collect(),
    };
    let mut response = api.peek_sync(&request).await?;
    let last = load_last_synced(db).await?;
    let detected =
        detect_workspace_conflicts(api, workspace_id, &last, local_files, &response, &pending)
            .await?;

    let mut blocked = pending;
    if register && !detected.is_empty() {
        tracing::warn!(
            "{} concurrent workspace edit conflict(s); wrote base/ours/theirs under .feanorfs/conflicts/",
            detected.len()
        );
        for (c, _) in &detected {
            tracing::warn!("  conflict: {}", c.path);
        }
        let new_paths =
            register_and_write_conflicts(base_path, api, db, &detected, password).await?;
        blocked.extend(new_paths);
    } else {
        for (c, _) in &detected {
            blocked.insert(c.path.clone());
        }
    }

    filter_blocked_paths(&mut response, &blocked);
    Ok((response, blocked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{classify_conflict_kind, detect_concurrent_edits};

    fn st(path: &str, hash: &str, deleted: bool) -> FileState {
        FileState {
            path: path.into(),
            hash: hash.into(),
            size: 1,
            mtime: 1,
            deleted,
        }
    }

    #[test]
    fn classify_edit_edit() {
        let base = st("f", "b", false);
        let ours = st("f", "o", false);
        let theirs = st("f", "t", false);
        assert_eq!(
            classify_conflict_kind(&base, Some(&ours), Some(&theirs), false),
            ConflictKind::EditEdit
        );
    }

    #[test]
    fn classify_edit_delete() {
        let base = st("f", "b", false);
        let ours = st("f", "o", false);
        assert_eq!(
            classify_conflict_kind(&base, Some(&ours), None, true),
            ConflictKind::EditDelete
        );
    }

    #[test]
    fn concurrent_delete_not_a_conflict() {
        let base = st("f", "b", false);
        let mut local = HashMap::new();
        local.insert("f".into(), st("f", "b", true));
        let mut their_deleted = HashSet::new();
        their_deleted.insert("f".into());
        let base_map = HashMap::from([("f".into(), base.clone())]);
        let edits = detect_concurrent_edits(
            &base_map,
            &local,
            &HashMap::new(),
            &their_deleted,
            vec!["f".into()],
            &HashSet::new(),
        );
        assert!(edits.is_empty());
    }

    #[test]
    fn filter_blocked_paths_strips_all_buckets() {
        let mut resp = SyncResponse {
            upload_required: vec!["a".into()],
            download_required: vec![st("b", "h", false)],
            delete_local: vec!["c".into()],
        };
        let blocked = HashSet::from(["a".into(), "b".into(), "c".into()]);
        filter_blocked_paths(&mut resp, &blocked);
        assert!(resp.upload_required.is_empty());
        assert!(resp.download_required.is_empty());
        assert!(resp.delete_local.is_empty());
    }

    #[test]
    fn conflicts_pending_uses_db_only() {
        assert!(!conflicts_pending(None));
        assert!(!conflicts_pending(Some(&HashSet::new())));
        assert!(conflicts_pending(Some(&HashSet::from(["x".into()]))));
    }
}
