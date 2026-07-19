use anyhow::{bail, Result};
use feanorfs_common::{detect_concurrent_edits, ConcurrentEdit, ConflictKind, FileState};
use std::collections::{HashMap, HashSet};

use crate::ctx::SyncCtx;
use crate::local::ClientDb;
use crate::paths::{agent_dir, validate_name};
use crate::snapshot::SnapshotEngine;

pub(super) struct AgentDiff {
    pub(super) current_head: String,
    pub(super) our_changes: Vec<FileState>,
    pub(super) their_changes: Vec<FileState>,
    pub(super) conflicts: Vec<(ConcurrentEdit, ConflictKind)>,
    pub(super) conflict_risk: Vec<String>,
}

pub(super) struct LandCandidate {
    pub(super) files: HashMap<String, FileState>,
    pub(super) conflicts: Vec<ConcurrentEdit>,
    pub(super) root: String,
}

pub(super) async fn build_land_candidate(
    snapshots: &SnapshotEngine<'_, '_>,
    diff: &AgentDiff,
) -> Result<LandCandidate> {
    let current_state = snapshots.load_state(&diff.current_head).await?;
    let mut files = current_state.files;
    for change in &diff.our_changes {
        if change.deleted {
            files.remove(&change.path);
        } else {
            files.insert(change.path.clone(), change.clone());
        }
    }
    let changed_paths = diff
        .our_changes
        .iter()
        .map(|change| change.path.as_str())
        .chain(diff.conflicts.iter().map(|(edit, _)| edit.path.as_str()))
        .collect::<HashSet<_>>();
    let mut conflicts = current_state
        .conflicts
        .into_iter()
        .filter(|edit| !changed_paths.contains(edit.path.as_str()))
        .collect::<Vec<_>>();
    conflicts.extend(diff.conflicts.iter().map(|(edit, _)| edit.clone()));
    let root = snapshots.candidate_root(&files, &conflicts)?;
    Ok(LandCandidate {
        files,
        conflicts,
        root,
    })
}

pub(super) async fn compute_agent_diff(ctx: &SyncCtx<'_>, name: &str) -> Result<AgentDiff> {
    validate_name(name)?;
    let agent_path = agent_dir(ctx.base, name)?;
    if !agent_path.exists() {
        bail!("Agent workspace '{name}' does not exist. Run `feanorfs agent spawn {name}` first.");
    }
    let snapshots = SnapshotEngine::new(ctx);
    let base_id = snapshots.read_agent_base(name).await?;
    let current_head = match ctx.api.get_head(ctx.workspace_id()).await? {
        Some(head) => head,
        None => {
            let legacy = crate::conflicts::load_server_view(ctx).await?;
            snapshots
                .publish_server_view(&legacy, "legacy-import")
                .await?
        }
    };
    let server_files = snapshots.load_files(&current_head).await?;
    let agent_cache = ClientDb::new(crate::workspace_layout::ensure_workspace_state(
        &agent_path,
    )?)
    .await?;
    let agent_scan =
        crate::local::scan_local_directory(&agent_path, &agent_cache, ctx.password()).await?;
    let our_diff = snapshots.diff_file_view(&base_id, &agent_scan).await?;
    let their_diff = snapshots.diff_file_view(&base_id, &server_files).await?;

    let mut base_map = HashMap::new();
    for change in our_diff.changes.iter().chain(&their_diff.changes) {
        if let Some(entry) = &change.before {
            base_map.insert(
                change.path.clone(),
                FileState {
                    path: change.path.clone(),
                    hash: entry.hash.clone(),
                    size: entry.size,
                    mtime: 0,
                    deleted: false,
                    mode: entry.mode,
                },
            );
        }
    }
    let their_changed = their_diff
        .changes
        .iter()
        .filter_map(|change| {
            change.after.as_ref().map(|entry| {
                (
                    change.path.clone(),
                    FileState {
                        path: change.path.clone(),
                        hash: entry.hash.clone(),
                        size: entry.size,
                        mtime: 0,
                        deleted: false,
                        mode: entry.mode,
                    },
                )
            })
        })
        .collect::<HashMap<_, _>>();
    let their_deleted = their_diff
        .changes
        .iter()
        .filter(|change| change.after.is_none())
        .map(|change| change.path.clone())
        .collect::<HashSet<_>>();
    let mut local_view = agent_scan.clone();
    for change in &our_diff.changes {
        if change.after.is_none() {
            if let Some(base_entry) = base_map.get(&change.path) {
                local_view.insert(
                    change.path.clone(),
                    FileState {
                        path: change.path.clone(),
                        hash: base_entry.hash.clone(),
                        size: base_entry.size,
                        mtime: base_entry.mtime,
                        deleted: true,
                        mode: base_entry.mode,
                    },
                );
            }
        }
    }
    let our_changed_paths = our_diff
        .changes
        .iter()
        .map(|change| change.path.clone())
        .collect::<HashSet<_>>();
    let conflicts = detect_concurrent_edits(
        &base_map,
        &local_view,
        &their_changed,
        &their_deleted,
        our_changed_paths.iter().cloned(),
        &HashSet::new(),
    );
    let conflict_paths = conflicts
        .iter()
        .map(|(edit, _)| edit.path.clone())
        .collect::<HashSet<_>>();
    let mut our_changes = Vec::new();
    for path in &our_changed_paths {
        if conflict_paths.contains(path) {
            continue;
        }
        if let Some(file) = agent_scan.get(path) {
            our_changes.push(file.clone());
        } else if let Some(base_entry) = base_map.get(path) {
            our_changes.push(FileState {
                path: path.clone(),
                hash: base_entry.hash.clone(),
                size: base_entry.size,
                mtime: base_entry.mtime,
                deleted: true,
                mode: base_entry.mode,
            });
        }
    }
    let mut their_changes = their_changed
        .iter()
        .filter(|(path, _)| !our_changed_paths.contains(*path))
        .map(|(_, file)| file.clone())
        .collect::<Vec<_>>();
    for path in &their_deleted {
        if !our_changed_paths.contains(path) {
            if let Some(base_entry) = base_map.get(path) {
                their_changes.push(FileState {
                    path: path.clone(),
                    hash: base_entry.hash.clone(),
                    size: base_entry.size,
                    mtime: 0,
                    deleted: true,
                    mode: base_entry.mode,
                });
            }
        }
    }
    let conflict_risk = their_changed
        .keys()
        .filter(|path| !our_changed_paths.contains(*path))
        .filter(|path| {
            base_map.get(*path).is_some_and(|base_entry| {
                agent_scan.get(*path).is_some_and(|agent_file| {
                    agent_file.hash == base_entry.hash && agent_file.mode == base_entry.mode
                })
            })
        })
        .cloned()
        .collect();
    Ok(AgentDiff {
        current_head,
        our_changes,
        their_changes,
        conflicts,
        conflict_risk,
    })
}
