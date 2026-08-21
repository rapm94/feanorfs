//! Sync-pass orchestration, the upload/delete side, and the typed outcome.

use crate::api::ApiClient;
use crate::conflicts;
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::local::{load_config, ClientDb};
use crate::lock::SyncLock;
use anyhow::{Context, Result};
use feanorfs_common::{is_safe_rel_path, FileState, SyncResponse};
use futures_util::StreamExt as _;
use std::collections::{HashMap, HashSet};
use std::path::Path;
#[cfg(debug_assertions)]
use tokio::fs;

mod download;
mod materialize;
mod negotiate;
mod rollback;

pub(crate) use self::download::prefetch_downloads;
pub use self::download::process_downloads;
use self::materialize::portable_mode;
pub(crate) use self::negotiate::preflight_download_projection;
use self::negotiate::{validate_cross_direction_structure, validate_final_candidate};
use self::rollback::recover_materialization_stages;

/// Build `SyncCtx` from on-disk config, or synthesize one when config.json is absent.
pub fn build_ctx_or_fallback<'a>(
    api: &'a ApiClient,
    db: &'a ClientDb,
    base_path: &'a Path,
    workspace_id: &str,
    password: Option<&str>,
) -> Result<SyncCtx<'a>> {
    if crate::workspace_layout::workspace_is_configured(base_path) {
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
    local_files_before: &HashMap<String, FileState>,
    conflict_paths: &HashSet<String>,
    mode: SyncMode,
    negotiated_head: Option<&str>,
) -> Result<()> {
    let current_files =
        crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    if ctx.format_version() >= 3 {
        let snapshots = crate::snapshot::SnapshotEngine::new(ctx);
        let expected = negotiated_head.map(str::to_owned);
        let (head_files, head_conflicts, current_root) = match &expected {
            Some(id) => {
                let snapshot = snapshots.load_snapshot(id).await?;
                let state = snapshots.load_state(id).await?;
                (state.files, state.conflicts, Some(snapshot.root))
            }
            None => (HashMap::new(), Vec::new(), None),
        };
        if mode == SyncMode::Pull {
            if let Some(committed) = expected.as_deref() {
                let local_root = snapshots.candidate_root(&current_files, &head_conflicts)?;
                if conflict_paths.is_empty() && current_root.as_deref() == Some(local_root.as_str())
                {
                    snapshots.record_committed_refs(committed).await?;
                } else {
                    snapshots.snapshot_local_view(&current_files, "you").await?;
                    snapshots.record_last_synced_ref(committed).await?;
                }
            } else {
                snapshots.snapshot_local_view(&current_files, "you").await?;
            }
            return Ok(());
        }
        let mut candidate_files = current_files.clone();
        for path in conflict_paths {
            match head_files.get(path) {
                Some(state) => {
                    candidate_files.insert(path.clone(), state.clone());
                }
                None => {
                    candidate_files.remove(path);
                }
            }
        }
        let root = snapshots.candidate_root(&candidate_files, &head_conflicts)?;
        let committed = if current_root.as_deref() == Some(root.as_str()) {
            expected.context("unchanged format v3 workspace has no head")?
        } else {
            let candidate = snapshots
                .write(crate::snapshot::SnapshotInput {
                    files: &candidate_files,
                    conflicts: &head_conflicts,
                    parents: expected.iter().cloned().collect(),
                    author: "sync",
                    message: None,
                })
                .await?;
            match ctx
                .api
                .swap_head(ctx.workspace_id(), expected.as_deref(), &candidate)
                .await?
            {
                crate::SwapHeadResult::Swapped => candidate,
                crate::SwapHeadResult::Conflict(_) => {
                    return Err(crate::agent::continuous::retryable_volatility_failure(
                        "workspace head changed during sync; retry",
                    ))
                }
            }
        };
        ctx.api.set_workspace_format(ctx.workspace_id(), 3).await?;
        if conflict_paths.is_empty() {
            snapshots.record_committed_refs(&committed).await?;
        } else {
            snapshots.snapshot_local_view(&current_files, "you").await?;
            snapshots.record_last_synced_ref(&committed).await?;
        }
        return Ok(());
    }
    let server_files = conflicts::load_server_view(ctx).await?;
    let snapshots = crate::snapshot::SnapshotEngine::new(ctx);
    let mut agreed = snapshots.load_last_synced().await?;
    let paths: HashSet<String> = agreed
        .keys()
        .chain(local_files_before.keys())
        .chain(current_files.keys())
        .chain(server_files.keys())
        .cloned()
        .collect();

    for path in paths {
        if conflict_paths.contains(&path) {
            continue;
        }

        let local = current_files.get(&path);
        let remote = server_files.get(&path);
        if !states_agree(local, remote) {
            continue;
        }

        if let Some(remote) = remote.filter(|state| !state.deleted) {
            agreed.insert(path, remote.clone());
            continue;
        }

        let source = local
            .or_else(|| local_files_before.get(&path))
            .or_else(|| agreed.get(&path));
        if let Some(source) = source {
            let mut tombstone = source.clone();
            tombstone.deleted = true;
            tombstone.size = 0;
            agreed.insert(path, tombstone);
        }
    }

    snapshots.record_last_synced(&agreed, "sync").await?;
    snapshots.snapshot_local_view(&current_files, "you").await?;
    snapshots.publish_server_view(&server_files, "sync").await?;
    Ok(())
}

