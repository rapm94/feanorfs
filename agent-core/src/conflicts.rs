use crate::conflict_artifacts::{
    is_cloud_deleted_sentinel, is_sentinel_content, resolve_artifact, write_conflict_triple,
    ArtifactRole,
};
use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::fs_util::{atomic_write, file_mtime_ms};
use crate::local::ClientDb;
use crate::paths::conflicts_dir;
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    conflict_candidate_paths, detect_concurrent_edits, is_safe_rel_path, ConcurrentEdit,
    ConflictKind, FileState, SyncRequest, SyncResponse,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;

fn file_changed_since(base: Option<&FileState>, current: &FileState) -> bool {
    match base {
        None => true,
        Some(base) => {
            current.hash != base.hash
                || current.deleted != base.deleted
                || current.mode != base.mode
        }
    }
}

fn live_state(state: Option<&FileState>) -> Option<&FileState> {
    state.filter(|state| !state.deleted)
}

fn same_content(left: Option<&FileState>, right: Option<&FileState>) -> bool {
    match (live_state(left), live_state(right)) {
        (Some(left), Some(right)) => left.hash == right.hash && left.mode == right.mode,
        (None, None) => true,
        _ => false,
    }
}

/// Load the complete active server view without relying on LWW delta direction.
pub async fn load_server_view(ctx: &SyncCtx<'_>) -> Result<HashMap<String, FileState>> {
    if ctx.format_version() >= 3 {
        return match ctx.api.get_head(ctx.workspace_id()).await? {
            Some(head) => {
                crate::snapshot::SnapshotEngine::new(ctx)
                    .load_files(&head)
                    .await
            }
            None => Ok(HashMap::new()),
        };
    }
    let response = ctx
        .api
        .peek_sync(&SyncRequest {
            workspace_id: ctx.workspace_id().to_string(),
            files: Vec::new(),
        })
        .await?;
    Ok(response
        .download_required
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect())
}

pub async fn load_last_synced_snapshot(ctx: &SyncCtx<'_>) -> Result<HashMap<String, FileState>> {
    crate::snapshot::SnapshotEngine::new(ctx)
        .load_last_synced()
        .await
}

pub async fn pending_conflict_paths(db: &ClientDb) -> Result<HashSet<String>> {
    Ok(db
        .list_pending_conflict_paths()
        .await?
        .into_iter()
        .collect())
}

pub fn conflicts_pending(pending_paths: Option<&HashSet<String>>) -> bool {
    pending_paths.is_some_and(|p| !p.is_empty())
}

pub async fn detect_workspace_conflicts(
    ctx: &SyncCtx<'_>,
    last_synced: &HashMap<String, FileState>,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
    already_pending: &HashSet<String>,
) -> Result<Vec<(ConcurrentEdit, ConflictKind)>> {
    let server_files = load_server_view(ctx).await?;
    detect_workspace_conflicts_with_server_view(
        last_synced,
        local_files,
        response,
        already_pending,
        &server_files,
    )
}

fn detect_workspace_conflicts_with_server_view(
    last_synced: &HashMap<String, FileState>,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
    already_pending: &HashSet<String>,
    server_files: &HashMap<String, FileState>,
) -> Result<Vec<(ConcurrentEdit, ConflictKind)>> {
    let their_changed: HashMap<String, FileState> = server_files
        .iter()
        .filter(|(path, remote)| !same_content(Some(remote), last_synced.get(*path)))
        .map(|(path, remote)| (path.clone(), remote.clone()))
        .collect();
    let their_deleted: HashSet<String> = last_synced
        .iter()
        .filter(|(path, base)| !base.deleted && !server_files.contains_key(*path))
        .map(|(path, _)| path.clone())
        .collect();

    let candidates = conflict_candidate_paths(response, already_pending)
        .into_iter()
        .chain(their_changed.keys().cloned())
        .chain(their_deleted.iter().cloned());
    let mut edits = detect_concurrent_edits(
        last_synced,
        local_files,
        &their_changed,
        &their_deleted,
        candidates,
        already_pending,
    );
    edits.retain(|(c, _)| is_safe_rel_path(&c.path));
    Ok(edits)
}

