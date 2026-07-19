use crate::ctx::SyncCtx;
use crate::fs_util::atomic_write;
use crate::prepared_tree::{PreparedTreeBundle, OBJECT_DOMAIN};
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    hash_bytes, is_valid_hash, pack_bytes, unpack_bytes_with_policy, ConcurrentEdit, FileState,
    LegacyPolicy, Snapshot, Tree, TreeBundle, TreeEntryKind,
};
use std::collections::{BTreeSet, HashMap};
use std::io::ErrorKind;
use tokio::fs;

const MAX_OBJECT_BYTES: usize = 16 * 1024 * 1024;
const MAX_OBJECT_CIPHERTEXT_BYTES: usize = MAX_OBJECT_BYTES + 64;

/// Encrypted immutable tree/snapshot store backed by FeanorFS CAS.
pub struct ObjectStore<'ctx, 'a> {
    ctx: &'ctx SyncCtx<'a>,
}

pub(crate) struct LoadedTree {
    pub files: HashMap<String, FileState>,
    pub conflicts: Vec<ConcurrentEdit>,
}

impl<'ctx, 'a> ObjectStore<'ctx, 'a> {
    /// Binds object operations to one workspace sync context.
    #[must_use]
    pub const fn new(ctx: &'ctx SyncCtx<'a>) -> Self {
        Self { ctx }
    }

    /// Seals, caches, and uploads one canonical tree.
    ///
    /// # Errors
    /// Returns an error when encryption, local persistence, or upload fails.
    pub async fn put_tree(&self, tree: &Tree) -> Result<String> {
        self.put_bytes(&tree.to_canonical_bytes()).await
    }

    /// Rewrites logical directory references to encrypted object ids and uploads the bundle.
    ///
    /// # Errors
    /// Returns an error for incomplete/cyclic bundles or failed object writes.
    pub async fn put_bundle(&self, bundle: &TreeBundle) -> Result<String> {
        let prepared = PreparedTreeBundle::new(bundle, self.ctx.password_str())?;
        for tree in prepared.trees.values() {
            self.put_tree(tree).await?;
        }
        Ok(prepared.root)
    }

    /// Fetches, verifies, decrypts, and decodes one canonical tree.
    ///
    /// # Errors
    /// Returns an error for invalid ids, corrupt ciphertext, or malformed trees.
    pub async fn get_tree(&self, id: &str) -> Result<Tree> {
        Tree::from_canonical_bytes(&self.get_bytes(id).await?)
            .with_context(|| format!("decode tree object {id}"))
    }

    /// Resolves an encrypted tree closure into its visible flat file view.
    ///
    /// # Errors
    /// Returns an error for corrupt objects, missing children, or cycles.
    pub async fn get_flat_tree(&self, root: &str) -> Result<HashMap<String, FileState>> {
        Ok(self.get_tree_state(root).await?.files)
    }

