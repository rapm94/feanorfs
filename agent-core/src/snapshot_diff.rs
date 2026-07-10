use crate::prepared_tree::PreparedTreeBundle;
use crate::SnapshotEngine;
use anyhow::Result;
use feanorfs_common::{
    flat_to_tree, flat_to_tree_with_conflicts, ConcurrentEdit, FileState, Tree, TreeChange,
    TreeChangeKind, TreeEntry,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Hash-pruned diff result with observable tree-read cost.
pub struct TreeDiffStats {
    pub changes: Vec<TreeChange>,
    pub object_reads: usize,
}

enum Work {
    Pair(String, String, String),
    Side(TreeEntry, String, TreeChangeKind),
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
        let mut object_reads = 0;
        let mut work = vec![Work::Pair(
            before_root.to_string(),
            after_root.to_string(),
            String::new(),
        )];
        while let Some(next) = work.pop() {
            match next {
                Work::Pair(before, after, prefix) => {
                    if before == after {
                        continue;
                    }
                    let left = self.objects.get_tree(&before).await?;
                    object_reads += 1;
                    let right = match prepared.and_then(|bundle| bundle.trees.get(&after)) {
                        Some(tree) => tree.clone(),
                        None => {
                            object_reads += 1;
                            self.objects.get_tree(&after).await?
                        }
                    };
                    compare_entries(left, right, &prefix, &mut work, &mut changes);
                }
                Work::Side(entry, path, kind) => {
                    if entry.is_dir() {
                        let tree = match prepared.and_then(|bundle| bundle.trees.get(&entry.hash)) {
                            Some(tree) => tree.clone(),
                            None => {
                                object_reads += 1;
                                self.objects.get_tree(&entry.hash).await?
                            }
                        };
                        for child in tree.entries.into_iter().rev() {
                            work.push(Work::Side(child.clone(), join(&path, &child.name), kind));
                        }
                    } else {
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
    work: &mut Vec<Work>,
    changes: &mut Vec<TreeChange>,
) {
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
        let path = join(prefix, name);
        match (left, right) {
            (Some(left), Some(right)) if left == right => {}
            (Some(left), Some(right)) if left.is_dir() && right.is_dir() => {
                work.push(Work::Pair(left.hash.clone(), right.hash.clone(), path));
            }
            (Some(left), Some(right)) if left.is_dir() || right.is_dir() => {
                work.push(Work::Side(
                    left.clone(),
                    path.clone(),
                    TreeChangeKind::Deleted,
                ));
                work.push(Work::Side(right.clone(), path, TreeChangeKind::Added));
            }
            (Some(left), Some(right)) => changes.push(TreeChange {
                path,
                kind: TreeChangeKind::Modified,
                before: Some(left.clone()),
                after: Some(right.clone()),
            }),
            (Some(left), None) => {
                work.push(Work::Side(left.clone(), path, TreeChangeKind::Deleted));
            }
            (None, Some(right)) => {
                work.push(Work::Side(right.clone(), path, TreeChangeKind::Added));
            }
            (None, None) => {}
        }
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}
