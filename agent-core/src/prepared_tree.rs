use anyhow::{bail, Context, Result};
use feanorfs_common::{
    hash_bytes, pack_bytes, Tree, TreeBundle, MAX_TREE_OBJECTS, MAX_TREE_WORK_ITEMS,
};
use std::collections::{BTreeSet, HashMap};

pub(crate) const OBJECT_DOMAIN: &str = "feanorfs:obj:v1";

pub(crate) struct PreparedTreeBundle {
    pub root: String,
    pub trees: HashMap<String, Tree>,
}

impl PreparedTreeBundle {
    pub(crate) fn new(bundle: &TreeBundle, password: &str) -> Result<Self> {
        if bundle.trees.len() > MAX_TREE_OBJECTS {
            bail!("tree bundle exceeds distinct-object limit");
        }
        if !bundle.trees.contains_key(&bundle.root) {
            bail!("tree bundle does not contain its root");
        }

        let mut remaining_children = HashMap::with_capacity(bundle.trees.len());
        let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut work_items = 0_usize;
        for (logical_id, tree) in &bundle.trees {
            tree.validate()?;
            work_items = work_items
                .checked_add(tree.entries.len().saturating_add(1))
                .context("tree bundle work counter overflow")?;
            if work_items > MAX_TREE_WORK_ITEMS {
                bail!("tree bundle exceeds work limit");
            }
            let mut children = 0_usize;
            for entry in &tree.entries {
                if entry.is_dir() {
                    if !bundle.trees.contains_key(&entry.hash) {
                        bail!("tree bundle contains a missing directory child");
                    }
                    children += 1;
                    parents
                        .entry(entry.hash.as_str())
                        .or_default()
                        .push(logical_id.as_str());
                }
            }
            remaining_children.insert(logical_id.as_str(), children);
        }

        let mut ready: BTreeSet<&str> = remaining_children
            .iter()
            .filter_map(|(&logical_id, &count)| (count == 0).then_some(logical_id))
            .collect();
        let mut encrypted_ids = HashMap::with_capacity(bundle.trees.len());
        let mut trees = HashMap::with_capacity(bundle.trees.len());
        while let Some(logical_id) = ready.pop_first() {
            let mut tree = bundle
                .trees
                .get(logical_id)
                .cloned()
                .context("ready tree disappeared from bundle")?;
            for entry in &mut tree.entries {
                if entry.is_dir() {
                    entry.hash = encrypted_ids
                        .get(entry.hash.as_str())
                        .cloned()
                        .context("directory child was not encrypted")?;
                }
            }
            let ciphertext = pack_bytes(&tree.to_canonical_bytes(), password, OBJECT_DOMAIN)?;
            let encrypted_id = hash_bytes(&ciphertext);
            trees.insert(encrypted_id.clone(), tree);
            encrypted_ids.insert(logical_id, encrypted_id);

            if let Some(parent_ids) = parents.get(logical_id) {
                for parent_id in parent_ids {
                    let count = remaining_children
                        .get_mut(parent_id)
                        .context("tree bundle parent disappeared")?;
                    *count = count
                        .checked_sub(1)
                        .context("tree bundle dependency counter underflow")?;
                    if *count == 0 {
                        ready.insert(parent_id);
                    }
                }
            }
        }
        if encrypted_ids.len() != bundle.trees.len() {
            bail!("tree bundle contains a cycle");
        }
        let root = encrypted_ids
            .remove(bundle.root.as_str())
            .context("tree bundle does not contain its root")?;
        Ok(Self { root, trees })
    }
}
