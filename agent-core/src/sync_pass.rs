use crate::api::ApiClient;
use crate::conflicts;
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::fs_util::{atomic_write, file_mtime_ms, set_readonly};
use crate::local::{load_config, CacheEntry, ClientDb};
use crate::lock::SyncLock;
use anyhow::{Context, Result};
use feanorfs_common::{is_safe_rel_path, unpack_bytes_with_policy, FileState, SyncResponse};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::fs;

/// Build `SyncCtx` from on-disk config, or synthesize one when config.json is absent.
pub fn build_ctx_or_fallback<'a>(
    api: &'a ApiClient,
    db: &'a ClientDb,
    base_path: &'a Path,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<SyncCtx<'a>> {
    let config_path = base_path.join(".feanorfs").join("config.json");
    if config_path.exists() {
        let config = load_config(base_path)?;
        SyncCtx::from_config(api, db, base_path, &config)
    } else {
        Ok(SyncCtx::new(
            api,
            db,
            base_path,
            workspace_id,
            password,
            feanorfs_common::LegacyPolicy::AllowXorFallback,
        ))
    }
}

async fn finish_sync_pass(
    ctx: &SyncCtx<'_>,
    local_files: &HashMap<String, FileState>,
    conflict_paths: &HashSet<String>,
    disk_mutated: bool,
) -> Result<()> {
    let current_files = if disk_mutated {
        crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?
    } else {
        local_files.clone()
    };
    conflicts::commit_last_synced(ctx.db, &current_files, conflict_paths).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Push,
    Pull,
    Full,
}

#[derive(Debug, Default)]
pub struct SyncPassOutcome {
    pub uploads: u32,
    pub downloads: u32,
    pub placeholders: u32,
    pub deletes_local: u32,
    pub deletes_remote: u32,
    pub remote_still_pending: bool,
}

