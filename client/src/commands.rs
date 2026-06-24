use crate::api::ApiClient;
use crate::local::{CacheEntry, ClientDb};
use anyhow::Result;
use feanorfs_common::FileState;
use std::path::Path;
use tokio::fs;

const LEGACY_DEFAULT_PASSWORD: &str = "default-secret-key";

pub(crate) fn password_or_default(password: Option<&str>) -> &str {
    match password {
        Some(p) => p,
        None => {
            tracing::warn!(
                "No E2EE password set in config. Using insecure legacy default. \
                 Run 'feanorfs init' to set a proper password."
            );
            LEGACY_DEFAULT_PASSWORD
        }
    }
}

#[derive(Debug, Default)]
pub struct PushResult {
    pub uploads: u32,
    pub deletes: u32,
    pub remote_updates_available: bool,
}

#[derive(Debug, Default)]
pub struct PullResult {
    pub downloads: u32,
    pub placeholders: u32,
    pub deletes: u32,
}

#[derive(Debug, Default)]
pub struct SyncResult {
    pub uploads: u32,
    pub downloads: u32,
    pub placeholders: u32,
    pub deletes_local: u32,
    pub deletes_remote: u32,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct HydrateResult {
    pub hydrated: Vec<String>,
    pub skipped: bool,
    pub message: String,
}

#[derive(Debug)]
pub struct CatResult {
    pub content: String,
    pub hydrated_first: bool,
    pub untracked: bool,
    pub not_found: bool,
}

#[derive(Debug)]
pub struct StatusResult {
    pub upload_required: Vec<String>,
    pub download_required: Vec<FileState>,
    pub delete_local: Vec<String>,
    pub local_files: std::collections::HashMap<String, FileState>,
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

    let mut result = PushResult::default();
    let password_str = password_or_default(password);

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
                result.uploads += 1;
            }
        }
    }

    for (path, local_file) in &local_files {
        if local_file.deleted {
            tracing::info!("Deleting cache entry: {}", path);
            db.delete_cache_entry(path).await?;
            result.deletes += 1;
        }
    }

    result.remote_updates_available =
        !response.download_required.is_empty() || !response.delete_local.is_empty();

    tracing::info!(
        "Push done: {} uploaded, {} deleted",
        result.uploads,
        result.deletes
    );
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

    let mut result = PullResult::default();
    let password_str = password_or_default(password);

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
            result.placeholders += 1;
        } else {
            tracing::info!("Downloading {} ({} bytes)", path, replica_file.size);
            let encrypted_content = api.download_file(&replica_file.hash).await?;
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
                .map(|d| d.as_millis() as i64)
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
            };
            db.upsert_cache_entry(&cache_entry).await?;
            result.downloads += 1;
        }
    }

    for path in &response.delete_local {
        let full_path = base_path.join(path);
        if full_path.exists() {
            tracing::info!("Remote deletion: {}", path);
            let _ = fs::remove_file(full_path).await;
        }
        db.delete_cache_entry(path).await?;
        result.deletes += 1;
    }

    for (path, local_file) in &local_files {
        if local_file.deleted {
            tracing::info!("Cleanup cache: {}", path);
            db.delete_cache_entry(path).await?;
        }
    }

    tracing::info!(
        "Pull done: {} downloaded, {} placeholders, {} deleted",
        result.downloads,
        result.placeholders,
        result.deletes
    );
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

    let mut result = SyncResult::default();
    let password_str = password_or_default(password);

    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if lazy {
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
            result.placeholders += 1;
        } else {
            tracing::info!("Downloading {} ({} bytes)", path, replica_file.size);
            let encrypted_content = api.download_file(&replica_file.hash).await?;
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
                .map(|d| d.as_millis() as i64)
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
            };
            db.upsert_cache_entry(&cache_entry).await?;
            result.downloads += 1;
        }
    }

    for path in &response.delete_local {
        let full_path = base_path.join(path);
        if full_path.exists() {
            tracing::info!("Remote deletion: {}", path);
            let _ = fs::remove_file(full_path).await;
        }
        db.delete_cache_entry(path).await?;
        result.deletes_local += 1;
    }

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
                result.uploads += 1;
            }
        }
    }

    for (path, local_file) in &local_files {
        if local_file.deleted {
            tracing::info!("Cleanup cache: {}", path);
            db.delete_cache_entry(path).await?;
            result.deletes_remote += 1;
        }
    }

    tracing::info!(
        "Sync done: {} up, {} down ({} lazy), {} local del, {} remote del",
        result.uploads,
        result.downloads,
        result.placeholders,
        result.deletes_local,
        result.deletes_remote
    );
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
                .map(|d| d.as_millis() as i64)
                .unwrap_or(entry.mtime);

            let plaintext_hash = feanorfs_common::hash_bytes(&plain_content);

            let updated_entry = CacheEntry {
                path: path.clone(),
                plaintext_hash,
                encrypted_hash: entry.encrypted_hash.clone(),
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

    let response = api.negotiate_sync(&request).await?;

    for (path, file) in &local_files {
        if file.deleted {
            let _ = db.delete_cache_entry(path).await;
        }
    }

    Ok(StatusResult {
        upload_required: response.upload_required,
        download_required: response.download_required,
        delete_local: response.delete_local,
        local_files,
    })
}