pub async fn register_and_write_conflicts(
    ctx: &SyncCtx<'_>,
    items: &[(ConcurrentEdit, ConflictKind)],
    ours_base: Option<&Path>,
) -> Result<(PathBuf, HashSet<String>)> {
    let ts = chrono::Utc::now().timestamp_millis();
    let dir = conflicts_dir(ctx.base).join(ts.to_string());
    fs::create_dir_all(&dir).await?;

    let password_str = ctx.password_str();
    let local_root = ours_base.unwrap_or(ctx.base);
    for (edit, kind) in items {
        let ours_src = edit.ours.as_ref().map(|o| local_root.join(&o.path));
        let ours_label = ours_missing_label(kind);
        write_conflict_triple(
            &dir,
            edit,
            ctx.api,
            password_str,
            ours_src.as_deref(),
            ours_label,
            ctx.policy,
        )
        .await?;
    }

    let paths: Vec<String> = items.iter().map(|(c, _)| c.path.clone()).collect();
    fs::write(dir.join("manifest.json"), serde_json::to_string(&paths)?).await?;

    let mut out = HashSet::new();
    for (c, kind) in items {
        ctx.db
            .upsert_conflict(&c.path, kind, &dir.to_string_lossy(), ts, "pending")
            .await?;
        out.insert(c.path.clone());
    }

    Ok((dir, out))
}

