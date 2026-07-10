use crate::{Tree, TreeChange, TreeChangeKind, TreeEntry};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};

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
        ancestors: Vec::new(),
        changes: Vec::new(),
    };
    traversal.visit_pair(before, after, "")?;
    traversal
        .changes
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(traversal.changes)
}

struct DiffTraversal<F> {
    fetch: F,
    ancestors: Vec<String>,
    changes: Vec<TreeChange>,
}

impl<F> DiffTraversal<F>
where
    F: FnMut(&str) -> Result<Tree>,
{
    fn visit_pair(&mut self, before_hash: &str, after_hash: &str, prefix: &str) -> Result<()> {
        if before_hash == after_hash {
            return Ok(());
        }
        self.enter(before_hash)?;
        self.enter(after_hash)?;
        let before_entries = self.fetch_entries(before_hash)?;
        let after_entries = self.fetch_entries(after_hash)?;
        let names: BTreeSet<_> = before_entries.keys().chain(after_entries.keys()).collect();
        for name in names {
            let before = before_entries.get(name);
            let after = after_entries.get(name);
            let path = join_path(prefix, name);
            match (before, after) {
                (Some(left), Some(right)) if left == right => {}
                (Some(left), Some(right)) if left.is_dir() && right.is_dir() => {
                    self.visit_pair(&left.hash, &right.hash, &path)?;
                }
                (Some(left), Some(right)) => self.changes.push(TreeChange {
                    path,
                    kind: TreeChangeKind::Modified,
                    before: Some(left.clone()),
                    after: Some(right.clone()),
                }),
                (Some(left), None) => self.collect_side(left, &path, TreeChangeKind::Deleted)?,
                (None, Some(right)) => self.collect_side(right, &path, TreeChangeKind::Added)?,
                (None, None) => {}
            }
        }
        self.ancestors.pop();
        self.ancestors.pop();
        Ok(())
    }

    fn collect_side(&mut self, entry: &TreeEntry, path: &str, kind: TreeChangeKind) -> Result<()> {
        if entry.is_dir() {
            self.enter(&entry.hash)?;
            for child in (self.fetch)(&entry.hash)?.entries {
                self.collect_side(&child, &join_path(path, &child.name), kind)?;
            }
            self.ancestors.pop();
        } else {
            self.changes.push(TreeChange {
                path: path.to_string(),
                kind,
                before: (kind == TreeChangeKind::Deleted).then(|| entry.clone()),
                after: (kind == TreeChangeKind::Added).then(|| entry.clone()),
            });
        }
        Ok(())
    }

    fn fetch_entries(&mut self, hash: &str) -> Result<BTreeMap<String, TreeEntry>> {
        Ok((self.fetch)(hash)?
            .entries
            .into_iter()
            .map(|entry| (entry.name.clone(), entry))
            .collect())
    }

    fn enter(&mut self, hash: &str) -> Result<()> {
        if self.ancestors.iter().any(|ancestor| ancestor == hash) {
            bail!("cycle in canonical tree at {hash}");
        }
        self.ancestors.push(hash.to_string());
        Ok(())
    }
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}
