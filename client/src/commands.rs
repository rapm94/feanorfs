use crate::api::ApiClient;
use crate::conflicts;
use crate::fs_util::{atomic_write, file_mtime_ms};
use crate::local::{CacheEntry, ClientDb};
use anyhow::{Context, Result};
use feanorfs_common::{is_safe_rel_path, pack_bytes, unpack_bytes, FileState, SyncResponse};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::fs;

/// Tray-friendly mirror state. Vocabulary intentionally avoids Git terms
/// (`commit`, `branch`, etc.). `syncing` is set on operation results while
/// bytes are in flight; `status --json` reports point-in-time state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MirrorState {
    #[default]
    Idle,
    OutOfSync,
    Offline,
    Conflict,
    Error,
    Syncing,
}

impl std::fmt::Display for MirrorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Idle => "idle",
            Self::OutOfSync => "out_of_sync",
            Self::Offline => "offline",
            Self::Conflict => "conflict",
            Self::Error => "error",
            Self::Syncing => "syncing",
        };
        write!(f, "{}", label)
    }
}

pub fn derive_mirror_state(
    response: Option<&SyncResponse>,
    pending_paths: Option<&HashSet<String>>,
) -> MirrorState {
    if conflicts::conflicts_pending(pending_paths) {
        return MirrorState::Conflict;
    }
    let Some(resp) = response else {
        return MirrorState::Offline;
    };
    if !resp.upload_required.is_empty()
        || !resp.download_required.is_empty()
        || !resp.delete_local.is_empty()
    {
        MirrorState::OutOfSync
    } else {
        MirrorState::Idle
    }
}

pub(crate) fn password_or_default(password: Option<&str>) -> &str {
    match password {
        Some(p) => p,
        None => {
            tracing::warn!(
                "No E2EE password set in config. Using insecure legacy default. \
                 Run 'feanorfs setup' to set a proper encryption key."
            );
            feanorfs_common::LEGACY_DEFAULT_PASSWORD
        }
    }
}

fn mirror_state_after_apply(blocked: &HashSet<String>, remote_still_pending: bool) -> MirrorState {
    if conflicts::conflicts_pending(Some(blocked)) {
        MirrorState::Conflict
    } else if remote_still_pending {
        MirrorState::OutOfSync
    } else {
        MirrorState::Idle
    }
}

