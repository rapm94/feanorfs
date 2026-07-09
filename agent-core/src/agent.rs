use crate::api::ApiClient;
use crate::conflict_artifacts::{enrich_conflict_edit, enrich_conflict_edit_preview};
use crate::conflicts::{
    commit_last_synced, load_last_synced, negotiate_sync_with_conflict_gate,
    pending_conflict_paths, register_and_write_conflicts,
};
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::fs_util::{atomic_write, file_mtime_ms};
use crate::local::{build_workspace_walker, ClientDb};
use crate::lock::{LandLock, SyncLock};
use crate::paths::{agent_dir, legacy_policy_for_config, validate_name};
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    detect_concurrent_edits, normalize_path, AgentCheckResult, AgentLandResult, AgentRefreshResult,
    AgentSnapshotEntry, ConcurrentEdit, ConflictKind, FileState, LandedPath, SyncRequest,
    SyncResponse,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;

const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

fn reflink_or_copy(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match reflink::reflink(src, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dest)?;
            Ok(())
        }
    }
}

struct AgentDiff {
    our_changes: Vec<FileState>,
    their_changes: Vec<FileState>,
    conflicts: Vec<(ConcurrentEdit, ConflictKind)>,
    conflict_risk: Vec<String>,
}

struct SpawnCleanupGuard {
    target: PathBuf,
    armed: bool,
}

impl Drop for SpawnCleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.target);
        }
    }
}

