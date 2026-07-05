use crate::api::ApiClient;
use crate::conflicts;
use crate::ctx::SyncCtx;
use crate::fs_util::{atomic_write, file_mtime_ms, set_readonly};
use crate::local::{load_config, CacheEntry, ClientDb};
use crate::lock::SyncLock;
use crate::migrate::legacy_policy_for_config;
use anyhow::{Context, Result};
use feanorfs_common::{
    is_safe_rel_path, pack_bytes, unpack_bytes_with_policy, FileState, SyncResponse,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::fs;

/// Mirror state for status output and `--json` consumers.
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

impl MirrorState {
    /// Human-readable label for status output.
    #[must_use]
    pub const fn human_label(self) -> &'static str {
        match self {
            Self::Idle => "up to date",
            Self::OutOfSync => "has changes",
            Self::Offline => "offline",
            Self::Conflict => "needs attention",
            Self::Error => "error",
            Self::Syncing => "syncing",
        }
    }
}

impl std::fmt::Display for MirrorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.human_label())
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

fn mirror_state_after_apply(blocked: &HashSet<String>, remote_still_pending: bool) -> MirrorState {
    if conflicts::conflicts_pending(Some(blocked)) {
        MirrorState::Conflict
    } else if remote_still_pending {
        MirrorState::OutOfSync
    } else {
        MirrorState::Idle
    }
}

/// Build `SyncCtx` from on-disk config, or synthesize one when config.json is absent.
fn build_ctx_or_fallback<'a>(
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
    /// Local changes waiting to sync when the server was unreachable (DX-8).
    pub offline_backlog: u32,
    /// Set when the server metadata looks older than this machine (DX-23).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_rollback_warning: Option<String>,
    /// Symlink paths skipped during scan (DX-19).
    pub skipped_symlinks: Vec<String>,
}

/// AEAD-seal plaintext into `(encrypt_hash, packed_bytes)`.
pub(crate) fn seal(content: &[u8], password: &str, path: &str) -> Result<(String, Vec<u8>)> {
    let packed = pack_bytes(content, password, path)?;
    let hash = feanorfs_common::hash_bytes(&packed);
    Ok((hash, packed))
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
    let ctx = build_ctx_or_fallback(api, db, base_path, workspace_id, password)?;
    let (outcome, blocked) = run_sync_pass(&ctx, SyncMode::Push, false).await?;
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
    let ctx = build_ctx_or_fallback(api, db, base_path, workspace_id, password)?;
    let (outcome, blocked) = run_sync_pass(&ctx, SyncMode::Pull, lazy).await?;
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
    let ctx = build_ctx_or_fallback(api, db, base_path, workspace_id, password)?;
    let (outcome, blocked) = run_sync_pass(&ctx, SyncMode::Full, lazy).await?;
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
    let ctx = match load_config(base_path) {
        Ok(config) => SyncCtx::from_config(api, db, base_path, &config)?,
        Err(_) => SyncCtx::new(
            api,
            db,
            base_path,
            "",
            password,
            feanorfs_common::LegacyPolicy::AllowXorFallback,
        ),
    };
    do_hydrate_with_ctx(&ctx, target_path).await
}