async fn finish_sync_pass(
    db: &ClientDb,
    password: Option<&str>,
    local_files: &HashMap<String, FileState>,
    conflict_paths: &HashSet<String>,
    disk_mutated: bool,
    base_path: &Path,
) -> Result<()> {
    let current_files = if disk_mutated {
        crate::local::scan_local_directory(base_path, db, password).await?
    } else {
        local_files.clone()
    };
    conflicts::commit_last_synced(db, &current_files, conflict_paths).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncMode {
    Push,
    Pull,
    Full,
}

#[derive(Debug, Default)]
struct SyncPassOutcome {
    uploads: u32,
    downloads: u32,
    placeholders: u32,
    deletes_local: u32,
    deletes_remote: u32,
    remote_still_pending: bool,
}

async fn run_sync_pass(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    mode: SyncMode,
    lazy: bool,
) -> Result<(SyncPassOutcome, HashSet<String>)> {
    let label = match mode {
        SyncMode::Push => "Push",
        SyncMode::Pull => "Pull",
        SyncMode::Full => "Sync",
    };
    tracing::info!("{label} started (lazy={lazy})");
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;
    tracing::debug!("Scanned {} entries", local_files.len());

    let password_str = password_or_default(password);
    let (response, blocked) = conflicts::negotiate_sync_with_conflict_gate(
        api,
        workspace_id,
        db,
        base_path,
        &local_files,
        password,
        true,
    )
    .await?;

    tracing::debug!(
        "Diff: upload={}, download={}, delete_local={}",
        response.upload_required.len(),
        response.download_required.len(),
        response.delete_local.len()
    );

    let mut outcome = SyncPassOutcome::default();

    if mode != SyncMode::Push {
        let (downloads, placeholders) = process_downloads(
            api,
            &response,
            base_path,
            db,
            &local_files,
            password_str,
            lazy,
        )
        .await?;
        outcome.downloads = downloads;
        outcome.placeholders = placeholders;
        outcome.deletes_local = process_delete_local(&response, base_path, db).await?;
    }

    if mode != SyncMode::Pull {
        outcome.uploads = process_uploads(
            api,
            db,
            &response,
            &local_files,
            base_path,
            workspace_id,
            password_str,
        )
        .await?;
        outcome.deletes_remote = cleanup_deleted_cache(&local_files, db).await?;
    }

    if mode == SyncMode::Push {
        outcome.remote_still_pending =
            !response.download_required.is_empty() || !response.delete_local.is_empty();
    }

    tracing::info!(
        "{label} done: up={}, down={} (lazy={}), local_del={}, remote_del={}",
        outcome.uploads,
        outcome.downloads,
        outcome.placeholders,
        outcome.deletes_local,
        outcome.deletes_remote
    );

    let disk_mutated = outcome.uploads > 0
        || outcome.downloads > 0
        || outcome.placeholders > 0
        || outcome.deletes_local > 0
        || outcome.deletes_remote > 0;
    finish_sync_pass(
        db,
        password,
        &local_files,
        &blocked,
        disk_mutated,
        base_path,
    )
    .await?;

    Ok((outcome, blocked))
}

#[derive(Debug, Default, Serialize)]
pub struct PushResult {
    pub mirror_state: MirrorState,
    pub uploads: u32,
    pub deletes: u32,
    pub remote_updates_available: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct PullResult {
    pub mirror_state: MirrorState,
    pub downloads: u32,
    pub placeholders: u32,
    pub deletes: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncResult {
    pub mirror_state: MirrorState,
    pub uploads: u32,
    pub downloads: u32,
    pub placeholders: u32,
    pub deletes_local: u32,
    pub deletes_remote: u32,
}

#[derive(Debug, Serialize)]
pub struct HydrateResult {
    pub hydrated: Vec<String>,
    pub skipped: bool,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct CatResult {
    pub content: Vec<u8>,
    pub hydrated_first: bool,
    pub untracked: bool,
    pub not_found: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusResult {
    pub mirror_state: MirrorState,
    pub upload_required: Vec<String>,
    pub download_required: Vec<FileState>,
    pub delete_local: Vec<String>,
    pub local_files: HashMap<String, FileState>,
    pub pending_conflicts: Vec<String>,
}

async fn process_downloads(
    api: &ApiClient,
    response: &SyncResponse,
    base_path: &Path,
    db: &ClientDb,
    local_files: &HashMap<String, FileState>,
    password_str: &str,
    lazy: bool,
) -> Result<(u32, u32)> {
    let mut downloads = 0u32;
    let mut placeholders = 0u32;

    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if !is_safe_rel_path(path) {
            continue;
        }
        if lazy {
            tracing::info!("Placeholder: {} ({} bytes)", path, replica_file.size);
            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            atomic_write(base_path, path, b"").await?;

            let cache_entry = CacheEntry {
                path: path.clone(),
                plaintext_hash: String::new(),
                encrypted_hash: replica_file.hash.clone(),
                size: replica_file.size,
                mtime: replica_file.mtime,
                server_mtime: replica_file.mtime,
                hydrated: false,
                deleted_at: None,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            placeholders += 1;
        } else {
            let full_path = base_path.join(path);
            let stale_local = match local_files.get(path) {
                None => false,
                Some(local) if local.deleted || !full_path.exists() => false,
                Some(local) => {
                    let meta = fs::metadata(&full_path).await?;
                    let current_mtime = file_mtime_ms(&full_path).await?;
                    current_mtime != local.mtime || meta.len() != local.size
                }
            };
            if stale_local {
                tracing::warn!("Skipping download for {path}: local file changed since scan");
                continue;
            }

            tracing::info!("Downloading {} ({} bytes)", path, replica_file.size);
            let encrypted_content = api.download_file(&replica_file.hash).await?;
            let computed_hash = feanorfs_common::hash_bytes(&encrypted_content);
            if computed_hash != replica_file.hash {
                anyhow::bail!(
                    "Integrity check failed for {}: expected hash {}, computed {} (server may be tampered or corrupt)",
                    path,
                    replica_file.hash,
                    computed_hash
                );
            }
            let plain_content = unpack_bytes(&encrypted_content, password_str, path)?;

            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            atomic_write(base_path, path, &plain_content).await?;

            let actual_mtime = file_mtime_ms(&full_path)
                .await
                .unwrap_or(replica_file.mtime);
            let plaintext_hash = feanorfs_common::hash_bytes(&plain_content);

            let cache_entry = CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: replica_file.hash.clone(),
                size: replica_file.size,
                mtime: actual_mtime,
                server_mtime: replica_file.mtime,
                hydrated: true,
                deleted_at: None,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            downloads += 1;
        }
    }

    Ok((downloads, placeholders))
}

async fn process_delete_local(
    response: &SyncResponse,
    base_path: &Path,
    db: &ClientDb,
) -> Result<u32> {
    let mut deletes = 0u32;
    for path in &response.delete_local {
        if !is_safe_rel_path(path) {
            continue;
        }
        let full_path = base_path.join(path);
        if full_path.exists() {
            tracing::info!("Remote deletion: {}", path);
            fs::remove_file(&full_path)
                .await
                .with_context(|| format!("failed to remove local file {path}"))?;
        }
        db.delete_cache_entry(path).await?;
        deletes += 1;
    }
    Ok(deletes)
}

async fn process_uploads(
    api: &ApiClient,
    db: &ClientDb,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    base_path: &Path,
    workspace_id: &str,
    password_str: &str,
) -> Result<u32> {
    let mut uploads = 0u32;
    for path in &response.upload_required {
        if !is_safe_rel_path(path) {
            continue;
        }
        let Some(local_file) = local_files.get(path) else {
            continue;
        };
        if local_file.deleted {
            tracing::info!("Uploading tombstone for {}", path);
            api.upload_tombstone(workspace_id, path, &local_file.hash, local_file.mtime)
                .await?;
            db.delete_cache_entry(path).await?;
            uploads += 1;
        } else {
            tracing::info!("Uploading {} ({} bytes)", path, local_file.size);
            let plain_content = fs::read(base_path.join(path)).await?;
            let encrypted_content = pack_bytes(&plain_content, password_str, path)?;
            let hash = feanorfs_common::hash_bytes(&encrypted_content);

            api.upload_file(
                workspace_id,
                path,
                &hash,
                local_file.size,
                local_file.mtime,
                encrypted_content,
            )
            .await?;
            db.set_cache_server_mtime(path, local_file.mtime).await?;
            uploads += 1;
        }
    }
    Ok(uploads)
}

async fn cleanup_deleted_cache(
    local_files: &HashMap<String, FileState>,
    db: &ClientDb,
) -> Result<u32> {
    let mut count = 0u32;
    for (path, local_file) in local_files {
        if local_file.deleted {
            tracing::info!("Cleanup cache: {}", path);
            db.delete_cache_entry(path).await?;
            count += 1;
        }
    }
    Ok(count)
}

pub async fn do_push_only(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<PushResult> {
    let (outcome, blocked) = run_sync_pass(
        api,
        db,
        base_path,
        workspace_id,
        password,
        SyncMode::Push,
        false,
    )
    .await?;
    Ok(PushResult {
        mirror_state: mirror_state_after_apply(&blocked, outcome.remote_still_pending),
        uploads: outcome.uploads,
        deletes: outcome.deletes_remote,
        remote_updates_available: outcome.remote_still_pending,
    })
}

pub async fn do_pull_only(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> Result<PullResult> {
    let (outcome, blocked) = run_sync_pass(
        api,
        db,
        base_path,
        workspace_id,
        password,
        SyncMode::Pull,
        lazy,
    )
    .await?;
    Ok(PullResult {
        mirror_state: mirror_state_after_apply(&blocked, false),
        downloads: outcome.downloads,
        placeholders: outcome.placeholders,
        deletes: outcome.deletes_local,
    })
}

pub async fn do_sync(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> Result<SyncResult> {
    let (outcome, blocked) = run_sync_pass(
        api,
        db,
        base_path,
        workspace_id,
        password,
        SyncMode::Full,
        lazy,
    )
    .await?;
    Ok(SyncResult {
        mirror_state: mirror_state_after_apply(&blocked, false),
        uploads: outcome.uploads,
        downloads: outcome.downloads,
        placeholders: outcome.placeholders,
        deletes_local: outcome.deletes_local,
        deletes_remote: outcome.deletes_remote,
    })
}

pub async fn do_hydrate(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    target_path: Option<String>,
    password: Option<&str>,
) -> Result<HydrateResult> {
    tracing::info!("Hydrate (target={:?})", target_path);
    let password_str = password_or_default(password);
    let cache_entries = db.get_cache_entries().await?;

    let mut hydrated = Vec::new();

    for (path, entry) in cache_entries {
        if let Some(ref target) = target_path {
            if path != *target {
                continue;
            }
        }

        if !entry.hydrated {
            tracing::info!("Hydrating {} (hash: {})", path, entry.encrypted_hash);
            let encrypted_content = api.download_file(&entry.encrypted_hash).await?;
            let computed_hash = feanorfs_common::hash_bytes(&encrypted_content);
            if computed_hash != entry.encrypted_hash {
                tracing::warn!(
                    "Integrity check failed for {}: expected {}, computed {} (skipping)",
                    path,
                    entry.encrypted_hash,
                    computed_hash
                );
                continue;
            }
            let plain_content = unpack_bytes(&encrypted_content, password_str, &path)?;

            let full_path = base_path.join(&path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            atomic_write(base_path, &path, &plain_content).await?;

            let actual_mtime = file_mtime_ms(&full_path).await.unwrap_or(entry.mtime);
            let plaintext_hash = feanorfs_common::hash_bytes(&plain_content);

            let updated_entry = CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: entry.encrypted_hash.clone(),
                size: plain_content.len() as u64,
                mtime: actual_mtime,
                server_mtime: entry.server_mtime,
                hydrated: true,
                deleted_at: None,
            };
            db.upsert_cache_entry(&updated_entry).await?;
            hydrated.push(path);
        }
    }

    let (skipped, message) = if hydrated.is_empty() {
        match &target_path {
            Some(t) => (
                true,
                format!("File '{}' is already hydrated or not found.", t),
            ),
            None => (true, "All files are already hydrated.".to_string()),
        }
    } else {
        (false, format!("Hydrated {} files.", hydrated.len()))
    };

    tracing::info!("{}", message);
    Ok(HydrateResult {
        hydrated,
        skipped,
        message,
    })
}

pub async fn do_cat(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    target_path: &str,
    password: Option<&str>,
) -> Result<CatResult> {
    tracing::info!("Cat (path={})", target_path);
    let cache_entries = db.get_cache_entries().await?;

    let mut hydrated_first = false;
    let mut untracked = false;

    if let Some(entry) = cache_entries.get(target_path) {
        if !entry.hydrated {
            tracing::info!("Auto-hydrating {}", target_path);
            do_hydrate(api, db, base_path, Some(target_path.to_string()), password).await?;
            hydrated_first = true;
        }
    } else {
        tracing::warn!("File '{}' not tracked", target_path);
        untracked = true;
    }

    let full_path = base_path.join(target_path);
    if !full_path.exists() {
        return Ok(CatResult {
            content: Vec::new(),
            hydrated_first,
            untracked,
            not_found: true,
        });
    }

    let content = fs::read(full_path).await?;
    Ok(CatResult {
        content,
        hydrated_first,
        untracked,
        not_found: false,
    })
}

pub async fn do_status(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<StatusResult> {
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;

    let pending = conflicts::pending_conflict_paths(db).await?;

    match conflicts::negotiate_sync_with_conflict_gate(
        api,
        workspace_id,
        db,
        base_path,
        &local_files,
        password,
        false,
    )
    .await
    {
        Err(_) => Ok(StatusResult {
            mirror_state: MirrorState::Offline,
            upload_required: Vec::new(),
            download_required: Vec::new(),
            delete_local: Vec::new(),
            local_files,
            pending_conflicts: pending.into_iter().collect(),
        }),
        Ok((response, blocked)) => Ok(StatusResult {
            mirror_state: derive_mirror_state(Some(&response), Some(&blocked)),
            upload_required: response.upload_required,
            download_required: response.download_required,
            delete_local: response.delete_local,
            local_files,
            pending_conflicts: blocked.into_iter().collect(),
        }),
    }
}

#[cfg(test)]
mod mirror_state_tests {
    use super::{derive_mirror_state, MirrorState};
    use feanorfs_common::SyncResponse;
    use std::collections::HashSet;

    #[test]
    fn idle_when_no_pending_changes() {
        let resp = SyncResponse {
            upload_required: vec![],
            download_required: vec![],
            delete_local: vec![],
        };
        assert_eq!(derive_mirror_state(Some(&resp), None), MirrorState::Idle);
    }

    #[test]
    fn out_of_sync_when_uploads_pending() {
        let resp = SyncResponse {
            upload_required: vec!["a.txt".into()],
            download_required: vec![],
            delete_local: vec![],
        };
        assert_eq!(
            derive_mirror_state(Some(&resp), None),
            MirrorState::OutOfSync
        );
    }

    #[test]
    fn offline_when_server_unreachable() {
        assert_eq!(derive_mirror_state(None, None), MirrorState::Offline);
    }

    #[test]
    fn conflict_when_pending_paths() {
        let resp = SyncResponse {
            upload_required: vec![],
            download_required: vec![],
            delete_local: vec![],
        };
        let pending = HashSet::from(["a.txt".into()]);
        assert_eq!(
            derive_mirror_state(Some(&resp), Some(&pending)),
            MirrorState::Conflict
        );
    }

    #[test]
    fn apply_mirror_idle_when_clean() {
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(
            super::mirror_state_after_apply(&empty, false),
            MirrorState::Idle
        );
    }

    #[test]
    fn apply_mirror_out_of_sync_when_remote_pending() {
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(
            super::mirror_state_after_apply(&empty, true),
            MirrorState::OutOfSync
        );
    }
}
