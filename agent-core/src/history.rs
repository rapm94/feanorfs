use crate::snapshot::{SnapshotEngine, SnapshotInput};
use crate::{SwapHeadResult, SyncCtx};
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    FileState, LogEntry, LogResult, SyncResponse, UndoResult, MAX_TREE_OUTPUT_PATHS,
    MAX_TREE_PATH_BYTES_TOTAL,
};
use std::collections::{HashMap, HashSet};

const MAX_LOG_LIMIT: usize = 1_000;
const MAX_UNDO_RETRIES: usize = 8;
const MAX_HISTORY_SNAPSHOTS: usize = 250_000;

/// Lists reachable snapshots from current workspace head.
///
/// # Errors
/// Returns an error when head objects cannot be fetched or decoded.
pub async fn log(ctx: &SyncCtx<'_>, limit: usize) -> Result<LogResult> {
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Ok(LogResult {
            entries: Vec::new(),
        });
    };
    let snapshots = SnapshotEngine::new(ctx);
    let mut pending = vec![head];
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut changed_path_count = 0_usize;
    let mut changed_path_bytes = 0_usize;
    while let Some(id) = pending.pop() {
        if entries.len() >= limit.min(MAX_LOG_LIMIT) || !seen.insert(id.clone()) {
            continue;
        }
        let snapshot = snapshots.load_snapshot(&id).await?;
        let changed_paths = if snapshot.parents.is_empty() {
            let mut paths: Vec<_> = snapshots.load_files(&id).await?.into_keys().collect();
            paths.sort();
            paths
        } else {
            // Union the diffs across all parents so merge/undo snapshots
            // report second-parent deltas too.
            let mut paths = std::collections::BTreeSet::new();
            for parent in &snapshot.parents {
                for change in snapshots.diff_snapshots(parent, &id).await?.changes {
                    paths.insert(change.path);
                }
            }
            paths.into_iter().collect()
        };
        changed_path_count = changed_path_count
            .checked_add(changed_paths.len())
            .context("history path counter overflow")?;
        changed_path_bytes = changed_paths
            .iter()
            .try_fold(changed_path_bytes, |total, path| {
                total
                    .checked_add(path.len())
                    .context("history path byte counter overflow")
            })?;
        if changed_path_count > MAX_TREE_OUTPUT_PATHS
            || changed_path_bytes > MAX_TREE_PATH_BYTES_TOTAL
        {
            bail!("history output exceeds aggregate path limit");
        }
        pending.extend(snapshot.parents.iter().rev().cloned());
        if seen.len().saturating_add(pending.len()) > MAX_HISTORY_SNAPSHOTS {
            bail!("snapshot history exceeds traversal limit");
        }
        entries.push(LogEntry {
            snapshot_id: id,
            parents: snapshot.parents,
            author: snapshot.author,
            created_at_ms: snapshot.created_at_ms,
            message: snapshot.message,
            changed_paths,
        });
    }
    Ok(LogResult { entries })
}

