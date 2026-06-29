use crate::api::ApiClient;
use crate::local::{CacheEntry, ClientDb};
use anyhow::Result;
use feanorfs_common::{FileState, SyncResponse};
use serde::Serialize;
use std::collections::HashMap;
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

pub fn conflicts_pending(base_path: &Path) -> bool {
    let conflicts_dir = base_path.join(".feanorfs/conflicts");
    if !conflicts_dir.is_dir() {
        return false;
    }
    std::fs::read_dir(&conflicts_dir)
        .map(|entries| entries.filter_map(Result::ok).any(|e| e.path().is_dir()))
        .unwrap_or(false)
}

pub fn derive_mirror_state(base_path: &Path, response: Option<&SyncResponse>) -> MirrorState {
    if conflicts_pending(base_path) {
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

async fn mirror_state_after_sync(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
) -> MirrorState {
    match do_status(api, db, base_path, workspace_id, password).await {
        Ok(s) => s.mirror_state,
        Err(_) => MirrorState::Error,
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
    pub content: String,
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
}

async fn process_downloads(
    api: &ApiClient,
    response: &feanorfs_common::SyncResponse,
    base_path: &Path,
    db: &ClientDb,
    password_str: &str,
    lazy: bool,
) -> Result<(u32, u32)> {
    let mut downloads = 0u32;
    let mut placeholders = 0u32;

    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if lazy {
            tracing::info!("Placeholder: {} ({} bytes)", path, replica_file.size);
            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, b"").await?;

            let cache_entry = CacheEntry {
                path: path.clone(),
                plaintext_hash: String::new(),
                encrypted_hash: replica_file.hash.clone(),
                size: replica_file.size,
                mtime: replica_file.mtime,
                server_mtime: replica_file.mtime,
                hydrated: false,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            placeholders += 1;
        } else {
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
            let plain_content =
                feanorfs_common::crypt_bytes(&encrypted_content, password_str, path);

            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let actual_mtime = fs::metadata(&full_path)
                .await?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(replica_file.mtime);

            let plaintext_hash = feanorfs_common::hash_bytes(&plain_content);

            let cache_entry = CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: replica_file.hash.clone(),
                // XOR keystream preserves byte length: equals plain_content.len() after decrypt.
                size: replica_file.size,
                mtime: actual_mtime,
                server_mtime: replica_file.mtime,
                hydrated: true,
            };
            db.upsert_cache_entry(&cache_entry).await?;
            downloads += 1;
        }
    }

    Ok((downloads, placeholders))
}

async fn process_delete_local(
    response: &feanorfs_common::SyncResponse,
    base_path: &Path,
    db: &ClientDb,
) -> Result<u32> {
    let mut deletes = 0u32;
    for path in &response.delete_local {
        let full_path = base_path.join(path);
        if full_path.exists() {
            tracing::info!("Remote deletion: {}", path);
            let _ = fs::remove_file(full_path).await;
        }
        db.delete_cache_entry(path).await?;
        deletes += 1;
    }
    Ok(deletes)
}

async fn process_uploads(
    api: &ApiClient,
    db: &ClientDb,
    response: &feanorfs_common::SyncResponse,
    local_files: &HashMap<String, FileState>,
    base_path: &Path,
    workspace_id: &str,
    password_str: &str,
) -> Result<u32> {
    let mut uploads = 0u32;
    for path in &response.upload_required {
        if let Some(local_file) = local_files.get(path) {
            if !local_file.deleted {
                tracing::info!("Uploading {} ({} bytes)", path, local_file.size);
                let plain_content = fs::read(base_path.join(path)).await?;
                let encrypted_content =
                    feanorfs_common::crypt_bytes(&plain_content, password_str, path);

                api.upload_file(
                    workspace_id,
                    path,
                    &local_file.hash,
                    local_file.size,
                    local_file.mtime,
                    encrypted_content,
                )
                .await?;
                // The server now records local_file.mtime as the file's mtime.
                // Mirror that into our cache so the next sync diff treats the
                // client view and server view as equal — otherwise the stale
                // server_mtime would force a needless re-download of the blob
                // we just uploaded.
                db.set_cache_server_mtime(path, local_file.mtime).await?;
                uploads += 1;
            }
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
    tracing::info!("Push started");
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;
    tracing::debug!("Scanned {} entries", local_files.len());

    let files_vec: Vec<FileState> = local_files.values().cloned().collect();
    let request = feanorfs_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    let response = api.negotiate_sync(&request).await?;
    tracing::debug!(
        "Diff: upload={}, download={}, delete_local={}",
        response.upload_required.len(),
        response.download_required.len(),
        response.delete_local.len()
    );

    let password_str = password_or_default(password);

    let mut result = PushResult::default();
    result.uploads = process_uploads(
        api,
        db,
        &response,
        &local_files,
        base_path,
        workspace_id,
        password_str,
    )
    .await?;
    result.deletes = cleanup_deleted_cache(&local_files, db).await?;

    result.remote_updates_available =
        !response.download_required.is_empty() || !response.delete_local.is_empty();

    tracing::info!(
        "Push done: {} uploaded, {} deleted",
        result.uploads,
        result.deletes
    );
    result.mirror_state = mirror_state_after_sync(api, db, base_path, workspace_id, password).await;
    Ok(result)
}

pub async fn do_pull_only(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> Result<PullResult> {
    tracing::info!("Pull started (lazy={})", lazy);
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;

    let files_vec: Vec<FileState> = local_files.values().cloned().collect();
    let request = feanorfs_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    let response = api.negotiate_sync(&request).await?;

    let password_str = password_or_default(password);

    let mut result = PullResult::default();
    let (downloads, placeholders) =
        process_downloads(api, &response, base_path, db, password_str, lazy).await?;
    result.downloads = downloads;
    result.placeholders = placeholders;
    result.deletes = process_delete_local(&response, base_path, db).await?;
    cleanup_deleted_cache(&local_files, db).await?;

    tracing::info!(
        "Pull done: {} downloaded, {} placeholders, {} deleted",
        result.downloads,
        result.placeholders,
        result.deletes
    );
    result.mirror_state = mirror_state_after_sync(api, db, base_path, workspace_id, password).await;
    Ok(result)
}

pub async fn do_sync(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> Result<SyncResult> {
    tracing::info!("Sync started (lazy={})", lazy);
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;

    let files_vec: Vec<FileState> = local_files.values().cloned().collect();
    let request = feanorfs_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    let response = api.negotiate_sync(&request).await?;

    let password_str = password_or_default(password);

    let mut result = SyncResult::default();
    let (downloads, placeholders) =
        process_downloads(api, &response, base_path, db, password_str, lazy).await?;
    result.downloads = downloads;
    result.placeholders = placeholders;
    result.deletes_local = process_delete_local(&response, base_path, db).await?;
    result.uploads = process_uploads(
        api,
        db,
        &response,
        &local_files,
        base_path,
        workspace_id,
        password_str,
    )
    .await?;
    result.deletes_remote = cleanup_deleted_cache(&local_files, db).await?;

    tracing::info!(
        "Sync done: {} up, {} down ({} lazy), {} local del, {} remote del",
        result.uploads,
        result.downloads,
        result.placeholders,
        result.deletes_local,
        result.deletes_remote
    );
    result.mirror_state = mirror_state_after_sync(api, db, base_path, workspace_id, password).await;
    Ok(result)
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
            let plain_content =
                feanorfs_common::crypt_bytes(&encrypted_content, password_str, &path);

            let full_path = base_path.join(&path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let actual_mtime = fs::metadata(&full_path)
                .await?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(entry.mtime);

            let plaintext_hash = feanorfs_common::hash_bytes(&plain_content);

            let updated_entry = CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: entry.encrypted_hash.clone(),
                // XOR keystream preserves byte length: equals replica_file.size from download path.
                size: plain_content.len() as u64,
                mtime: actual_mtime,
                server_mtime: entry.server_mtime,
                hydrated: true,
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
            content: String::new(),
            hydrated_first,
            untracked,
            not_found: true,
        });
    }

    let content = fs::read_to_string(full_path).await?;
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

    let files_vec: Vec<FileState> = local_files.values().cloned().collect();
    let request = feanorfs_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    match api.negotiate_sync(&request).await {
        Err(_) => Ok(StatusResult {
            mirror_state: MirrorState::Offline,
            upload_required: Vec::new(),
            download_required: Vec::new(),
            delete_local: Vec::new(),
            local_files,
        }),
        Ok(response) => Ok(StatusResult {
            mirror_state: derive_mirror_state(base_path, Some(&response)),
            upload_required: response.upload_required,
            download_required: response.download_required,
            delete_local: response.delete_local,
            local_files,
        }),
    }
}

#[cfg(test)]
mod mirror_state_tests {
    use super::{conflicts_pending, derive_mirror_state, MirrorState};
    use feanorfs_common::SyncResponse;
    use std::path::Path;

    #[test]
    fn idle_when_no_pending_changes() {
        let base = Path::new("/tmp/unused");
        let resp = SyncResponse {
            upload_required: vec![],
            download_required: vec![],
            delete_local: vec![],
        };
        assert_eq!(derive_mirror_state(base, Some(&resp)), MirrorState::Idle);
    }

    #[test]
    fn out_of_sync_when_uploads_pending() {
        let base = Path::new("/tmp/unused");
        let resp = SyncResponse {
            upload_required: vec!["a.txt".into()],
            download_required: vec![],
            delete_local: vec![],
        };
        assert_eq!(
            derive_mirror_state(base, Some(&resp)),
            MirrorState::OutOfSync
        );
    }

    #[test]
    fn offline_when_server_unreachable() {
        let base = Path::new("/tmp/unused");
        assert_eq!(derive_mirror_state(base, None), MirrorState::Offline);
    }

    #[test]
    fn conflicts_dir_absent_is_not_pending() {
        let base = Path::new("/tmp/feanorfs-nonexistent-conflicts-test");
        assert!(!conflicts_pending(base));
    }
}
