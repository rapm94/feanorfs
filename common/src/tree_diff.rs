use crate::{
    is_safe_rel_path, Tree, TreeChange, TreeChangeKind, TreeEntry, MAX_TREE_DEPTH,
    MAX_TREE_OBJECTS, MAX_TREE_OUTPUT_PATHS, MAX_TREE_PATH_BYTES_TOTAL, MAX_TREE_WORK_ITEMS,
};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Diffs two canonical roots while skipping identical subtree hashes.
///
/// # Errors
/// Returns an error when either tree cannot be fetched or contains a cycle.
pub fn diff_trees<F>(before: &str, after: &str, fetch: F) -> Result<Vec<TreeChange>>
where
    F: FnMut(&str) -> Result<Tree>,
{
    if before == after {
        return Ok(Vec::new());
    }
    let mut traversal = DiffTraversal {
        fetch,
        before_active: HashSet::new(),
        after_active: HashSet::new(),
        objects: HashSet::new(),
        work_items: 0,
        constructed_path_bytes: 0,
        pending_path_bytes: 0,
        changes: Vec::new(),
    };
    traversal.run(before, after)?;
    traversal
        .changes
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(traversal.changes)
}

#[derive(Clone, Copy)]
enum DiffSide {
    Before,
    After,
}

enum DiffWork {
    Pair {
        before_hash: String,
        after_hash: String,
        prefix: String,
        depth: usize,
    },
    CollectDir {
        hash: String,
        path: String,
        kind: TreeChangeKind,
        side: DiffSide,
        depth: usize,
    },
    Exit {
        hash: String,
        side: DiffSide,
    },
}

impl DiffWork {
    fn pending_path_len(&self) -> usize {
        match self {
            Self::Pair { prefix, .. } => prefix.len(),
            Self::CollectDir { path, .. } => path.len(),
            Self::Exit { .. } => 0,
        }
    }
}

struct DiffTraversal<F> {
    fetch: F,
    before_active: HashSet<String>,
    after_active: HashSet<String>,
    objects: HashSet<String>,
    work_items: usize,
    constructed_path_bytes: usize,
    pending_path_bytes: usize,
    changes: Vec<TreeChange>,
}

