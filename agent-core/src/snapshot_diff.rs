use crate::prepared_tree::PreparedTreeBundle;
use crate::SnapshotEngine;
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    flat_to_tree, flat_to_tree_with_conflicts, is_safe_rel_path, ConcurrentEdit, FileState, Tree,
    TreeChange, TreeChangeKind, TreeEntry, MAX_TREE_DEPTH, MAX_TREE_OBJECTS, MAX_TREE_OUTPUT_PATHS,
    MAX_TREE_PATH_BYTES_TOTAL, MAX_TREE_WORK_ITEMS,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Hash-pruned diff result with observable tree-read cost.
pub struct TreeDiffStats {
    pub changes: Vec<TreeChange>,
    pub object_reads: usize,
}

#[derive(Clone, Copy)]
enum SideSource {
    Before,
    After,
}

enum Work {
    PairEnter {
        before: String,
        after: String,
        prefix: String,
        depth: usize,
    },
    PairExit {
        before: String,
        after: String,
    },
    SideEnter {
        entry: TreeEntry,
        path: String,
        kind: TreeChangeKind,
        source: SideSource,
        depth: usize,
    },
    SideExit {
        id: String,
        source: SideSource,
    },
}

impl<'ctx, 'a> SnapshotEngine<'ctx, 'a> {
    pub(crate) fn candidate_root(
        &self,
        files: &HashMap<String, FileState>,
        conflicts: &[ConcurrentEdit],
    ) -> Result<String> {
        Ok(PreparedTreeBundle::new(
            &flat_to_tree_with_conflicts(files, conflicts)?,
            self.ctx.password_str(),
        )?
        .root)
    }

    /// Diffs one snapshot against a flat candidate without uploading candidate objects.
    ///
    /// # Errors
    /// Returns an error when source objects or candidate paths are invalid.
    pub async fn diff_file_view(
        &self,
        snapshot_id: &str,
        files: &HashMap<String, FileState>,
    ) -> Result<TreeDiffStats> {
        let snapshot = self.load_snapshot(snapshot_id).await?;
        let prepared = PreparedTreeBundle::new(&flat_to_tree(files)?, self.ctx.password_str())?;
        self.diff_roots(&snapshot.root, &prepared.root, Some(&prepared))
            .await
    }

    pub(crate) async fn diff_snapshots(
        &self,
        before_id: &str,
        after_id: &str,
    ) -> Result<TreeDiffStats> {
        let before = self.load_snapshot(before_id).await?;
        let after = self.load_snapshot(after_id).await?;
        self.diff_roots(&before.root, &after.root, None).await
    }

    async fn diff_roots(
        &self,
        before_root: &str,
        after_root: &str,
        prepared: Option<&PreparedTreeBundle>,
    ) -> Result<TreeDiffStats> {
        if before_root == after_root {
            return Ok(TreeDiffStats {
                changes: Vec::new(),
                object_reads: 0,
            });
        }
        let mut changes = Vec::new();
        let mut object_reads = 0_usize;
        let mut work_items = 0_usize;
        let mut path_bytes = 0_usize;
        let mut objects = std::collections::HashSet::new();
        let mut active_before = std::collections::HashSet::new();
        let mut active_after = std::collections::HashSet::new();
        let mut work = vec![Work::PairEnter {
            before: before_root.to_string(),
            after: after_root.to_string(),
            prefix: String::new(),
            depth: 0,
        }];
        while let Some(next) = work.pop() {
            work_items = work_items
                .checked_add(1)
                .context("tree diff work counter overflow")?;
            if work_items > MAX_TREE_WORK_ITEMS {
                bail!("tree diff exceeds traversal work limit");
            }
            match next {
                Work::PairExit { before, after } => {
                    active_before.remove(&before);
                    active_after.remove(&after);
                }
                Work::SideExit { id, source } => {
                    active_set(source, &mut active_before, &mut active_after).remove(&id);
                }
                Work::PairEnter {
                    before,
                    after,
                    prefix,
                    depth,
                } => {
                    if before == after {
                        continue;
                    }
                    if depth > MAX_TREE_DEPTH
                        || !active_before.insert(before.clone())
                        || !active_after.insert(after.clone())
                    {
                        bail!("tree diff encountered a cycle or excessive depth");
                    }
                    track_object(&mut objects, &before)?;
                    track_object(&mut objects, &after)?;
                    work.push(Work::PairExit {
                        before: before.clone(),
                        after: after.clone(),
                    });
                    let left = self.objects.get_tree(&before).await?;
                    object_reads += 1;
                    let right = match prepared.and_then(|bundle| bundle.trees.get(&after)) {
                        Some(tree) => tree.clone(),
                        None => {
                            object_reads += 1;
                            self.objects.get_tree(&after).await?
                        }
                    };
                    compare_entries(
                        left,
                        right,
                        &prefix,
                        depth,
                        &mut work,
                        &mut changes,
                        &mut path_bytes,
                    )?;
                }
                Work::SideEnter {
                    entry,
                    path,
                    kind,
                    source,
                    depth,
                } => {
                    if entry.is_dir() {
                        if depth > MAX_TREE_DEPTH
                            || !active_set(source, &mut active_before, &mut active_after)
                                .insert(entry.hash.clone())
                        {
                            bail!("tree diff encountered a cycle or excessive depth");
                        }
                        track_object(&mut objects, &entry.hash)?;
                        work.push(Work::SideExit {
                            id: entry.hash.clone(),
                            source,
                        });
                        let tree = match prepared.and_then(|bundle| bundle.trees.get(&entry.hash)) {
                            Some(tree) => tree.clone(),
                            None => {
                                object_reads += 1;
                                self.objects.get_tree(&entry.hash).await?
                            }
                        };
                        work_items = work_items
                            .checked_add(tree.entries.len())
                            .context("tree diff work counter overflow")?;
                        if work_items > MAX_TREE_WORK_ITEMS {
                            bail!("tree diff exceeds traversal work limit");
                        }
                        for child in tree.entries.into_iter().rev() {
                            let child_path = bounded_join(&path, &child.name, &mut path_bytes)?;
                            work.push(Work::SideEnter {
                                entry: child,
                                path: child_path,
                                kind,
                                source,
                                depth: depth + 1,
                            });
                        }
                    } else {
                        if changes.len() >= MAX_TREE_OUTPUT_PATHS {
                            bail!("tree diff exceeds output limit");
                        }
                        changes.push(TreeChange {
                            path,
                            kind,
                            before: (kind == TreeChangeKind::Deleted).then_some(entry.clone()),
                            after: (kind == TreeChangeKind::Added).then_some(entry),
                        });
                    }
                }
            }
        }
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(TreeDiffStats {
            changes,
            object_reads,
        })
    }
}