async fn do_hydrate_with_ctx(
    ctx: &SyncCtx<'_>,
    target_path: Option<String>,
) -> Result<HydrateResult> {
    tracing::info!("Hydrate (target={:?})", target_path);
    let password_str = ctx.password_str();
    let cache_entries = ctx.db.get_cache_entries().await?;

    let mut hydrated = Vec::new();

    for (path, entry) in cache_entries {
        if let Some(ref target) = target_path {
            if path != *target {
                continue;
            }
        }

        if !entry.hydrated {
            tracing::info!("Hydrating {} (hash: {})", path, entry.encrypted_hash);
            let encrypted_content = ctx.api.download_file(&entry.encrypted_hash).await?;
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
                unpack_bytes_with_policy(&encrypted_content, password_str, &path, ctx.policy)?;

            let full_path = ctx.base.join(&path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            set_readonly(&full_path, false).await?;
            atomic_write(ctx.base, &path, &plain_content).await?;

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
            ctx.db.upsert_cache_entry(&updated_entry).await?;
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
    let ctx = build_ctx_or_fallback(api, db, base_path, "", password)?;
    do_cat_with_ctx(&ctx, target_path).await
}

async fn do_cat_with_ctx(ctx: &SyncCtx<'_>, target_path: &str) -> Result<CatResult> {
    tracing::info!("Cat (path={})", target_path);
    let cache_entries = ctx.db.get_cache_entries().await?;

    let mut hydrated_first = false;
    let mut untracked = false;

    if let Some(entry) = cache_entries.get(target_path) {
        if !entry.hydrated {
            tracing::info!("Auto-hydrating {}", target_path);
            do_hydrate_with_ctx(ctx, Some(target_path.to_string())).await?;
            hydrated_first = true;
        }
    } else {
        tracing::warn!("File '{}' not tracked", target_path);
        untracked = true;
    }

    let full_path = ctx.base.join(target_path);
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
    let ctx = build_ctx_or_fallback(api, db, base_path, workspace_id, password)?;
    do_status_with_ctx(&ctx).await
}

async fn do_status_with_ctx(ctx: &SyncCtx<'_>) -> Result<StatusResult> {
    let local_files = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let skipped_symlinks = crate::local::collect_symlink_warnings(ctx.base);
    let pending = conflicts::pending_conflict_paths(ctx.db).await?;
    let last = conflicts::load_last_synced(ctx.db).await?;

    match conflicts::negotiate_sync_with_conflict_gate(ctx, &local_files, false).await {
        Err(_) => {
            let offline_backlog = local_files
                .iter()
                .filter(|(path, state)| {
                    !last
                        .get(*path)
                        .is_some_and(|ls| ls.hash == state.hash && ls.deleted == state.deleted)
                })
                .count() as u32;
            Ok(StatusResult {
                mirror_state: MirrorState::Offline,
                upload_required: Vec::new(),
                download_required: Vec::new(),
                delete_local: Vec::new(),
                local_files,
                pending_conflicts: pending.into_iter().collect(),
                offline_backlog,
                server_rollback_warning: None,
                skipped_symlinks,
            })
        }
        Ok((response, blocked)) => {
            let server_files: Vec<FileState> = response.download_required.clone();
            Ok(StatusResult {
                mirror_state: derive_mirror_state(Some(&response), Some(&blocked)),
                upload_required: response.upload_required,
                download_required: response.download_required,
                delete_local: response.delete_local,
                local_files,
                pending_conflicts: blocked.into_iter().collect(),
                offline_backlog: 0,
                server_rollback_warning: conflicts::detect_server_rollback(&last, &server_files),
                skipped_symlinks,
            })
        }
    }
}

/// Remove tracked files that match built-in or `.feanorfsignore` patterns (DX-5).
pub async fn prune_ignored(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    dry_run: bool,
) -> Result<PruneIgnoredResult> {
    let config = load_config(base_path)?;
    let ctx = SyncCtx::new(
        api,
        db,
        base_path,
        workspace_id,
        config.encryption_password.as_deref(),
        legacy_policy_for_config(&config),
    );
    prune_ignored_with_ctx(&ctx, dry_run).await
}

async fn prune_ignored_with_ctx(ctx: &SyncCtx<'_>, dry_run: bool) -> Result<PruneIgnoredResult> {
    let tracked = ctx.db.get_cache_entries().await?;
    let mut igb = ignore::gitignore::GitignoreBuilder::new(ctx.base);
    for pattern in crate::local::DEFAULT_IGNORES {
        let _ = igb.add_line(None, pattern);
    }
    if let Ok(content) = tokio::fs::read_to_string(ctx.base.join(".feanorfsignore")).await {
        for line in content.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                let _ = igb.add_line(None, line);
            }
        }
    }
    let ig = igb.build()?;
    let mut to_prune = Vec::new();
    for (path, entry) in &tracked {
        if entry.deleted_at.is_some() {
            continue;
        }
        if ig.matched(path, false).is_ignore() {
            to_prune.push(path.clone());
        }
    }
    if dry_run {
        return Ok(PruneIgnoredResult {
            pruned: Vec::new(),
            dry_run: true,
            candidates: to_prune,
        });
    }
    let mut pruned = Vec::new();
    for path in &to_prune {
        let full = ctx.base.join(path);
        if full.exists() {
            tokio::fs::remove_file(&full).await.ok();
        }
        ctx.db.delete_cache_entry(path).await?;
        let mtime = chrono::Utc::now().timestamp_millis();
        let hash = feanorfs_common::hash_bytes(b"");
        ctx.api
            .upload_tombstone(ctx.workspace_id(), path, &hash, mtime)
            .await?;
        pruned.push(path.clone());
    }
    Ok(PruneIgnoredResult {
        pruned,
        dry_run: false,
        candidates: to_prune,
    })
}

#[derive(Debug, Serialize)]
pub struct PruneIgnoredResult {
    pub pruned: Vec<String>,
    pub dry_run: bool,
    pub candidates: Vec<String>,
}

#[cfg(test)]
mod mirror_state_tests {
    use super::{derive_mirror_state, MirrorState};
    use feanorfs_common::SyncResponse;
    use std::collections::HashSet;

    #[test]
    fn human_label_idle() {
        assert_eq!(MirrorState::Idle.human_label(), "up to date");
        assert_eq!(MirrorState::Conflict.human_label(), "needs attention");
    }

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