fn ours_missing_label(kind: &ConflictKind) -> &'static str {
    match kind {
        ConflictKind::EditDelete => "deleted-locally",
        ConflictKind::DeleteEdit => "no-local-changes",
        ConflictKind::EditEdit => "no-local-snapshot",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKeep {
    Local,
    Cloud,
    Both,
    File,
}

fn conflict_artifact_paths(conflict_dir: &Path, path: &str) -> [PathBuf; 3] {
    [
        resolve_artifact(conflict_dir, path, ArtifactRole::Original),
        resolve_artifact(conflict_dir, path, ArtifactRole::Local),
        resolve_artifact(conflict_dir, path, ArtifactRole::Cloud),
    ]
}

async fn remove_path_artifacts(conflict_dir: &Path, path: &str) -> Result<()> {
    for artifact in conflict_artifact_paths(conflict_dir, path) {
        if artifact.is_file() {
            fs::remove_file(&artifact).await?;
        }
    }
    Ok(())
}

pub async fn resolve_conflict(
    ctx: &SyncCtx<'_>,
    path: &str,
    keep: ResolveKeep,
    file_source: Option<&Path>,
) -> Result<()> {
    if !is_safe_rel_path(path) {
        bail!("unsafe path: {path}");
    }
    let record = ctx
        .db
        .get_conflict_record(path)
        .await?
        .with_context(|| format!("no pending conflict for {path}"))?;
    let before = crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?;
    crate::snapshot::SnapshotEngine::new(ctx)
        .snapshot_local_view(&before, "you")
        .await?;
    let conflict_dir = PathBuf::from(&record.conflict_dir);

    match keep {
        ResolveKeep::File => {
            let src = file_source.with_context(|| "conflicts keep --file requires a path")?;
            let content = fs::read(src).await?;
            atomic_write(ctx.base, path, &content).await?;
            upload_sealed(ctx, path, &content, chrono::Utc::now().timestamp_millis()).await?;
        }
        ResolveKeep::Local => {
            let ours_path = ctx.base.join(path);
            if ours_path.exists() {
                let plain = fs::read(&ours_path).await?;
                let mtime = file_mtime_ms(&ours_path).await?;
                upload_sealed(ctx, path, &plain, mtime).await?;
            } else {
                upload_tombstone_for(ctx, path).await?;
            }
        }
        ResolveKeep::Cloud => {
            let theirs_file = resolve_artifact(&conflict_dir, path, ArtifactRole::Cloud);
            if theirs_file.exists() {
                let content = fs::read(&theirs_file).await?;
                if is_cloud_deleted_sentinel(&content) {
                    let ours_path = ctx.base.join(path);
                    if ours_path.exists() {
                        fs::remove_file(&ours_path).await?;
                    }
                    upload_tombstone_for(ctx, path).await?;
                } else if is_sentinel_content(&content) {
                    bail!("theirs version unavailable on disk; re-run sync while online");
                } else {
                    atomic_write(ctx.base, path, &content).await?;
                    upload_sealed(ctx, path, &content, chrono::Utc::now().timestamp_millis())
                        .await?;
                }
            } else {
                bail!("cloud version artifact missing for {path}");
            }
        }
        ResolveKeep::Both => {
            let theirs_file = resolve_artifact(&conflict_dir, path, ArtifactRole::Cloud);
            let hostname = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "other".into());
            let safe_path: String = path
                .chars()
                .map(|c| {
                    if c.is_control() || "/\\:|".contains(c) {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let alt_path = format!("{safe_path} (conflicted copy {hostname})");
            let ours_path = ctx.base.join(path);
            if ours_path.exists() {
                let content = fs::read(&ours_path).await?;
                upload_sealed(ctx, path, &content, file_mtime_ms(&ours_path).await?).await?;
            }
            if theirs_file.exists() {
                let content = fs::read(&theirs_file).await?;
                if !is_sentinel_content(&content) {
                    atomic_write(ctx.base, &alt_path, &content).await?;
                    upload_sealed(
                        ctx,
                        &alt_path,
                        &content,
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .await?;
                }
            }
        }
    }

    let resolved_files = if ctx.format_version() >= 3 {
        crate::local::scan_local_directory(ctx.base, ctx.db, ctx.password()).await?
    } else {
        load_server_view(ctx).await?
    };
    let resolver = std::env::var("FEANORFS_AGENT").unwrap_or_else(|_| "human".into());
    crate::snapshot::SnapshotEngine::new(ctx)
        .resolve_conflict(path, &resolved_files, &resolver)
        .await?;

    ctx.db.resolve_conflict_path(path).await?;
    remove_path_artifacts(&conflict_dir, path).await?;

    let method = match keep {
        ResolveKeep::Local => "local",
        ResolveKeep::Cloud => "cloud",
        ResolveKeep::Both => "both",
        ResolveKeep::File => "file",
    };
    let source_hash = file_source
        .and_then(|p| std::fs::read(p).ok())
        .map(|b| feanorfs_common::hash_bytes(&b));
    ctx.db
        .record_conflict_resolution(path, method, source_hash.as_deref(), &resolver)
        .await?;

    if ctx.db.count_pending_in_dir(&record.conflict_dir).await? == 0 && conflict_dir.is_dir() {
        if let Err(e) = fs::remove_dir_all(&conflict_dir).await {
            tracing::warn!(
                "failed to clean conflict dir {}: {e}",
                conflict_dir.display()
            );
        }
    }
    Ok(())
}

async fn upload_sealed(ctx: &SyncCtx<'_>, path: &str, content: &[u8], mtime: i64) -> Result<()> {
    let (hash, packed) = seal(content, ctx.password_str(), path)?;
    let file = FileState {
        path: path.to_string(),
        hash,
        size: content.len() as u64,
        mtime,
        deleted: false,
        mode: 0,
    };
    if ctx.format_version() >= 3 {
        ctx.api
            .upload_object(ctx.workspace_id(), &file.hash, packed)
            .await
    } else {
        ctx.api.upload_file(ctx.workspace_id(), &file, packed).await
    }
}

async fn upload_tombstone_for(ctx: &SyncCtx<'_>, path: &str) -> Result<()> {
    let cached = ctx.db.get_cache_entries().await?;
    let hash = cached
        .get(path)
        .map(|c| c.encrypted_hash.clone())
        .unwrap_or_else(|| feanorfs_common::hash_bytes(b""));
    let mtime = chrono::Utc::now().timestamp_millis();
    if ctx.format_version() >= 3 {
        return Ok(());
    }
    ctx.api
        .upload_tombstone(ctx.workspace_id(), path, &hash, mtime)
        .await
}

pub async fn seed_last_synced_from_server(
    ctx: &SyncCtx<'_>,
    local_files: &HashMap<String, FileState>,
) -> Result<u32> {
    let mut synced = load_last_synced_snapshot(ctx).await?;
    let before = synced.len();
    let server_files = load_server_view(ctx).await?;
    for (path, local) in local_files {
        if !is_safe_rel_path(path) {
            continue;
        }
        if let Some(remote) = server_files.get(path) {
            if same_content(Some(local), Some(remote)) {
                synced.insert(path.clone(), remote.clone());
            }
        }
    }
    crate::snapshot::SnapshotEngine::new(ctx)
        .record_last_synced(&synced, "seed")
        .await?;
    Ok(u32::try_from(synced.len().saturating_sub(before)).unwrap_or(u32::MAX))
}

pub fn filter_blocked_paths(response: &mut SyncResponse, blocked: &HashSet<String>) {
    response.upload_required.retain(|p| !blocked.contains(p));
    response
        .download_required
        .retain(|f| !blocked.contains(&f.path));
    response.delete_local.retain(|p| !blocked.contains(p));
}

/// Paths where a lazy placeholder was written to locally (DX-10).
pub async fn detect_placeholder_corruptions(
    base_path: &Path,
    db: &ClientDb,
) -> Result<Vec<String>> {
    let cached = db.get_cache_entries().await?;
    let mut out = Vec::new();
    for (path, entry) in &cached {
        if entry.hydrated || entry.deleted_at.is_some() {
            continue;
        }
        let full = base_path.join(path);
        if !full.is_file() {
            continue;
        }
        let meta = fs::metadata(&full).await?;
        if meta.len() > 0 {
            out.push(path.clone());
        }
    }
    Ok(out)
}

pub async fn register_placeholder_corruption(base: &Path, db: &ClientDb, path: &str) -> Result<()> {
    if db.get_conflict_record(path).await?.is_some() {
        return Ok(());
    }
    let ts = chrono::Utc::now().timestamp_millis();
    let dir = conflicts_dir(base).join(format!("placeholder_{ts}"));
    fs::create_dir_all(&dir).await?;
    let stray = fs::read(base.join(path)).await?;
    let local_dest = resolve_artifact(&dir, path, ArtifactRole::Local);
    if let Some(parent) = local_dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&local_dest, &stray).await?;
    let cloud_dest = resolve_artifact(&dir, path, ArtifactRole::Cloud);
    fs::write(
        &cloud_dest,
        format!(
            "{}hydrate-to-compare>\n",
            crate::conflict_artifacts::SENTINEL_PREFIX
        ),
    )
    .await?;
    let original_dest = resolve_artifact(&dir, path, ArtifactRole::Original);
    fs::write(
        &original_dest,
        format!(
            "{}placeholder>\n",
            crate::conflict_artifacts::SENTINEL_PREFIX
        ),
    )
    .await?;
    db.upsert_conflict(
        path,
        &ConflictKind::EditEdit,
        &dir.to_string_lossy(),
        ts,
        "pending",
    )
    .await?;
    Ok(())
}

fn paths_case_collide(a: &str, b: &str) -> bool {
    a != b && a.eq_ignore_ascii_case(b)
}

/// Detect case-only path collisions during pull (DX-15).
pub fn case_conflict_paths(
    download_paths: &[FileState],
    local_paths: &HashMap<String, FileState>,
) -> Vec<String> {
    let mut out = Vec::new();
    for remote in download_paths {
        for local_path in local_paths.keys() {
            if paths_case_collide(&remote.path, local_path) {
                out.push(remote.path.clone());
                break;
            }
        }
    }
    out
}

/// Warn when server metadata regressed vs last agreed state (DX-23).
pub fn detect_server_rollback(
    last_synced: &HashMap<String, FileState>,
    server_files: &[FileState],
) -> Option<String> {
    if last_synced.is_empty() {
        return None;
    }
    let server_map: HashMap<_, _> = server_files.iter().map(|f| (f.path.clone(), f)).collect();
    let mut regressed = 0u32;
    for (path, agreed) in last_synced {
        if agreed.deleted {
            continue;
        }
        if let Some(remote) = server_map.get(path) {
            if remote.mtime < agreed.mtime && remote.hash != agreed.hash {
                regressed += 1;
            }
        }
    }
    if regressed > 0 {
        Some(format!(
            "Server looks older than this machine on {regressed} path(s); \
             run `feanorfs sync --up` to restore it instead of mass-downloading stale files."
        ))
    } else {
        None
    }
}

/// After upload, detect silent create/create collisions (DX-22).
pub async fn detect_post_upload_collisions(
    ctx: &SyncCtx<'_>,
    local_files: &HashMap<String, FileState>,
    uploaded_paths: &[String],
) -> Result<Vec<(ConcurrentEdit, ConflictKind)>> {
    if uploaded_paths.is_empty() {
        return Ok(Vec::new());
    }
    let last = load_last_synced_snapshot(ctx).await?;
    let server_map = load_server_view(ctx).await?;
    let mut out = Vec::new();
    for path in uploaded_paths {
        let Some(local) = local_files.get(path) else {
            continue;
        };
        if let Some(remote) = server_map.get(path) {
            if remote.hash != local.hash {
                let base = last.get(path).cloned();
                out.push((
                    ConcurrentEdit::new(
                        path.clone(),
                        base,
                        Some(local.clone()),
                        Some(remote.clone()),
                    ),
                    ConflictKind::EditEdit,
                ));
            }
        }
    }
    Ok(out)
}

/// Peek server delta, detect workspace conflicts, optionally register them, and
/// return the filtered response plus blocked paths.
pub async fn negotiate_sync_with_conflict_gate(
    ctx: &SyncCtx<'_>,
    local_files: &HashMap<String, FileState>,
    register: bool,
) -> Result<(SyncResponse, HashSet<String>)> {
    let pending = pending_conflict_paths(ctx.db).await?;
    let server_files = load_server_view(ctx).await?;
    let reconciled = crate::tree_reconcile::reconcile(ctx, local_files, &server_files).await?;
    let last = reconciled.base;
    let mut response = reconciled.response;
    let detected = detect_workspace_conflicts_with_server_view(
        &last,
        local_files,
        &response,
        &pending,
        &server_files,
    )?;

    let mut all_detected = detected;
    for remote_path in case_conflict_paths(&response.download_required, local_files) {
        if pending.contains(&remote_path) {
            continue;
        }
        let Some(remote) = response
            .download_required
            .iter()
            .find(|f| f.path == remote_path)
            .cloned()
        else {
            continue;
        };
        let local_key = local_files
            .keys()
            .find(|p| paths_case_collide(p, &remote_path))
            .cloned();
        if let Some(local_key) = local_key {
            if let Some(local) = local_files.get(&local_key) {
                let base = last.get(&remote_path).cloned();
                all_detected.push((
                    ConcurrentEdit::new(
                        remote_path.clone(),
                        base,
                        Some(local.clone()),
                        Some(remote),
                    ),
                    ConflictKind::EditEdit,
                ));
            }
        }
    }

    let mut seen_paths: HashSet<String> =
        all_detected.iter().map(|(c, _)| c.path.clone()).collect();
    for remote in &response.download_required {
        if pending.contains(&remote.path) || !seen_paths.insert(remote.path.clone()) {
            continue;
        }
        let Some(local) = local_files.get(&remote.path) else {
            continue;
        };
        if same_content(Some(local), Some(remote)) {
            continue;
        }
        let we_changed = file_changed_since(last.get(&remote.path), local);
        let they_changed = file_changed_since(last.get(&remote.path), remote);
        if !(we_changed && they_changed) {
            continue;
        }
        let base = last
            .get(&remote.path)
            .cloned()
            .or_else(|| Some(local.clone()));
        all_detected.push((
            ConcurrentEdit::new(
                remote.path.clone(),
                base,
                Some(local.clone()),
                Some(remote.clone()),
            ),
            ConflictKind::EditEdit,
        ));
    }

    let needs_upload_collision_scan = response
        .upload_required
        .iter()
        .any(|path| local_files.contains_key(path) && !last.contains_key(path));
    if needs_upload_collision_scan {
        for path in &response.upload_required {
            if pending.contains(path) || !seen_paths.insert(path.clone()) {
                continue;
            }
            let Some(local) = local_files.get(path) else {
                continue;
            };
            let Some(remote) = server_files.get(path) else {
                continue;
            };
            if same_content(Some(local), Some(remote)) {
                continue;
            }
            let we_changed = file_changed_since(last.get(path), local);
            let they_changed = file_changed_since(last.get(path), remote);
            if !(we_changed && they_changed) {
                continue;
            }
            let base = last.get(path).cloned().or_else(|| Some(local.clone()));
            all_detected.push((
                ConcurrentEdit::new(
                    path.clone(),
                    base,
                    Some(local.clone()),
                    Some(remote.clone()),
                ),
                ConflictKind::EditEdit,
            ));
        }
    }

    let mut blocked = pending;

    if register {
        for path in detect_placeholder_corruptions(ctx.base, ctx.db).await? {
            register_placeholder_corruption(ctx.base, ctx.db, &path).await?;
            blocked.insert(path);
        }
    }

    if register && !all_detected.is_empty() {
        tracing::warn!(
            "{} concurrent workspace edit conflict(s); wrote base/ours/theirs under .feanorfs/conflicts/",
            all_detected.len()
        );
        for (c, _) in &all_detected {
            tracing::warn!("  conflict: {}", c.path);
        }
        let (_conflict_dir, new_paths) =
            register_and_write_conflicts(ctx, &all_detected, None).await?;
        blocked.extend(new_paths);
    } else {
        for (c, _) in &all_detected {
            blocked.insert(c.path.clone());
        }
    }

    filter_blocked_paths(&mut response, &blocked);
    Ok((response, blocked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{classify_conflict_kind, detect_concurrent_edits};

    fn st(path: &str, hash: &str, deleted: bool) -> FileState {
        FileState {
            path: path.into(),
            hash: hash.into(),
            size: 1,
            mtime: 1,
            deleted,
            mode: 0,
        }
    }

    #[test]
    fn classify_edit_edit() {
        let base = st("f", "b", false);
        let ours = st("f", "o", false);
        let theirs = st("f", "t", false);
        assert_eq!(
            classify_conflict_kind(&base, Some(&ours), Some(&theirs), false),
            ConflictKind::EditEdit
        );
    }

    #[test]
    fn classify_edit_delete() {
        let base = st("f", "b", false);
        let ours = st("f", "o", false);
        assert_eq!(
            classify_conflict_kind(&base, Some(&ours), None, true),
            ConflictKind::EditDelete
        );
    }

    #[test]
    fn concurrent_delete_not_a_conflict() {
        let base = st("f", "b", false);
        let mut local = HashMap::new();
        local.insert("f".into(), st("f", "b", true));
        let mut their_deleted = HashSet::new();
        their_deleted.insert("f".into());
        let base_map = HashMap::from([("f".into(), base.clone())]);
        let edits = detect_concurrent_edits(
            &base_map,
            &local,
            &HashMap::new(),
            &their_deleted,
            vec!["f".into()],
            &HashSet::new(),
        );
        assert!(edits.is_empty());
    }

    #[test]
    fn filter_blocked_paths_strips_all_buckets() {
        let mut resp = SyncResponse {
            upload_required: vec!["a".into()],
            download_required: vec![st("b", "h", false)],
            delete_local: vec!["c".into()],
        };
        let blocked = HashSet::from(["a".into(), "b".into(), "c".into()]);
        filter_blocked_paths(&mut resp, &blocked);
        assert!(resp.upload_required.is_empty());
        assert!(resp.download_required.is_empty());
        assert!(resp.delete_local.is_empty());
    }

    #[test]
    fn conflicts_pending_uses_db_only() {
        assert!(!conflicts_pending(None));
        assert!(!conflicts_pending(Some(&HashSet::new())));
        assert!(conflicts_pending(Some(&HashSet::from(["x".into()]))));
    }
}
