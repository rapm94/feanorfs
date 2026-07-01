use crate::api::ApiClient;
use crate::conflict_artifacts::write_conflict_triple;
use crate::local::ClientDb;
use anyhow::{bail, Result};
use feanorfs_common::{
    conflict_candidate_paths, detect_concurrent_edits, normalize_path, pack_bytes,
    AgentCommitResult, AgentSnapshotEntry, ConcurrentEdit, FileState, SyncRequest,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;

#[must_use]
pub fn agents_dir(base: &Path) -> PathBuf {
    base.join(".feanorfs").join("agents")
}

#[must_use]
pub fn agent_dir(base: &Path, name: &str) -> PathBuf {
    agents_dir(base).join(name)
}

#[must_use]
pub fn conflicts_dir(base: &Path) -> PathBuf {
    base.join(".feanorfs").join("conflicts")
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Agent name must not be empty");
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        bail!("Agent name must be a single path segment: '{}'", name);
    }
    Ok(())
}

/// Snapshot `base` into `.feanorfs/agents/<name>/` by copying files.
/// Each tracked path records the server's current hash+mtime+size into
/// `agent_snapshots`, which is the base `commit` diffs against later.
pub async fn spawn_agent(
    base: &Path,
    db: &ClientDb,
    name: &str,
    password: Option<&str>,
) -> Result<usize> {
    validate_name(name)?;
    let target = agent_dir(base, name);
    if target.exists() {
        bail!(
            "Agent workspace '{}' already exists. Run `feanorfs agent clean {}` first.",
            name,
            name
        );
    }

    fs::create_dir_all(&target).await?;

    let cached = db.get_cache_entries().await?;
    let password_str = password.unwrap_or(feanorfs_common::LEGACY_DEFAULT_PASSWORD);

    let mut copied = 0usize;
    let mut snapshot_entries = Vec::new();

    for result in ignore::WalkBuilder::new(base)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .build()
    {
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let abs = entry.path();
        let Ok(rel) = abs.strip_prefix(base) else {
            continue;
        };
        let Some(rel_str) = rel.to_str() else {
            continue;
        };
        let normalized = normalize_path(rel_str);
        if !feanorfs_common::is_safe_rel_path(&normalized) {
            continue;
        }

        let dest = target.join(&normalized);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }

        fs::copy(abs, &dest).await?;
        copied += 1;

        let cache_row = cached.get(&normalized);
        let (base_hash, base_size, base_mtime) = match cache_row {
            Some(c) => (c.encrypted_hash.clone(), c.size, c.server_mtime),
            None => {
                let bytes = std::fs::read(abs)?;
                let enc = pack_bytes(&bytes, password_str, &normalized)?;
                let eh = feanorfs_common::hash_bytes(&enc);
                (eh, bytes.len() as u64, 0)
            }
        };
        snapshot_entries.push(AgentSnapshotEntry {
            agent_name: name.to_string(),
            path: normalized,
            base_hash,
            base_size,
            base_mtime,
        });
    }

    if let Err(e) = db.record_agent_snapshot(&snapshot_entries).await {
        let _ = fs::remove_dir_all(&target).await;
        return Err(e);
    }

    Ok(copied)
}