fn states_agree(local: Option<&FileState>, remote: Option<&FileState>) -> bool {
    match (
        local.filter(|state| !state.deleted),
        remote.filter(|state| !state.deleted),
    ) {
        (Some(local), Some(remote)) => local.hash == remote.hash,
        (None, None) => true,
        _ => false,
    }
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
    pub large_file_count: usize,
    pub large_file_examples: Vec<String>,
}

pub async fn run_sync_pass(
    ctx: &SyncCtx<'_>,
    mode: SyncMode,
    lazy: bool,
) -> Result<(SyncPassOutcome, HashSet<String>)> {
    let _lock = SyncLock::acquire(ctx.base)?;
    run_sync_pass_locked(ctx, mode, lazy).await
}

/// When the server metadata regressed to an older mtime than the last agreed
/// state while the local file still matches that agreed state (restored server
/// backup, clock skew), restore the agreed bytes instead of mass-downloading
/// the stale version. Format-v3 snapshots flatten mtimes to 0 on both sides so
/// this is a no-op there; hash-based reconciliation and the conflict gate own
/// v3 rollback protection.
fn promote_rollback_restores(
    response: &mut SyncResponse,
    local_files: &HashMap<String, FileState>,
    last_synced: &HashMap<String, FileState>,
) {
    let mut restore_paths = Vec::new();
    response.download_required.retain(|remote| {
        let base = last_synced.get(&remote.path);
        let local = local_files.get(&remote.path);
        let is_rollback = base.is_some_and(|b| {
            !b.deleted
                && remote.mtime < b.mtime
                && remote.hash != b.hash
                && local.is_some_and(|l| !l.deleted && l.hash == b.hash)
        });
        if is_rollback {
            restore_paths.push(remote.path.clone());
        }
        !is_rollback
    });
    response.upload_required.extend(restore_paths);
    response.upload_required.sort_unstable();
    response.upload_required.dedup();
}

/// True when a hub rejected a reachability manifest because a referenced blob
/// is missing (fresh/restored hub data or a GC race). Recovery re-uploads all
/// objects, so this must only be treated as recoverable for this exact
/// endpoint/status combination; the response body is never consulted.
fn is_manifest_rejection(error: &anyhow::Error) -> bool {
    matches!(
        crate::api::api_failure_kind(error),
        Some(crate::api::ApiFailureKind::ManifestReferencesMissingBlob)
    )
}

