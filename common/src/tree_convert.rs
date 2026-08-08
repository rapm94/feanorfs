use crate::{
    is_safe_rel_path, ConcurrentEdit, ConflictModes, FileState, Tree, TreeBundle, TreeEntry,
    TreeEntryKind, EXECUTABLE_MODE, MAX_TREE_DEPTH, MAX_TREE_OBJECTS, MAX_TREE_OUTPUT_PATHS,
    MAX_TREE_PATH_BYTES_TOTAL, MAX_TREE_WORK_ITEMS,
};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Default)]
struct Node {
    files: BTreeMap<String, FileState>,
    conflicts: BTreeMap<String, TreeEntry>,
    directories: BTreeMap<String, usize>,
}

struct TreeBudget {
    work_items: usize,
    objects: usize,
    output_paths: usize,
    path_bytes: usize,
}

impl TreeBudget {
    const fn new() -> Self {
        Self {
            work_items: 1, // root tree object
            objects: 1,
            output_paths: 0,
            path_bytes: 0,
        }
    }

    fn add_work(&mut self, amount: usize) -> Result<()> {
        self.work_items = self
            .work_items
            .checked_add(amount)
            .context("canonical tree work counter overflow")?;
        if self.work_items > MAX_TREE_WORK_ITEMS {
            bail!("canonical tree exceeds {MAX_TREE_WORK_ITEMS} work items");
        }
        Ok(())
    }

    fn add_directory(&mut self) -> Result<()> {
        self.objects = self
            .objects
            .checked_add(1)
            .context("canonical tree object counter overflow")?;
        if self.objects > MAX_TREE_OBJECTS {
            bail!("canonical tree exceeds {MAX_TREE_OBJECTS} tree objects");
        }
        self.add_work(1)
    }

    fn add_output(&mut self) -> Result<()> {
        self.output_paths = self
            .output_paths
            .checked_add(1)
            .context("canonical tree output counter overflow")?;
        if self.output_paths > MAX_TREE_OUTPUT_PATHS {
            bail!("canonical tree exceeds {MAX_TREE_OUTPUT_PATHS} output paths");
        }
        self.add_work(1)
    }

    fn add_path(&mut self, path: &str) -> Result<()> {
        self.path_bytes = self
            .path_bytes
            .checked_add(path.len())
            .context("canonical tree path-byte counter overflow")?;
        if self.path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
            bail!("canonical tree paths exceed aggregate byte limit");
        }
        Ok(())
    }
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
    let mut nodes = vec![Node::default()];
    let mut budget = TreeBudget::new();
    for (path, state) in files {
        if path != &state.path {
            bail!(
                "flat tree key {path:?} does not match embedded path {:?}",
                state.path
            );
        }
        validate_path(&state.path)?;
        if state.deleted {
            continue;
        }
        budget.add_path(&state.path)?;
        budget.add_work(state.path.split('/').count())?;
        insert_state(&mut nodes, state, &mut budget)?;
    }
    for conflict in conflicts {
        budget.add_path(&conflict.path)?;
        budget.add_work(conflict.path.split('/').count())?;
        insert_conflict(&mut nodes, conflict, &mut budget)?;
    }
    let mut trees = HashMap::new();
    let root = build_node(nodes, &mut trees)?;
    Ok(TreeBundle { root, trees })
}

