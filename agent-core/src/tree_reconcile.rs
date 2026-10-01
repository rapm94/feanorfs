use crate::{SnapshotEngine, SyncCtx};
use anyhow::Result;
use feanorfs_common::{FileState, SyncResponse, TreeChange};
use std::collections::{HashMap, HashSet};

pub(crate) struct Reconciliation {
    pub base: HashMap<String, FileState>,
    pub response: SyncResponse,
}

pub(crate) async fn reconcile(
    ctx: &SyncCtx<'_>,
    local: &HashMap<String, FileState>,
    remote: &HashMap<String, FileState>,
    server_head: Option<&str>,
) -> Result<Reconciliation> {
    let snapshots = SnapshotEngine::new(ctx);
    let Some(base_id) = snapshots.last_synced_id().await? else {
        let local_changed: HashSet<_> = local.keys().map(String::as_str).collect();
        let remote_changed: HashSet<_> = remote.keys().map(String::as_str).collect();
        return Ok(reconcile_sets(
            HashMap::new(),
            &local_changed,
            &remote_changed,
            local,
            remote,
        ));
    };
    // A restored/fresh hub has no authoritative state while the client holds
    // an agreed view. The empty remote is an absence of data, not a deletion
    // of every file: treat it as "upload the whole local view" instead of
    // deleting the local workspace. Format v3 signals this through the
    // missing head; legacy flat rows have no head marker, so an empty remote
    // with a non-empty agreed base takes the same data-loss-safe direction.
    let restored_hub = if ctx.format_version() >= 3 {
        server_head.is_none()
    } else {
        remote.is_empty()
    };
    if restored_hub {
        let base_files = snapshots.load_files(&base_id).await?;
        if !base_files.is_empty() {
            let mut upload_required = local
                .iter()
                .filter(|(_, state)| !state.deleted)
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            upload_required.sort_unstable();
            upload_required.dedup();
            return Ok(Reconciliation {
                base: base_files,
                response: SyncResponse {
                    upload_required,
                    download_required: Vec::new(),
                    delete_local: Vec::new(),
                },
            });
        }
    }
    let local_changes = snapshots.diff_file_view(&base_id, local).await?.changes;
    let remote_changes = snapshots.diff_file_view(&base_id, remote).await?.changes;
    Ok(reconcile_changes(
        &local_changes,
        &remote_changes,
        local,
        remote,
    ))
}

fn reconcile_changes(
    local_changes: &[TreeChange],
    remote_changes: &[TreeChange],
    local: &HashMap<String, FileState>,
    remote: &HashMap<String, FileState>,
) -> Reconciliation {
    let local_changed: HashSet<_> = local_changes
        .iter()
        .filter(|change| file_content_changed(change))
        .map(|change| change.path.as_str())
        .collect();
    let remote_changed: HashSet<_> = remote_changes
        .iter()
        .filter(|change| file_content_changed(change))
        .map(|change| change.path.as_str())
        .collect();
    let mut base = HashMap::new();
    for change in local_changes.iter().chain(remote_changes) {
        if let Some(entry) = &change.before {
            base.insert(
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
    reconcile_sets(base, &local_changed, &remote_changed, local, remote)
}

// File views flatten a conflict entry into its visible leg. Removing the
// conflict metadata is not a local byte edit. Treating it as one suppresses
// the resolved download and lets the stale local leg be published again.
fn file_content_changed(change: &TreeChange) -> bool {
    match (&change.before, &change.after) {
        (Some(before), Some(after)) if !before.is_dir() && !after.is_dir() => {
            before.hash != after.hash || before.mode != after.mode
        }
        _ => true,
    }
}

fn reconcile_sets(
    base: HashMap<String, FileState>,
    local_changed: &HashSet<&str>,
    remote_changed: &HashSet<&str>,
    local: &HashMap<String, FileState>,
    remote: &HashMap<String, FileState>,
) -> Reconciliation {
    let paths: HashSet<_> = local_changed.union(remote_changed).copied().collect();
    let mut response = SyncResponse {
        upload_required: Vec::new(),
        download_required: Vec::new(),
        delete_local: Vec::new(),
    };
    for path in paths {
        let local_state = local.get(path).filter(|state| !state.deleted);
        let remote_state = remote.get(path).filter(|state| !state.deleted);
        if same_content(local_state, remote_state) {
            continue;
        }
        match (local_changed.contains(path), remote_changed.contains(path)) {
            (true, false) => response.upload_required.push(path.to_string()),
            (false, true) => match remote_state {
                Some(state) => response.download_required.push(state.clone()),
                None => response.delete_local.push(path.to_string()),
            },
            (true, true) | (false, false) => {}
        }
    }
    response.upload_required.sort();
    response
        .download_required
        .sort_by(|left, right| left.path.cmp(&right.path));
    response.delete_local.sort();
    Reconciliation { base, response }
}

fn same_content(left: Option<&FileState>, right: Option<&FileState>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.hash == right.hash && left.mode == right.mode,
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{ConflictModes, TreeChangeKind, TreeEntry, TreeEntryKind};

    #[test]
    fn resolved_conflict_downloads_without_treating_flattening_as_a_local_edit() {
        let before = TreeEntry {
            name: "file.txt".into(),
            kind: TreeEntryKind::Conflict {
                base: Some("c".repeat(64)),
                ours: Some("b".repeat(64)),
                theirs: Some("a".repeat(64)),
                modes: ConflictModes::default(),
            },
            hash: "a".repeat(64),
            size: 9,
            mode: 0,
        };
        let mut local_entry = before.clone();
        local_entry.kind = TreeEntryKind::File;
        let mut remote_entry = local_entry.clone();
        remote_entry.hash = "d".repeat(64);
        let state = |entry: &TreeEntry| FileState {
            path: entry.name.clone(),
            hash: entry.hash.clone(),
            size: entry.size,
            mtime: 0,
            deleted: false,
            mode: entry.mode,
        };
        let change = |entry: &TreeEntry| TreeChange {
            path: entry.name.clone(),
            kind: TreeChangeKind::Modified,
            before: Some(before.clone()),
            after: Some(entry.clone()),
        };
        let remote = HashMap::from([("file.txt".into(), state(&remote_entry))]);
        let response = reconcile_changes(
            &[change(&local_entry)],
            &[change(&remote_entry)],
            &HashMap::from([("file.txt".into(), state(&local_entry))]),
            &remote,
        )
        .response;
        assert_eq!(response.download_required, vec![state(&remote_entry)]);
        assert!(response.upload_required.is_empty());
        // A real newer edit, including executable intent, stays protected.
        local_entry.mode = 0o111;
        let response = reconcile_changes(
            &[change(&local_entry)],
            &[change(&remote_entry)],
            &HashMap::from([("file.txt".into(), state(&local_entry))]),
            &remote,
        )
        .response;
        assert!(response.download_required.is_empty());
        assert!(response.upload_required.is_empty());
    }
}