pub(crate) async fn run_sync_pass_locked(
    ctx: &SyncCtx<'_>,
    mode: SyncMode,
    lazy: bool,
) -> Result<(SyncPassOutcome, HashSet<String>)> {
    match run_sync_pass_once(ctx, mode, lazy, false).await {
        Err(error) if is_manifest_rejection(&error) => {
            tracing::warn!(
                "Hub rejected a reachability manifest; clearing the uploaded-object registry and retrying with full uploads"
            );
            if let Ok(state_dir) = ctx.state_dir() {
                let _ = crate::upload_registry::clear(&state_dir).await;
            }
            run_sync_pass_once(ctx, mode, lazy, true).await
        }
        result => result,
    }
}

pub(crate) async fn pause_sync_test(ctx: &SyncCtx<'_>, point: &str) -> Result<()> {
    #[cfg(debug_assertions)]
    {
        let state = ctx.state_dir()?;
        let control = state.join("test-sync-pause");
        if fs::read_to_string(&control).await.ok().as_deref() == Some(point) {
            let reached = state.join("test-sync-pause-reached");
            fs::write(&reached, point).await?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while control.exists() {
                if tokio::time::Instant::now() >= deadline {
                    let _ = fs::remove_file(&reached).await;
                    anyhow::bail!("test sync pause timed out at {point}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let _ = fs::remove_file(reached).await;
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = (ctx, point);
    Ok(())
}

async fn run_sync_pass_once(
    ctx: &SyncCtx<'_>,
    mode: SyncMode,
    lazy: bool,
    force_upload_all: bool,
) -> Result<(SyncPassOutcome, HashSet<String>)> {
    let label = match mode {
        SyncMode::Push => "Push",
        SyncMode::Pull => "Pull",
        SyncMode::Full => "Sync",
    };
    tracing::info!("{label} started (lazy={lazy})");
    recover_materialization_stages(ctx).await?;
    discard_hard_excluded_state(ctx).await?;
    let local_files = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    tracing::debug!("Scanned {} entries", local_files.len());
    crate::snapshot::SnapshotEngine::new(ctx)
        .snapshot_local_view(&local_files, "you")
        .await?;

    let (mut response, mut blocked, negotiated_head) =
        conflicts::negotiate_sync_with_conflict_gate(ctx, &local_files, true).await?;
    pause_sync_test(ctx, "after-negotiate").await?;
    if mode == SyncMode::Push {
        let last_synced = conflicts::load_last_synced_snapshot(ctx).await?;
        promote_rollback_restores(&mut response, &local_files, &last_synced);
    }
    validate_cross_direction_structure(&response)?;
    validate_final_candidate(ctx, mode, &local_files, &response, &blocked).await?;

    tracing::debug!(
        "Diff: upload={}, download={}, delete_local={}",
        response.upload_required.len(),
        response.download_required.len(),
        response.delete_local.len()
    );

    let mut outcome = SyncPassOutcome::default();
    let mut large_files = response
        .upload_required
        .iter()
        .filter(|path| {
            local_files.get(*path).is_some_and(|file| {
                !file.deleted && crate::large_file::exceeds_legacy_single_blob_limit(file.size)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    large_files.sort_unstable();
    large_files.dedup();
    outcome.large_file_count = large_files.len();
    outcome.large_file_examples = large_files.into_iter().take(5).collect();
    if ctx.format_version() < 3 && outcome.large_file_count > 0 {
        let examples = outcome.large_file_examples.join(", ");
        let remainder = outcome
            .large_file_count
            .saturating_sub(outcome.large_file_examples.len());
        let remainder = if remainder == 0 {
            String::new()
        } else {
            format!(" (and {remainder} more)")
        };
        anyhow::bail!(
            "Large files cannot use the legacy single-blob transport: {examples}{remainder}. Run `feanorfs migrate` to enable authenticated encrypted chunks, or `feanorfs ignore <pattern>` for disposable files."
        );
    }

    if mode != SyncMode::Push {
        let (downloads, placeholders) =
            process_downloads(ctx, &response, &local_files, lazy).await?;
        outcome.downloads = downloads;
        outcome.placeholders = placeholders;
        outcome.deletes_local = process_delete_local(&response, ctx.base, ctx.db).await?;
    }

    if mode != SyncMode::Pull {
        outcome.uploads = process_uploads(ctx, &response, &local_files, force_upload_all).await?;
        outcome.deletes_remote = cleanup_deleted_cache(&local_files, ctx.db).await?;

        if ctx.format_version() < 3 && !response.upload_required.is_empty() {
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

    finish_sync_pass(
        ctx,
        &local_files,
        &blocked,
        mode,
        negotiated_head.as_deref(),
    )
    .await?;

    Ok((outcome, blocked))
}

async fn discard_hard_excluded_state(ctx: &SyncCtx<'_>) -> Result<()> {
    let cache = ctx.db.get_cache_entries().await?;
    let pending = ctx.db.list_pending_conflict_paths().await?;
    let mut excluded = cache
        .keys()
        .chain(pending.iter())
        .filter(|path| crate::local::is_always_excluded(Path::new(path)))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for path in &excluded {
        if ctx.format_version() < 3 {
            let hash = cache.get(path).map_or_else(
                || feanorfs_common::hash_bytes(b""),
                |entry| entry.encrypted_hash.clone(),
            );
            ctx.api
                .upload_tombstone(
                    ctx.workspace_id(),
                    path,
                    &hash,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await?;
        }
        ctx.db.delete_cache_entry(path).await?;
        conflicts::discard_excluded_conflict(ctx.db, path).await?;
    }
    if !excluded.is_empty() {
        tracing::info!(
            "Removed {} excluded metadata path(s) from transport state without touching local files",
            excluded.len()
        );
    }
    excluded.clear();
    Ok(())
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

/// Run a full sync while the caller retains the supplied workspace sync guard.
#[doc(hidden)]
pub async fn do_sync_guarded(
    api: &ApiClient,
    db: &ClientDb,
    base_path: &Path,
    workspace_id: &str,
    password: Option<&str>,
    lazy: bool,
    _guard: &SyncLock,
) -> Result<(SyncPassOutcome, HashSet<String>)> {
    let ctx = build_ctx_or_fallback(api, db, base_path, workspace_id, password)?;
    run_sync_pass_locked(&ctx, SyncMode::Full, lazy).await
}

pub(crate) async fn process_delete_local(
    response: &SyncResponse,
    _base_path: &Path,
    _db: &ClientDb,
) -> Result<u32> {
    let mut deletes = 0_u32;
    for path in &response.delete_local {
        if !is_safe_rel_path(path) {
            anyhow::bail!("server requested an unsafe deletion path: {path}");
        }
        deletes = deletes.saturating_add(1);
    }
    Ok(deletes)
}

pub(crate) async fn read_upload_source(
    root: &crate::workspace_read::WorkspaceReadRoot,
    path: &str,
    expected: &FileState,
) -> Result<Vec<u8>> {
    let (bytes, metadata) = root
        .read_regular_stable(path, crate::large_file::CHUNK_THRESHOLD_BYTES)
        .await
        .with_context(|| format!("read local upload source {path}"))?;
    if metadata.len() != expected.size || portable_mode(&metadata) != expected.mode {
        anyhow::bail!("local upload source {path} changed after it was scanned; retry sync");
    }
    Ok(bytes)
}

/// Bounded upload concurrency: the hub admits at most four protected upload
/// bodies at once and rejects saturation with 503, so the client must not
/// exceed that permit cap.
const MAX_PARALLEL_UPLOADS: usize = 4;

pub(crate) async fn process_uploads(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    force_upload_all: bool,
) -> Result<u32> {
    let password_str = ctx.password_str();
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    // After a manifest rejection the registry is cleared; retrying with every
    // live file restores a fresh/restored hub instead of skipping forever.
    let mut paths: Vec<String> = if force_upload_all {
        local_files
            .iter()
            .filter(|(_, file)| !file.deleted)
            .map(|(path, _)| path.clone())
            .collect()
    } else {
        response.upload_required.clone()
    };
    paths.sort_unstable();
    paths.dedup();

    // Blob uploads are independent CAS writes, so bounded concurrency turns N
    // hub round trips into a small batch. The server-mtime bookkeeping that
    // used to rewrite the whole JSON state file per upload is applied once
    // after the pass instead.
    let mut uploads = 0u32;
    let mut server_mtimes = Vec::new();
    let mut stream = futures_util::stream::iter(paths.into_iter().map(|path| {
        let local_file = local_files.get(&path).cloned();
        let read_root = read_root.clone();
        async move {
            if !is_safe_rel_path(&path) {
                tracing::warn!("skipping upload of unsafe path: {path}");
                return Ok(None);
            }
            let Some(local_file) = local_file else {
                return Ok(None);
            };
            upload_one(ctx, &read_root, path, local_file, password_str).await
        }
    }))
    .buffer_unordered(MAX_PARALLEL_UPLOADS);
    while let Some(result) = stream.next().await {
        if let Some((path, server_mtime)) = result? {
            if let Some(mtime) = server_mtime {
                server_mtimes.push((path, mtime));
            }
            uploads += 1;
        }
    }
    if !server_mtimes.is_empty() {
        ctx.db.set_cache_server_mtimes(&server_mtimes).await?;
    }
    Ok(uploads)
}

/// Uploads one file or tombstone. Returns `None` for a v3 deleted skip, or the
/// uploaded path paired with the server mtime to record (legacy tombstones
/// remove their cache entry instead and report `None`).
async fn upload_one(
    ctx: &SyncCtx<'_>,
    read_root: &crate::workspace_read::WorkspaceReadRoot,
    path: String,
    local_file: FileState,
    password: &str,
) -> Result<Option<(String, Option<i64>)>> {
    if ctx.format_version() >= 3 {
        if !local_file.deleted {
            if crate::large_file::uses_chunk_transport(local_file.size) {
                crate::large_file::upload(ctx, &path, &local_file.hash).await?;
            } else {
                let plain_content = read_upload_source(read_root, &path, &local_file).await?;
                let (hash, encrypted_content) = seal(&plain_content, password, &path)?;
                if hash != local_file.hash {
                    anyhow::bail!("local upload source {path} changed after scanning; retry sync");
                }
                ctx.api
                    .upload_object(ctx.workspace_id(), &hash, encrypted_content)
                    .await?;
            }
            return Ok(Some((path, Some(local_file.mtime))));
        }
        return Ok(None);
    }
    if !local_file.deleted && crate::large_file::uses_chunk_transport(local_file.size) {
        anyhow::bail!(
            "Cannot upload large file {path} from this legacy workspace. Run `feanorfs migrate` to enable authenticated encrypted chunks, or `feanorfs ignore <pattern>` for disposable files."
        );
    }
    if local_file.deleted {
        tracing::debug!("Uploading tombstone for {}", path);
        ctx.api
            .upload_tombstone(
                ctx.workspace_id(),
                &path,
                &local_file.hash,
                local_file.mtime,
            )
            .await?;
        ctx.db.delete_cache_entry(&path).await?;
        return Ok(Some((path, None)));
    }
    tracing::debug!("Uploading {} ({} bytes)", path, local_file.size);
    let plain_content = read_upload_source(read_root, &path, &local_file).await?;
    let (hash, encrypted_content) = seal(&plain_content, password, &path)?;
    if hash != local_file.hash {
        anyhow::bail!("local upload source {path} changed after scanning; retry sync");
    }
    let mut upload = local_file.clone();
    upload.hash = hash;
    ctx.api
        .upload_file(ctx.workspace_id(), &upload, encrypted_content)
        .await?;
    Ok(Some((path, Some(local_file.mtime))))
}

pub(crate) async fn cleanup_deleted_cache(
    local_files: &HashMap<String, FileState>,
    db: &ClientDb,
) -> Result<u32> {
    let paths = local_files
        .iter()
        .filter(|(_, local_file)| local_file.deleted)
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(0);
    }
    db.delete_cache_entries(&paths).await?;
    tracing::info!("Cleaned {} deleted cache entries", paths.len());
    Ok(u32::try_from(paths.len()).unwrap_or(u32::MAX))
}