fn insert_conflict(
    nodes: &mut Vec<Node>,
    conflict: &ConcurrentEdit,
    budget: &mut TreeBudget,
) -> Result<()> {
    validate_path(&conflict.path)?;
    for leg in [
        conflict.base.as_ref(),
        conflict.ours.as_ref(),
        conflict.theirs.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if leg.path != conflict.path {
            bail!(
                "conflict path {:?} does not match leg path {:?}",
                conflict.path,
                leg.path
            );
        }
    }
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
            modes: ConflictModes {
                base: live_mode(conflict.base.as_ref()),
                ours: live_mode(conflict.ours.as_ref()),
                theirs: live_mode(conflict.theirs.as_ref()),
            },
        },
        hash: representative.hash.clone(),
        size: representative.size,
        mode: representative.mode,
    };
    let mut parts = conflict.path.split('/').peekable();
    let mut node_index = 0_usize;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let node = nodes
                .get_mut(node_index)
                .context("canonical tree conflict node disappeared")?;
            if node.directories.contains_key(part) || node.conflicts.contains_key(part) {
                bail!(
                    "conflict path collides with existing entry: {:?}",
                    conflict.path
                );
            }
            let replaced_file = node.files.remove(part).is_some();
            if !replaced_file {
                budget.add_output()?;
            }
            node.conflicts.insert(
                part.to_string(),
                TreeEntry {
                    name: part.to_string(),
                    ..entry.clone()
                },
            );
        } else {
            let child_index = {
                let node = nodes
                    .get(node_index)
                    .context("canonical tree conflict node disappeared")?;
                if node.files.contains_key(part) || node.conflicts.contains_key(part) {
                    bail!(
                        "conflict path traverses file component: {:?}",
                        conflict.path
                    );
                }
                node.directories.get(part).copied()
            };
            node_index = if let Some(child_index) = child_index {
                child_index
            } else {
                budget.add_directory()?;
                let child_index = nodes.len();
                nodes.push(Node::default());
                nodes
                    .get_mut(node_index)
                    .context("canonical tree conflict parent disappeared")?
                    .directories
                    .insert(part.to_string(), child_index);
                child_index
            };
        }
    }
    Ok(())
}

fn live_mode(state: Option<&FileState>) -> u32 {
    state
        .filter(|state| !state.deleted)
        .map_or(0, |state| state.mode)
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
        active: HashSet::new(),
        files: HashMap::new(),
        work_items: 0,
        objects: HashSet::new(),
        constructed_path_bytes: 0,
        pending_path_bytes: 0,
    };
    traversal.run(root)?;
    Ok(traversal.files)
}

fn insert_state(nodes: &mut Vec<Node>, state: &FileState, budget: &mut TreeBudget) -> Result<()> {
    validate_path(&state.path)?;
    if state.mode != 0 && state.mode != EXECUTABLE_MODE {
        bail!("invalid portable mode {} for {:?}", state.mode, state.path);
    }
    let mut parts = state.path.split('/').peekable();
    let mut node_index = 0_usize;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let node = nodes
                .get_mut(node_index)
                .context("canonical tree file node disappeared")?;
            if node.directories.contains_key(part) || node.conflicts.contains_key(part) {
                bail!("path is both file and directory: {:?}", state.path);
            }
            if !node.files.contains_key(part) {
                budget.add_output()?;
            }
            node.files.insert(part.to_string(), state.clone());
        } else {
            let child_index = {
                let node = nodes
                    .get(node_index)
                    .context("canonical tree file node disappeared")?;
                if node.files.contains_key(part) || node.conflicts.contains_key(part) {
                    bail!("path traverses file component: {:?}", state.path);
                }
                node.directories.get(part).copied()
            };
            node_index = if let Some(child_index) = child_index {
                child_index
            } else {
                budget.add_directory()?;
                let child_index = nodes.len();
                nodes.push(Node::default());
                nodes
                    .get_mut(node_index)
                    .context("canonical tree file parent disappeared")?
                    .directories
                    .insert(part.to_string(), child_index);
                child_index
            };
        }
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    if path.split('/').count() > MAX_TREE_DEPTH {
        bail!("canonical tree path exceeds {MAX_TREE_DEPTH} directory levels");
    }
    if !is_safe_rel_path(path)
        || path.contains('\\')
        || path.split('/').any(|part| part.is_empty() || part == ".")
    {
        bail!("invalid canonical tree path {path:?}");
    }
    Ok(())
}