/// Restores one reachable snapshot by appending a new snapshot.
///
/// # Errors
/// Returns an error for invalid selectors, CAS exhaustion, or projection failures.
pub async fn undo(ctx: &SyncCtx<'_>, selector: &str) -> Result<UndoResult> {
    let _sync_guard = crate::lock::SyncLock::acquire(ctx.base)?;
    let mut expected = ctx
        .api
        .get_head(ctx.workspace_id())
        .await?
        .context("workspace has no snapshot history")?;
    let snapshots = SnapshotEngine::new(ctx);
    let restored_snapshot_id = resolve_reachable(&snapshots, &expected, selector).await?;
    let target = snapshots.load_state(&restored_snapshot_id).await?;
    let local_before = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let materialization = SyncResponse {
        upload_required: Vec::new(),
        download_required: target.files.values().cloned().collect(),
        delete_local: local_before
            .keys()
            .filter(|path| !target.files.contains_key(*path))
            .cloned()
            .collect(),
    };
    crate::sync_pass::preflight_download_projection(ctx.base, &local_before, &materialization)
        .await?;
    // Authenticate and fsync the complete target before the head CAS. The
    // subsequent activation reuses the verified object cache, so missing or
    // corrupt historical blobs cannot commit an unmaterializable undo.
    crate::sync_pass::prefetch_downloads(ctx, &materialization).await?;
    let state_dir = ctx.state_dir()?;
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    for state in local_before.values().filter(|state| !state.deleted) {
        if crate::large_file::uses_chunk_transport(state.size) {
            crate::large_file::upload(ctx, &state.path, &state.hash).await?;
            continue;
        }
        let content = crate::sync_pass::read_upload_source(&read_root, &state.path, state).await?;
        let (hash, ciphertext) = crate::crypto::seal(&content, ctx.password_str(), &state.path)?;
        anyhow::ensure!(hash == state.hash, "worktree changed during undo");
        if !crate::upload_registry::known(&state_dir, &hash).await {
            ctx.api
                .upload_object(ctx.workspace_id(), &hash, ciphertext)
                .await?;
            crate::upload_registry::remember(&state_dir, &hash);
        }
    }
    let backup = snapshots
        .write(SnapshotInput {
            files: &local_before,
            conflicts: &[],
            parents: vec![expected.clone()],
            author: "you",
            message: Some("before undo".to_string()),
        })
        .await?;

    let mut committed = None;
    for _ in 0..MAX_UNDO_RETRIES {
        let candidate = snapshots
            .write(SnapshotInput {
                files: &target.files,
                conflicts: &target.conflicts,
                parents: vec![expected.clone(), backup.clone()],
                author: "you",
                message: Some(format!("undo {restored_snapshot_id}")),
            })
            .await?;
        match ctx
            .api
            .swap_head(ctx.workspace_id(), Some(&expected), &candidate)
            .await?
        {
            SwapHeadResult::Swapped => {
                committed = Some(candidate);
                break;
            }
            SwapHeadResult::Conflict(Some(current)) => expected = current,
            SwapHeadResult::Conflict(None) => bail!("workspace head disappeared during undo"),
        }
    }
    let snapshot_id = committed.context("workspace head changed too many times during undo")?;
    crate::sync_pass::pause_sync_test(ctx, "undo-after-cas").await?;
    let changed_paths: Vec<_> = snapshots
        .diff_snapshots(&backup, &restored_snapshot_id)
        .await?
        .changes
        .into_iter()
        .map(|change| change.path)
        .collect();
    materialize_and_project(ctx, &target.files, &local_before).await?;
    snapshots.record_committed_refs(&snapshot_id).await?;
    Ok(UndoResult {
        snapshot_id,
        restored_snapshot_id,
        changed_paths,
    })
}

async fn resolve_reachable(
    snapshots: &SnapshotEngine<'_, '_>,
    head: &str,
    selector: &str,
) -> Result<String> {
    if selector.len() < 8 || !selector.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("snapshot id must be at least 8 hexadecimal characters");
    }
    let mut pending = vec![head.to_string()];
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if seen.len() > MAX_HISTORY_SNAPSHOTS {
            bail!("snapshot history exceeds traversal limit");
        }
        let snapshot = snapshots.load_snapshot(&id).await?;
        if id.starts_with(selector) {
            matches.push(id.clone());
        }
        pending.extend(snapshot.parents);
        if pending.len() > MAX_HISTORY_SNAPSHOTS {
            bail!("snapshot history frontier exceeds traversal limit");
        }
    }
    match matches.as_slice() {
        [id] => Ok(id.clone()),
        [] => bail!("snapshot {selector} is not reachable from current head"),
        _ => bail!("snapshot prefix {selector} is ambiguous"),
    }
}

async fn materialize_and_project(
    ctx: &SyncCtx<'_>,
    target: &HashMap<String, FileState>,
    local_before: &HashMap<String, FileState>,
) -> Result<()> {
    let response = SyncResponse {
        upload_required: Vec::new(),
        download_required: target.values().cloned().collect(),
        delete_local: local_before
            .keys()
            .filter(|path| !target.contains_key(*path))
            .cloned()
            .collect(),
    };
    crate::sync_pass::process_downloads(ctx, &response, local_before, false).await?;
    crate::sync_pass::process_delete_local(&response, ctx.base, ctx.db).await?;

    if ctx.format_version() >= 3 {
        return Ok(());
    }

    let mut projection =
        crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    let remote = crate::conflicts::load_server_view(ctx).await?;
    let mut upload_required = Vec::new();
    for (path, state) in target {
        if remote
            .get(path)
            .is_none_or(|current| current.hash != state.hash || current.mode != state.mode)
        {
            upload_required.push(path.clone());
        }
    }
    for (path, state) in remote {
        if !target.contains_key(&path) {
            let mut tombstone = state;
            tombstone.deleted = true;
            tombstone.size = 0;
            projection.insert(path.clone(), tombstone);
            upload_required.push(path);
        }
    }
    crate::sync_pass::process_uploads(
        ctx,
        &SyncResponse {
            upload_required,
            download_required: Vec::new(),
            delete_local: Vec::new(),
        },
        &projection,
        false,
    )
    .await?;
    Ok(())
}
