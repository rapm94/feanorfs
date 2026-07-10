use crate::{
    is_safe_rel_path, ConcurrentEdit, FileState, Tree, TreeBundle, TreeEntry, TreeEntryKind,
    EXECUTABLE_MODE,
};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
struct Node {
    files: BTreeMap<String, FileState>,
    conflicts: BTreeMap<String, TreeEntry>,
    directories: BTreeMap<String, Node>,
}

/// Converts normalized live file states into bottom-up canonical trees.
///
/// Deleted states are absent from snapshots and therefore ignored.
///
/// # Errors
/// Returns an error for unsafe paths, file/directory collisions, or invalid modes.
pub fn flat_to_tree(files: &HashMap<String, FileState>) -> Result<TreeBundle> {
    flat_to_tree_with_conflicts(files, &[])
}

/// Builds canonical trees with first-class conflicts overlaid on the live file view.
///
/// # Errors
/// Returns an error for invalid paths, missing conflict legs, or path collisions.
pub fn flat_to_tree_with_conflicts(
    files: &HashMap<String, FileState>,
    conflicts: &[ConcurrentEdit],
) -> Result<TreeBundle> {
    let mut root = Node::default();
    for state in files.values().filter(|state| !state.deleted) {
        insert_state(&mut root, state)?;
    }
    for conflict in conflicts {
        insert_conflict(&mut root, conflict)?;
    }
    let mut trees = HashMap::new();
    let root = build_node(root, &mut trees);
    Ok(TreeBundle { root, trees })
}

fn insert_conflict(root: &mut Node, conflict: &ConcurrentEdit) -> Result<()> {
    validate_path(&conflict.path)?;
    let representative = conflict
        .theirs
        .as_ref()
        .filter(|state| !state.deleted)
        .or_else(|| conflict.ours.as_ref().filter(|state| !state.deleted))
        .or_else(|| conflict.base.as_ref().filter(|state| !state.deleted))
        .context("conflict has no content leg")?;
    let entry = TreeEntry {
        name: String::new(),
        kind: TreeEntryKind::Conflict {
            base: live_hash(conflict.base.as_ref()),
            ours: live_hash(conflict.ours.as_ref()),
            theirs: live_hash(conflict.theirs.as_ref()),
        },
        hash: representative.hash.clone(),
        size: representative.size,
        mode: 0,
    };
    let mut parts = conflict.path.split('/').peekable();
    let mut node = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if node.directories.contains_key(part) || node.conflicts.contains_key(part) {
                bail!(
                    "conflict path collides with existing entry: {:?}",
                    conflict.path
                );
            }
            node.files.remove(part);
            node.conflicts.insert(
                part.to_string(),
                TreeEntry {
                    name: part.to_string(),
                    ..entry.clone()
                },
            );
        } else {
            if node.files.contains_key(part) || node.conflicts.contains_key(part) {
                bail!(
                    "conflict path traverses file component: {:?}",
                    conflict.path
                );
            }
            node = node.directories.entry(part.to_string()).or_default();
        }
    }
    Ok(())
}

fn live_hash(state: Option<&FileState>) -> Option<String> {
    state
        .filter(|state| !state.deleted)
        .map(|state| state.hash.clone())
}

/// Expands a canonical root tree into live file states.
///
/// Returned mtimes are zero because snapshot identity deliberately excludes clocks.
///
/// # Errors
/// Returns an error when fetching fails or a tree contains an invalid cycle.
pub fn tree_to_flat<F>(root: &str, fetch: F) -> Result<HashMap<String, FileState>>
where
    F: FnMut(&str) -> Result<Tree>,
{
    let mut traversal = FlattenTraversal {
        fetch,
        ancestors: Vec::new(),
        files: HashMap::new(),
    };
    traversal.visit(root, "")?;
    Ok(traversal.files)
}

fn insert_state(root: &mut Node, state: &FileState) -> Result<()> {
    validate_path(&state.path)?;
    if state.mode != 0 && state.mode != EXECUTABLE_MODE {
        bail!("invalid portable mode {} for {:?}", state.mode, state.path);
    }
    let mut parts = state.path.split('/').peekable();
    let mut node = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if node.directories.contains_key(part) || node.conflicts.contains_key(part) {
                bail!("path is both file and directory: {:?}", state.path);
            }
            node.files.insert(part.to_string(), state.clone());
        } else {
            if node.files.contains_key(part) || node.conflicts.contains_key(part) {
                bail!("path traverses file component: {:?}", state.path);
            }
            node = node.directories.entry(part.to_string()).or_default();
        }
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    if !is_safe_rel_path(path)
        || path.contains('\\')
        || path.split('/').any(|part| part.is_empty() || part == ".")
    {
        bail!("invalid canonical tree path {path:?}");
    }
    Ok(())
}

fn build_node(node: Node, trees: &mut HashMap<String, Tree>) -> String {
    let mut entries =
        Vec::with_capacity(node.files.len() + node.conflicts.len() + node.directories.len());
    for (name, state) in node.files {
        entries.push(TreeEntry {
            name,
            kind: TreeEntryKind::File,
            hash: state.hash,
            size: state.size,
            mode: state.mode,
        });
    }
    entries.extend(node.conflicts.into_values());
    for (name, child) in node.directories {
        let hash = build_node(child, trees);
        entries.push(TreeEntry {
            name,
            kind: TreeEntryKind::Dir,
            hash,
            size: 0,
            mode: 0,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let tree = Tree { entries };
    let hash = tree.id();
    trees.insert(hash.clone(), tree);
    hash
}

struct FlattenTraversal<F> {
    fetch: F,
    ancestors: Vec<String>,
    files: HashMap<String, FileState>,
}

impl<F> FlattenTraversal<F>
where
    F: FnMut(&str) -> Result<Tree>,
{
    fn visit(&mut self, hash: &str, prefix: &str) -> Result<()> {
        if self.ancestors.iter().any(|ancestor| ancestor == hash) {
            bail!("cycle in canonical tree at {hash}");
        }
        self.ancestors.push(hash.to_string());
        let tree = (self.fetch)(hash)?;
        for entry in tree.entries {
            let path = join_path(prefix, &entry.name);
            match entry.kind {
                TreeEntryKind::Dir => self.visit(&entry.hash, &path)?,
                TreeEntryKind::File | TreeEntryKind::Conflict { .. } => {
                    self.files.insert(
                        path.clone(),
                        FileState {
                            path,
                            hash: entry.hash,
                            size: entry.size,
                            mtime: 0,
                            deleted: false,
                            mode: entry.mode,
                        },
                    );
                }
            }
        }
        self.ancestors.pop();
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