fn build_node(mut nodes: Vec<Node>, trees: &mut HashMap<String, Tree>) -> Result<String> {
    let mut hashes = vec![None; nodes.len()];
    while let Some(node) = nodes.pop() {
        let index = nodes.len();
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
        for (name, child_index) in node.directories {
            let hash = hashes
                .get(child_index)
                .and_then(Option::as_ref)
                .cloned()
                .context("canonical tree child was not built")?;
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
        tree.validate()?;
        let hash = tree.id();
        trees.insert(hash.clone(), tree);
        hashes[index] = Some(hash);
    }
    hashes
        .into_iter()
        .next()
        .flatten()
        .context("canonical tree root was not built")
}

enum FlattenWork {
    Enter {
        hash: String,
        prefix: String,
        depth: usize,
    },
    Exit {
        hash: String,
    },
}

impl FlattenWork {
    fn pending_path_len(&self) -> usize {
        match self {
            Self::Enter { prefix, .. } => prefix.len(),
            Self::Exit { .. } => 0,
        }
    }
}

struct FlattenTraversal<F> {
    fetch: F,
    active: HashSet<String>,
    files: HashMap<String, FileState>,
    work_items: usize,
    objects: HashSet<String>,
    constructed_path_bytes: usize,
    pending_path_bytes: usize,
}

impl<F> FlattenTraversal<F>
where
    F: FnMut(&str) -> Result<Tree>,
{
    fn run(&mut self, root: &str) -> Result<()> {
        let mut pending = vec![FlattenWork::Enter {
            hash: root.to_string(),
            prefix: String::new(),
            depth: 0,
        }];
        while let Some(work) = pending.pop() {
            self.release_pending_path(work.pending_path_len())?;
            match work {
                FlattenWork::Exit { hash } => {
                    if !self.active.remove(&hash) {
                        bail!("canonical tree traversal lost active object {hash}");
                    }
                }
                FlattenWork::Enter {
                    hash,
                    prefix,
                    depth,
                } => {
                    if depth > MAX_TREE_DEPTH {
                        bail!("canonical tree exceeds {MAX_TREE_DEPTH} directory levels");
                    }
                    if !self.active.insert(hash.clone()) {
                        bail!("cycle in canonical tree at {hash}");
                    }
                    if self.objects.insert(hash.clone()) && self.objects.len() > MAX_TREE_OBJECTS {
                        bail!("canonical tree exceeds {MAX_TREE_OBJECTS} distinct tree objects");
                    }
                    let tree = (self.fetch)(&hash)?;
                    tree.validate()?;
                    if depth > 0 && tree.entries.is_empty() {
                        bail!("canonical tree contains a non-root empty directory");
                    }
                    self.add_work(tree.entries.len().saturating_add(1))?;

                    let mut directories = Vec::new();
                    for entry in tree.entries {
                        let path = join_path(&prefix, &entry.name);
                        validate_path(&path)?;
                        self.record_constructed_path(&path)?;
                        if entry.is_dir() {
                            self.reserve_pending_path(path.len())?;
                            directories.push(FlattenWork::Enter {
                                hash: entry.hash,
                                prefix: path,
                                depth: depth + 1,
                            });
                        } else {
                            if self.files.len() >= MAX_TREE_OUTPUT_PATHS {
                                bail!(
                                    "canonical tree flat output exceeds {MAX_TREE_OUTPUT_PATHS} paths"
                                );
                            }
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
                    pending.push(FlattenWork::Exit { hash });
                    pending.extend(directories.into_iter().rev());
                }
            }
        }
        debug_assert!(self.active.is_empty());
        debug_assert_eq!(self.pending_path_bytes, 0);
        Ok(())
    }

    fn add_work(&mut self, amount: usize) -> Result<()> {
        self.work_items = self
            .work_items
            .checked_add(amount)
            .context("canonical tree work counter overflow")?;
        if self.work_items > MAX_TREE_WORK_ITEMS {
            bail!("canonical tree traversal exceeds {MAX_TREE_WORK_ITEMS} work items");
        }
        Ok(())
    }

    fn record_constructed_path(&mut self, path: &str) -> Result<()> {
        self.constructed_path_bytes = self
            .constructed_path_bytes
            .checked_add(path.len())
            .context("canonical tree constructed-path counter overflow")?;
        if self.constructed_path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
            bail!("canonical tree traversal exceeds aggregate path-byte limit");
        }
        Ok(())
    }

    fn reserve_pending_path(&mut self, amount: usize) -> Result<()> {
        self.pending_path_bytes = self
            .pending_path_bytes
            .checked_add(amount)
            .context("canonical tree pending-path counter overflow")?;
        if self.pending_path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
            bail!("canonical tree traversal exceeds pending path-byte limit");
        }
        Ok(())
    }

    fn release_pending_path(&mut self, amount: usize) -> Result<()> {
        self.pending_path_bytes = self
            .pending_path_bytes
            .checked_sub(amount)
            .context("canonical tree pending-path counter underflow")?;
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
