use crate::api::ApiClient;
use crate::conflicts;
use crate::crypto::seal;
use crate::ctx::SyncCtx;
#[cfg(not(unix))]
use crate::fs_util::{apply_executable_mode, set_readonly};
use crate::fs_util::{atomic_write, file_mtime_ms};
use crate::local::{load_config, CacheEntry, ClientDb};
use crate::lock::SyncLock;
use anyhow::{Context, Result};
use feanorfs_common::{is_safe_rel_path, FileState, SyncResponse};
use futures_util::StreamExt as _;
use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use tokio::fs;

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
                    anyhow::bail!("workspace head changed during sync; retry")
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
/// objects, so this must only be treated as recoverable for this exact error.
fn is_manifest_rejection(error: &anyhow::Error) -> bool {
    error.to_string().contains("references missing blob")
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

fn validate_cross_direction_structure(response: &SyncResponse) -> Result<()> {
    let remote_changes = response
        .download_required
        .iter()
        .map(|file| file.path.clone())
        .chain(response.delete_local.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    for local_path in &response.upload_required {
        if let Some(remote_path) = related_path(&remote_changes, local_path) {
            anyhow::bail!(
                "concurrent file/directory conflict between local {local_path} and cloud {remote_path}; reconcile the hierarchy explicitly before syncing"
            );
        }
    }
    Ok(())
}

fn related_path<'a>(paths: &'a std::collections::BTreeSet<String>, path: &str) -> Option<&'a str> {
    if let Some(exact) = paths.get(path) {
        return Some(exact);
    }
    for (index, _) in path.match_indices('/') {
        if let Some(ancestor) = paths.get(&path[..index]) {
            return Some(ancestor);
        }
    }
    let descendant_prefix = format!("{path}/");
    paths
        .range(descendant_prefix.clone()..)
        .next()
        .filter(|candidate| candidate.starts_with(&descendant_prefix))
        .map(String::as_str)
}

async fn validate_final_candidate(
    ctx: &SyncCtx<'_>,
    mode: SyncMode,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
    blocked: &HashSet<String>,
) -> Result<()> {
    let mut projected = local_files.clone();
    if mode != SyncMode::Push {
        for path in &response.delete_local {
            projected.remove(path);
        }
        for file in &response.download_required {
            projected.insert(file.path.clone(), file.clone());
        }
    }
    feanorfs_common::flat_to_tree(&projected)
        .context("sync result contains a file/directory path collision")?;
    if ctx.format_version() < 3 || mode == SyncMode::Pull {
        return Ok(());
    }
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Ok(());
    };
    let snapshots = crate::snapshot::SnapshotEngine::new(ctx);
    let state = snapshots.load_state(&head).await?;
    for path in blocked {
        match state.files.get(path) {
            Some(file) => {
                projected.insert(path.clone(), file.clone());
            }
            None => {
                projected.remove(path);
            }
        }
    }
    snapshots
        .candidate_root(&projected, &state.conflicts)
        .context("pending conflicts would create a file/directory path collision")?;
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

struct PreparedDownload {
    file: FileState,
    #[cfg(not(unix))]
    staged_path: PathBuf,
    plaintext_hash: String,
    hydrated: bool,
}

#[derive(Debug)]
struct PreserveMaterializationStage(anyhow::Error);

impl std::fmt::Display for PreserveMaterializationStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for PreserveMaterializationStage {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

fn preserve_materialization_stage(error: anyhow::Error) -> anyhow::Error {
    PreserveMaterializationStage(error).into()
}

fn must_preserve_materialization_stage(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<PreserveMaterializationStage>()
        .is_some()
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct JournalDownload {
    file: FileState,
    plaintext_hash: String,
    hydrated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct MaterializationDirectoryIdentity {
    volume_serial: u32,
    file_index: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MaterializationDirectoryProof {
    path: String,
    #[serde(default)]
    identity: Option<MaterializationDirectoryIdentity>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MaterializationJournal {
    phase: String,
    /// Explicitly distinguishes journals written by the current activation
    /// protocol from older journals that predate per-publication progress.
    #[serde(default)]
    publication_progress_recorded: bool,
    original_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    original_readonly: std::collections::BTreeMap<String, bool>,
    downloads: Vec<JournalDownload>,
    delete_paths: Vec<String>,
    /// Paths whose publication hard link was verified and durably recorded.
    /// Missing in older journals, which retain the conservative legacy
    /// recovery heuristics below.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    published_paths: Vec<String>,
    /// Path whose publication is between the pre-link journal write and the
    /// post-link progress write. Missing in older journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publishing_path: Option<String>,
    /// Exact identities of destination directories created by this
    /// activation. Missing in old journals, so recovery removes none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    created_directories: Vec<MaterializationDirectoryProof>,
}

struct MaterializationAnchors {
    #[cfg(unix)]
    base: std::fs::File,
    #[cfg(unix)]
    stage: std::fs::File,
}

struct MaterializationBackup {
    path: String,
    #[cfg(not(unix))]
    original: PathBuf,
    #[cfg(not(unix))]
    backup: PathBuf,
    #[cfg(not(unix))]
    readonly: bool,
    #[cfg(unix)]
    original_parent: std::fs::File,
    #[cfg(unix)]
    backup_parent: std::fs::File,
}

#[cfg(unix)]
struct CreatedMaterializationDirectory {
    parent: std::fs::File,
    directory: std::fs::File,
    name: std::ffi::CString,
    path: String,
}

struct PublishedDownload {
    destination: PathBuf,
    expected: JournalDownload,
    mode_applied: bool,
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(windows)]
    /// Delete-capable handle for the exact inode published by this
    /// transaction.  Windows rollback must use this handle rather than
    /// reopening the destination path, which may have been replaced by a
    /// user while activation was in flight.
    file: std::fs::File,
    #[cfg(unix)]
    directory_chain: Vec<std::fs::File>,
    #[cfg(unix)]
    created_directories: Vec<CreatedMaterializationDirectory>,
    #[cfg(not(unix))]
    created_directories: Vec<MaterializationDirectoryProof>,
}

#[doc(hidden)]
pub async fn process_downloads(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    lazy: bool,
) -> Result<(u32, u32)> {
    if recover_materialization_stages(ctx).await? {
        anyhow::bail!("recovered an interrupted materialization; retry the operation");
    }
    if response.download_required.is_empty() && response.delete_local.is_empty() {
        return Ok((0, 0));
    }
    let mut download_paths = HashSet::with_capacity(response.download_required.len());
    for file in &response.download_required {
        if !download_paths.insert(file.path.as_str()) {
            anyhow::bail!("server requested duplicate download path: {}", file.path);
        }
    }
    let mut delete_paths = HashSet::with_capacity(response.delete_local.len());
    for path in &response.delete_local {
        if !delete_paths.insert(path.as_str()) {
            anyhow::bail!("server requested duplicate local deletion path: {path}");
        }
        if download_paths.contains(path.as_str()) {
            anyhow::bail!("server requested both download and deletion for path: {path}");
        }
    }
    preflight_download_projection(ctx.base, local_files, response).await?;
    let stage = create_materialization_stage(ctx).await?;
    let result = process_downloads_in_stage(ctx, response, local_files, lazy, &stage).await;
    let preserve = result
        .as_ref()
        .err()
        .is_some_and(must_preserve_materialization_stage);
    if !preserve {
        if let Err(error) = remove_materialization_stage(&stage, ctx.base).await {
            tracing::warn!(
                "failed to remove materialization staging directory {} durably: {error}",
                stage.display()
            );
        }
    } else {
        tracing::error!(
            "materialization recovery data retained at {}",
            stage.display()
        );
    }
    result
}

async fn process_downloads_in_stage(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    lazy: bool,
    stage: &Path,
) -> Result<(u32, u32)> {
    let cached_entries = ctx.db.get_cache_entries().await?;
    let placeholder_anchors =
        capture_placeholder_anchors(ctx, response, local_files, &cached_entries)?;
    // Download, authenticate, and fsync every replacement before changing the
    // worktree. A corrupt or unavailable later object therefore cannot leave a
    // half-applied file/directory transition. Each staged file is independent,
    // so bounded concurrency batches the hub round trips; activation below
    // still applies the staged files strictly sequentially.
    let mut prepared = Vec::with_capacity(response.download_required.len());
    let mut downloads = futures_util::stream::iter(response.download_required.iter().cloned().map(
        |replica_file| {
            prepare_download(ctx, &cached_entries, local_files, lazy, stage, replica_file)
        },
    ))
    .buffer_unordered(MAX_PARALLEL_DOWNLOADS);
    while let Some(item) = downloads.next().await {
        prepared.push(item?);
    }

    pause_sync_test(ctx, "before-final-validation").await?;
    revalidate_materialization_inputs(
        ctx,
        response,
        local_files,
        &cached_entries,
        &placeholder_anchors,
    )
    .await?;
    activate_prepared_downloads(
        ctx,
        response,
        local_files,
        &cached_entries,
        &placeholder_anchors,
        prepared,
        stage,
    )
    .await
}

/// Bounded upload concurrency: the hub admits at most four protected upload
/// bodies at once and rejects saturation with 503, so the client must not
/// exceed that permit cap.
const MAX_PARALLEL_UPLOADS: usize = 4;
/// Bounded download concurrency for independent staged blob fetches.
const MAX_PARALLEL_DOWNLOADS: usize = 8;
const MAX_MATERIALIZATION_STAGES: usize = 64;
const MAX_MATERIALIZATION_JOURNAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_MATERIALIZATION_DIRECTORY_PROOFS: usize = feanorfs_common::MAX_TREE_OUTPUT_PATHS;

/// Fetches, authenticates, and fsyncs one staged download inside the
/// materialization stage. Worktree changes are never made here; activation
/// applies every staged file after the whole batch is durable.
async fn prepare_download(
    ctx: &SyncCtx<'_>,
    cached_entries: &HashMap<String, CacheEntry>,
    local_files: &HashMap<String, FileState>,
    lazy: bool,
    stage: &Path,
    replica_file: FileState,
) -> Result<PreparedDownload> {
    let path = &replica_file.path;
    if !is_safe_rel_path(path) {
        anyhow::bail!("server requested an unsafe download path: {path}");
    }
    let full_path = ctx.base.join(path);
    let stale_local = match local_files.get(path) {
        None => false,
        Some(local) if local.deleted || !full_path.exists() => false,
        Some(_) => match cached_entries.get(path) {
            Some(entry) if !entry.hydrated => false,
            Some(entry) => match fs::metadata(&full_path).await {
                Ok(meta) => {
                    let current_mtime = file_mtime_ms(&full_path).await?;
                    current_mtime != entry.mtime || meta.len() != entry.size
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            },
            None => false,
        },
    };
    if stale_local {
        anyhow::bail!("local path {path} changed while downloads were staged");
    }

    let new_root = stage.join("new");
    let staged_path = new_root.join(path);
    if let Some(parent) = staged_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let (plaintext_hash, materialized_size) = if lazy {
        tracing::debug!(
            "Preparing placeholder: {} ({} bytes)",
            path,
            replica_file.size
        );
        atomic_write(&new_root, path, b"").await?;
        (String::new(), replica_file.size)
    } else {
        tracing::debug!("Preparing download: {} ({} bytes)", path, replica_file.size);
        let materialized = crate::large_file::materialize_to(
            ctx,
            path,
            &replica_file.hash,
            replica_file.size,
            &staged_path,
        )
        .await?;
        (materialized.plaintext_hash, materialized.size)
    };
    if let Some(parent) = staged_path.parent() {
        sync_directory_chain(parent, stage).await?;
    }
    let mut prepared_file = replica_file.clone();
    if !lazy && prepared_file.size == 0 {
        prepared_file.size = materialized_size;
    }
    Ok(PreparedDownload {
        file: prepared_file,
        #[cfg(not(unix))]
        staged_path,
        plaintext_hash,
        hydrated: !lazy,
    })
}

async fn sync_directory(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    let display_path = path.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        #[cfg(windows)]
        let directory = {
            use std::os::windows::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1 | 0x2 | 0x4)
                .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
                .open(&path)?
        };
        #[cfg(windows)]
        {
            // Windows has no portable directory-entry fsync: FlushFileBuffers
            // on a directory handle returns ERROR_ACCESS_DENIED. Files and the
            // materialization journal are flushed before publication. Keep the
            // directory open/metadata check so missing or inaccessible paths
            // still fail closed.
            if !directory.metadata()?.is_dir() {
                return Err(std::io::Error::other(
                    "directory sync target is not a directory",
                ));
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let directory = std::fs::File::open(&path)?;
            directory.sync_all()
        }
    })
    .await
    .context("join directory sync task")?
    .with_context(|| format!("sync directory {}", display_path.display()))
}

async fn sync_directory_chain(start: &Path, root: &Path) -> Result<()> {
    anyhow::ensure!(
        start.starts_with(root),
        "directory sync escaped transaction root"
    );
    let mut current = start.to_path_buf();
    loop {
        sync_directory(&current).await?;
        if current == root {
            return Ok(());
        }
        anyhow::ensure!(current.pop(), "directory sync escaped transaction root");
    }
}

#[cfg(not(unix))]
async fn sync_file(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let file = fs::OpenOptions::new()
        // FlushFileBuffers requires a write-capable handle on Windows. All
        // callers flush before restoring a placeholder's read-only bit.
        .read(true)
        .write(true)
        .open(path)
        .await;
    #[cfg(not(windows))]
    let file = fs::File::open(path).await;
    file.with_context(|| format!("open {} for durability sync", path.display()))?
        .sync_all()
        .await
        .with_context(|| format!("sync {}", path.display()))
}

#[cfg(unix)]
fn open_directory_at_tracked(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    create: bool,
) -> std::io::Result<(std::fs::File, bool)> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated and `parent` remains open for the call.
    let mut fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    let mut created_here = false;
    if fd < 0 && create && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        // SAFETY: arguments are valid for the duration of this call. The mode
        // is filtered by the process umask just like `create_dir_all`.
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) };
        if created == 0 {
            created_here = true;
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        // SAFETY: same valid parent/name pair as above.
        fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor.
    Ok((unsafe { std::fs::File::from_raw_fd(fd) }, created_here))
}

#[cfg(unix)]
fn open_directory_at(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    create: bool,
) -> std::io::Result<std::fs::File> {
    open_directory_at_tracked(parent, name, create).map(|(directory, _)| directory)
}

#[cfg(unix)]
fn open_regular_at(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    write: bool,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let access = if write { libc::O_RDWR } else { libc::O_RDONLY };
    // SAFETY: `name` is NUL-terminated and `parent` remains open for the call.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            access | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other(
            "materialization entry is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn portable_component(value: &str) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(value.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(unix)]
struct RelativeParentAt {
    parent: std::fs::File,
    final_name: std::ffi::CString,
}

#[cfg(unix)]
fn open_materialization_anchors_blocking(
    base: &Path,
    stage: &Path,
) -> std::io::Result<MaterializationAnchors> {
    use std::os::unix::fs::OpenOptionsExt as _;

    if stage.parent() != Some(base) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization stage is not directly beneath the workspace",
        ));
    }
    let canonical_base = std::fs::canonicalize(base)?;
    let base_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(canonical_base)?;
    let stage_name = stage
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| std::io::Error::other("materialization stage name is not UTF-8"))?;
    let stage_name = portable_component(stage_name)?;
    let stage_file = open_directory_at(&base_file, &stage_name, false)?;
    Ok(MaterializationAnchors {
        base: base_file,
        stage: stage_file,
    })
}

