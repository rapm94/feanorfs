use anyhow::{bail, Context, Result};
use feanorfs_common::{hash_bytes, pack_bytes, Tree, TreeBundle, TreeEntryKind};
use std::collections::{BTreeSet, HashMap};

pub(crate) const OBJECT_DOMAIN: &str = "feanorfs:obj:v1";

pub(crate) struct PreparedTreeBundle {
    pub root: String,
    pub trees: HashMap<String, Tree>,
}

impl PreparedTreeBundle {
    pub(crate) fn new(bundle: &TreeBundle, password: &str) -> Result<Self> {
        let mut encrypted_ids = HashMap::new();
        let mut trees = HashMap::new();
        let mut pending: BTreeSet<_> = bundle.trees.keys().cloned().collect();
        while !pending.is_empty() {
            let ready: Vec<_> = pending
                .iter()
                .filter(|logical_id| {
                    bundle.trees.get(*logical_id).is_some_and(|tree| {
                        tree.entries
                            .iter()
                            .all(|entry| !entry.is_dir() || encrypted_ids.contains_key(&entry.hash))
                    })
                })
                .cloned()
                .collect();
            if ready.is_empty() {
                bail!("tree bundle contains missing children or a cycle");
            }
            for logical_id in ready {
                let mut tree = bundle
                    .trees
                    .get(&logical_id)
                    .cloned()
                    .context("ready tree disappeared from bundle")?;
                for entry in &mut tree.entries {
                    if matches!(entry.kind, TreeEntryKind::Dir) {
                        entry.hash = encrypted_ids
                            .get(&entry.hash)
                            .cloned()
                            .context("directory child was not encrypted")?;
                    }
                }
                let ciphertext = pack_bytes(&tree.to_canonical_bytes(), password, OBJECT_DOMAIN)?;
                let encrypted_id = hash_bytes(&ciphertext);
                trees.insert(encrypted_id.clone(), tree);
                encrypted_ids.insert(logical_id.clone(), encrypted_id);
                pending.remove(&logical_id);
            }
        }
        let root = encrypted_ids
            .remove(&bundle.root)
            .context("tree bundle does not contain its root")?;
        Ok(Self { root, trees })
    }
}