impl<F> DiffTraversal<F>
where
    F: FnMut(&str) -> Result<Tree>,
{
    fn run(&mut self, before: &str, after: &str) -> Result<()> {
        let mut pending = vec![DiffWork::Pair {
            before_hash: before.to_string(),
            after_hash: after.to_string(),
            prefix: String::new(),
            depth: 0,
        }];
        while let Some(work) = pending.pop() {
            self.release_pending_path(work.pending_path_len())?;
            match work {
                DiffWork::Pair {
                    before_hash,
                    after_hash,
                    prefix,
                    depth,
                } => self.process_pair(&mut pending, before_hash, after_hash, prefix, depth)?,
                DiffWork::CollectDir {
                    hash,
                    path,
                    kind,
                    side,
                    depth,
                } => {
                    self.process_collect_dir(&mut pending, hash, path, kind, side, depth)?;
                }
                DiffWork::Exit { hash, side } => self.exit(side, &hash)?,
            }
        }
        debug_assert!(self.before_active.is_empty());
        debug_assert!(self.after_active.is_empty());
        debug_assert_eq!(self.pending_path_bytes, 0);
        Ok(())
    }

    fn process_pair(
        &mut self,
        pending: &mut Vec<DiffWork>,
        before_hash: String,
        after_hash: String,
        prefix: String,
        depth: usize,
    ) -> Result<()> {
        if before_hash == after_hash {
            return Ok(());
        }
        self.enter(DiffSide::Before, &before_hash, depth)?;
        self.enter(DiffSide::After, &after_hash, depth)?;
        let before_entries = self.fetch_entries(&before_hash, depth)?;
        let after_entries = self.fetch_entries(&after_hash, depth)?;
        let names: BTreeSet<_> = before_entries.keys().chain(after_entries.keys()).collect();
        let mut children = Vec::new();
        for name in names {
            let before = before_entries.get(name);
            let after = after_entries.get(name);
            let path = self.construct_path(&prefix, name, depth + 1)?;
            match (before, after) {
                (Some(left), Some(right)) if left == right => {}
                (Some(left), Some(right)) if left.is_dir() && right.is_dir() => {
                    self.queue_work(
                        &mut children,
                        DiffWork::Pair {
                            before_hash: left.hash.clone(),
                            after_hash: right.hash.clone(),
                            prefix: path,
                            depth: depth + 1,
                        },
                    )?;
                }
                (Some(left), Some(right)) if left.is_dir() || right.is_dir() => {
                    self.collect_or_change(
                        &mut children,
                        left,
                        &path,
                        TreeChangeKind::Deleted,
                        DiffSide::Before,
                        depth + 1,
                    )?;
                    self.collect_or_change(
                        &mut children,
                        right,
                        &path,
                        TreeChangeKind::Added,
                        DiffSide::After,
                        depth + 1,
                    )?;
                }
                (Some(left), Some(right)) => self.push_change(TreeChange {
                    path,
                    kind: TreeChangeKind::Modified,
                    before: Some(left.clone()),
                    after: Some(right.clone()),
                })?,
                (Some(left), None) => self.collect_or_change(
                    &mut children,
                    left,
                    &path,
                    TreeChangeKind::Deleted,
                    DiffSide::Before,
                    depth + 1,
                )?,
                (None, Some(right)) => self.collect_or_change(
                    &mut children,
                    right,
                    &path,
                    TreeChangeKind::Added,
                    DiffSide::After,
                    depth + 1,
                )?,
                (None, None) => {}
            }
        }

        pending.push(DiffWork::Exit {
            hash: before_hash,
            side: DiffSide::Before,
        });
        pending.push(DiffWork::Exit {
            hash: after_hash,
            side: DiffSide::After,
        });
        pending.extend(children.into_iter().rev());
        Ok(())
    }

    fn process_collect_dir(
        &mut self,
        pending: &mut Vec<DiffWork>,
        hash: String,
        path: String,
        kind: TreeChangeKind,
        side: DiffSide,
        depth: usize,
    ) -> Result<()> {
        self.enter(side, &hash, depth)?;
        let tree = self.fetch_tree(&hash, depth)?;
        let mut children = Vec::new();
        for child in tree.entries {
            let child_path = self.construct_path(&path, &child.name, depth + 1)?;
            self.collect_or_change(&mut children, &child, &child_path, kind, side, depth + 1)?;
        }
        pending.push(DiffWork::Exit { hash, side });
        pending.extend(children.into_iter().rev());
        Ok(())
    }

    fn collect_or_change(
        &mut self,
        children: &mut Vec<DiffWork>,
        entry: &TreeEntry,
        path: &str,
        kind: TreeChangeKind,
        side: DiffSide,
        depth: usize,
    ) -> Result<()> {
        if entry.is_dir() {
            self.queue_work(
                children,
                DiffWork::CollectDir {
                    hash: entry.hash.clone(),
                    path: path.to_string(),
                    kind,
                    side,
                    depth,
                },
            )
        } else {
            self.push_change(TreeChange {
                path: path.to_string(),
                kind,
                before: (kind == TreeChangeKind::Deleted).then(|| entry.clone()),
                after: (kind == TreeChangeKind::Added).then(|| entry.clone()),
            })
        }
    }

    fn fetch_tree(&mut self, hash: &str, depth: usize) -> Result<Tree> {
        if self.objects.insert(hash.to_string()) && self.objects.len() > MAX_TREE_OBJECTS {
            bail!("canonical tree diff exceeds {MAX_TREE_OBJECTS} distinct tree objects");
        }
        let tree = (self.fetch)(hash)?;
        tree.validate()?;
        if depth > 0 && tree.entries.is_empty() {
            bail!("canonical tree contains a non-root empty directory");
        }
        self.work_items = self
            .work_items
            .checked_add(tree.entries.len().saturating_add(1))
            .context("canonical tree diff work counter overflow")?;
        if self.work_items > MAX_TREE_WORK_ITEMS {
            bail!("canonical tree diff exceeds {MAX_TREE_WORK_ITEMS} work items");
        }
        Ok(tree)
    }

    fn push_change(&mut self, change: TreeChange) -> Result<()> {
        if self.changes.len() >= MAX_TREE_OUTPUT_PATHS {
            bail!("canonical tree diff exceeds {MAX_TREE_OUTPUT_PATHS} output paths");
        }
        self.changes.push(change);
        Ok(())
    }

    fn fetch_entries(&mut self, hash: &str, depth: usize) -> Result<BTreeMap<String, TreeEntry>> {
        Ok(self
            .fetch_tree(hash, depth)?
            .entries
            .into_iter()
            .map(|entry| (entry.name.clone(), entry))
            .collect())
    }

    fn construct_path(&mut self, prefix: &str, name: &str, depth: usize) -> Result<String> {
        let path = join_path(prefix, name);
        validate_output_path(&path, depth)?;
        self.constructed_path_bytes = self
            .constructed_path_bytes
            .checked_add(path.len())
            .context("canonical tree diff constructed-path counter overflow")?;
        if self.constructed_path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
            bail!("canonical tree diff exceeds aggregate path-byte limit");
        }
        Ok(path)
    }

    fn queue_work(&mut self, children: &mut Vec<DiffWork>, work: DiffWork) -> Result<()> {
        self.pending_path_bytes = self
            .pending_path_bytes
            .checked_add(work.pending_path_len())
            .context("canonical tree diff pending-path counter overflow")?;
        if self.pending_path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
            bail!("canonical tree diff exceeds pending path-byte limit");
        }
        children.push(work);
        Ok(())
    }

    fn release_pending_path(&mut self, amount: usize) -> Result<()> {
        self.pending_path_bytes = self
            .pending_path_bytes
            .checked_sub(amount)
            .context("canonical tree diff pending-path counter underflow")?;
        Ok(())
    }

    fn enter(&mut self, side: DiffSide, hash: &str, depth: usize) -> Result<()> {
        if depth > MAX_TREE_DEPTH {
            bail!("canonical tree diff exceeds {MAX_TREE_DEPTH} directory levels");
        }
        let active = match side {
            DiffSide::Before => &mut self.before_active,
            DiffSide::After => &mut self.after_active,
        };
        if !active.insert(hash.to_string()) {
            bail!("cycle in canonical tree at {hash}");
        }
        Ok(())
    }

    fn exit(&mut self, side: DiffSide, hash: &str) -> Result<()> {
        let active = match side {
            DiffSide::Before => &mut self.before_active,
            DiffSide::After => &mut self.after_active,
        };
        if !active.remove(hash) {
            bail!("canonical tree diff lost active object {hash}");
        }
        Ok(())
    }
}

fn validate_output_path(path: &str, depth: usize) -> Result<()> {
    if depth > MAX_TREE_DEPTH || !is_safe_rel_path(path) {
        bail!("canonical tree diff produced an unsafe or oversized path");
    }
    Ok(())
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}