pub async fn run_sync_pass(
    ctx: &SyncCtx<'_>,
    mode: SyncMode,
    lazy: bool,
) -> Result<(SyncPassOutcome, HashSet<String>)> {
    let _lock = SyncLock::acquire(ctx.base)?;
    let label = match mode {
        SyncMode::Push => "Push",
        SyncMode::Pull => "Pull",
        SyncMode::Full => "Sync",
    };
    tracing::info!("{label} started (lazy={lazy})");
    let local_files = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    tracing::debug!("Scanned {} entries", local_files.len());

    let (response, mut blocked) =
        conflicts::negotiate_sync_with_conflict_gate(ctx, &local_files, true).await?;

    tracing::debug!(
        "Diff: upload={}, download={}, delete_local={}",
        response.upload_required.len(),
        response.download_required.len(),
        response.delete_local.len()
    );

    let mut outcome = SyncPassOutcome::default();

    if mode != SyncMode::Push {
        let (downloads, placeholders) =
            process_downloads(ctx, &response, &local_files, lazy).await?;
        outcome.downloads = downloads;
        outcome.placeholders = placeholders;
        outcome.deletes_local = process_delete_local(&response, ctx.base, ctx.db).await?;
    }

    if mode != SyncMode::Pull {
        outcome.uploads = process_uploads(ctx, &response, &local_files).await?;
        outcome.deletes_remote = cleanup_deleted_cache(&local_files, ctx.db).await?;

        if !response.upload_required.is_empty() {
            let post = conflicts::detect_post_upload_collisions(
                ctx,
                &local_files,
                &response.upload_required,
            )
            .await?;
            if !post.is_empty() {
                let (_post_dir, post_paths) =
                    conflicts::register_and_write_conflicts(ctx, &post, None).await?;
                blocked.extend(post_paths);
            }
        }
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
    finish_sync_pass(ctx, &local_files, &blocked, disk_mutated).await?;

    Ok((outcome, blocked))
}

pub async fn do_sync(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
) -> Result<(SyncPassOutcome, HashSet<String>)> {
    let ctx = build_ctx_or_fallback(api, db, base_path, workspace_id, password)?;
    run_sync_pass(&ctx, SyncMode::Full, lazy).await
}

pub(crate) async fn process_downloads(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    lazy: bool,
) -> Result<(u32, u32)> {
    let mut downloads = 0u32;
    let mut placeholders = 0u32;
    let password_str = ctx.password_str();

    for replica_file in &response.download_required {
        let path = &replica_file.path;
        if !is_safe_rel_path(path) {
            tracing::warn!("skipping download of unsafe path from server: {path}");
            continue;
        }
        if lazy {
            tracing::info!("Placeholder: {} ({} bytes)", path, replica_file.size);
            let full_path = ctx.base.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            atomic_write(ctx.base, path, b"").await?;
            set_readonly(&ctx.base.join(path), true).await?;

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
            ctx.db.upsert_cache_entry(&cache_entry).await?;
            placeholders += 1;
        } else {
            let full_path = ctx.base.join(path);
            let stale_local = match local_files.get(path) {
                None => false,
                Some(local) if local.deleted || !full_path.exists() => false,
                Some(local) => match fs::metadata(&full_path).await {
                    Ok(meta) => {
                        let current_mtime = file_mtime_ms(&full_path).await?;
                        current_mtime != local.mtime || meta.len() != local.size
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => return Err(e.into()),
                },
            };
            if stale_local {
                tracing::warn!("Skipping download for {path}: local file changed since scan");
                continue;
            }

            tracing::info!("Downloading {} ({} bytes)", path, replica_file.size);
            let encrypted_content = ctx.api.download_file(&replica_file.hash).await?;
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
                unpack_bytes_with_policy(&encrypted_content, password_str, path, ctx.policy)?;

            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            set_readonly(&full_path, false).await?;
            atomic_write(ctx.base, path, &plain_content).await?;

            let actual_mtime = file_mtime_ms(&full_path).await.unwrap_or_else(|e| {
                tracing::warn!("failed to read mtime after download of {path}: {e}; using server mtime");
                replica_file.mtime
            });
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
            ctx.db.upsert_cache_entry(&cache_entry).await?;
            downloads += 1;
        }
    }

    Ok((downloads, placeholders))
}

pub(crate) async fn process_delete_local(
    response: &SyncResponse,
    base_path: &Path,
    db: &ClientDb,
) -> Result<u32> {
    let mut deletes = 0u32;
    for path in &response.delete_local {
        if !is_safe_rel_path(path) {
            tracing::warn!("skipping delete of unsafe path from server: {path}");
            continue;
        }
        let full_path = base_path.join(path);
        if full_path.exists() {
            tracing::info!("Remote deletion: {}", path);
            crate::fs_util::set_readonly(&full_path, false).await.ok();
            fs::remove_file(&full_path)
                .await
                .with_context(|| format!("failed to remove local file {path}"))?;
        }
        db.delete_cache_entry(path).await?;
        deletes += 1;
    }
    Ok(deletes)
}

pub(crate) async fn process_uploads(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
) -> Result<u32> {
    let mut uploads = 0u32;
    let password_str = ctx.password_str();
    for path in &response.upload_required {
        if !is_safe_rel_path(path) {
            tracing::warn!("skipping upload of unsafe path: {path}");
            continue;
        }
        let Some(local_file) = local_files.get(path) else {
            continue;
        };
        if local_file.deleted {
            tracing::info!("Uploading tombstone for {}", path);
            ctx.api
                .upload_tombstone(ctx.workspace_id(), path, &local_file.hash, local_file.mtime)
                .await?;
            ctx.db.delete_cache_entry(path).await?;
            uploads += 1;
        } else {
            tracing::info!("Uploading {} ({} bytes)", path, local_file.size);
            let plain_content = fs::read(ctx.base.join(path)).await?;
            let (hash, encrypted_content) = seal(&plain_content, password_str, path)?;
            ctx.api
                .upload_file(
                    ctx.workspace_id(),
                    path,
                    &hash,
                    local_file.size,
                    local_file.mtime,
                    encrypted_content,
                )
                .await?;
            ctx.db
                .set_cache_server_mtime(path, local_file.mtime)
                .await?;
            uploads += 1;
        }
    }
    Ok(uploads)
}

pub(crate) async fn cleanup_deleted_cache(
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