async fn compute_agent_diff(ctx: &SyncCtx<'_>, name: &str) -> Result<AgentDiff> {
    validate_name(name)?;
    let agent_path = agent_dir(ctx.base, name);
    if !agent_path.exists() {
        bail!(
            "Agent workspace '{}' does not exist. Run `feanorfs agent spawn {}` first.",
            name,
            name
        );
    }

    let snapshot = ctx.db.get_agent_snapshot(name).await?;
    // Empty snapshot is valid after greenfield spawn (files_copied=0): agent-only additions land fine.

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
        workspace_id: ctx.workspace_id().to_string(),
        files: base_map.values().cloned().collect(),
    };
    let response = ctx.api.peek_sync(&request).await?;

    let their_changed: HashMap<String, FileState> = response
        .download_required
        .into_iter()
        .filter(|f| {
            base_map
                .get(&f.path)
                .map_or(true, |b| b.hash != f.hash || b.deleted != f.deleted)
        })
        .map(|f| (f.path.clone(), f))
        .collect();
    let their_deleted: HashSet<String> = response
        .delete_local
        .into_iter()
        .filter(|path| base_map.get(path).map_or(true, |b| !b.deleted))
        .collect();

    let agent_cache = ClientDb::new(agent_path.join(".feanorfs")).await?;
    let agent_scan =
        crate::local::scan_local_directory(&agent_path, &agent_cache, ctx.password()).await?;

    let mut local_view = agent_scan.clone();
    for (path, base_entry) in &base_map {
        if !agent_scan.contains_key(path) {
            local_view.insert(
                path.clone(),
                FileState {
                    path: path.clone(),
                    hash: base_entry.hash.clone(),
                    size: base_entry.size,
                    mtime: base_entry.mtime,
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
    let candidates: Vec<String> = our_changed_paths.iter().cloned().collect();
    let conflict_edits: Vec<(ConcurrentEdit, ConflictKind)> = detect_concurrent_edits(
        &base_map,
        &local_view,
        &their_changed,
        &their_deleted,
        candidates,
        &empty_pending,
    );

    let conflict_paths: HashSet<String> =
        conflict_edits.iter().map(|(c, _)| c.path.clone()).collect();

    let mut clean_our = Vec::new();
    for path in &our_changed_paths {
        if conflict_paths.contains(path) {
            continue;
        }
        if let Some(f) = agent_scan.get(path) {
            clean_our.push(f.clone());
        } else if let Some(base_entry) = base_map.get(path) {
            clean_our.push(FileState {
                path: path.clone(),
                hash: base_entry.hash.clone(),
                size: base_entry.size,
                mtime: base_entry.mtime,
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

    let mut conflict_risk = Vec::new();
    for path in their_changed.keys() {
        if our_changed_paths.contains(path) {
            continue;
        }
        if let Some(base_entry) = base_map.get(path) {
            if let Some(agent_file) = agent_scan.get(path) {
                if agent_file.hash == base_entry.hash {
                    conflict_risk.push(path.clone());
                }
            }
        }
    }

    Ok(AgentDiff {
        our_changes: clean_our,
        their_changes: clean_their,
        conflicts: conflict_edits,
        conflict_risk,
    })
}

/// Snapshot `base` into `.feanorfs/agents/<name>/` after syncing.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
    no_sync: bool,
    replace: bool,
) -> Result<usize> {
    let config_path = base.join(".feanorfs/config.json");
    let config = if config_path.exists() {
        crate::local::load_config(base)?
    } else {
        crate::local::Config {
            server_url: String::new(),
            workspace_id: workspace_id.to_string(),
            encryption_password: password.map(ToString::to_string),
            server_password: None,
            format_version: 1,
            hub_local: false,
        }
    };
    let ctx = SyncCtx::new(
        api,
        db,
        base,
        workspace_id,
        password,
        legacy_policy_for_config(&config),
    );
    spawn_agent_with_ctx(&ctx, name, no_sync, replace).await
}

async fn spawn_agent_with_ctx(
    ctx: &SyncCtx<'_>,
    name: &str,
    no_sync: bool,
    replace: bool,
) -> Result<usize> {
    validate_name(name)?;
    let target = agent_dir(ctx.base, name);

    if target.exists() {
        if replace {
            clean_agent(ctx.base, ctx.db, name).await?;
        } else {
            bail!(
                "Agent workspace '{}' already exists. Run `feanorfs agent clean {}` or use `--replace`.",
                name,
                name
            );
        }
    }

    let _sync_guard = SyncLock::acquire(ctx.base)?;

    if !no_sync {
        let pending = pending_conflict_paths(ctx.db).await?;
        if !pending.is_empty() {
            let paths: Vec<_> = pending.into_iter().collect();
            bail!(
                "Your folder needs attention before an agent can copy it. Conflicts: {}",
                paths.join(", ")
            );
        }
        crate::sync_pass::do_sync(
            ctx.api,
            ctx.db,
            ctx.base,
            ctx.workspace_id(),
            ctx.password(),
            false,
        )
        .await?;
    } else {
        let local = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
        let last = load_last_synced(ctx.db).await?;
        let mut dirty = Vec::new();
        for (path, state) in &local {
            match last.get(path) {
                Some(ls) if ls.hash == state.hash && ls.deleted == state.deleted => {}
                _ => dirty.push(path.clone()),
            }
        }
        if !dirty.is_empty() {
            bail!(
                "Folder is not in sync with last agreed state. Dirty paths: {}",
                dirty.join(", ")
            );
        }
    }

    let cached = ctx.db.get_cache_entries().await?;
    let dehydrated: Vec<String> = cached
        .iter()
        .filter(|(_, e)| !e.hydrated && e.deleted_at.is_none())
        .map(|(p, _)| p.clone())
        .collect();
    if !dehydrated.is_empty() {
        bail!(
            "Cannot spawn with unhydrated placeholders. Run `feanorfs hydrate` first: {}",
            dehydrated.join(", ")
        );
    }

    fs::create_dir_all(&target).await?;
    let password_str = ctx.password_str();

    let mut guard = SpawnCleanupGuard {
        target: target.clone(),
        armed: true,
    };

    let mut copied = 0usize;
    let mut snapshot_entries = Vec::new();

    for result in build_workspace_walker(ctx.base, false).build() {
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let abs = entry.path();
        let Ok(rel) = abs.strip_prefix(ctx.base) else {
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
        reflink_or_copy(abs, &dest)?;

        copied += 1;

        let cache_row = cached.get(&normalized);
        let (base_hash, base_size, base_mtime) = match cache_row {
            Some(c) => (c.encrypted_hash.clone(), c.size, c.server_mtime),
            None => {
                let bytes = std::fs::read(abs)?;
                let (eh, _enc) = seal(&bytes, password_str, &normalized)?;
                (
                    eh,
                    bytes.len() as u64,
                    file_mtime_ms(abs).await.unwrap_or(0),
                )
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

    ctx.db.record_agent_snapshot(&snapshot_entries).await?;
    guard.armed = false;

    Ok(copied)
}

/// Read-only preview of agent changes vs folder.
pub async fn check_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
) -> Result<AgentCheckResult> {
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::new(
        api,
        db,
        base,
        workspace_id,
        password,
        legacy_policy_for_config(&config),
    );
    let diff = compute_agent_diff(&ctx, name).await?;
    let conflicts: Vec<ConcurrentEdit> = diff
        .conflicts
        .iter()
        .map(|(c, k)| enrich_conflict_edit_preview(c.clone(), *k))
        .collect();
    Ok(AgentCheckResult {
        agent_name: name.to_string(),
        our_changes: diff.our_changes,
        their_changes: diff.their_changes,
        conflicts,
        conflict_risk: diff.conflict_risk,
    })
}

/// Apply clean agent work into the main folder and upload.
#[allow(clippy::too_many_arguments)]
pub async fn land_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
    clean_after: bool,
    propose: bool,
) -> Result<AgentLandResult> {
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::new(
        api,
        db,
        base,
        workspace_id,
        password,
        legacy_policy_for_config(&config),
    );
    land_agent_with_ctx(&ctx, name, clean_after, propose).await
}

async fn land_agent_with_ctx(
    ctx: &SyncCtx<'_>,
    name: &str,
    clean_after: bool,
    propose: bool,
) -> Result<AgentLandResult> {
    let _land_guard = LandLock::acquire(ctx.base)?;
    let _sync_guard = SyncLock::acquire(ctx.base)?;

    let pending = pending_conflict_paths(ctx.db).await?;
    if !pending.is_empty() {
        bail!(
            "Your folder needs attention before landing agent work. Conflicts: {}",
            pending.into_iter().collect::<Vec<_>>().join(", ")
        );
    }

    let diff = compute_agent_diff(ctx, name).await?;

    if diff.our_changes.is_empty() && diff.conflicts.is_empty() {
        return Ok(AgentLandResult {
            agent_name: name.to_string(),
            our_changes: diff.our_changes,
            their_changes: diff.their_changes,
            conflicts: diff
                .conflicts
                .iter()
                .map(|(c, k)| enrich_conflict_edit_preview(c.clone(), *k))
                .collect(),
            landed: Vec::new(),
            message: "Nothing to land.".to_string(),
        });
    }

    let agent_path = agent_dir(ctx.base, name);
    let gate_local = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let (_, blocked) = negotiate_sync_with_conflict_gate(ctx, &gate_local, false).await?;

    let mut landed = Vec::new();
    let mut landed_states = HashMap::new();

    for change in &diff.our_changes {
        if change.size > MAX_UPLOAD_BYTES {
            landed.push(LandedPath {
                path: change.path.clone(),
                action: "failed: too large".to_string(),
            });
            continue;
        }

        let main_path = ctx.base.join(&change.path);
        if main_path.exists() && !change.deleted {
            let meta = fs::metadata(&main_path).await?;
            let mtime = match file_mtime_ms(&main_path).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("failed to stat {}: {e}", change.path);
                    landed.push(LandedPath {
                        path: change.path.clone(),
                        action: "diverted: failed to read metadata".to_string(),
                    });
                    continue;
                }
            };
            if let Some(gate) = gate_local.get(&change.path) {
                if mtime != gate.mtime || meta.len() != gate.size {
                    landed.push(LandedPath {
                        path: change.path.clone(),
                        action: "diverted: folder changed during land".to_string(),
                    });
                    continue;
                }
            }
        }

        if change.deleted {
            if main_path.exists() {
                fs::remove_file(&main_path).await?;
            }
            ctx.db.delete_cache_entry(&change.path).await?;
            landed.push(LandedPath {
                path: change.path.clone(),
                action: "deleted".to_string(),
            });
            landed_states.insert(change.path.clone(), change.clone());
        } else {
            let src = agent_path.join(&change.path);
            let bytes = fs::read(&src).await?;
            atomic_write(ctx.base, &change.path, &bytes).await?;
            landed.push(LandedPath {
                path: change.path.clone(),
                action: "updated".to_string(),
            });
            landed_states.insert(change.path.clone(), change.clone());
        }
    }

    if !landed_states.is_empty() {
        let upload_response = SyncResponse {
            upload_required: landed_states.keys().cloned().collect(),
            download_required: Vec::new(),
            delete_local: Vec::new(),
        };
        let local_after =
            crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
        let _ = crate::sync_pass::process_uploads(ctx, &upload_response, &local_after).await?;
        commit_last_synced(ctx.db, &local_after, &blocked).await?;

        for path in landed_states.keys() {
            if let Some(state) = local_after.get(path) {
                ctx.db.update_agent_snapshot_base(name, path, state).await?;
            }
        }
    }

    let mut registered_count = 0usize;
    let mut conflict_dir: Option<PathBuf> = None;
    if !diff.conflicts.is_empty() {
        let (dir, paths) =
            register_and_write_conflicts(ctx, &diff.conflicts, Some(&agent_path)).await?;
        conflict_dir = Some(dir);
        registered_count = paths.len();
    }

    let empty_path = PathBuf::new();
    let conflict_dir_ref = conflict_dir.as_ref().unwrap_or(&empty_path);
    let mut conflicts: Vec<ConcurrentEdit> = diff
        .conflicts
        .iter()
        .map(|(c, k)| {
            if conflict_dir.is_some() {
                enrich_conflict_edit(c.clone(), *k, conflict_dir_ref)
            } else {
                enrich_conflict_edit_preview(c.clone(), *k)
            }
        })
        .collect();

    if propose {
        for edit in &mut conflicts {
            write_proposal_if_clean(conflict_dir_ref, edit)?;
        }
    }

    if clean_after {
        clean_agent(ctx.base, ctx.db, name).await?;
    }

    let message = if landed.is_empty() && conflicts.is_empty() {
        "Nothing to land.".to_string()
    } else {
        let applied = landed
            .iter()
            .filter(|l| matches!(l.action.as_str(), "updated" | "deleted"))
            .count();
        format!(
            "Landed {} path(s){}.",
            applied,
            if registered_count > 0 {
                format!(", {} need attention", registered_count)
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
    })
}

/// Conflict-free catch-up: pull server changes the agent hasn't touched.
pub async fn refresh_agent(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    workspace_id: &str,
    name: &str,
    password: Option<&str>,
) -> Result<AgentRefreshResult> {
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    let diff = compute_agent_diff(&ctx, name).await?;
    let agent_path = agent_dir(base, name);

    let mut refreshed = Vec::new();
    let mut deferred = Vec::new();

    for path in &diff.conflict_risk {
        if let Some(theirs) = diff.their_changes.iter().find(|f| &f.path == path) {
            let response = SyncResponse {
                upload_required: Vec::new(),
                download_required: vec![theirs.clone()],
                delete_local: Vec::new(),
            };
            let agent_db = ClientDb::new(agent_path.join(".feanorfs")).await?;
            let agent_ctx = SyncCtx::new(
                api,
                &agent_db,
                &agent_path,
                workspace_id,
                password,
                ctx.policy,
            );
            let local = HashMap::new();
            let _ =
                crate::sync_pass::process_downloads(&agent_ctx, &response, &local, false).await?;
            db.update_agent_snapshot_base(name, path, theirs).await?;
            refreshed.push(path.clone());
        }
    }

    for (c, _) in &diff.conflicts {
        deferred.push(c.path.clone());
    }

    Ok(AgentRefreshResult {
        agent_name: name.to_string(),
        refreshed,
        deferred,
    })
}

/// Back-compat alias for `land_agent` (default: no clean-after, no propose).
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

fn write_proposal_if_clean(conflict_dir: &Path, edit: &mut ConcurrentEdit) -> Result<()> {
    use crate::conflict_artifacts::{
        is_binary_content, is_sentinel_content, resolve_artifact, ArtifactRole,
    };
    let original = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Original);
    let local = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Local);
    let cloud = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Cloud);
    let lb = std::fs::read(&local).with_context(|| "missing local artifact for proposal")?;
    let cb = std::fs::read(&cloud).with_context(|| "missing cloud artifact for proposal")?;
    if is_binary_content(&lb) || is_binary_content(&cb) {
        return Ok(());
    }
    let ob = if original.is_file() {
        let bytes = std::fs::read(&original).context("read original artifact for proposal")?;
        if is_sentinel_content(&bytes) {
            return Ok(());
        }
        bytes
    } else {
        Vec::new()
    };
    let os = String::from_utf8_lossy(&ob);
    let ls = String::from_utf8_lossy(&lb);
    let cs = String::from_utf8_lossy(&cb);
    let proposed_path = conflict_dir.join(format!("{}.proposed", edit.path));
    if let Some(parent) = proposed_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match diffy::merge(os.as_ref(), ls.as_ref(), cs.as_ref()) {
        Ok(merged) => {
            std::fs::write(&proposed_path, &merged)?;
            edit.proposed_file = Some(proposed_path.to_string_lossy().into_owned());
            edit.proposal_clean = Some(true);
        }
        Err(merged) => {
            std::fs::write(&proposed_path, &merged)?;
            edit.proposed_file = Some(proposed_path.to_string_lossy().into_owned());
            edit.proposal_clean = Some(false);
        }
    }
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
            "expected 'empty' in error, got: {err}"
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
    fn validate_name_rejects_control_chars() {
        assert!(validate_name("a\nb").is_err());
        assert!(validate_name("a\tb").is_err());
        assert!(validate_name("a\0b").is_err());
    }
}
