use crate::api::ApiClient;
use crate::local::ClientDb;
use anyhow::{bail, Result};
use feanorfs_common::{
    normalize_path, AgentCommitResult, AgentSnapshotEntry, ConcurrentEdit, FileState, SyncRequest,
};
use std::collections::HashMap;
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

/// Snapshot `base` into `.feanorfs/agents/<name>/` using hardlinks (CoW via
/// atomic rename on edit); falls back to copy when hardlinks are unavailable.
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

    let mut linked = 0usize;
    let mut snapshot_entries = Vec::new();

    for result in ignore::WalkBuilder::new(base)
        .hidden(false)
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
        if normalized.starts_with(".feanorfs")
            || normalized.starts_with(".git")
            || normalized.contains("/.git/")
            || normalized.contains("/.feanorfs/")
        {
            continue;
        }

        let dest = target.join(&normalized);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }

        if let Err(e) = std::fs::hard_link(abs, &dest) {
            tracing::debug!(
                error = %e,
                src = %abs.display(),
                dest = %dest.display(),
                "hard_link failed, falling back to copy"
            );
            fs::copy(abs, &dest).await?;
        }
        linked += 1;

        let cache_row = cached.get(&normalized);
        let (base_hash, base_size, base_mtime) = match cache_row {
            Some(c) => (c.encrypted_hash.clone(), c.size, c.server_mtime),
            None => {
                let bytes = std::fs::read(abs)?;
                let enc = feanorfs_common::crypt_bytes(&bytes, password_str, &normalized);
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

    Ok(linked)
}

/// Diff agent workspace against base snapshot, split into clean-our /
/// clean-their / concurrent-edit buckets. Conflicts are written under
/// `.feanorfs/conflicts/<ts>_<name>/` as `path.base`, `path.ours`,
/// `path.theirs`.
///
/// To learn "what changed on the server since spawn", we send the base
/// snapshot as the client view to `/api/sync/diff`. The server then sees
/// every current-vs-base difference as a download (their change) — reusing
/// the existing read-only diff endpoint without adding a new one.
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

    let base_map: HashMap<String, &AgentSnapshotEntry> =
        snapshot.iter().map(|e| (e.path.clone(), e)).collect();

    let base_request_files: Vec<FileState> = snapshot
        .iter()
        .map(|e| FileState {
            path: e.path.clone(),
            hash: e.base_hash.clone(),
            size: e.base_size,
            mtime: e.base_mtime,
            deleted: false,
        })
        .collect();
    let request = SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: base_request_files,
    };
    let response = api.negotiate_sync(&request).await?;

    let their_changed: HashMap<String, FileState> = response
        .download_required
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();
    let their_deleted: std::collections::HashSet<String> =
        response.delete_local.into_iter().collect();

    let mut our_changed: Vec<FileState> = Vec::new();
    let agent_cache = ClientDb::new(agent_path.join(".feanorfs")).await?;
    let agent_scan =
        crate::local::scan_local_directory(&agent_path, &agent_cache, password).await?;

    for (path, agent_file) in &agent_scan {
        if let Some(base_entry) = base_map.get(path) {
            if agent_file.hash != base_entry.base_hash {
                our_changed.push(agent_file.clone());
            }
        }
    }

    let mut conflicts = Vec::new();
    let mut clean_our = Vec::new();
    for ours in &our_changed {
        if let Some(theirs) = their_changed.get(&ours.path) {
            let Some(base_entry) = base_map.get(&ours.path) else {
                anyhow::bail!(
                    "internal: our_changed path '{}' is missing from base_map; \
                     agent snapshot may be corrupt or partially committed",
                    ours.path
                );
            };
            let base_state = FileState {
                path: ours.path.clone(),
                hash: base_entry.base_hash.clone(),
                size: base_entry.base_size,
                mtime: base_entry.base_mtime,
                deleted: false,
            };
            conflicts.push(ConcurrentEdit {
                path: ours.path.clone(),
                base: Some(base_state),
                ours: Some(ours.clone()),
                theirs: Some(theirs.clone()),
            });
        } else {
            clean_our.push(ours.clone());
        }
    }

    let mut clean_their = Vec::new();
    for (path, theirs) in &their_changed {
        if their_deleted.contains(path) {
            continue;
        }
        if !our_changed.iter().any(|f| f.path == *path) {
            clean_their.push(theirs.clone());
        }
    }

    if !conflicts.is_empty() {
        write_conflict_files(base, api, name, &conflicts, password).await?;
    }

    Ok(AgentCommitResult {
        agent_name: name.to_string(),
        our_changes: clean_our,
        their_changes: clean_their,
        conflicts,
    })
}

async fn write_conflict_files(
    base: &Path,
    api: &ApiClient,
    agent_name: &str,
    conflicts: &[ConcurrentEdit],
    password: Option<&str>,
) -> Result<()> {
    let ts = chrono::Utc::now().timestamp_millis();
    let dir = conflicts_dir(base).join(format!("{}_{}", ts, agent_name));
    fs::create_dir_all(&dir).await?;

    let password_str = password.unwrap_or(feanorfs_common::LEGACY_DEFAULT_PASSWORD);
    for c in conflicts {
        let base_dest = dir.join(format!("{}.base", c.path));
        if let Some(parent) = base_dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        let ours_dest = dir.join(format!("{}.ours", c.path));
        let theirs_dest = dir.join(format!("{}.theirs", c.path));

        write_version_file(&base_dest, c.base.as_ref(), api, password_str, &c.path).await?;

        if let Some(ref o) = c.ours {
            let agent_file = agent_dir(base, agent_name).join(&o.path);
            if agent_file.exists() {
                fs::copy(&agent_file, &ours_dest).await?;
            } else {
                fs::write(&ours_dest, b"<agent file missing>\n").await?;
            }
        } else {
            fs::write(&ours_dest, b"<no agent changes>\n").await?;
        }

        write_version_file(&theirs_dest, c.theirs.as_ref(), api, password_str, &c.path).await?;
    }
    Ok(())
}

async fn write_version_file(
    dest: &Path,
    state: Option<&FileState>,
    api: &ApiClient,
    password: &str,
    path: &str,
) -> Result<()> {
    match state {
        Some(f) => {
            let encrypted = api.download_file(&f.hash).await;
            match encrypted {
                Ok(bytes) => {
                    let computed_hash = feanorfs_common::hash_bytes(&bytes);
                    if computed_hash == f.hash {
                        let plain = feanorfs_common::crypt_bytes(&bytes, password, path);
                        fs::write(dest, &plain).await?;
                    } else {
                        let msg = format!(
                            "<integrity check failed for blob {}: expected {}, computed {}>\n",
                            f.hash, f.hash, computed_hash
                        );
                        fs::write(dest, msg).await?;
                    }
                }
                Err(e) => {
                    let msg = format!("<could not fetch blob {}: {:?}>\n", f.hash, e);
                    fs::write(dest, msg).await?;
                }
            }
        }
        None => {
            fs::write(dest, b"<missing>\n").await?;
        }
    }
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