    pub(crate) async fn get_tree_state(&self, root: &str) -> Result<LoadedTree> {
        let mut state = LoadedTree {
            files: HashMap::new(),
            conflicts: Vec::new(),
        };
        let mut pending = vec![(root.to_string(), String::new(), Vec::<String>::new())];
        while let Some((id, prefix, mut ancestors)) = pending.pop() {
            if ancestors.iter().any(|ancestor| ancestor == &id) {
                bail!("cycle in encrypted tree at {id}");
            }
            ancestors.push(id.clone());
            for entry in self.get_tree(&id).await?.entries {
                let path = if prefix.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{prefix}/{}", entry.name)
                };
                match entry.kind {
                    TreeEntryKind::Dir => {
                        pending.push((entry.hash, path, ancestors.clone()));
                    }
                    TreeEntryKind::File => {
                        state.files.insert(
                            path.clone(),
                            FileState {
                                path: path.clone(),
                                hash: entry.hash,
                                size: entry.size,
                                mtime: 0,
                                deleted: false,
                                mode: entry.mode,
                            },
                        );
                    }
                    TreeEntryKind::Conflict { base, ours, theirs } => {
                        state.files.insert(
                            path.clone(),
                            FileState {
                                path: path.clone(),
                                hash: entry.hash,
                                size: entry.size,
                                mtime: 0,
                                deleted: theirs.is_none(),
                                mode: 0,
                            },
                        );
                        state.conflicts.push(ConcurrentEdit::new(
                            path.clone(),
                            base.map(|hash| conflict_leg(&path, hash, entry.size)),
                            ours.map(|hash| conflict_leg(&path, hash, entry.size)),
                            theirs.map(|hash| conflict_leg(&path, hash, entry.size)),
                        ));
                    }
                }
            }
        }
        Ok(state)
    }

    /// Seals, caches, and uploads one canonical snapshot.
    ///
    /// # Errors
    /// Returns an error when encryption, local persistence, or upload fails.
    pub async fn put_snapshot(&self, snapshot: &Snapshot) -> Result<String> {
        self.put_bytes(&snapshot.to_canonical_bytes()).await
    }

    /// Fetches, verifies, decrypts, and decodes one canonical snapshot.
    ///
    /// # Errors
    /// Returns an error for invalid ids, corrupt ciphertext, or malformed snapshots.
    pub async fn get_snapshot(&self, id: &str) -> Result<Snapshot> {
        Snapshot::from_canonical_bytes(&self.get_bytes(id).await?)
            .with_context(|| format!("decode snapshot object {id}"))
    }

    pub(crate) async fn snapshot_reachability(
        &self,
        id: &str,
        expand_chunked_files: bool,
    ) -> Result<Vec<String>> {
        let snapshot = self.get_snapshot(id).await?;
        let mut hashes = BTreeSet::from([id.to_string()]);
        let mut pending = vec![(snapshot.root, String::new())];
        while let Some((tree_id, prefix)) = pending.pop() {
            if !hashes.insert(tree_id.clone()) {
                continue;
            }
            for entry in self.get_tree(&tree_id).await?.entries {
                let path = if prefix.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{prefix}/{}", entry.name)
                };
                match entry.kind {
                    TreeEntryKind::Dir => pending.push((entry.hash, path)),
                    TreeEntryKind::File => {
                        hashes.insert(entry.hash.clone());
                        if expand_chunked_files {
                            hashes.extend(
                                crate::large_file::reachable_chunks(
                                    self.ctx,
                                    &path,
                                    &entry.hash,
                                    entry.size,
                                )
                                .await?,
                            );
                        }
                    }
                    TreeEntryKind::Conflict { base, ours, theirs } => {
                        let mut legs = vec![entry.hash];
                        legs.extend(base);
                        legs.extend(ours);
                        legs.extend(theirs);
                        legs.sort_unstable();
                        legs.dedup();
                        for leg in legs {
                            hashes.insert(leg.clone());
                            if expand_chunked_files {
                                hashes.extend(
                                    crate::large_file::reachable_chunks(
                                        self.ctx, &path, &leg, entry.size,
                                    )
                                    .await?,
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(hashes.into_iter().collect())
    }

    pub(crate) async fn cache_manifest(&self, id: &str, hashes: &[String]) -> Result<()> {
        let mut manifest = hashes.join("\n").into_bytes();
        manifest.push(b'\n');
        let state = self.ctx.state_dir()?;
        atomic_write(&state, &format!("manifests/{id}"), &manifest).await?;
        crate::object_gc::prune(self.ctx.base).await
    }

    async fn put_bytes(&self, bytes: &[u8]) -> Result<String> {
        if bytes.len() > MAX_OBJECT_BYTES {
            bail!("canonical object exceeds 16 MiB limit");
        }
        let ciphertext = pack_bytes(bytes, self.ctx.password_str(), OBJECT_DOMAIN)?;
        let id = hash_bytes(&ciphertext);
        self.cache(&id, &ciphertext).await?;
        self.ctx
            .api
            .upload_object(self.ctx.workspace_id(), &id, ciphertext)
            .await?;
        Ok(id)
    }

    async fn get_bytes(&self, id: &str) -> Result<Vec<u8>> {
        if !is_valid_hash(id) {
            bail!("invalid object id {id:?}");
        }
        let cache_path = self.cache_path(id)?;
        let ciphertext = match fs::read(&cache_path).await {
            Ok(bytes) if hash_bytes(&bytes) == id => bytes,
            Ok(_) => {
                match fs::remove_file(&cache_path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("remove corrupt object cache"),
                }
                self.fetch_remote(id).await?
            }
            Err(error) if error.kind() == ErrorKind::NotFound => self.fetch_remote(id).await?,
            Err(error) => return Err(error).context("read object cache"),
        };
        unpack_bytes_with_policy(
            &ciphertext,
            self.ctx.password_str(),
            OBJECT_DOMAIN,
            LegacyPolicy::Reject,
        )
        .with_context(|| format!("decrypt object {id}"))
    }

    async fn fetch_remote(&self, id: &str) -> Result<Vec<u8>> {
        let ciphertext = self.ctx.api.download_file(id).await?;
        if ciphertext.len() > MAX_OBJECT_CIPHERTEXT_BYTES {
            bail!("downloaded object exceeds ciphertext size limit");
        }
        if hash_bytes(&ciphertext) != id {
            bail!("downloaded object hash mismatch for {id}");
        }
        self.cache(id, &ciphertext).await?;
        Ok(ciphertext)
    }

    async fn cache(&self, id: &str, ciphertext: &[u8]) -> Result<()> {
        let state = self.ctx.state_dir()?;
        atomic_write(&state, &format!("objects/{id}"), ciphertext)
            .await
            .with_context(|| format!("cache object {id}"))
    }

    fn cache_path(&self, id: &str) -> Result<std::path::PathBuf> {
        Ok(self.ctx.state_dir()?.join("objects").join(id))
    }
}

fn conflict_leg(path: &str, hash: String, size: u64) -> FileState {
    FileState {
        path: path.to_string(),
        hash,
        size,
        mtime: 0,
        deleted: false,
        mode: 0,
    }
}
