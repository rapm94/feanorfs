use crate::{ConcurrentEdit, ConflictKind, FileState, SyncResponse};
use std::collections::{HashMap, HashSet};

/// Classify a three-way conflict (base / ours / theirs).
#[must_use]
pub fn classify_conflict_kind(
    base: &FileState,
    ours: Option<&FileState>,
    theirs: Option<&FileState>,
    their_deleted: bool,
) -> ConflictKind {
    let we_deleted = ours.is_some_and(|o| o.deleted);
    let they_deleted = their_deleted || theirs.is_some_and(|t| t.deleted);
    if we_deleted && !they_deleted && !base.deleted {
        ConflictKind::DeleteEdit
    } else if !we_deleted && they_deleted && !base.deleted {
        ConflictKind::EditDelete
    } else {
        ConflictKind::EditEdit
    }
}

fn theirs_state(
    path: &str,
    their_changed: &HashMap<String, FileState>,
    their_deleted: bool,
) -> Option<FileState> {
    their_changed.get(path).cloned().or_else(|| {
        if their_deleted {
            Some(FileState {
                path: path.to_string(),
                hash: String::new(),
                size: 0,
                mtime: 0,
                deleted: true,
            })
        } else {
            None
        }
    })
}

/// Detect concurrent edits given a base snapshot, current local view, and server
/// changes since base (from a peek with the base as the client view).
pub fn detect_concurrent_edits(
    base: &HashMap<String, FileState>,
    local: &HashMap<String, FileState>,
    their_changed: &HashMap<String, FileState>,
    their_deleted: &HashSet<String>,
    candidate_paths: impl IntoIterator<Item = String>,
    already_pending: &HashSet<String>,
) -> Vec<(ConcurrentEdit, ConflictKind)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for path in candidate_paths {
        if !seen.insert(path.clone()) {
            continue;
        }

        let ours = local.get(&path);
        let base_entry = base.get(&path);

        if let Some(base_entry) = base_entry {
            let we_changed = ours
                .map(|o| o.hash != base_entry.hash || o.deleted != base_entry.deleted)
                .unwrap_or(false);
            let server_changed = their_changed.contains_key(&path) || their_deleted.contains(&path);

            if we_changed && server_changed {
                if their_deleted.contains(&path) && ours.is_some_and(|o| o.deleted) {
                    continue;
                }
                let kind = classify_conflict_kind(
                    base_entry,
                    ours,
                    their_changed.get(&path),
                    their_deleted.contains(&path),
                );
                out.push((
                    ConcurrentEdit::new(
                        path.clone(),
                        Some(base_entry.clone()),
                        ours.cloned(),
                        theirs_state(&path, their_changed, their_deleted.contains(&path)),
                    ),
                    kind,
                ));
            }
        } else if already_pending.contains(&path) {
            if let (Some(o), Some(t)) = (ours, their_changed.get(&path)) {
                if o.hash != t.hash {
                    out.push((
                        ConcurrentEdit::new(path.clone(), None, Some(o.clone()), Some(t.clone())),
                        ConflictKind::EditEdit,
                    ));
                }
            }
        }
    }

    out
}

/// Collect candidate paths from a sync response plus any already-blocked paths.
#[must_use]
pub fn conflict_candidate_paths(
    response: &SyncResponse,
    already_pending: &HashSet<String>,
) -> Vec<String> {
    response
        .download_required
        .iter()
        .map(|f| f.path.clone())
        .chain(response.upload_required.iter().cloned())
        .chain(response.delete_local.iter().cloned())
        .chain(already_pending.iter().cloned())
        .collect()
}