fn compare_entries(
    before: Tree,
    after: Tree,
    prefix: &str,
    depth: usize,
    work: &mut Vec<Work>,
    changes: &mut Vec<TreeChange>,
    path_bytes: &mut usize,
) -> Result<()> {
    let before: BTreeMap<_, _> = before
        .entries
        .into_iter()
        .map(|entry| (entry.name.clone(), entry))
        .collect();
    let after: BTreeMap<_, _> = after
        .entries
        .into_iter()
        .map(|entry| (entry.name.clone(), entry))
        .collect();
    let names: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    for name in names.into_iter().rev() {
        let left = before.get(name);
        let right = after.get(name);
        let path = bounded_join(prefix, name, path_bytes)?;
        match (left, right) {
            (Some(left), Some(right)) if left == right => {}
            (Some(left), Some(right)) if left.is_dir() && right.is_dir() => {
                work.push(Work::PairEnter {
                    before: left.hash.clone(),
                    after: right.hash.clone(),
                    prefix: path,
                    depth: depth + 1,
                });
            }
            (Some(left), Some(right)) if left.is_dir() || right.is_dir() => {
                work.push(Work::SideEnter {
                    entry: left.clone(),
                    path: path.clone(),
                    kind: TreeChangeKind::Deleted,
                    source: SideSource::Before,
                    depth: depth + 1,
                });
                work.push(Work::SideEnter {
                    entry: right.clone(),
                    path,
                    kind: TreeChangeKind::Added,
                    source: SideSource::After,
                    depth: depth + 1,
                });
            }
            (Some(left), Some(right)) => {
                if changes.len() >= MAX_TREE_OUTPUT_PATHS {
                    bail!("tree diff exceeds output limit");
                }
                changes.push(TreeChange {
                    path,
                    kind: TreeChangeKind::Modified,
                    before: Some(left.clone()),
                    after: Some(right.clone()),
                });
            }
            (Some(left), None) => work.push(Work::SideEnter {
                entry: left.clone(),
                path,
                kind: TreeChangeKind::Deleted,
                source: SideSource::Before,
                depth: depth + 1,
            }),
            (None, Some(right)) => work.push(Work::SideEnter {
                entry: right.clone(),
                path,
                kind: TreeChangeKind::Added,
                source: SideSource::After,
                depth: depth + 1,
            }),
            (None, None) => {}
        }
    }
    Ok(())
}

fn active_set<'a>(
    source: SideSource,
    before: &'a mut std::collections::HashSet<String>,
    after: &'a mut std::collections::HashSet<String>,
) -> &'a mut std::collections::HashSet<String> {
    match source {
        SideSource::Before => before,
        SideSource::After => after,
    }
}

fn track_object(objects: &mut std::collections::HashSet<String>, id: &str) -> Result<()> {
    if objects.insert(id.to_string()) && objects.len() > MAX_TREE_OBJECTS {
        bail!("tree diff exceeds distinct-object limit");
    }
    Ok(())
}

fn bounded_join(prefix: &str, name: &str, path_bytes: &mut usize) -> Result<String> {
    let path = if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    };
    if !is_safe_rel_path(&path) || path.split('/').count() > MAX_TREE_DEPTH {
        bail!("tree diff produced an unsafe or oversized path");
    }
    *path_bytes = path_bytes
        .checked_add(path.len())
        .context("tree diff path counter overflow")?;
    if *path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
        bail!("tree diff paths exceed aggregate byte limit");
    }
    Ok(path)
}