/// Diff agent workspace against base snapshot, split into clean-our /
/// clean-their / concurrent-edit buckets. Conflicts are written under
/// `.feanorfs/conflicts/<ts>_<name>/` as `path.base`, `path.ours`,
/// `path.theirs`.
pub async fn commit_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
) -> Result<AgentCommitResult> {
    validate_name(name)?;
    let agent_path = agent_dir(base, name);
    if !agent_path.exists() {
        bail!(
            "Agent workspace '{}' does not exist. Run `feanorfs agent spawn {}` first.",
            name,
            name
        );
    }

    let snapshot = db.get_agent_snapshot(name).await?;
    if snapshot.is_empty() {
        bail!(
            "No snapshot rows for agent '{}'. Workspace may have been created externally.",
            name
        );
    }

    let base_map: HashMap<String, FileState> = snapshot
        .iter()
        .map(|e| {
            (
                e.path.clone(),
                FileState {
                    path: e.path.clone(),
                    hash: e.base_hash.clone(),
                    size: e.base_size,
                    mtime: e.base_mtime,
                    deleted: false,
                },
            )
        })
        .collect();

    let request = SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: base_map.values().cloned().collect(),
    };
    let response = api.peek_sync(&request).await?;

    let their_changed: HashMap<String, FileState> = response
        .download_required
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();
    let their_deleted: HashSet<String> = response.delete_local.into_iter().collect();

    let agent_cache = ClientDb::new(agent_path.join(".feanorfs")).await?;
    let agent_scan =
        crate::local::scan_local_directory(&agent_path, &agent_cache, password).await?;

    let mut local_view = agent_scan.clone();
    for (path, base) in &base_map {
        if !agent_scan.contains_key(path) {
            local_view.insert(
                path.clone(),
                FileState {
                    path: path.clone(),
                    hash: base.hash.clone(),
                    size: base.size,
                    mtime: base.mtime,
                    deleted: true,
                },
            );
        }
    }

    let mut our_changed_paths = HashSet::new();
    for (path, agent_file) in &agent_scan {
        if let Some(base_entry) = base_map.get(path) {
            if agent_file.hash != base_entry.hash {
                our_changed_paths.insert(path.clone());
            }
        } else {
            our_changed_paths.insert(path.clone());
        }
    }
    for path in base_map.keys() {
        if !agent_scan.contains_key(path) {
            our_changed_paths.insert(path.clone());
        }
    }

    let empty_pending = HashSet::new();
    let empty_response = feanorfs_common::SyncResponse {
        upload_required: our_changed_paths.iter().cloned().collect(),
        download_required: Vec::new(),
        delete_local: Vec::new(),
    };
    let candidates = conflict_candidate_paths(&empty_response, &empty_pending);
    let conflict_edits: Vec<ConcurrentEdit> = detect_concurrent_edits(
        &base_map,
        &local_view,
        &their_changed,
        &their_deleted,
        candidates,
        &empty_pending,
    )
    .into_iter()
    .map(|(edit, _)| edit)
    .collect();

    let conflict_paths: HashSet<String> = conflict_edits.iter().map(|c| c.path.clone()).collect();

    let mut clean_our = Vec::new();
    for path in &our_changed_paths {
        if conflict_paths.contains(path) {
            continue;
        }
        if let Some(f) = agent_scan.get(path) {
            clean_our.push(f.clone());
        } else if let Some(base) = base_map.get(path) {
            clean_our.push(FileState {
                path: path.clone(),
                hash: base.hash.clone(),
                size: base.size,
                mtime: base.mtime,
                deleted: true,
            });
        }
    }

    let mut clean_their = Vec::new();
    for (path, theirs) in &their_changed {
        if their_deleted.contains(path) || our_changed_paths.contains(path) {
            continue;
        }
        clean_their.push(theirs.clone());
    }

    if !conflict_edits.is_empty() {
        write_agent_conflict_files(base, api, name, &conflict_edits, password).await?;
    }

    Ok(AgentCommitResult {
        agent_name: name.to_string(),
        our_changes: clean_our,
        their_changes: clean_their,
        conflicts: conflict_edits,
    })
}

async fn write_agent_conflict_files(
    base: &Path,
    api: &ApiClient,
    agent_name: &str,
    conflicts: &[ConcurrentEdit],
    password: Option<&str>,
) -> Result<()> {
    let ts = chrono::Utc::now().timestamp_millis();
    let dir = conflicts_dir(base).join(format!("{ts}_{agent_name}"));
    fs::create_dir_all(&dir).await?;

    let password_str = password.unwrap_or(feanorfs_common::LEGACY_DEFAULT_PASSWORD);
    let agent_root = agent_dir(base, agent_name);
    for c in conflicts {
        let ours_src = c.ours.as_ref().map(|o| agent_root.join(&o.path));
        write_conflict_triple(
            &dir,
            c,
            api,
            password_str,
            ours_src.as_deref(),
            "no-agent-changes",
        )
        .await?;
    }

    let paths: Vec<String> = conflicts.iter().map(|c| c.path.clone()).collect();
    fs::write(dir.join("manifest.json"), serde_json::to_string(&paths)?).await?;
    Ok(())
}

pub async fn list_agents(base: &Path, db: &ClientDb) -> Result<Vec<String>> {
    let names = db.list_agent_snapshots().await?;
    let mut visible = Vec::new();
    for name in &names {
        if agent_dir(base, name).exists() {
            visible.push(name.clone());
        }
    }
    Ok(visible)
}

pub async fn clean_agent(base: &Path, db: &ClientDb, name: &str) -> Result<()> {
    validate_name(name)?;
    let target = agent_dir(base, name);
    if target.exists() {
        fs::remove_dir_all(&target).await?;
    }
    db.drop_agent_snapshot(name).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_name;

    #[test]
    fn validate_name_accepts_simple_identifier() {
        assert!(validate_name("ci1").is_ok());
        assert!(validate_name("agent-foo").is_ok());
        assert!(validate_name("agent_foo").is_ok());
        assert!(validate_name("agent.foo").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        let err = validate_name("").unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected 'empty' in error, got: {}",
            err
        );
    }

    #[test]
    fn validate_name_rejects_forward_slash() {
        assert!(validate_name("a/b").is_err());
    }

    #[test]
    fn validate_name_rejects_backslash() {
        assert!(validate_name(r"a\b").is_err());
    }

    #[test]
    fn validate_name_rejects_dot() {
        assert!(validate_name(".").is_err());
    }

    #[test]
    fn validate_name_rejects_dotdot() {
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn validate_name_rejects_path_traversal_segments() {
        assert!(validate_name("../etc/passwd").is_err());
        assert!(validate_name(r"..\windows\system32").is_err());
    }
}
