use crate::api::ApiClient;
use crate::local::{CacheEntry, ClientDb};
use anyhow::Result;
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

pub async fn do_push_only(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<()> {
    tracing::info!("Push started");
    tracing::debug!("Scanning directory: {:?}", base_path);
    println!("Scanning directory...");
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;
    tracing::debug!(
        "Scanned local directory: {} entries found",
        local_files.len()
    );

    let files_vec: Vec<feanorfs_common::FileState> = local_files.values().cloned().collect();
    let request = feanorfs_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    tracing::debug!("Querying server for diff...");
    println!("Querying server for diff...");
    let response = api.negotiate_sync(&request).await?;
    tracing::debug!(
        "Sync diff response: upload_required={}, download_required={}, delete_local={}",
        response.upload_required.len(),
        response.download_required.len(),
        response.delete_local.len()
    );

    let mut uploads = 0;
    let mut deletes = 0;

    let password_str = password_or_default(password);

    for path in &response.upload_required {
        if let Some(local_file) = local_files.get(path) {
            if !local_file.deleted {
                tracing::info!(
                    "Uploading {} (size: {} bytes, hash: {})",
                    path,
                    local_file.size,
                    local_file.hash
                );
                println!("Uploading {}...", path);
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
                uploads += 1;
            }
        }
    }

    for (path, local_file) in &local_files {
        if local_file.deleted {
            tracing::info!("Deleting cache entry for deleted local file: {}", path);
            db.delete_cache_entry(path).await?;
            deletes += 1;
        }
    }

    tracing::info!(
        "Push completed. Uploaded {} files, processed {} deletions.",
        uploads,
        deletes
    );
    println!(
        "Push completed. Uploaded {} files, processed {} deletions.",
        uploads, deletes
    );

    if !response.download_required.is_empty() || !response.delete_local.is_empty() {
        println!("Note: There are remote updates available. Run 'feanorfs pull' to apply them.");
    }

    Ok(())
}

pub async fn do_pull_only(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> Result<()> {
    tracing::info!("Pull started (lazy={})", lazy);
    tracing::debug!("Scanning directory: {:?}", base_path);
    println!("Scanning directory...");
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;
    tracing::debug!(
        "Scanned local directory: {} entries found",
        local_files.len()
    );

    let files_vec: Vec<feanorfs_common::FileState> = local_files.values().cloned().collect();
    let request = feanorfs_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    tracing::debug!("Querying server for diff...");
    println!("Querying server for diff...");
    let response = api.negotiate_sync(&request).await?;
    tracing::debug!(
        "Sync diff response: upload_required={}, download_required={}, delete_local={}",
        response.upload_required.len(),
        response.download_required.len(),
        response.delete_local.len()
    );

    let mut downloads = 0;
    let mut deletes = 0;
    let mut placeholders = 0;

    let password_str = password_or_default(password);

    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if lazy {
            tracing::info!(
                "Creating lazy placeholder: {} (remote size: {} bytes, hash: {})",
                path,
                replica_file.size,
                replica_file.hash
            );
            println!("Creating lazy placeholder for {}...", path);
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
            tracing::info!(
                "Downloading {} (size: {} bytes, hash: {})",
                path,
                replica_file.size,
                replica_file.hash
            );
            println!("Downloading {}...", path);
            let encrypted_content = api.download_file(&replica_file.hash).await?;
            let plain_content =
                feanorfs_common::crypt_bytes(&encrypted_content, password_str, path);

            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let metadata = fs::metadata(&full_path).await?;
            let actual_mtime = metadata
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
            downloads += 1;
        }
    }

    for path in &response.delete_local {
        let full_path = base_path.join(path);
        if full_path.exists() {
            tracing::info!("Applying remote deletion of {}", path);
            println!("Deleting {} (remote deletion)...", path);
            let _ = fs::remove_file(full_path).await;
        }
        db.delete_cache_entry(path).await?;
        deletes += 1;
    }

    for (path, local_file) in &local_files {
        if local_file.deleted {
            tracing::info!("Cleaning up cache entry for locally deleted file: {}", path);
            db.delete_cache_entry(path).await?;
        }
    }

    tracing::info!(
        "Pull completed. Downloaded {} files, created {} lazy placeholders, deleted {} files.",
        downloads,
        placeholders,
        deletes
    );
    println!(
        "Pull completed. Downloaded {} files, created {} lazy placeholders, deleted {} files.",
        downloads, placeholders, deletes
    );
    Ok(())
}

pub async fn do_sync(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> Result<()> {
    tracing::info!("Bidirectional sync started (lazy={})", lazy);
    tracing::debug!("Scanning directory: {:?}", base_path);
    println!("Scanning directory...");
    let local_files = crate::local::scan_local_directory(base_path, db, password).await?;
    tracing::debug!(
        "Scanned local directory: {} entries found",
        local_files.len()
    );

    let files_vec: Vec<feanorfs_common::FileState> = local_files.values().cloned().collect();
    let request = feanorfs_common::SyncRequest {
        workspace_id: workspace_id.to_string(),
        files: files_vec,
    };

    tracing::debug!("Querying server for diff...");
    println!("Querying server for diff...");
    let response = api.negotiate_sync(&request).await?;
    tracing::debug!(
        "Sync diff response: upload_required={}, download_required={}, delete_local={}",
        response.upload_required.len(),
        response.download_required.len(),
        response.delete_local.len()
    );

    let mut uploads = 0;
    let mut downloads = 0;
    let mut placeholders = 0;
    let mut deletes_local = 0;
    let mut deletes_remote = 0;

    let password_str = password_or_default(password);

    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if lazy {
            tracing::info!(
                "Creating lazy placeholder (sync): {} (size: {})",
                path,
                replica_file.size
            );
            println!("Creating lazy placeholder for {}...", path);
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
            tracing::info!("Downloading (sync): {} (size: {})", path, replica_file.size);
            println!("Downloading {}...", path);
            let encrypted_content = api.download_file(&replica_file.hash).await?;
            let plain_content =
                feanorfs_common::crypt_bytes(&encrypted_content, password_str, path);

            let full_path = base_path.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let metadata = fs::metadata(&full_path).await?;
            let actual_mtime = metadata
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
            downloads += 1;
        }
    }

    for path in &response.delete_local {
        let full_path = base_path.join(path);
        if full_path.exists() {
            tracing::info!("Applying remote deletion (sync): {}", path);
            println!("Deleting {} (remote deletion)...", path);
            let _ = fs::remove_file(full_path).await;
        }
        db.delete_cache_entry(path).await?;
        deletes_local += 1;
    }

    for path in &response.upload_required {
        if let Some(local_file) = local_files.get(path) {
            if !local_file.deleted {
                tracing::info!("Uploading (sync): {} (size: {})", path, local_file.size);
                println!("Uploading {}...", path);
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
                uploads += 1;
            }
        }
    }

    for (path, local_file) in &local_files {
        if local_file.deleted {
            tracing::info!(
                "Cleaning up cache entry for local deletion (sync): {}",
                path
            );
            db.delete_cache_entry(path).await?;
            deletes_remote += 1;
        }
    }

    tracing::info!(
        "Sync completed. Uploaded {}, Downloaded {} (lazy placeholders: {}), Local Deletes {}, Remote Deletes {}.",
        uploads, downloads, placeholders, deletes_local, deletes_remote
    );
    println!(
        "Sync completed. Uploaded {}, Downloaded {} (lazy placeholders: {}), Local Deletes {}, Remote Deletes {}.",
        uploads, downloads, placeholders, deletes_local, deletes_remote
    );
    Ok(())
}

pub async fn do_hydrate(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    target_path: Option<String>,
    password: Option<&str>,
) -> Result<()> {
    tracing::info!("Hydration started (target={:?})", target_path);
    let password_str = password_or_default(password);
    let cache_entries = db.get_cache_entries().await?;

    let mut hydrated_count = 0;

    for (path, entry) in cache_entries {
        if let Some(ref target) = target_path {
            if path != *target {
                continue;
            }
        }

        if !entry.hydrated {
            tracing::info!("Hydrating file: {} (hash: {})", path, entry.encrypted_hash);
            println!("Hydrating {}...", path);
            let encrypted_content = api.download_file(&entry.encrypted_hash).await?;
            let plain_content =
                feanorfs_common::crypt_bytes(&encrypted_content, password_str, &path);

            let full_path = base_path.join(&path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&full_path, &plain_content).await?;

            let metadata = fs::metadata(&full_path).await?;
            let actual_mtime = metadata
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
            hydrated_count += 1;
        }
    }

    if hydrated_count == 0 {
        if let Some(ref target) = target_path {
            println!("File '{}' is already hydrated or not found.", target);
        } else {
            println!("All files are already hydrated.");
        }
    } else {
        tracing::info!("Hydrated {} files.", hydrated_count);
        println!("Hydrated {} files.", hydrated_count);
    }

    Ok(())
}

pub async fn do_cat(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    target_path: &str,
    password: Option<&str>,
) -> Result<()> {
    tracing::info!("Cat file started (path={})", target_path);
    let cache_entries = db.get_cache_entries().await?;
    if let Some(entry) = cache_entries.get(target_path) {
        if !entry.hydrated {
            tracing::info!("File {} is not hydrated, triggering hydration", target_path);
            do_hydrate(api, db, base_path, Some(target_path.to_string()), password).await?;
        }
    } else {
        tracing::warn!(
            "File '{}' is not tracked in cache. Reading directly.",
            target_path
        );
        println!(
            "Warning: file '{}' is not tracked. Reading directly.",
            target_path
        );
    }

    let full_path = base_path.join(target_path);
    if full_path.exists() {
        let content = fs::read_to_string(full_path).await?;
        print!("{}", content);
    } else {
        tracing::error!("File '{}' does not exist on disk", target_path);
        println!("Error: file '{}' does not exist.", target_path);
    }
    Ok(())
}
