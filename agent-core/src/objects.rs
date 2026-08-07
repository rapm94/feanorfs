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
                        // The tree stores only the visible leg's size and
                        // content id; the visible leg is always a live blob
                        // (insert_conflict refuses all-deleted conflicts), so
                        // the flattened file is present in the working copy.
                        state.files.insert(
                            path.clone(),
                            FileState {
                                path: path.clone(),
                                hash: entry.hash,
                                size: entry.size,
                                mtime: 0,
                                deleted: false,
                                mode: 0,
                            },
                        );
                        // Non-visible legs carry size 0 ("unknown"): their
                        // sizes are not part of the tree format.
                        let visible_hash = theirs
                            .clone()
                            .or_else(|| ours.clone())
                            .or_else(|| base.clone());
                        let size_of = |hash: &str| {
                            if Some(hash) == visible_hash.as_deref() {
                                entry.size
                            } else {
                                0
                            }
                        };
                        state.conflicts.push(ConcurrentEdit::new(
                            path.clone(),
                            base.map(|hash| {
                                let size = size_of(&hash);
                                conflict_leg(&path, hash, size)
                            }),
                            ours.map(|hash| {
                                let size = size_of(&hash);
                                conflict_leg(&path, hash, size)
                            }),
                            theirs.map(|hash| {
                                let size = size_of(&hash);
                                conflict_leg(&path, hash, size)
                            }),
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
                                    Some(entry.size),
                                )
                                .await?,
                            );
                        }
                    }
                    TreeEntryKind::Conflict { base, ours, theirs } => {
                        let mut legs = vec![entry.hash.clone()];
                        legs.extend(base);
                        legs.extend(ours);
                        legs.extend(theirs);
                        legs.sort_unstable();
                        legs.dedup();
                        for leg in legs {
                            hashes.insert(leg.clone());
                            if expand_chunked_files {
                                // Only the visible leg's size is stored in the
                                // tree; hidden legs are size-unknown, so their
                                // chunks must be discovered from the blob
                                // itself or server GC could delete them.
                                let size = (leg == entry.hash).then_some(entry.size);
                                hashes.extend(
                                    crate::large_file::reachable_chunks(
                                        self.ctx, &path, &leg, size,
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
        // Tree/snapshot ids are deterministic: unchanged subtrees keep their
        // id across syncs, so skip blobs the hub already accepted instead of
        // re-uploading byte-identical ciphertext on every pass.
        let state_dir = self.ctx.state_dir()?;
        if !crate::upload_registry::known(&state_dir, &id).await {
            self.ctx
                .api
                .upload_object(self.ctx.workspace_id(), &id, ciphertext)
                .await?;
            crate::upload_registry::remember(&state_dir, &id);
        }
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

/// Reads a verified ciphertext object from the local object cache when
/// present, without touching the network. Corrupt cache entries are dropped.
pub(crate) async fn cached_object(ctx: &SyncCtx<'_>, id: &str) -> Result<Option<Vec<u8>>> {
    let cache_path = ctx.state_dir()?.join("objects").join(id);
    match fs::read(&cache_path).await {
        Ok(bytes) if hash_bytes(&bytes) == id => Ok(Some(bytes)),
        Ok(_) => {
            let _ = fs::remove_file(&cache_path).await;
            Ok(None)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("read object cache"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiClient;
    use crate::ctx::SyncCtx;
    use crate::hub::LocalHub;
    use crate::local::ClientDb;
    use feanorfs_common::{ConcurrentEdit, Snapshot};
    use std::collections::HashMap;
    use std::io::{Seek as _, Write as _};

    const TEST_PASSWORD: &str = "unit-test-password";

    /// A conflict entry's hidden (non-visible) leg may be a chunked file while
    /// the visible leg is small. Its chunk hashes must still be included in
    /// snapshot reachability or server GC can delete them.
    #[tokio::test]
    async fn reachability_expands_hidden_chunked_conflict_leg() {
        const SIZE: u64 = 65 * 1024 * 1024 + 5;
        const CHUNK_BYTES: usize = crate::large_file::CHUNK_BYTES;

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("ws");
        std::fs::create_dir_all(&base).unwrap();
        let state = crate::workspace_layout::ensure_workspace_state(&base).unwrap();
        let hub = LocalHub::open(dir.path().join("hub-data"), None)
            .await
            .unwrap();
        let api = ApiClient::local(hub, None);
        let db = ClientDb::new(state).await.unwrap();
        let ctx = SyncCtx::new(
            &api,
            &db,
            &base,
            "test-ws",
            Some(TEST_PASSWORD),
            feanorfs_common::LegacyPolicy::Reject,
        );

        // Chunked file with a deterministic first chunk.
        let path = base.join("large.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(SIZE).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file.write_all(b"A-LARGEMK").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let fingerprint =
            crate::large_file::fingerprint(&path, TEST_PASSWORD, "large.bin").unwrap();
        crate::large_file::upload(&ctx, "large.bin", &fingerprint.encrypted_hash)
            .await
            .unwrap();

        // Visible leg = theirs (small); hidden leg = ours (the chunked file).
        let small_hash = feanorfs_common::hash_bytes(b"tiny");
        let conflict = ConcurrentEdit::new(
            "large.bin".to_string(),
            None,
            Some(FileState {
                path: "large.bin".into(),
                hash: fingerprint.encrypted_hash.clone(),
                size: SIZE,
                mtime: 0,
                deleted: false,
                mode: 0,
            }),
            Some(FileState {
                path: "large.bin".into(),
                hash: small_hash,
                size: 4,
                mtime: 0,
                deleted: false,
                mode: 0,
            }),
        );
        let bundle =
            feanorfs_common::flat_to_tree_with_conflicts(&HashMap::new(), &[conflict]).unwrap();
        let objects = ObjectStore::new(&ctx);
        let root = objects.put_bundle(&bundle).await.unwrap();
        let id = objects
            .put_snapshot(&Snapshot {
                root,
                parents: Vec::new(),
                author: "test".into(),
                created_at_ms: 0,
                message: None,
            })
            .await
            .unwrap();
        let hashes = objects.snapshot_reachability(&id, true).await.unwrap();

        // Chunk 0 of the hidden leg, sealed exactly like the engine seals it.
        let mut chunk0 = vec![0_u8; CHUNK_BYTES];
        chunk0[..9].copy_from_slice(b"A-LARGEMK");
        let sealed0 = feanorfs_common::pack_bytes(
            &chunk0,
            TEST_PASSWORD,
            &format!("feanorfs-file-chunk-v1\0large.bin\0{index}", index = 0),
        )
        .unwrap();
        let chunk0_hash = feanorfs_common::hash_bytes(&sealed0);
        assert!(
            hashes.contains(&chunk0_hash),
            "hidden chunked conflict leg chunks must be reachable"
        );
    }

    /// The visible leg keeps its fast path: a small visible leg is not fetched
    /// for chunk expansion, and a chunked visible leg still expands.
    #[tokio::test]
    async fn reachability_visible_leg_chunk_expansion_uses_stored_size() {
        const SIZE: u64 = 65 * 1024 * 1024 + 5;
        const CHUNK_BYTES: usize = crate::large_file::CHUNK_BYTES;

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("ws");
        std::fs::create_dir_all(&base).unwrap();
        let state = crate::workspace_layout::ensure_workspace_state(&base).unwrap();
        let hub = LocalHub::open(dir.path().join("hub-data"), None)
            .await
            .unwrap();
        let api = ApiClient::local(hub, None);
        let db = ClientDb::new(state).await.unwrap();
        let ctx = SyncCtx::new(
            &api,
            &db,
            &base,
            "test-ws",
            Some(TEST_PASSWORD),
            feanorfs_common::LegacyPolicy::Reject,
        );

        let path = base.join("large.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(SIZE).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file.write_all(b"B-LARGEMK").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let fingerprint =
            crate::large_file::fingerprint(&path, TEST_PASSWORD, "large.bin").unwrap();
        crate::large_file::upload(&ctx, "large.bin", &fingerprint.encrypted_hash)
            .await
            .unwrap();

        // Visible leg = theirs = the chunked file itself.
        let conflict = ConcurrentEdit::new(
            "large.bin".to_string(),
            None,
            Some(FileState {
                path: "large.bin".into(),
                hash: fingerprint.encrypted_hash.clone(),
                size: SIZE,
                mtime: 0,
                deleted: false,
                mode: 0,
            }),
            Some(FileState {
                path: "large.bin".into(),
                hash: fingerprint.encrypted_hash.clone(),
                size: SIZE,
                mtime: 0,
                deleted: false,
                mode: 0,
            }),
        );
        let bundle =
            feanorfs_common::flat_to_tree_with_conflicts(&HashMap::new(), &[conflict]).unwrap();
        let objects = ObjectStore::new(&ctx);
        let root = objects.put_bundle(&bundle).await.unwrap();
        let id = objects
            .put_snapshot(&Snapshot {
                root,
                parents: Vec::new(),
                author: "test".into(),
                created_at_ms: 0,
                message: None,
            })
            .await
            .unwrap();
        let hashes = objects.snapshot_reachability(&id, true).await.unwrap();

        let mut chunk0 = vec![0_u8; CHUNK_BYTES];
        chunk0[..9].copy_from_slice(b"B-LARGEMK");
        let sealed0 = feanorfs_common::pack_bytes(
            &chunk0,
            TEST_PASSWORD,
            &format!("feanorfs-file-chunk-v1\0large.bin\0{index}", index = 0),
        )
        .unwrap();
        let chunk0_hash = feanorfs_common::hash_bytes(&sealed0);
        assert!(hashes.contains(&chunk0_hash));
    }
}