#[cfg(unix)]
async fn open_materialization_anchors(base: &Path, stage: &Path) -> Result<MaterializationAnchors> {
    let base = base.to_path_buf();
    let stage = stage.to_path_buf();
    tokio::task::spawn_blocking(move || open_materialization_anchors_blocking(&base, &stage))
        .await
        .context("join materialization anchor task")?
        .context("open no-follow materialization anchors")
}

#[cfg(not(unix))]
async fn open_materialization_anchors(
    _base: &Path,
    _stage: &Path,
) -> Result<MaterializationAnchors> {
    Ok(MaterializationAnchors {})
}

#[cfg(unix)]
fn open_relative_parent_at(
    root: &std::fs::File,
    relative: &str,
    create: bool,
) -> std::io::Result<RelativeParentAt> {
    let mut chain = vec![root.try_clone()?];
    let mut components = relative.split('/').peekable();
    let mut final_name = None;
    while let Some(component) = components.next() {
        let component = portable_component(component)?;
        if components.peek().is_none() {
            final_name = Some(component);
            break;
        }
        let parent = chain
            .last()
            .ok_or_else(|| std::io::Error::other("missing relative parent"))?;
        chain.push(open_directory_at(parent, &component, create)?);
    }
    let final_name = final_name
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty path"))?;
    let parent = chain
        .last()
        .ok_or_else(|| std::io::Error::other("missing relative parent"))?
        .try_clone()?;
    Ok(RelativeParentAt { parent, final_name })
}

#[cfg(unix)]
fn same_open_file(left: &std::fs::File, right: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(unix)]
fn unlink_regular_at(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    expected: &std::fs::File,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let current = open_regular_at(parent, name, false)?;
    if !same_open_file(&current, expected)? {
        return Err(std::io::Error::other(
            "materialized destination identity changed",
        ));
    }
    // SAFETY: the descriptor and NUL-terminated component remain valid.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    parent.sync_all()
}

#[cfg(unix)]
fn remove_created_directories_at(
    directories: &[CreatedMaterializationDirectory],
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    for created in directories.iter().rev() {
        let current = match open_directory_at(&created.parent, &created.name, false) {
            Ok(current) => current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !same_open_file(&current, &created.directory)? {
            return Err(std::io::Error::other(format!(
                "created materialization directory {} changed identity",
                created.path
            )));
        }
        // SAFETY: this removes only the proven directory entry from its opened parent.
        if unsafe {
            libc::unlinkat(
                created.parent.as_raw_fd(),
                created.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.kind(),
                std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
            ) {
                continue;
            }
            return Err(error);
        }
        created.parent.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn backup_original_no_follow_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::AsRawFd as _;

    let original = open_relative_parent_at(&base, relative, false)?;
    let backup_name = portable_component("backup")?;
    let backup_root = open_directory_at(&stage, &backup_name, true)?;
    let backup = open_relative_parent_at(&backup_root, relative, true)?;
    // SAFETY: both directory descriptors and the shared final component remain valid.
    if unsafe {
        libc::renameat(
            original.parent.as_raw_fd(),
            original.final_name.as_ptr(),
            backup.parent.as_raw_fd(),
            backup.final_name.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok((original.parent, backup.parent))
}

#[cfg(unix)]
async fn backup_original_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<(std::fs::File, std::fs::File)> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || backup_original_no_follow_blocking(base, stage, &relative))
        .await
        .context("join no-follow backup task")?
        .with_context(|| format!("back up local path {display}"))
}

#[cfg(unix)]
async fn sync_backup_directories(backup: &MaterializationBackup) -> Result<()> {
    let original = backup.original_parent.try_clone()?;
    let staged = backup.backup_parent.try_clone()?;
    tokio::task::spawn_blocking(move || {
        original.sync_all()?;
        staged.sync_all()
    })
    .await
    .context("join backup directory sync task")?
    .context("sync no-follow backup directories")
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackupRecoveryState {
    Missing,
    DestinationMissing,
    AlreadyRestored,
    DestinationOccupied,
}

#[cfg(unix)]
fn inspect_backup_recovery_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<BackupRecoveryState> {
    use std::os::fd::AsRawFd as _;

    let backup_name = portable_component("backup")?;
    let backup_root = match open_directory_at(&stage, &backup_name, false) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackupRecoveryState::Missing)
        }
        Err(error) => return Err(error),
    };
    let backup = match open_relative_parent_at(&backup_root, relative, false) {
        Ok(backup) => backup,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(BackupRecoveryState::Missing)
        }
        Err(error) => return Err(error),
    };
    let source = open_regular_at(&backup.parent, &backup.final_name, false)?;
    let original = match open_relative_parent_at(&base, relative, false) {
        Ok(original) => original,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(BackupRecoveryState::DestinationMissing)
        }
        Err(error) => return Err(error),
    };
    match open_regular_at(&original.parent, &original.final_name, false) {
        Ok(current) if same_open_file(&source, &current)? => {
            // SAFETY: remove only the redundant staged hard link.
            if unsafe { libc::unlinkat(backup.parent.as_raw_fd(), backup.final_name.as_ptr(), 0) }
                != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            backup.parent.sync_all()?;
            Ok(BackupRecoveryState::AlreadyRestored)
        }
        Ok(_) => Ok(BackupRecoveryState::DestinationOccupied),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(BackupRecoveryState::DestinationMissing)
        }
        // Any non-missing entry that cannot be opened as the backed-up regular
        // file is occupied. Recovery will remove it only if it is a proven
        // empty transaction-created directory; symlinks and special files stay.
        Err(_) => Ok(BackupRecoveryState::DestinationOccupied),
    }
}

#[cfg(unix)]
async fn inspect_backup_recovery(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<BackupRecoveryState> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    tokio::task::spawn_blocking(move || inspect_backup_recovery_blocking(base, stage, &relative))
        .await
        .context("join no-follow backup inspection task")?
        .context("inspect no-follow materialization backup")
}

#[cfg(unix)]
fn restore_backup_no_follow_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd as _;

    let backup_name = portable_component("backup")?;
    let backup_root = open_directory_at(&stage, &backup_name, false)?;
    let backup = open_relative_parent_at(&backup_root, relative, false)?;
    let original = open_relative_parent_at(&base, relative, true)?;
    let source = open_regular_at(&backup.parent, &backup.final_name, false)?;
    // No-clobber restoration: a concurrent entry is retained and recovery fails closed.
    if unsafe {
        libc::linkat(
            backup.parent.as_raw_fd(),
            backup.final_name.as_ptr(),
            original.parent.as_raw_fd(),
            original.final_name.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let restored = match open_regular_at(&original.parent, &original.final_name, false) {
        Ok(restored) => restored,
        Err(error) => {
            // SAFETY: remove only the entry just linked into the anchored parent.
            unsafe {
                libc::unlinkat(original.parent.as_raw_fd(), original.final_name.as_ptr(), 0);
            }
            return Err(error);
        }
    };
    if !same_open_file(&source, &restored)? {
        // SAFETY: same anchored destination as above.
        unsafe {
            libc::unlinkat(original.parent.as_raw_fd(), original.final_name.as_ptr(), 0);
        }
        return Err(std::io::Error::other("restored backup identity changed"));
    }
    original.parent.sync_all()?;
    // SAFETY: the backup identity remains opened and the name is anchored.
    if unsafe { libc::unlinkat(backup.parent.as_raw_fd(), backup.final_name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    backup.parent.sync_all()?;
    Ok(restored)
}

#[cfg(unix)]
async fn restore_backup_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<std::fs::File> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || restore_backup_no_follow_blocking(base, stage, &relative))
        .await
        .context("join no-follow backup restoration task")?
        .with_context(|| format!("restore materialization backup {display}"))
}

#[cfg(unix)]
fn remove_current_regular_no_follow_blocking(
    base: std::fs::File,
    relative: &str,
) -> std::io::Result<bool> {
    let destination = match open_relative_parent_at(&base, relative, false) {
        Ok(destination) => destination,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let current = open_regular_at(&destination.parent, &destination.final_name, false)?;
    unlink_regular_at(&destination.parent, &destination.final_name, &current)?;
    Ok(true)
}

#[cfg(unix)]
async fn remove_current_regular_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<bool> {
    let base = anchors.base.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || remove_current_regular_no_follow_blocking(base, &relative))
        .await
        .context("join no-follow current-file removal task")?
        .with_context(|| format!("remove interrupted materialization {display}"))
}

#[cfg(unix)]
fn remove_recovered_publication_no_follow_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<bool> {
    let new_name = portable_component("new")?;
    let new_root = match open_directory_at(&stage, &new_name, false) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let staged = match open_relative_parent_at(&new_root, relative, false) {
        Ok(staged) => staged,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let destination = match open_relative_parent_at(&base, relative, false) {
        Ok(destination) => destination,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let source = open_regular_at(&staged.parent, &staged.final_name, false)?;
    let current = open_regular_at(&destination.parent, &destination.final_name, false)?;
    if !same_open_file(&source, &current)? {
        return Ok(false);
    }
    unlink_regular_at(&destination.parent, &destination.final_name, &source)?;
    Ok(true)
}

#[cfg(unix)]
async fn remove_recovered_publication_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<bool> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || {
        remove_recovered_publication_no_follow_blocking(base, stage, &relative)
    })
    .await
    .context("join no-follow recovered publication task")?
    .with_context(|| format!("remove interrupted materialization {display}"))
}

#[cfg(unix)]
fn remove_empty_relative_directory_at(
    base: std::fs::File,
    relative: &str,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    let target = match open_relative_parent_at(&base, relative, false) {
        Ok(target) => target,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let directory = match open_directory_at(&target.parent, &target.final_name, false) {
        Ok(directory) => directory,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let current = open_directory_at(&target.parent, &target.final_name, false)?;
    if !same_open_file(&directory, &current)? {
        return Err(std::io::Error::other(
            "materialization directory changed identity",
        ));
    }
    // SAFETY: remove only this opened, empty directory from its anchored parent.
    if unsafe {
        libc::unlinkat(
            target.parent.as_raw_fd(),
            target.final_name.as_ptr(),
            libc::AT_REMOVEDIR,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
        ) {
            return Ok(false);
        }
        return Err(error);
    }
    target.parent.sync_all()?;
    Ok(true)
}

#[cfg(unix)]
async fn remove_empty_directory_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<bool> {
    let base = anchors.base.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || remove_empty_relative_directory_at(base, &relative))
        .await
        .context("join no-follow directory removal task")?
        .with_context(|| format!("remove empty materialization directory {display}"))
}

#[cfg(unix)]
async fn remove_created_descendants_for_backup(
    anchors: &MaterializationAnchors,
    backup_path: &str,
    downloads: &[JournalDownload],
) -> Result<()> {
    let prefix = format!("{backup_path}/");
    let mut directories = std::collections::BTreeSet::new();
    for item in downloads {
        if !item.file.path.starts_with(&prefix) {
            continue;
        }
        let mut current = item.file.path.as_str();
        while let Some((parent, _)) = current.rsplit_once('/') {
            if parent.len() < backup_path.len() {
                break;
            }
            directories.insert(parent.to_string());
            if parent == backup_path {
                break;
            }
            current = parent;
        }
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| left.cmp(right))
    });
    for directory in directories {
        let _ = remove_empty_directory_no_follow(anchors, &directory).await?;
    }
    let _ = remove_empty_directory_no_follow(anchors, backup_path).await?;
    Ok(())
}

#[cfg(unix)]
fn publish_staged_no_follow_blocking(
    base: &Path,
    stage: &Path,
    relative: &str,
) -> std::io::Result<(
    std::fs::File,
    Vec<std::fs::File>,
    Vec<CreatedMaterializationDirectory>,
)> {
    use std::os::fd::AsRawFd as _;

    let anchors = open_materialization_anchors_blocking(base, stage)?;
    let new_name = portable_component("new")?;
    let mut source_parent = open_directory_at(&anchors.stage, &new_name, false)?;
    let mut destination_chain = vec![anchors.base];
    let mut created_directories = Vec::new();
    let result = (|| {
        let mut components = relative.split('/').peekable();
        let mut final_name = None;
        let mut prefix = String::new();
        while let Some(component_text) = components.next() {
            let component = portable_component(component_text)?;
            if components.peek().is_none() {
                final_name = Some(component);
                break;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component_text);
            source_parent = open_directory_at(&source_parent, &component, false)?;
            let destination_parent = destination_chain
                .last()
                .ok_or_else(|| std::io::Error::other("missing destination directory"))?;
            let (directory, created) =
                open_directory_at_tracked(destination_parent, &component, true).map_err(
                    |error| {
                        std::io::Error::new(
                            error.kind(),
                            format!("no-follow destination ancestor rejected: {error}"),
                        )
                    },
                )?;
            if created {
                created_directories.push(CreatedMaterializationDirectory {
                    parent: destination_parent.try_clone()?,
                    directory: directory.try_clone()?,
                    name: component.clone(),
                    path: prefix.clone(),
                });
            }
            destination_chain.push(directory);
        }
        let final_name = final_name
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty path"))?;
        let source = open_regular_at(&source_parent, &final_name, false)?;
        let destination_parent = destination_chain
            .last()
            .ok_or_else(|| std::io::Error::other("missing destination directory"))?;

        // SAFETY: both descriptors and the NUL-terminated component remain valid.
        if unsafe {
            libc::linkat(
                source_parent.as_raw_fd(),
                final_name.as_ptr(),
                destination_parent.as_raw_fd(),
                final_name.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let destination = match open_regular_at(destination_parent, &final_name, true) {
            Ok(file) => file,
            Err(error) => {
                // SAFETY: removes only the entry in the opened destination directory.
                unsafe {
                    libc::unlinkat(destination_parent.as_raw_fd(), final_name.as_ptr(), 0);
                }
                return Err(error);
            }
        };
        if !same_open_file(&source, &destination)? {
            // SAFETY: same anchored destination as above.
            unsafe {
                libc::unlinkat(destination_parent.as_raw_fd(), final_name.as_ptr(), 0);
            }
            return Err(std::io::Error::other(
                "staged download changed during no-follow publication",
            ));
        }
        Ok(destination)
    })();
    match result {
        Ok(destination) => Ok((destination, destination_chain, created_directories)),
        Err(error) => {
            let _ = remove_created_directories_at(&created_directories);
            Err(error)
        }
    }
}

#[cfg(unix)]
async fn publish_staged_no_follow(
    base: &Path,
    stage: &Path,
    relative: &str,
) -> Result<(
    std::fs::File,
    Vec<std::fs::File>,
    Vec<CreatedMaterializationDirectory>,
)> {
    let base = base.to_path_buf();
    let stage = stage.to_path_buf();
    let relative = relative.to_string();
    let display_relative = relative.clone();
    tokio::task::spawn_blocking(move || publish_staged_no_follow_blocking(&base, &stage, &relative))
        .await
        .context("join no-follow publication task")?
        .with_context(|| format!("publish staged download {display_relative}"))
}

#[cfg(unix)]
async fn sync_published_directories(published: &PublishedDownload) -> Result<()> {
    let directories = published
        .directory_chain
        .iter()
        .map(std::fs::File::try_clone)
        .collect::<std::io::Result<Vec<_>>>()?;
    tokio::task::spawn_blocking(move || {
        for directory in directories.iter().rev() {
            directory.sync_all()?;
        }
        Ok::<_, std::io::Error>(())
    })
    .await
    .context("join published-directory sync task")??;
    Ok(())
}

#[cfg(unix)]
async fn apply_materialized_file_state(
    file: &std::fs::File,
    mode: u32,
    readonly: bool,
) -> Result<i64> {
    let file = file.try_clone()?;
    tokio::task::spawn_blocking(move || -> Result<i64> {
        use std::os::unix::fs::PermissionsExt as _;

        let metadata = file.metadata()?;
        let mut permissions = metadata.permissions();
        let current = permissions.mode();
        let updated = if mode == feanorfs_common::EXECUTABLE_MODE {
            current | 0o111
        } else {
            current & !0o111
        };
        permissions.set_mode(updated);
        if readonly {
            permissions.set_readonly(true);
        }
        file.set_permissions(permissions)?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        Ok(metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(0))
    })
    .await
    .context("join materialized-file sync task")?
}

async fn remove_materialization_stage(stage: &Path, base: &Path) -> Result<()> {
    fs::remove_dir_all(stage)
        .await
        .with_context(|| format!("remove materialization stage {}", stage.display()))?;
    sync_directory(base).await
}

async fn write_materialization_journal(
    stage: &Path,
    journal: &MaterializationJournal,
) -> Result<()> {
    let bytes = serde_json::to_vec(journal)?;
    if bytes.len() > MAX_MATERIALIZATION_JOURNAL_BYTES {
        anyhow::bail!("materialization journal exceeds bounded size");
    }
    atomic_write(stage, "journal.json", &bytes).await?;
    sync_directory(stage).await
}

async fn read_materialization_journal(stage: &Path) -> Result<MaterializationJournal> {
    use tokio::io::AsyncReadExt as _;

    let path = stage.join("journal.json");
    let metadata = fs::metadata(&path).await?;
    if metadata.len() > MAX_MATERIALIZATION_JOURNAL_BYTES as u64 {
        anyhow::bail!("materialization journal exceeds bounded size");
    }
    let file = fs::File::open(&path).await?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_MATERIALIZATION_JOURNAL_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_MATERIALIZATION_JOURNAL_BYTES {
        anyhow::bail!("materialization journal exceeds bounded size");
    }
    let journal: MaterializationJournal = serde_json::from_slice(&bytes)?;
    let download_paths = journal
        .downloads
        .iter()
        .map(|item| item.file.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut published_path_set = std::collections::BTreeSet::new();
    let published_paths_valid = journal.published_paths.len()
        <= MAX_MATERIALIZATION_DIRECTORY_PROOFS
        && journal.published_paths.iter().all(|path| {
            is_safe_rel_path(path)
                && download_paths.contains(path.as_str())
                && published_path_set.insert(path.as_str())
        });
    let publishing_path_valid = journal.publishing_path.as_ref().is_none_or(|path| {
        is_safe_rel_path(path)
            && download_paths.contains(path.as_str())
            && !published_path_set.contains(path.as_str())
    });
    let publication_progress_valid = if journal.publication_progress_recorded {
        published_paths_valid && publishing_path_valid
    } else {
        journal.published_paths.is_empty() && journal.publishing_path.is_none()
    };
    let created_directories_valid = journal.created_directories.len()
        <= MAX_MATERIALIZATION_DIRECTORY_PROOFS
        && journal.created_directories.iter().all(|proof| {
            let prefix = format!("{}/", proof.path);
            is_safe_rel_path(&proof.path)
                && proof
                    .identity
                    .as_ref()
                    .is_none_or(|identity| identity.file_index != 0)
                && journal
                    .downloads
                    .iter()
                    .any(|item| item.file.path.starts_with(&prefix))
        });
    if journal.original_paths.len() > feanorfs_common::MAX_TREE_OUTPUT_PATHS
        || journal.downloads.len() > feanorfs_common::MAX_TREE_OUTPUT_PATHS
        || journal.delete_paths.len() > feanorfs_common::MAX_TREE_OUTPUT_PATHS
        || !publication_progress_valid
        || journal.created_directories.len() > MAX_MATERIALIZATION_DIRECTORY_PROOFS
        || !journal
            .original_paths
            .iter()
            .all(|path| is_safe_rel_path(path))
        || !journal
            .downloads
            .iter()
            .all(|item| is_safe_rel_path(&item.file.path))
        || !journal
            .delete_paths
            .iter()
            .all(|path| is_safe_rel_path(path))
        || !created_directories_valid
    {
        anyhow::bail!("materialization journal contains invalid or excessive paths");
    }
    Ok(journal)
}

async fn recover_materialization_stages(ctx: &SyncCtx<'_>) -> Result<bool> {
    let mut entries = fs::read_dir(ctx.base).await?;
    let mut stages = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(".feanorfs-tmp-materialize-") && entry.file_type().await?.is_dir() {
            stages.push(entry.path());
            if stages.len() > MAX_MATERIALIZATION_STAGES {
                anyhow::bail!("too many interrupted materialization stages");
            }
        }
    }
    stages.sort();
    let recovered = !stages.is_empty();
    for stage in stages {
        let journal = match read_materialization_journal(&stage).await {
            Ok(journal) => journal,
            Err(error) => {
                quarantine_unreadable_materialization_stage(ctx, &stage, &error).await?;
                continue;
            }
        };
        match journal.phase.as_str() {
            "preparing" => {}
            "activating" => recover_activating_materialization(ctx, &stage, &journal).await?,
            "activated" => recover_activated_materialization(ctx, &stage, &journal).await?,
            phase => anyhow::bail!("unknown materialization journal phase {phase:?}"),
        }
        remove_materialization_stage(&stage, ctx.base)
            .await
            .with_context(|| format!("remove recovered materialization {}", stage.display()))?;
    }
    Ok(recovered)
}

async fn quarantine_unreadable_materialization_stage(
    ctx: &SyncCtx<'_>,
    stage: &Path,
    error: &anyhow::Error,
) -> Result<()> {
    let mut entries = fs::read_dir(stage).await?;
    if entries.next_entry().await?.is_none() {
        remove_materialization_stage(stage, ctx.base).await?;
        tracing::warn!(
            "removed empty journal-less materialization stage {}: {error:#}",
            stage.display()
        );
        return Ok(());
    }
    // `backup/` is created only when activation is about to move an original
    // worktree file. Without a readable journal those are the sole recovery
    // copies, so never hide the stage or let a later sync proceed over it.
    if fs::symlink_metadata(stage.join("backup")).await.is_ok() {
        return Err(anyhow::anyhow!("{error:#}")).with_context(|| {
            format!(
                "materialization recovery journal is unreadable and backups remain at {}",
                stage.display()
            )
        });
    }
    // A journal-less preparing/new-only stage has not removed an original.
    // Move it out of the active recovery namespace instead of deleting unknown
    // bytes; the walker still excludes the `.feanorfs-tmp-` quarantine name.
    let name = stage
        .file_name()
        .and_then(|name| name.to_str())
        .context("materialization stage name is not UTF-8")?;
    let suffix = name
        .strip_prefix(".feanorfs-tmp-materialize-")
        .context("invalid materialization stage name")?;
    let quarantine = ctx
        .base
        .join(format!(".feanorfs-tmp-recovery-materialize-{suffix}"));
    match fs::symlink_metadata(&quarantine).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => anyhow::bail!("materialization quarantine destination already exists"),
        Err(error) => return Err(error).context("inspect materialization quarantine destination"),
    }
    fs::rename(stage, &quarantine).await.with_context(|| {
        format!(
            "quarantine unreadable materialization stage {}",
            stage.display()
        )
    })?;
    sync_directory(ctx.base).await?;
    tracing::error!(
        "quarantined unreadable materialization preparation at {}: {error:#}",
        quarantine.display()
    );
    Ok(())
}

async fn recover_activating_materialization(
    ctx: &SyncCtx<'_>,
    stage: &Path,
    journal: &MaterializationJournal,
) -> Result<()> {
    #[cfg(unix)]
    {
        let anchors = open_materialization_anchors(ctx.base, stage).await?;
        for item in journal.downloads.iter().rev() {
            validate_worktree_ancestors(ctx.base, &item.file.path).await?;
            if verify_materialized_destination(ctx, item, false)
                .await
                .is_err()
            {
                continue;
            }
            if !remove_recovered_publication_no_follow(&anchors, &item.file.path).await? {
                let _ = remove_current_regular_no_follow(&anchors, &item.file.path).await?;
            }
        }
        for path in journal.original_paths.iter().rev() {
            let mut state = inspect_backup_recovery(&anchors, path).await?;
            if state == BackupRecoveryState::Missing
                || state == BackupRecoveryState::AlreadyRestored
            {
                continue;
            }
            if state == BackupRecoveryState::DestinationOccupied {
                remove_created_descendants_for_backup(&anchors, path, &journal.downloads).await?;
                state = inspect_backup_recovery(&anchors, path).await?;
            }
            match state {
                BackupRecoveryState::Missing | BackupRecoveryState::AlreadyRestored => {}
                BackupRecoveryState::DestinationMissing => {
                    let restored = restore_backup_no_follow(&anchors, path).await?;
                    restored.sync_all()?;
                }
                BackupRecoveryState::DestinationOccupied => anyhow::bail!(
                    "local path {path} changed during interrupted materialization recovery"
                ),
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let recreate_ancestors =
            derive_windows_restore_ancestors(&journal.original_paths, &journal.downloads)?;
        let has_publication_progress = journal.publication_progress_recorded;
        for item in journal.downloads.iter().rev() {
            let staged = stage.join("new").join(&item.file.path);
            let destination = ctx.base.join(&item.file.path);
            let staged_exists = match fs::symlink_metadata(&staged).await {
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            let backup_exists =
                windows_backup_exists_for_download(stage, &item.file.path, &journal.original_paths)
                    .await?;
            let destination_exists = match fs::symlink_metadata(&destination).await {
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            if has_publication_progress {
                let published = journal
                    .published_paths
                    .iter()
                    .any(|path| path == &item.file.path);
                let publishing =
                    journal.publishing_path.as_deref() == Some(item.file.path.as_str());
                if published || publishing {
                    if !staged_exists || !destination_exists {
                        anyhow::bail!(
                            "interrupted materialization {} lost a recorded publication; refusing automatic recovery",
                            item.file.path
                        );
                    }
                    let removed = remove_verified_published_file_async(
                        &staged,
                        &destination,
                        item,
                        ctx.password_str(),
                    )
                    .await
                    .with_context(|| {
                        format!("remove interrupted materialization {}", item.file.path)
                    })?;
                    if !removed {
                        anyhow::bail!(
                            "interrupted materialization {} changed; refusing automatic recovery",
                            item.file.path
                        );
                    }
                    if let Some(parent) = destination.parent() {
                        let _ = sync_directory(parent).await;
                    }
                    continue;
                }

                // No durable publication progress exists for this item. A
                // staged copy with no destination is definitively pending;
                // leave it for the original-path backup restore below. Any
                // other missing-name combination with a backup is ambiguous.
                if staged_exists && !destination_exists {
                    continue;
                }
                if (!staged_exists || !destination_exists) && backup_exists {
                    anyhow::bail!(
                        "interrupted materialization {} is missing its staged publication; refusing automatic recovery",
                        item.file.path
                    );
                }
                continue;
            }

            // Journals written before per-item publication progress retain the
            // conservative identity/content heuristic. In particular, a
            // backup with both publication names absent remains ambiguous.
            match classify_windows_publication_recovery(
                staged_exists,
                destination_exists,
                backup_exists,
            ) {
                WindowsPublicationRecovery::NotPublished => continue,
                WindowsPublicationRecovery::Ambiguous => {
                    anyhow::bail!(
                        "interrupted materialization {} is missing its staged publication; refusing automatic recovery",
                        item.file.path
                    );
                }
                WindowsPublicationRecovery::Published => {}
            }
            let removed = remove_verified_published_file_async(
                &staged,
                &destination,
                item,
                ctx.password_str(),
            )
            .await
            .with_context(|| format!("remove interrupted materialization {}", item.file.path))?;
            if !removed {
                if backup_exists {
                    anyhow::bail!(
                        "interrupted materialization {} changed; refusing automatic recovery",
                        item.file.path
                    );
                }
                continue;
            }
            if let Some(parent) = destination.parent() {
                let _ = sync_directory(parent).await;
            }
        }
        // Remove only the exact transaction-created directories before trying
        // to restore backups. A user-created/replaced directory is skipped;
        // restore then fails closed rather than recursively deleting it.
        cleanup_materialization_directories(ctx.base, &journal.created_directories).await?;
        for path in journal.original_paths.iter().rev() {
            let readonly = journal
                .original_readonly
                .get(path)
                .copied()
                .unwrap_or(false);
            restore_windows_backup(ctx.base, stage, path, readonly, &recreate_ancestors)
                .await
                .with_context(|| format!("restore interrupted materialization {path}"))?;
        }
        Ok(())
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        for item in journal.downloads.iter().rev() {
            validate_worktree_ancestors(ctx.base, &item.file.path).await?;
            let staged = stage.join("new").join(&item.file.path);
            let destination = ctx.base.join(&item.file.path);
            let staged_exists = fs::symlink_metadata(&staged).await.is_ok();
            let destination_exists = match fs::symlink_metadata(&destination).await {
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            if !destination_exists {
                continue;
            }

            let backup_exists = fs::symlink_metadata(stage.join("backup").join(&item.file.path))
                .await
                .is_ok();
            let published = if staged_exists {
                let same_identity = same_file_identity(&staged, &destination).await?;
                same_identity
                    && verify_materialized_destination(ctx, item, false)
                        .await
                        .is_ok()
            } else {
                verify_materialized_destination(ctx, item, false)
                    .await
                    .is_ok()
            };
            if !published {
                if backup_exists {
                    anyhow::bail!(
                        "interrupted materialization {} changed; refusing automatic recovery",
                        item.file.path
                    );
                }
                // A completed rollback may leave the original destination in place
                // if deleting the now-disposable stage failed. Preserve it.
                continue;
            }
            fs::remove_file(&destination).await.with_context(|| {
                format!("remove interrupted materialization {}", item.file.path)
            })?;
            if let Some(parent) = destination.parent() {
                sync_directory(parent).await?;
            }
        }
        for path in journal.original_paths.iter().rev() {
            let backup = stage.join("backup").join(path);
            let original = ctx.base.join(path);
            let backup_exists = match fs::symlink_metadata(&backup).await {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => true,
                Ok(_) => anyhow::bail!("materialization backup {path} changed type"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            if !backup_exists {
                continue;
            }
            if fs::symlink_metadata(&original).await.is_ok()
                && same_file_identity(&backup, &original).await?
            {
                fs::remove_file(&backup).await?;
                if let Some(parent) = backup.parent() {
                    sync_directory(parent).await?;
                }
                continue;
            }
            match fs::symlink_metadata(&original).await {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    remove_empty_destination_tree(&original).await?;
                }
                Ok(_) => anyhow::bail!(
                    "local path {path} changed during interrupted materialization recovery"
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(parent) = original.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::hard_link(&backup, &original)
                .await
                .with_context(|| format!("restore interrupted materialization {path}"))?;
            if let Some(parent) = original.parent() {
                sync_directory_chain(parent, ctx.base).await?;
            }
            fs::remove_file(&backup).await?;
            if let Some(parent) = backup.parent() {
                sync_directory(parent).await?;
            }
            sync_file(&original).await?;
        }
        Ok(())
    }
}

#[cfg(all(not(unix), not(windows)))]
async fn same_file_identity(left: &Path, right: &Path) -> Result<bool> {
    let left = left.to_path_buf();
    let right = right.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let left = open_regular_no_follow_absolute(&left)?;
        let right = open_regular_no_follow_absolute(&right)?;
        // On Windows this compares volume serial plus file index from the
        // opened handles, never content, mtime, or a pathname.
        same_open_file_identity(&left, &right)
    })
    .await
    .context("join materialization file identity check")?
}

async fn recover_activated_materialization(
    ctx: &SyncCtx<'_>,
    _stage: &Path,
    journal: &MaterializationJournal,
) -> Result<()> {
    let mut cache_entries = Vec::with_capacity(journal.downloads.len());
    for item in &journal.downloads {
        verify_recovered_destination(ctx, item).await?;
        let destination = ctx.base.join(&item.file.path);
        let actual_mtime = file_mtime_ms(&destination).await.unwrap_or(item.file.mtime);
        cache_entries.push(CacheEntry {
            path: item.file.path.clone(),
            plaintext_hash: item.plaintext_hash.clone(),
            encrypted_hash: item.file.hash.clone(),
            size: item.file.size,
            mtime: actual_mtime,
            server_mtime: item.file.mtime,
            mode: item.file.mode,
            hydrated: item.hydrated,
            deleted_at: None,
        });
    }
    ctx.db
        .apply_cache_changes(&cache_entries, &journal.delete_paths)
        .await?;
    for entry in &cache_entries {
        crate::upload_registry::remember(&ctx.state_dir()?, &entry.encrypted_hash);
    }
    Ok(())
}

async fn verify_recovered_destination(ctx: &SyncCtx<'_>, item: &JournalDownload) -> Result<()> {
    verify_materialized_destination(ctx, item, true).await
}

async fn verify_materialized_destination(
    ctx: &SyncCtx<'_>,
    item: &JournalDownload,
    check_mode: bool,
) -> Result<()> {
    let destination = ctx.base.join(&item.file.path);
    if !item.hydrated {
        let metadata = fs::symlink_metadata(&destination).await?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != 0
            || (check_mode
                && !materialization_mode_matches(portable_mode(&metadata), item.file.mode))
            || (check_mode && !metadata.permissions().readonly())
        {
            anyhow::bail!(
                "interrupted placeholder {} changed; refusing automatic recovery",
                item.file.path
            );
        }
        return Ok(());
    }
    let actual = fingerprint_local_path(ctx, &item.file.path, item.file.size).await?;
    if actual.hash != item.file.hash
        || actual.size != item.file.size
        || (check_mode && !materialization_mode_matches(actual.mode, item.file.mode))
    {
        anyhow::bail!(
            "interrupted materialization {} changed; refusing automatic recovery",
            item.file.path
        );
    }
    Ok(())
}

async fn activate_prepared_downloads(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
    placeholder_anchors: &HashMap<String, std::fs::File>,
    mut prepared: Vec<PreparedDownload>,
    stage: &Path,
) -> Result<(u32, u32)> {
    use std::collections::{BTreeMap, BTreeSet};

    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    prepared.sort_by(|left, right| left.file.path.cmp(&right.file.path));
    let mut original_path_set = response
        .delete_local
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    original_path_set.extend(prepared.iter().map(|item| item.file.path.clone()));
    let mut original_paths = original_path_set.iter().cloned().collect::<Vec<_>>();
    original_paths.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| left.cmp(right))
    });

    ensure_same_device(ctx.base, stage, &original_paths, &prepared).await?;
    let mut original_readonly = BTreeMap::new();
    for path in &original_paths {
        if let Ok(metadata) = fs::symlink_metadata(ctx.base.join(path)).await {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                original_readonly.insert(path.clone(), metadata.permissions().readonly());
            }
        }
    }
    let mut journal = MaterializationJournal {
        phase: "activating".to_string(),
        publication_progress_recorded: true,
        original_paths: original_paths.clone(),
        original_readonly,
        downloads: prepared
            .iter()
            .map(|item| JournalDownload {
                file: item.file.clone(),
                plaintext_hash: item.plaintext_hash.clone(),
                hydrated: item.hydrated,
            })
            .collect(),
        delete_paths: response.delete_local.clone(),
        published_paths: Vec::new(),
        publishing_path: None,
        created_directories: Vec::new(),
    };
    write_materialization_journal(stage, &journal).await?;
    let backup_root = stage.join("backup");
    let anchors = open_materialization_anchors(ctx.base, stage).await?;
    let mut backups = Vec::new();
    let mut published = Vec::new();
    let activation: Result<(Vec<CacheEntry>, u32, u32)> = async {
        for path in &original_paths {
            let original = ctx.base.join(path);
            let scheduled_file_ancestor = path
                .match_indices('/')
                .map(|(index, _)| &path[..index])
                .any(|ancestor| {
                    original_path_set.contains(ancestor)
                        && local_files
                            .get(ancestor)
                            .is_some_and(|state| !state.deleted)
                });
            if !scheduled_file_ancestor {
                validate_worktree_ancestors(ctx.base, path).await?;
            }
            revalidate_materialization_input_at(
                ctx,
                path,
                &original,
                local_files.get(path),
                cached_entries,
                placeholder_anchors.get(path),
                &read_root,
            )
            .await?;
            let metadata = match fs::symlink_metadata(&original).await {
                Ok(metadata) => metadata,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            };
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                continue;
            }
            let backup = backup_root.join(path);
            #[cfg(not(unix))]
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent).await?;
            }
            #[cfg(not(unix))]
            let readonly = journal
                .original_readonly
                .get(path)
                .copied()
                .unwrap_or(false);
            #[cfg(windows)]
            if readonly {
                set_readonly(&original, false).await?;
            }
            if !scheduled_file_ancestor {
                validate_worktree_ancestors(ctx.base, path).await?;
            }
            let point = format!("before-backup-{}", backups.len() + 1);
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            #[cfg(unix)]
            let (original_parent, backup_parent) =
                backup_original_no_follow(&anchors, path).await?;
            #[cfg(not(unix))]
            if let Err(error) = fs::rename(&original, &backup).await {
                #[cfg(windows)]
                if readonly {
                    let _ = set_readonly(&original, true).await;
                }
                return Err(error).with_context(|| format!("back up local path {path}"));
            }
            backups.push(MaterializationBackup {
                path: path.clone(),
                #[cfg(not(unix))]
                original,
                #[cfg(not(unix))]
                backup: backup.clone(),
                #[cfg(not(unix))]
                readonly,
                #[cfg(unix)]
                original_parent,
                #[cfg(unix)]
                backup_parent,
            });
            // Record the namespace mutation before any fallible durability
            // step so rollback still owns the moved original if fsync fails.
            let mutation_point = format!("after-backup-mutation-{}", backups.len());
            pause_sync_test(ctx, &mutation_point).await?;
            inject_materialization_failure(ctx, &mutation_point).await?;
            #[cfg(unix)]
            sync_backup_directories(backups.last().context("missing backup state")?).await?;
            #[cfg(not(unix))]
            if let Some(parent) = backups.last().and_then(|item| item.original.parent()) {
                sync_directory(parent).await?;
            }
            #[cfg(not(unix))]
            if let Some(parent) = backup.parent() {
                sync_directory_chain(parent, stage).await?;
            }
            #[cfg(windows)]
            if readonly {
                set_readonly(&backup, true).await?;
            }
            // Verify the file after the atomic move. An edit racing the
            // pre-move fingerprint is now preserved in the backup and aborts
            // the transaction rather than being silently discarded.
            revalidate_materialization_input_at(
                ctx,
                path,
                &backup,
                local_files.get(path),
                cached_entries,
                placeholder_anchors.get(path),
                &read_root,
            )
            .await?;
            let point = format!("after-backup-{}", backups.len());
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
        }

        let mut cache_entries = Vec::with_capacity(prepared.len());
        let mut downloads = 0_u32;
        let mut placeholders = 0_u32;
        for item in prepared {
            let path = &item.file.path;
            let destination = ctx.base.join(path);
            let point = format!("before-publish-{}", published.len() + 1);
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            validate_worktree_ancestors(ctx.base, path).await?;
            remove_empty_destination_tree(&destination).await?;
            validate_worktree_ancestors(ctx.base, path).await?;
            match fs::symlink_metadata(&destination).await {
                Ok(_) => anyhow::bail!(
                    "local path {path} appeared while a staged download was being published"
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            let point = format!("after-final-validation-{}", published.len() + 1);
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            let expected = JournalDownload {
                file: item.file.clone(),
                plaintext_hash: item.plaintext_hash.clone(),
                hydrated: item.hydrated,
            };
            // Publish relative to already-open, no-follow directory handles on
            // Unix. This prevents an ancestor swap from redirecting the link
            // or subsequent mode/fsync operations outside the workspace.
            #[cfg(unix)]
            let (published_file, published_directories, created_directories) =
                publish_staged_no_follow(ctx.base, stage, path).await?;
            #[cfg(not(unix))]
            let publication_created_directories = {
                // A same-device hard link is an atomic no-clobber publication.
                // Unlike rename it cannot replace a path created after the
                // preceding absence check.
                let created_directories =
                    ensure_materialization_ancestors(ctx.base, path, stage, &mut journal).await?;
                #[cfg(windows)]
                {
                    // Persist the in-flight marker before the namespace
                    // mutation. A crash after this write but before the hard
                    // link must be treated as ambiguous, never guessed as a
                    // completed publication.
                    journal.publishing_path = Some(path.clone());
                    write_materialization_journal(stage, &journal).await?;
                }
                fs::hard_link(&item.staged_path, &destination)
                    .await
                    .with_context(|| format!("publish staged download {path}"))?;
                created_directories
            };
            #[cfg(windows)]
            let published_file = match open_verified_published_file_async(
                &item.staged_path,
                &destination,
                &expected,
                ctx.password_str(),
            )
            .await
            {
                Ok(file) => file,
                Err(error) => {
                    // Publication already created a hard link, but no
                    // ownership record can be constructed without a
                    // validated destination handle. Keep the journal/stage
                    // for handle-based crash recovery instead of allowing
                    // the ordinary rollback to discard its only proof.
                    return Err(preserve_materialization_stage(
                        error.context(format!("verify published download {path}")),
                    ));
                }
            };
            published.push(PublishedDownload {
                destination: destination.clone(),
                expected,
                mode_applied: false,
                #[cfg(unix)]
                file: published_file,
                #[cfg(windows)]
                file: published_file,
                #[cfg(unix)]
                directory_chain: published_directories,
                #[cfg(unix)]
                created_directories,
                #[cfg(not(unix))]
                created_directories: publication_created_directories,
            });
            #[cfg(windows)]
            {
                // The handle proof is now retained in `published`; record
                // the completed namespace mutation and clear the in-flight
                // marker. If this write fails, ordinary rollback still owns
                // the exact destination handle and the journal remains
                // conservatively marked as publishing for crash recovery.
                journal.published_paths.push(path.clone());
                journal.publishing_path = None;
                write_materialization_journal(stage, &journal).await?;
            }
            // Rollback ownership begins at the namespace mutation, not after
            // the following directory durability step succeeds.
            let mutation_point = format!("after-publish-mutation-{}", published.len());
            pause_sync_test(ctx, &mutation_point).await?;
            inject_materialization_failure(ctx, &mutation_point).await?;
            #[cfg(unix)]
            sync_published_directories(published.last().context("missing publication state")?)
                .await?;
            #[cfg(not(unix))]
            if let Some(parent) = destination.parent() {
                sync_directory_chain(parent, ctx.base).await?;
            }
            // Keep the staged hard link until the cache commits and the whole
            // stage is durably removed. Crash recovery can therefore identify
            // the exact published inode instead of trusting content equality.
            let point = format!("after-publish-{}", published.len());
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            #[cfg(unix)]
            let actual_mtime = apply_materialized_file_state(
                &published.last().context("missing publication state")?.file,
                item.file.mode,
                !item.hydrated,
            )
            .await?;
            #[cfg(not(unix))]
            {
                apply_executable_mode(&destination, item.file.mode).await?;
            }
            if let Some(published) = published.last_mut() {
                published.mode_applied = true;
            }
            let point = format!("after-mode-{}", published.len());
            pause_sync_test(ctx, &point).await?;
            inject_materialization_failure(ctx, &point).await?;
            if !item.hydrated {
                placeholders += 1;
            } else {
                downloads += 1;
            }
            #[cfg(not(unix))]
            sync_file(&destination).await?;
            if !item.hydrated {
                #[cfg(not(unix))]
                set_readonly(&destination, true).await?;
            }
            #[cfg(not(unix))]
            let actual_mtime = file_mtime_ms(&destination).await.unwrap_or_else(|error| {
                tracing::warn!(
                    "failed to read mtime after download of {path}: {error}; using server mtime"
                );
                item.file.mtime
            });

            cache_entries.push(CacheEntry {
                path: path.clone(),
                plaintext_hash: item.plaintext_hash,
                encrypted_hash: item.file.hash.clone(),
                size: item.file.size,
                mtime: actual_mtime,
                server_mtime: item.file.mtime,
                mode: item.file.mode,
                hydrated: item.hydrated,
                deleted_at: None,
            });
        }
        Ok((cache_entries, downloads, placeholders))
    }
    .await;

    let (cache_entries, downloads, placeholders) = match activation {
        Ok(value) => value,
        Err(error) => {
            if let Err(rollback) = rollback_materialization(
                ctx,
                &anchors,
                stage,
                &journal.original_paths,
                published,
                &backups,
                &journal.downloads,
                &journal.created_directories,
            )
            .await
            {
                return Err(preserve_materialization_stage(
                    error.context(format!("materialization rollback failed: {rollback:#}")),
                ));
            }
            return Err(error);
        }
    };
    journal.phase = "activated".to_string();
    if let Err(error) = write_materialization_journal(stage, &journal).await {
        if let Err(rollback) = rollback_materialization(
            ctx,
            &anchors,
            stage,
            &journal.original_paths,
            published,
            &backups,
            &journal.downloads,
            &journal.created_directories,
        )
        .await
        {
            return Err(preserve_materialization_stage(error.context(format!(
                "journal update and materialization rollback failed: {rollback:#}"
            ))));
        }
        return Err(error).context("mark materialization activated");
    }
    let before_cache = match pause_sync_test(ctx, "before-cache").await {
        Ok(()) => inject_materialization_failure(ctx, "before-cache").await,
        Err(error) => Err(error),
    };
    if let Err(error) = before_cache {
        if let Err(rollback) = rollback_materialization(
            ctx,
            &anchors,
            stage,
            &journal.original_paths,
            published,
            &backups,
            &journal.downloads,
            &journal.created_directories,
        )
        .await
        {
            return Err(preserve_materialization_stage(
                error.context(format!("materialization rollback failed: {rollback:#}")),
            ));
        }
        return Err(error);
    }
    if let Err(error) = ctx
        .db
        .apply_cache_changes(&cache_entries, &response.delete_local)
        .await
    {
        if crate::durable::commit_durability_is_uncertain(&error) {
            return Err(preserve_materialization_stage(
                error.context("commit materialized cache state"),
            ));
        }
        if let Err(rollback) = rollback_materialization(
            ctx,
            &anchors,
            stage,
            &journal.original_paths,
            published,
            &backups,
            &journal.downloads,
            &journal.created_directories,
        )
        .await
        {
            return Err(preserve_materialization_stage(error.context(format!(
                "cache commit and materialization rollback failed: {rollback:#}"
            ))));
        }
        return Err(error).context("commit materialized cache state");
    }
    for entry in &cache_entries {
        crate::upload_registry::remember(&ctx.state_dir()?, &entry.encrypted_hash);
    }
    Ok((downloads, placeholders))
}

async fn inject_materialization_failure(ctx: &SyncCtx<'_>, point: &str) -> Result<()> {
    #[cfg(debug_assertions)]
    {
        let path = ctx.state_dir()?.join("test-materialize-failpoint");
        if fs::read_to_string(&path).await.ok().as_deref() == Some(point) {
            fs::remove_file(path).await?;
            anyhow::bail!("injected materialization failure at {point}");
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = (ctx, point);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn rollback_materialization(
    ctx: &SyncCtx<'_>,
    anchors: &MaterializationAnchors,
    stage: &Path,
    original_paths: &[String],
    published: Vec<PublishedDownload>,
    backups: &[MaterializationBackup],
    downloads: &[JournalDownload],
    created_directories: &[MaterializationDirectoryProof],
) -> Result<()> {
    #[cfg(not(windows))]
    let _ = (stage, original_paths);
    #[cfg(windows)]
    let recreate_ancestors = derive_windows_restore_ancestors(original_paths, downloads)?;
    for published in published.into_iter().rev() {
        #[cfg(not(windows))]
        validate_worktree_ancestors(ctx.base, &published.expected.file.path).await?;
        #[cfg(unix)]
        {
            match verify_materialized_destination(ctx, &published.expected, published.mode_applied)
                .await
            {
                Ok(()) => {}
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        )
                    }) =>
                {
                    remove_created_directories_at(&published.created_directories)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let parent = published
                .directory_chain
                .last()
                .context("missing anchored publication parent")?;
            let name = portable_component(
                published
                    .expected
                    .file
                    .path
                    .rsplit('/')
                    .next()
                    .context("empty materialization path")?,
            )?;
            unlink_regular_at(parent, &name, &published.file).with_context(|| {
                format!(
                    "remove materialized destination {} during rollback",
                    published.destination.display()
                )
            })?;
            remove_created_directories_at(&published.created_directories)?;
            continue;
        }
        #[cfg(windows)]
        {
            // The retained handle is the ownership proof.  Reopening or
            // deleting the destination pathname could target a user-created
            // replacement after publication, so verify and dispose the exact
            // published file handle instead.
            let created_directories = published.created_directories.clone();
            remove_retained_published_file_async(&published, ctx.password_str()).await?;
            // The retained publication handle keeps a delete-pending inode
            // alive on Windows. Close it before removing transaction-created
            // parent directories or restoring backups.
            drop(published.file);
            cleanup_materialization_directories(ctx.base, &created_directories).await?;
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            match fs::symlink_metadata(&published.destination).await {
                Ok(metadata) => {
                    if !metadata.is_file() || metadata.file_type().is_symlink() {
                        anyhow::bail!(
                            "local path {} changed during materialization rollback",
                            published.expected.file.path
                        );
                    }
                    verify_materialized_destination(
                        ctx,
                        &published.expected,
                        published.mode_applied,
                    )
                    .await?;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            }
            fs::remove_file(&published.destination)
                .await
                .with_context(|| {
                    format!(
                        "remove materialized destination {} during rollback",
                        published.destination.display()
                    )
                })?;
            if let Some(parent) = published.destination.parent() {
                sync_directory(parent).await?;
            }
        }
    }
    for item in backups.iter().rev() {
        #[cfg(unix)]
        {
            remove_created_descendants_for_backup(anchors, &item.path, downloads).await?;
            let restored = restore_backup_no_follow(anchors, &item.path).await?;
            restored.sync_all()?;
            continue;
        }
        #[cfg(windows)]
        {
            restore_windows_backup(
                ctx.base,
                stage,
                &item.path,
                item.readonly,
                &recreate_ancestors,
            )
            .await
            .with_context(|| {
                format!("restore materialization backup {}", item.original.display())
            })?;
            continue;
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let backup_metadata = match fs::symlink_metadata(&item.backup).await {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    metadata
                }
                Ok(_) => anyhow::bail!(
                    "materialization backup {} changed type",
                    item.backup.display()
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            };
            let _ = backup_metadata;
            match fs::symlink_metadata(&item.original).await {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    remove_empty_destination_tree(&item.original).await?;
                }
                Ok(_) => anyhow::bail!(
                    "local path {} changed during materialization rollback",
                    item.original.display()
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(parent) = item.original.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::hard_link(&item.backup, &item.original)
                .await
                .with_context(|| {
                    format!("restore materialization backup {}", item.original.display())
                })?;
            if let Some(parent) = item.original.parent() {
                sync_directory_chain(parent, ctx.base).await?;
            }
            fs::remove_file(&item.backup).await.with_context(|| {
                format!("retire materialization backup {}", item.backup.display())
            })?;
            if let Some(parent) = item.backup.parent() {
                sync_directory(parent).await?;
            }
            sync_file(&item.original).await?;
        }
    }
    #[cfg(windows)]
    cleanup_materialization_directories(ctx.base, created_directories).await?;
    #[cfg(all(not(unix), not(windows)))]
    cleanup_materialization_directories(ctx.base, created_directories).await?;
    #[cfg(unix)]
    let _ = (created_directories, downloads);
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
async fn cleanup_materialization_directories(
    base: &Path,
    created_directories: &[MaterializationDirectoryProof],
) -> Result<()> {
    for proof in created_directories.iter().rev() {
        let Some(identity) = proof.identity.as_ref() else {
            // Unsupported non-Unix platforms have no exact directory identity
            // primitive; retaining the directory is the fail-closed choice.
            continue;
        };
        let current = base.join(&proof.path);
        if capture_directory_identity(&current).await?.as_ref() != Some(identity) {
            // A user replacement, rename, or other identity change means this
            // path is no longer proven to be transaction-owned.
            continue;
        }
        match fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        match fs::remove_dir(&current).await {
            Ok(()) => {
                if let Some(parent) = current.parent() {
                    sync_directory(parent).await?;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty
                        | std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn ensure_same_device(
    base: &Path,
    stage: &Path,
    original_paths: &[String],
    prepared: &[PreparedDownload],
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let stage_device = fs::metadata(stage).await?.dev();
        for path in original_paths {
            match fs::symlink_metadata(base.join(path)).await {
                Ok(metadata) if metadata.dev() != stage_device => {
                    anyhow::bail!("materialization path {path} is on a different filesystem")
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        for item in prepared {
            let mut ancestor = base.join(&item.file.path);
            while !ancestor.exists() {
                if !ancestor.pop() {
                    anyhow::bail!("materialization destination escaped workspace");
                }
            }
            if fs::metadata(&ancestor).await?.dev() != stage_device {
                anyhow::bail!(
                    "materialization destination {} is on a different filesystem",
                    item.file.path
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (base, stage, original_paths, prepared);
    }
    Ok(())
}

fn capture_placeholder_anchors(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
) -> Result<HashMap<String, std::fs::File>> {
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    let mut anchors = HashMap::new();
    for path in response
        .download_required
        .iter()
        .map(|file| file.path.as_str())
        .chain(response.delete_local.iter().map(String::as_str))
    {
        let Some(expected) = local_files.get(path).filter(|state| !state.deleted) else {
            continue;
        };
        if !cached_entries
            .get(path)
            .is_some_and(|entry| !entry.hydrated && entry.encrypted_hash == expected.hash)
        {
            continue;
        }
        let file = read_root
            .open_regular(path)
            .with_context(|| format!("anchor local placeholder {path} before staging"))?;
        anchors.insert(path.to_string(), file);
    }
    Ok(anchors)
}

fn same_open_file_identity(left: &std::fs::File, right: &std::fs::File) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let left = left.metadata()?;
        let right = right.metadata()?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(windows)]
    {
        Ok(windows_file_identity(left)? == windows_file_identity(right)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Ok(false)
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &std::fs::File) -> Result<MaterializationDirectoryIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file owns a valid handle and the output points to writable
    // storage for the duration of this synchronous call.
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the successful call initialized the complete structure.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    anyhow::ensure!(file_index != 0, "Windows file identity has a zero index");
    Ok(MaterializationDirectoryIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index,
    })
}

#[cfg(windows)]
fn open_materialization_file_for_delete(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization path is not a non-reparse regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_materialization_directory_for_delete(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization path is not a non-reparse directory",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn delete_materialization_handle(file: &std::fs::File) -> std::io::Result<()> {
    delete_materialization_handle_with_options(file, false)
}

#[cfg(windows)]
fn delete_materialization_handle_with_options(
    file: &std::fs::File,
    force_basic_disposition: bool,
) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, FileDispositionInfoEx, SetFileInformationByHandle,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    };

    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    if !force_basic_disposition {
        // SAFETY: the handle is owned by `file`, and the disposition structure
        // is initialized for the duration of this synchronous system call.
        let result = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfoEx,
                (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        };
        if result != 0 {
            return Ok(());
        }
    }
    let extended_error = if force_basic_disposition {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "test-forced FileDispositionInfoEx fallback",
        )
    } else {
        std::io::Error::last_os_error()
    };
    let basic = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: same valid handle and initialized fallback structure. This
    // supports filesystems that do not implement FileDispositionInfoEx.
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&basic as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result != 0 {
        return Ok(());
    }
    let fallback_error = std::io::Error::last_os_error();
    Err(std::io::Error::new(
        fallback_error.kind(),
        format!("delete disposition failed ({extended_error}); fallback failed ({fallback_error})"),
    ))
}

#[cfg(windows)]
fn verify_materialized_handle(
    file: &mut std::fs::File,
    item: &JournalDownload,
    password: &str,
    check_mode: bool,
) -> Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        anyhow::bail!("materialization destination is not a non-reparse regular file");
    }
    if !item.hydrated {
        if metadata.len() != 0
            || (check_mode && !metadata.permissions().readonly())
            || (check_mode
                && !materialization_mode_matches(portable_mode(&metadata), item.file.mode))
        {
            anyhow::bail!(
                "interrupted placeholder {} changed; refusing automatic recovery",
                item.file.path
            );
        }
        return Ok(());
    }
    let encrypted_hash = if crate::large_file::uses_chunk_transport(item.file.size) {
        file.seek(SeekFrom::Start(0))?;
        crate::large_file::fingerprint_opened(file, password, &item.file.path)?.encrypted_hash
    } else {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(crate::large_file::CHUNK_THRESHOLD_BYTES as usize)
                .min(crate::large_file::CHUNK_THRESHOLD_BYTES as usize),
        );
        file.take(crate::large_file::CHUNK_THRESHOLD_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > crate::large_file::CHUNK_THRESHOLD_BYTES {
            anyhow::bail!("materialization destination grew while it was being checked");
        }
        seal(&bytes, password, &item.file.path)?.0
    };
    let after = file.metadata()?;
    if after.len() != metadata.len() {
        anyhow::bail!(
            "interrupted materialization {} changed while it was being checked",
            item.file.path
        );
    }
    if encrypted_hash != item.file.hash
        || after.len() != item.file.size
        || (check_mode && !materialization_mode_matches(portable_mode(&after), item.file.mode))
    {
        anyhow::bail!(
            "interrupted materialization {} changed; refusing automatic recovery",
            item.file.path
        );
    }
    Ok(())
}

#[cfg(windows)]
fn open_verified_published_file(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<std::fs::File> {
    let mut staged_file = open_materialization_file_for_delete(staged)
        .with_context(|| format!("open staged materialization {}", staged.display()))?;
    let mut destination_file = open_materialization_file_for_delete(destination)
        .with_context(|| format!("open published materialization {}", destination.display()))?;
    anyhow::ensure!(
        same_open_file_identity(&staged_file, &destination_file)?,
        "staged and published materializations do not share an identity"
    );
    verify_materialized_handle(&mut staged_file, item, password, false)?;
    verify_materialized_handle(&mut destination_file, item, password, false)?;
    Ok(destination_file)
}

#[cfg(windows)]
fn remove_verified_published_file(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<bool> {
    let mut staged_file = match open_materialization_file_for_delete(staged) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut destination_file = match open_materialization_file_for_delete(destination) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !same_open_file_identity(&staged_file, &destination_file)? {
        return Ok(false);
    }
    verify_materialized_handle(&mut staged_file, item, password, false)?;
    verify_materialized_handle(&mut destination_file, item, password, false)?;
    if destination_file.metadata()?.permissions().readonly() {
        set_readonly_materialization_handle(&destination_file, false)?;
    }
    delete_materialization_handle(&destination_file)?;
    Ok(true)
}

#[cfg(windows)]
async fn open_verified_published_file_async(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<std::fs::File> {
    let staged = staged.to_path_buf();
    let destination = destination.to_path_buf();
    let item = item.clone();
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        open_verified_published_file(&staged, &destination, &item, &password)
    })
    .await
    .context("join published materialization verification task")?
}

#[cfg(windows)]
async fn remove_verified_published_file_async(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<bool> {
    let staged = staged.to_path_buf();
    let destination = destination.to_path_buf();
    let item = item.clone();
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        remove_verified_published_file(&staged, &destination, &item, &password)
    })
    .await
    .context("join interrupted materialization removal task")?
}

#[cfg(windows)]
async fn remove_retained_published_file_async(
    published: &PublishedDownload,
    password: &str,
) -> Result<()> {
    let mut file = published.file.try_clone()?;
    let item = published.expected.clone();
    let password = password.to_owned();
    let check_mode = published.mode_applied;
    tokio::task::spawn_blocking(move || {
        verify_materialized_handle(&mut file, &item, &password, check_mode)?;
        if file.metadata()?.permissions().readonly() {
            set_readonly_materialization_handle(&file, false)?;
        }
        delete_materialization_handle(&file)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("join retained materialization removal task")??;
    Ok(())
}

#[cfg(windows)]
fn open_windows_restore_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    // Ancestor directory handles below are intentionally opened without
    // FILE_SHARE_DELETE to pin the namespace. File handles themselves must
    // share deletion because the destination may already be a hard link to
    // this same inode and Windows rejects a second open otherwise.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization restore source is not a non-reparse regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_restore_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let directory = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization restore ancestor is not a non-reparse directory",
        ));
    }
    Ok(directory)
}

#[cfg(windows)]
fn open_windows_restore_ancestors(
    root: &Path,
    relative: &str,
) -> std::io::Result<Vec<std::fs::File>> {
    open_windows_restore_ancestors_with_creation(root, relative, &[])
}

#[cfg(windows)]
fn open_windows_restore_ancestors_with_creation(
    root: &Path,
    relative: &str,
    recreate: &[String],
) -> std::io::Result<Vec<std::fs::File>> {
    if !relative.is_empty() {
        if !is_safe_rel_path(relative) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe restore ancestor path",
            ));
        }
    }
    for path in recreate {
        if !is_safe_rel_path(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe restore ancestor allowlist path",
            ));
        }
    }
    let mut current = root.to_path_buf();
    let mut chain = vec![open_windows_restore_directory(&current)?];
    let mut current_relative = String::new();
    for component in relative
        .split('/')
        .filter(|component| !component.is_empty())
    {
        if !current_relative.is_empty() {
            current_relative.push('/');
        }
        current_relative.push_str(component);
        current.push(component);
        let directory = match open_windows_restore_directory(&current) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !recreate.iter().any(|path| path == &current_relative) {
                    return Err(error);
                }
                match std::fs::create_dir(&current) {
                    Ok(()) => open_windows_restore_directory(&current)?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        // A concurrent creator owns this namespace entry. Reopen
                        // it with no-follow semantics and pin the resulting
                        // identity before continuing.
                        open_windows_restore_directory(&current)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        chain.push(directory);
    }
    Ok(chain)
}

#[cfg(windows)]
fn derive_windows_restore_ancestors(
    original_paths: &[String],
    downloads: &[JournalDownload],
) -> Result<Vec<String>> {
    let mut ancestors = std::collections::BTreeSet::new();
    for download in downloads {
        let destination = &download.file.path;
        let destination_depth = destination.split('/').count();
        let prefix = format!("{destination}/");
        for original in original_paths {
            if !original.starts_with(&prefix) {
                continue;
            }
            let components = original.split('/').collect::<Vec<_>>();
            let parent_depth = components.len().saturating_sub(1);
            for depth in destination_depth..=parent_depth {
                let candidate = components[..depth].join("/");
                if ancestors.insert(candidate)
                    && ancestors.len() > MAX_MATERIALIZATION_DIRECTORY_PROOFS
                {
                    anyhow::bail!("materialization restore ancestor allowlist exceeds bound");
                }
            }
        }
    }
    Ok(ancestors.into_iter().collect())
}

#[cfg(windows)]
async fn windows_backup_exists_for_download(
    stage: &Path,
    download_path: &str,
    original_paths: &[String],
) -> Result<bool> {
    let prefix = format!("{download_path}/");
    for original in original_paths
        .iter()
        .filter(|original| original.as_str() == download_path || original.starts_with(&prefix))
    {
        let backup = stage.join("backup").join(original);
        match fs::symlink_metadata(&backup).await {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                return Ok(true)
            }
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                // A directory is only the container for descendant backups;
                // the exact original path has no file backup at this level.
            }
            Ok(_) => anyhow::bail!("materialization backup {original} changed type"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsPublicationRecovery {
    NotPublished,
    Published,
    Ambiguous,
}

#[cfg(windows)]
fn classify_windows_publication_recovery(
    staged_exists: bool,
    destination_exists: bool,
    backup_exists: bool,
) -> WindowsPublicationRecovery {
    if staged_exists && destination_exists {
        return WindowsPublicationRecovery::Published;
    }
    if backup_exists {
        WindowsPublicationRecovery::Ambiguous
    } else {
        WindowsPublicationRecovery::NotPublished
    }
}

#[cfg(all(test, windows))]
mod windows_publication_recovery_tests {
    use super::{classify_windows_publication_recovery, WindowsPublicationRecovery};

    #[test]
    fn publication_progress_truth_table_is_conservative() {
        assert_eq!(
            classify_windows_publication_recovery(true, false, true),
            WindowsPublicationRecovery::Ambiguous
        );
        assert_eq!(
            classify_windows_publication_recovery(true, false, false),
            WindowsPublicationRecovery::NotPublished
        );
        assert_eq!(
            classify_windows_publication_recovery(false, false, true),
            WindowsPublicationRecovery::Ambiguous
        );
        assert_eq!(
            classify_windows_publication_recovery(false, true, true),
            WindowsPublicationRecovery::Ambiguous
        );
        assert_eq!(
            classify_windows_publication_recovery(true, true, true),
            WindowsPublicationRecovery::Published
        );
        assert_eq!(
            classify_windows_publication_recovery(true, true, false),
            WindowsPublicationRecovery::Published
        );
    }
}

#[cfg(windows)]
fn set_readonly_materialization_handle(
    file: &std::fs::File,
    readonly: bool,
) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    let mut permissions = metadata.permissions();
    permissions.set_readonly(readonly);
    file.set_permissions(permissions)
}

#[cfg(windows)]
fn force_basic_delete_disposition_for_tests(base: &Path) -> bool {
    #[cfg(debug_assertions)]
    {
        crate::workspace_layout::ensure_workspace_state(base)
            .map(|state| state.join("test-force-basic-delete").is_file())
            .unwrap_or(false)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = base;
        false
    }
}

#[cfg(windows)]
fn restore_windows_backup_blocking(
    base: &Path,
    stage: &Path,
    relative: &str,
    readonly: bool,
    recreate_ancestors: &[String],
) -> std::io::Result<bool> {
    let parent = relative.rsplit_once('/').map_or("", |(parent, _)| parent);
    let backup_relative = if parent.is_empty() {
        "backup".to_owned()
    } else {
        format!("backup/{parent}")
    };
    // Open and retain every ancestor without delete sharing before opening
    // either file.  This rejects junctions/reparse points and prevents an
    // ancestor replacement from redirecting the later path operations.
    let _backup_ancestors = match open_windows_restore_ancestors(stage, &backup_relative) {
        Ok(chain) => chain,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let backup_path = stage.join("backup").join(relative);
    let original_path = base.join(relative);
    match std::fs::symlink_metadata(&backup_path) {
        Ok(metadata) if metadata.is_dir() => {
            // A directory here is the container for descendant backups (for
            // example stage/backup/d when restoring d/f), not a backed-up file
            // for this exact journal path. Validate it as a non-reparse
            // directory and leave it for the descendant restore.
            open_windows_restore_directory(&backup_path)?;
            return Ok(false);
        }
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "materialization backup changed type",
            ))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    }
    let _original_ancestors =
        open_windows_restore_ancestors_with_creation(base, parent, recreate_ancestors)?;
    let force_basic_delete = force_basic_delete_disposition_for_tests(base);
    let backup = open_windows_restore_file(&backup_path)?;
    let backup_identity = windows_file_identity(&backup)
        .map_err(|error| std::io::Error::other(format!("read backup identity: {error:#}")))?;
    // The extended disposition API ignores this bit, but the fallback
    // FILE_DISPOSITION_INFO path does not. Clear it through the retained
    // handle before linking/deleting, then apply the recorded intent to the
    // restored destination below.
    if backup.metadata()?.permissions().readonly() {
        set_readonly_materialization_handle(&backup, false)?;
    }

    let existing = match open_windows_restore_file(&original_path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(existing) = existing {
        if same_open_file_identity(&backup, &existing).map_err(|error| {
            std::io::Error::other(format!("compare restore identity: {error:#}"))
        })? {
            delete_materialization_handle_with_options(&backup, force_basic_delete)?;
            set_readonly_materialization_handle(&existing, readonly)?;
            return Ok(true);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "materialization restore destination is occupied",
        ));
    }

    // The final destination was absent under an anchored, non-reparse parent.
    // hard_link is no-clobber: a concurrent creator causes AlreadyExists and
    // leaves both the destination and the backup intact for a later retry.
    std::fs::hard_link(&backup_path, &original_path)?;
    let restored = match open_windows_restore_file(&original_path) {
        Ok(file) => file,
        Err(error) => return Err(error),
    };
    let restored_identity = windows_file_identity(&restored)
        .map_err(|error| std::io::Error::other(format!("read restored identity: {error:#}")))?;
    if restored_identity != backup_identity {
        return Err(std::io::Error::other(
            "restored materialization identity changed",
        ));
    }
    delete_materialization_handle_with_options(&backup, force_basic_delete)?;
    set_readonly_materialization_handle(&restored, readonly)?;
    Ok(true)
}

#[cfg(windows)]
async fn restore_windows_backup(
    base: &Path,
    stage: &Path,
    relative: &str,
    readonly: bool,
    recreate_ancestors: &[String],
) -> Result<bool> {
    let base = base.to_path_buf();
    let stage = stage.to_path_buf();
    let relative = relative.to_owned();
    let recreate_ancestors = recreate_ancestors.to_vec();
    tokio::task::spawn_blocking(move || {
        restore_windows_backup_blocking(&base, &stage, &relative, readonly, &recreate_ancestors)
    })
    .await
    .context("join Windows materialization backup restoration task")?
    .map_err(Into::into)
}

#[cfg(windows)]
fn cleanup_materialization_directories_windows_blocking(
    base: &Path,
    proofs: &[MaterializationDirectoryProof],
) -> std::io::Result<()> {
    for proof in proofs.iter().rev() {
        let Some(identity) = proof.identity.as_ref() else {
            continue;
        };
        let parent = proof.path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let _ancestors = match open_windows_restore_ancestors(base, parent) {
            Ok(chain) => chain,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        let current = base.join(&proof.path);
        let directory = match open_materialization_directory_for_delete(&current) {
            Ok(directory) => directory,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        if windows_file_identity(&directory)
            .map_err(|error| std::io::Error::other(format!("read directory identity: {error:#}")))?
            != *identity
        {
            continue;
        }
        match delete_materialization_handle(&directory) {
            Ok(()) => {
                if let Some(parent_handle) = _ancestors.last() {
                    let _ = parent_handle.sync_all();
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn cleanup_materialization_directories(
    base: &Path,
    created_directories: &[MaterializationDirectoryProof],
) -> Result<()> {
    let base = base.to_path_buf();
    let created_directories = created_directories.to_vec();
    tokio::task::spawn_blocking(move || {
        cleanup_materialization_directories_windows_blocking(&base, &created_directories)
    })
    .await
    .context("join Windows materialization directory cleanup task")?
    .map_err(Into::into)
}

fn open_regular_no_follow_absolute(path: &Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        anyhow::ensure!(file.metadata()?.is_file(), "path is not a regular file");
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(0x0020_0000)
            .open(path)?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file() && metadata.file_attributes() & 0x0000_0400 == 0,
            "path is not a non-reparse regular file"
        );
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        anyhow::bail!("stable placeholder identity is unsupported on this platform")
    }
}

#[cfg(windows)]
fn open_directory_no_follow_absolute(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(0x0200_0000 | 0x0020_0000)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.file_attributes() & 0x0000_0400 == 0,
        "path is not a non-reparse directory"
    );
    Ok(file)
}

#[cfg(not(unix))]
async fn capture_directory_identity(
    path: &Path,
) -> Result<Option<MaterializationDirectoryIdentity>> {
    #[cfg(windows)]
    {
        let path = path.to_path_buf();
        return tokio::task::spawn_blocking(move || {
            let directory = open_directory_no_follow_absolute(&path)?;
            Ok(Some(windows_file_identity(&directory)?))
        })
        .await
        .context("join materialization directory identity check")?;
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(None)
    }
}

async fn revalidate_materialization_inputs(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
    placeholder_anchors: &HashMap<String, std::fs::File>,
) -> Result<()> {
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    let mut touched = response
        .delete_local
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    touched.extend(
        response
            .download_required
            .iter()
            .map(|item| item.path.as_str()),
    );
    for path in touched {
        revalidate_materialization_input_at(
            ctx,
            path,
            &ctx.base.join(path),
            local_files.get(path),
            cached_entries,
            placeholder_anchors.get(path),
            &read_root,
        )
        .await?;
    }
    preflight_download_projection(ctx.base, local_files, response).await
}

async fn revalidate_materialization_input_at(
    ctx: &SyncCtx<'_>,
    path: &str,
    actual_path: &Path,
    expected: Option<&FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
    placeholder_anchor: Option<&std::fs::File>,
    read_root: &crate::workspace_read::WorkspaceReadRoot,
) -> Result<()> {
    match expected {
        Some(expected) if !expected.deleted => {
            if let Some(placeholder) = cached_entries
                .get(path)
                .filter(|entry| !entry.hydrated && entry.encrypted_hash == expected.hash)
            {
                let anchor = placeholder_anchor
                    .with_context(|| format!("missing staged placeholder identity for {path}"))?;
                let worktree_path = ctx.base.join(path);
                let current = if actual_path == worktree_path {
                    read_root.open_regular(path).with_context(|| {
                        format!("reopen local placeholder {path} before activation")
                    })?
                } else {
                    open_regular_no_follow_absolute(actual_path).with_context(|| {
                        format!("reopen moved local placeholder {path} before activation")
                    })?
                };
                if !same_open_file_identity(anchor, &current)? {
                    anyhow::bail!(
                        "local placeholder {path} was replaced while downloads were staged"
                    );
                }
                let metadata = current.metadata()?;
                let observed_mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|duration| i64::try_from(duration.as_millis()).ok())
                    .unwrap_or(0);
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.len() != 0
                    || !materialization_mode_matches(portable_mode(&metadata), expected.mode)
                    || !metadata.permissions().readonly()
                    || observed_mtime != placeholder.mtime
                {
                    anyhow::bail!("local placeholder {path} changed while downloads were staged");
                }
            } else {
                let actual = fingerprint_path(ctx, actual_path, path, expected.size).await?;
                if actual.hash != expected.hash
                    || actual.size != expected.size
                    || actual.mode != expected.mode
                {
                    anyhow::bail!("local path {path} changed while downloads were staged");
                }
            }
        }
        _ => match fs::symlink_metadata(actual_path).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => anyhow::bail!("untracked local path {path} appeared during download"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

fn materialization_mode_matches(observed: u32, expected: u32) -> bool {
    #[cfg(unix)]
    {
        observed == expected
    }
    #[cfg(not(unix))]
    {
        let _ = (observed, expected);
        true
    }
}

fn portable_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 != 0 {
            return feanorfs_common::EXECUTABLE_MODE;
        }
    }
    0
}

#[cfg(not(unix))]
async fn ensure_materialization_ancestors(
    base: &Path,
    path: &str,
    stage: &Path,
    journal: &mut MaterializationJournal,
) -> Result<Vec<MaterializationDirectoryProof>> {
    anyhow::ensure!(is_safe_rel_path(path), "unsafe materialization path {path}");
    let components = path.split('/').collect::<Vec<_>>();
    let mut current = base.to_path_buf();
    let mut relative = String::new();
    let mut created = Vec::new();

    for component in components.iter().take(components.len().saturating_sub(1)) {
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(component);
        current.push(component);
        let existing = match fs::symlink_metadata(&current).await {
            Ok(metadata) => Some(metadata),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                None
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(metadata) = existing {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                continue;
            }
            anyhow::bail!(
                "materialization path {path} traverses a non-directory or symlink ancestor"
            );
        }

        if journal.created_directories.len() >= MAX_MATERIALIZATION_DIRECTORY_PROOFS {
            anyhow::bail!("materialization directory proof count exceeds bound");
        }

        match fs::create_dir(&current).await {
            Ok(()) => {
                // Capture immediately after creation. If the platform cannot
                // provide an exact identity (unsupported non-Unix target),
                // keep the proof explicitly unowned so rollback will not
                // guess at ownership. Errors leave the just-created directory
                // unowned as well and abort activation.
                let identity = capture_directory_identity(&current)
                    .await
                    .with_context(|| {
                        format!("capture created materialization directory {relative}")
                    })?;
                let proof = MaterializationDirectoryProof {
                    path: relative.clone(),
                    identity,
                };
                journal.created_directories.push(proof.clone());
                // Persist every proof as soon as it is captured. A later
                // ancestor failure must not make earlier transaction-owned
                // directories unknowable to crash recovery.
                write_materialization_journal(stage, journal).await?;
                created.push(proof);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A creator racing this activation owns the namespace entry;
                // re-inspect it and deliberately record no proof. This is the
                // key boundary that prevents rollback from deleting a user's
                // directory merely because it appeared after the first check.
                match fs::symlink_metadata(&current).await {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Ok(_) => anyhow::bail!(
                        "materialization path {path} traverses a non-directory or symlink ancestor"
                    ),
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create materialization directory {relative}"))
            }
        }
    }
    Ok(created)
}

async fn fingerprint_local_path(ctx: &SyncCtx<'_>, path: &str, size: u64) -> Result<FileState> {
    fingerprint_path(ctx, &ctx.base.join(path), path, size).await
}

async fn fingerprint_path(
    ctx: &SyncCtx<'_>,
    actual_path: &Path,
    logical_path: &str,
    size: u64,
) -> Result<FileState> {
    let relative = actual_path.strip_prefix(ctx.base).with_context(|| {
        format!(
            "local path {} is outside its workspace read root",
            actual_path.display()
        )
    })?;
    let root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    let mut file = root.open_regular_path(relative).with_context(|| {
        format!("open local path {logical_path} through its workspace read root")
    })?;
    let metadata = file.metadata()?;
    let hash = if crate::large_file::uses_chunk_transport(size) {
        crate::large_file::fingerprint_opened(&mut file, ctx.password_str(), logical_path)?
            .encrypted_hash
    } else {
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(crate::large_file::CHUNK_THRESHOLD_BYTES as usize)
                .min(crate::large_file::CHUNK_THRESHOLD_BYTES as usize),
        );
        (&mut file)
            .take(crate::large_file::CHUNK_THRESHOLD_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > crate::large_file::CHUNK_THRESHOLD_BYTES {
            anyhow::bail!("local path {logical_path} grew while it was being read");
        }
        seal(&bytes, ctx.password_str(), logical_path)?.0
    };
    let after = file.metadata()?;
    if metadata.len() != after.len() || metadata.modified().ok() != after.modified().ok() {
        anyhow::bail!("local path {logical_path} changed while it was being read");
    }
    Ok(FileState {
        path: logical_path.to_string(),
        hash,
        size: metadata.len(),
        mtime: 0,
        deleted: false,
        mode: portable_mode(&metadata),
    })
}

pub(crate) async fn prefetch_downloads(ctx: &SyncCtx<'_>, response: &SyncResponse) -> Result<()> {
    if response.download_required.is_empty() {
        return Ok(());
    }
    if recover_materialization_stages(ctx).await? {
        anyhow::bail!("recovered an interrupted materialization; retry the operation");
    }
    let stage = create_materialization_stage(ctx).await?;
    // Prefetch is cache-warming only: each file writes into the disposable
    // stage, so independent files can be fetched with bounded concurrency.
    let result = async {
        let mut prefetches =
            futures_util::stream::iter(response.download_required.iter().cloned().map(|file| {
                let stage = stage.clone();
                async move {
                    if !is_safe_rel_path(&file.path) {
                        anyhow::bail!("server requested an unsafe download path: {}", file.path);
                    }
                    let destination = stage.join("new").join(&file.path);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    crate::large_file::materialize_to(
                        ctx,
                        &file.path,
                        &file.hash,
                        file.size,
                        &destination,
                    )
                    .await
                }
            }))
            .buffer_unordered(MAX_PARALLEL_DOWNLOADS);
        while let Some(result) = prefetches.next().await {
            result?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = remove_materialization_stage(&stage, ctx.base).await {
        tracing::warn!(
            "failed to remove prefetch staging directory {} durably: {error}",
            stage.display()
        );
    }
    result
}

pub(crate) async fn preflight_download_projection(
    base: &Path,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
) -> Result<()> {
    validate_local_projection(local_files, response)?;
    let delete_paths: HashSet<&str> = response.delete_local.iter().map(String::as_str).collect();
    for replica_file in &response.download_required {
        ensure_destination_replaceable(
            base,
            &replica_file.path,
            &delete_paths,
            local_files.contains_key(&replica_file.path),
        )
        .await?;
    }
    Ok(())
}

fn validate_local_projection(
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
) -> Result<()> {
    let mut projected = local_files.clone();
    for path in &response.delete_local {
        projected.remove(path);
    }
    for file in &response.download_required {
        projected.insert(file.path.clone(), file.clone());
    }
    feanorfs_common::flat_to_tree(&projected)
        .context("remote update would create a file/directory path collision")?;
    Ok(())
}

async fn create_materialization_stage(ctx: &SyncCtx<'_>) -> Result<PathBuf> {
    let base = ctx.base;
    for _ in 0..16 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).context("generate materialization staging id")?;
        let id = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = base.join(format!(".feanorfs-tmp-materialize-{id}"));
        match fs::create_dir(&path).await {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await?;
                }
                pause_sync_test(ctx, "stage-before-journal").await?;
                let journal = MaterializationJournal {
                    phase: "preparing".to_string(),
                    publication_progress_recorded: false,
                    original_paths: Vec::new(),
                    original_readonly: std::collections::BTreeMap::new(),
                    downloads: Vec::new(),
                    delete_paths: Vec::new(),
                    published_paths: Vec::new(),
                    publishing_path: None,
                    created_directories: Vec::new(),
                };
                if let Err(error) = write_materialization_journal(&path, &journal).await {
                    let _ = remove_materialization_stage(&path, base).await;
                    return Err(error);
                }
                sync_directory(base).await?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create materialization staging directory"),
        }
    }
    anyhow::bail!("could not allocate a materialization staging directory")
}

async fn validate_worktree_ancestors(base: &Path, path: &str) -> Result<()> {
    anyhow::ensure!(is_safe_rel_path(path), "unsafe materialization path {path}");
    let mut current = base.to_path_buf();
    let components = path.split('/').collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component);
        match fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => anyhow::bail!(
                "materialization path {path} traverses a non-directory or symlink ancestor"
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                break
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn ensure_destination_replaceable(
    base: &Path,
    path: &str,
    delete_paths: &HashSet<&str>,
    destination_was_tracked: bool,
) -> Result<()> {
    let destination = base.join(path);
    let mut prefix = String::new();
    let components: Vec<&str> = path.split('/').collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        match fs::symlink_metadata(base.join(&prefix)).await {
            Ok(metadata) => {
                if (!metadata.is_dir() || metadata.file_type().is_symlink())
                    && !delete_paths.contains(prefix.as_str())
                {
                    anyhow::bail!(
                        "cannot materialize {path}: ancestor {prefix} is a local file not scheduled for replacement"
                    );
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = match fs::symlink_metadata(&destination).await {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        anyhow::bail!("refusing to replace symlink at {path}");
    }
    if !metadata.is_dir() {
        if !destination_was_tracked {
            anyhow::bail!("refusing to replace untracked local file at {path}");
        }
        return Ok(());
    }
    let mut pending = vec![destination];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = fs::symlink_metadata(entry.path()).await?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(entry.path());
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(base)
                .context("replacement entry escaped workspace")?
                .to_str()
                .context("replacement entry is not valid UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if !delete_paths.contains(relative.as_str()) {
                anyhow::bail!(
                    "cannot replace directory {path}: untracked entry {relative} would be lost"
                );
            }
        }
    }
    Ok(())
}

async fn remove_empty_destination_tree(destination: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(destination).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mut pending = vec![destination.to_path_buf()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        directories.push(directory.clone());
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = fs::symlink_metadata(entry.path()).await?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(entry.path());
            } else {
                anyhow::bail!(
                    "refusing to replace non-empty directory {}",
                    destination.display()
                );
            }
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        fs::remove_dir(&directory)
            .await
            .with_context(|| format!("remove empty directory {}", directory.display()))?;
        if let Some(parent) = directory.parent() {
            sync_directory(parent).await?;
        }
    }
    Ok(())
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
