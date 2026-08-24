use crate::ctx::SyncCtx;
use crate::fs_util::atomic_write_visible;
use crate::prepared_tree::{PreparedTreeBundle, OBJECT_DOMAIN};
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    hash_bytes, is_safe_rel_path, is_valid_hash, pack_bytes, unpack_bytes_with_policy,
    ConcurrentEdit, FileState, LegacyPolicy, Snapshot, Tree, TreeBundle, TreeEntryKind,
    MANIFEST_MAX_ENTRIES, MAX_CANONICAL_OBJECT_BYTES, MAX_ENCRYPTED_OBJECT_BYTES, MAX_TREE_DEPTH,
    MAX_TREE_OBJECTS, MAX_TREE_OUTPUT_PATHS, MAX_TREE_PATH_BYTES_TOTAL, MAX_TREE_WORK_ITEMS,
};
use std::collections::{BTreeSet, HashMap};
use std::io::ErrorKind;
use tokio::fs;

/// Encrypted immutable tree/snapshot store backed by FeanorFS CAS.
pub struct ObjectStore<'ctx, 'a> {
    ctx: &'ctx SyncCtx<'a>,
}

pub(crate) struct LoadedTree {
    pub files: HashMap<String, FileState>,
    pub conflicts: Vec<ConcurrentEdit>,
}

enum TreeStateWork {
    Enter {
        id: String,
        prefix: String,
        depth: usize,
    },
    Exit(String),
}

#[derive(Clone, Copy)]
enum ObjectReadSource {
    CacheOrHub,
    CacheOnly,
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
        tree.validate()?;
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
        self.get_tree_from(id, ObjectReadSource::CacheOrHub).await
    }

    async fn get_tree_from(&self, id: &str, source: ObjectReadSource) -> Result<Tree> {
        Tree::from_canonical_bytes(&self.get_bytes_from(id, source).await?)
            .with_context(|| format!("decode tree object {id}"))
    }

    /// Resolves an encrypted tree closure into its visible flat file view.
    ///
    /// # Errors
    /// Returns an error for corrupt objects, missing children, or cycles.
    pub async fn get_flat_tree(&self, root: &str) -> Result<HashMap<String, FileState>> {
        Ok(self.get_tree_state(root).await?.files)
    }

    pub(crate) async fn get_flat_tree_local(
        &self,
        root: &str,
    ) -> Result<HashMap<String, FileState>> {
        Ok(self
            .get_tree_state_from(root, ObjectReadSource::CacheOnly)
            .await?
            .files)
    }

    pub(crate) async fn get_tree_state(&self, root: &str) -> Result<LoadedTree> {
        self.get_tree_state_from(root, ObjectReadSource::CacheOrHub)
            .await
    }

    async fn get_tree_state_from(
        &self,
        root: &str,
        source: ObjectReadSource,
    ) -> Result<LoadedTree> {
        let mut state = LoadedTree {
            files: HashMap::new(),
            conflicts: Vec::new(),
        };
        let mut pending = vec![TreeStateWork::Enter {
            id: root.to_string(),
            prefix: String::new(),
            depth: 0,
        }];
        let mut active = std::collections::HashSet::new();
        let mut objects = std::collections::HashSet::new();
        let mut work_items = 0_usize;
        let mut path_bytes = 0_usize;
        while let Some(work) = pending.pop() {
            match work {
                TreeStateWork::Exit(id) => {
                    active.remove(&id);
                }
                TreeStateWork::Enter { id, prefix, depth } => {
                    if depth > MAX_TREE_DEPTH {
                        bail!("encrypted tree exceeds supported depth");
                    }
                    if !active.insert(id.clone()) {
                        bail!("cycle in encrypted tree at {id}");
                    }
                    if objects.insert(id.clone()) && objects.len() > MAX_TREE_OBJECTS {
                        bail!("encrypted tree exceeds distinct-object limit");
                    }
                    pending.push(TreeStateWork::Exit(id.clone()));
                    let tree = self.get_tree_from(&id, source).await?;
                    work_items = work_items
                        .checked_add(tree.entries.len().saturating_add(1))
                        .context("encrypted tree work counter overflow")?;
                    if work_items > MAX_TREE_WORK_ITEMS {
                        bail!("encrypted tree exceeds traversal work limit");
                    }
                    for entry in tree.entries.into_iter().rev() {
                        let path = if prefix.is_empty() {
                            entry.name.clone()
                        } else {
                            format!("{prefix}/{}", entry.name)
                        };
                        if !is_safe_rel_path(&path) || path.split('/').count() > MAX_TREE_DEPTH {
                            bail!("encrypted tree produced an unsafe or oversized path");
                        }
                        path_bytes = path_bytes
                            .checked_add(path.len())
                            .context("encrypted tree path counter overflow")?;
                        if path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
                            bail!("encrypted tree paths exceed aggregate byte limit");
                        }
                        match entry.kind {
                            TreeEntryKind::Dir => pending.push(TreeStateWork::Enter {
                                id: entry.hash,
                                prefix: path,
                                depth: depth + 1,
                            }),
                            TreeEntryKind::File => {
                                if state.files.len() >= MAX_TREE_OUTPUT_PATHS {
                                    bail!("encrypted tree exceeds flat-output limit");
                                }
                                state.files.insert(
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
                            TreeEntryKind::Conflict {
                                base,
                                ours,
                                theirs,
                                modes,
                            } => {
                                if state.files.len() >= MAX_TREE_OUTPUT_PATHS {
                                    bail!("encrypted tree exceeds flat-output limit");
                                }
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
                                        conflict_leg(&path, hash, size, modes.base)
                                    }),
                                    ours.map(|hash| {
                                        let size = size_of(&hash);
                                        conflict_leg(&path, hash, size, modes.ours)
                                    }),
                                    theirs.map(|hash| {
                                        let size = size_of(&hash);
                                        conflict_leg(&path, hash, size, modes.theirs)
                                    }),
                                ));
                            }
                        }
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
        snapshot.validate()?;
        self.put_bytes(&snapshot.to_canonical_bytes()).await
    }

    /// Fetches, verifies, decrypts, and decodes one canonical snapshot.
    ///
    /// # Errors
    /// Returns an error for invalid ids, corrupt ciphertext, or malformed snapshots.
    pub async fn get_snapshot(&self, id: &str) -> Result<Snapshot> {
        self.get_snapshot_from(id, ObjectReadSource::CacheOrHub)
            .await
    }

    pub(crate) async fn get_snapshot_local(&self, id: &str) -> Result<Snapshot> {
        self.get_snapshot_from(id, ObjectReadSource::CacheOnly)
            .await
    }

    async fn get_snapshot_from(&self, id: &str, source: ObjectReadSource) -> Result<Snapshot> {
        Snapshot::from_canonical_bytes(&self.get_bytes_from(id, source).await?)
            .with_context(|| format!("decode snapshot object {id}"))
    }

    pub(crate) async fn snapshot_reachability(
        &self,
        id: &str,
        expand_chunked_files: bool,
    ) -> Result<Vec<String>> {
        let mut hashes = BTreeSet::new();
        let mut snapshot_ids = std::collections::HashSet::new();
        let mut pending_snapshots = vec![id.to_string()];
        let mut tree_roots = Vec::new();
        let mut work_items = 0_usize;
        while let Some(snapshot_id) = pending_snapshots.pop() {
            if !snapshot_ids.insert(snapshot_id.clone()) {
                continue;
            }
            insert_reachable_hash(&mut hashes, snapshot_id.clone())?;
            let snapshot = self.get_snapshot(&snapshot_id).await?;
            work_items = work_items
                .checked_add(snapshot.parents.len().saturating_add(1))
                .context("reachability snapshot work counter overflow")?;
            if work_items > MAX_TREE_WORK_ITEMS {
                bail!("snapshot reachability exceeds traversal work limit");
            }
            tree_roots.push(snapshot.root);
            pending_snapshots.extend(snapshot.parents.into_iter().rev());
        }

        let mut pending = tree_roots
            .into_iter()
            .rev()
            .map(|id| TreeStateWork::Enter {
                id,
                prefix: String::new(),
                depth: 0,
            })
            .collect::<Vec<_>>();
        let mut active = std::collections::HashSet::new();
        let mut objects = std::collections::HashSet::new();
        let mut expanded = std::collections::HashSet::new();
        let mut path_bytes = 0_usize;
        while let Some(work) = pending.pop() {
            match work {
                TreeStateWork::Exit(tree_id) => {
                    active.remove(&tree_id);
                }
                TreeStateWork::Enter {
                    id: tree_id,
                    prefix,
                    depth,
                } => {
                    if depth > MAX_TREE_DEPTH || active.contains(&tree_id) {
                        bail!("cycle or excessive depth in reachable encrypted tree");
                    }
                    if !expanded.insert((tree_id.clone(), prefix.clone())) {
                        continue;
                    }
                    active.insert(tree_id.clone());
                    if objects.insert(tree_id.clone()) && objects.len() > MAX_TREE_OBJECTS {
                        bail!("reachable encrypted tree exceeds distinct-object limit");
                    }
                    insert_reachable_hash(&mut hashes, tree_id.clone())?;
                    pending.push(TreeStateWork::Exit(tree_id.clone()));
                    let tree = self.get_tree(&tree_id).await?;
                    work_items = work_items
                        .checked_add(tree.entries.len().saturating_add(1))
                        .context("reachability work counter overflow")?;
                    if work_items > MAX_TREE_WORK_ITEMS {
                        bail!("snapshot reachability exceeds traversal work limit");
                    }
                    for entry in tree.entries.into_iter().rev() {
                        let path = if prefix.is_empty() {
                            entry.name.clone()
                        } else {
                            format!("{prefix}/{}", entry.name)
                        };
                        if !is_safe_rel_path(&path) || path.split('/').count() > MAX_TREE_DEPTH {
                            bail!("reachable encrypted tree produced an unsafe path");
                        }
                        path_bytes = path_bytes
                            .checked_add(path.len())
                            .context("reachability path counter overflow")?;
                        if path_bytes > MAX_TREE_PATH_BYTES_TOTAL {
                            bail!("snapshot reachability paths exceed aggregate byte limit");
                        }
                        match entry.kind {
                            TreeEntryKind::Dir => pending.push(TreeStateWork::Enter {
                                id: entry.hash,
                                prefix: path,
                                depth: depth + 1,
                            }),
                            TreeEntryKind::File => {
                                insert_reachable_hash(&mut hashes, entry.hash.clone())?;
                                if expand_chunked_files {
                                    let chunks = crate::large_file::reachable_chunks(
                                        self.ctx,
                                        &path,
                                        &entry.hash,
                                        Some(entry.size),
                                    )
                                    .await?;
                                    work_items = work_items
                                        .checked_add(chunks.len())
                                        .context("reachability chunk counter overflow")?;
                                    if work_items > MAX_TREE_WORK_ITEMS {
                                        bail!("snapshot reachability exceeds work limit");
                                    }
                                    for chunk in chunks {
                                        insert_reachable_hash(&mut hashes, chunk)?;
                                    }
                                }
                            }
                            TreeEntryKind::Conflict {
                                base, ours, theirs, ..
                            } => {
                                let mut legs = vec![entry.hash.clone()];
                                legs.extend(base);
                                legs.extend(ours);
                                legs.extend(theirs);
                                legs.sort_unstable();
                                legs.dedup();
                                for leg in legs {
                                    insert_reachable_hash(&mut hashes, leg.clone())?;
                                    if expand_chunked_files {
                                        let size = (leg == entry.hash).then_some(entry.size);
                                        let chunks = crate::large_file::reachable_chunks(
                                            self.ctx, &path, &leg, size,
                                        )
                                        .await?;
                                        work_items = work_items
                                            .checked_add(chunks.len())
                                            .context("reachability chunk counter overflow")?;
                                        if work_items > MAX_TREE_WORK_ITEMS {
                                            bail!("snapshot reachability exceeds work limit");
                                        }
                                        for chunk in chunks {
                                            insert_reachable_hash(&mut hashes, chunk)?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(hashes.into_iter().collect())
    }

    pub(crate) async fn publish_manifest(&self, id: &str, hashes: &[String]) -> Result<()> {
        let first = self
            .ctx
            .api
            .upload_manifest(self.ctx.workspace_id(), id, hashes)
            .await;
        if let Err(error) = first {
            if crate::api::api_failure_kind(&error)
                != Some(crate::api::ApiFailureKind::ManifestReferencesMissingBlob)
            {
                return Err(error);
            }

            let state_dir = self.ctx.state_dir()?;
            crate::upload_registry::clear(&state_dir).await?;
            for hash in hashes {
                if let Some(ciphertext) =
                    cached_object(self.ctx, hash, MAX_ENCRYPTED_OBJECT_BYTES).await?
                {
                    self.ctx
                        .api
                        .upload_object(self.ctx.workspace_id(), hash, ciphertext)
                        .await?;
                }
            }
            self.ctx
                .api
                .upload_manifest(self.ctx.workspace_id(), id, hashes)
                .await?;
        }

        if let Ok(state_dir) = self.ctx.state_dir() {
            let _ = crate::upload_registry::record_many(&state_dir, hashes).await;
        }
        Ok(())
    }

    pub(crate) async fn cache_manifest(&self, id: &str, hashes: &[String]) -> Result<()> {
        let canonical = feanorfs_common::canonical_manifest_hash_list(id, hashes)?;
        let mut manifest = canonical.join("\n").into_bytes();
        manifest.push(b'\n');
        let state = self.ctx.state_dir()?;
        atomic_write_visible(&state, &format!("manifests/{id}"), &manifest).await?;
        crate::object_gc::prune(self.ctx.base).await
    }

    async fn put_bytes(&self, bytes: &[u8]) -> Result<String> {
        if bytes.len() > MAX_CANONICAL_OBJECT_BYTES {
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

    async fn get_bytes_from(&self, id: &str, source: ObjectReadSource) -> Result<Vec<u8>> {
        if !is_valid_hash(id) {
            bail!("invalid object id {id:?}");
        }
        let ciphertext = match cached_object(self.ctx, id, MAX_ENCRYPTED_OBJECT_BYTES).await? {
            Some(bytes) => bytes,
            None => match source {
                ObjectReadSource::CacheOrHub => self.fetch_remote(id).await?,
                ObjectReadSource::CacheOnly => bail!(
                    "local snapshot state is incomplete or corrupt: object {id} is unavailable in the local cache"
                ),
            },
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
        let ciphertext = self
            .ctx
            .api
            .download_file_bounded(id, MAX_ENCRYPTED_OBJECT_BYTES)
            .await?;
        if ciphertext.len() > MAX_ENCRYPTED_OBJECT_BYTES {
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
        atomic_write_visible(&state, &format!("objects/{id}"), ciphertext)
            .await
            .with_context(|| format!("cache object {id}"))
    }
}

/// Reads a verified ciphertext object from the local object cache when
/// present, without touching the network. Corrupt cache entries are dropped.
pub(crate) async fn cached_object(
    ctx: &SyncCtx<'_>,
    id: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let cache_path = ctx.state_dir()?.join("objects").join(id);
    let metadata = match fs::metadata(&cache_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read object-cache metadata"),
    };
    if metadata.len() > max_bytes as u64 {
        let _ = fs::remove_file(&cache_path).await;
        return Ok(None);
    }
    let file = fs::File::open(&cache_path).await?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    use tokio::io::AsyncReadExt as _;
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > max_bytes || hash_bytes(&bytes) != id {
        let _ = fs::remove_file(&cache_path).await;
        Ok(None)
    } else {
        Ok(Some(bytes))
    }
}

fn insert_reachable_hash(hashes: &mut BTreeSet<String>, hash: String) -> Result<()> {
    if !is_valid_hash(&hash) {
        bail!("snapshot reachability contains an invalid object id");
    }
    if !hashes.contains(&hash) && hashes.len() >= MANIFEST_MAX_ENTRIES {
        bail!("snapshot reachability exceeds manifest object limit");
    }
    hashes.insert(hash);
    Ok(())
}

fn conflict_leg(path: &str, hash: String, size: u64, mode: u32) -> FileState {
    FileState {
        path: path.to_string(),
        hash,
        size,
        mtime: 0,
        deleted: false,
        mode,
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

    #[tokio::test]
    async fn conflict_tree_roundtrip_preserves_visible_and_hidden_modes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("ws");
        std::fs::create_dir_all(&base).unwrap();
        let state_dir = crate::workspace_layout::ensure_workspace_state(&base).unwrap();
        let hub = LocalHub::open(dir.path().join("hub-data"), None)
            .await
            .unwrap();
        let api = ApiClient::local(hub, None);
        let db = ClientDb::new(state_dir).await.unwrap();
        let ctx = SyncCtx::new(
            &api,
            &db,
            &base,
            "test-ws",
            Some(TEST_PASSWORD),
            feanorfs_common::LegacyPolicy::Reject,
        );
        let ordinary = FileState {
            path: "run.sh".into(),
            hash: feanorfs_common::hash_bytes(b"ordinary"),
            size: 8,
            mtime: 0,
            deleted: false,
            mode: 0,
        };
        let executable = FileState {
            hash: feanorfs_common::hash_bytes(b"executable"),
            size: 10,
            mode: feanorfs_common::EXECUTABLE_MODE,
            ..ordinary.clone()
        };
        let visible = FileState {
            hash: feanorfs_common::hash_bytes(b"visible"),
            size: 7,
            ..ordinary.clone()
        };
        let conflict = ConcurrentEdit::new(
            "run.sh".into(),
            Some(ordinary),
            Some(executable),
            Some(visible),
        );
        let bundle =
            feanorfs_common::flat_to_tree_with_conflicts(&HashMap::new(), &[conflict]).unwrap();
        let objects = ObjectStore::new(&ctx);
        let root = objects.put_bundle(&bundle).await.unwrap();
        let loaded = objects.get_tree_state(&root).await.unwrap();

        assert_eq!(loaded.files["run.sh"].mode, 0);
        assert_eq!(loaded.conflicts.len(), 1);
        let conflict = &loaded.conflicts[0];
        assert_eq!(conflict.base.as_ref().unwrap().mode, 0);
        assert_eq!(
            conflict.ours.as_ref().unwrap().mode,
            feanorfs_common::EXECUTABLE_MODE
        );
        assert_eq!(conflict.theirs.as_ref().unwrap().mode, 0);
    }

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
            crate::large_file::fingerprint(&base, TEST_PASSWORD, "large.bin").unwrap();
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
            crate::large_file::fingerprint(&base, TEST_PASSWORD, "large.bin").unwrap();
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

    #[tokio::test]
    async fn reachability_includes_parent_snapshots_and_repairs_cached_missing_objects() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("ws");
        std::fs::create_dir_all(&base).unwrap();
        let state = crate::workspace_layout::ensure_workspace_state(&base).unwrap();
        let hub_data = dir.path().join("hub-data");
        let hub = LocalHub::open(hub_data.clone(), None).await.unwrap();
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
        let objects = ObjectStore::new(&ctx);
        let bundle = feanorfs_common::flat_to_tree_with_conflicts(&HashMap::new(), &[]).unwrap();
        let parent_root = objects.put_bundle(&bundle).await.unwrap();
        let parent_id = objects
            .put_snapshot(&Snapshot {
                root: parent_root.clone(),
                parents: Vec::new(),
                author: "parent".into(),
                created_at_ms: 1,
                message: None,
            })
            .await
            .unwrap();
        let child_id = objects
            .put_snapshot(&Snapshot {
                root: parent_root.clone(),
                parents: vec![parent_id.clone()],
                author: "child".into(),
                created_at_ms: 2,
                message: None,
            })
            .await
            .unwrap();

        let hashes = objects
            .snapshot_reachability(&child_id, true)
            .await
            .unwrap();
        assert!(hashes.contains(&child_id));
        assert!(hashes.contains(&parent_id));
        assert!(hashes.contains(&parent_root));

        tokio::fs::remove_file(hub_data.join("blobs").join(&parent_id))
            .await
            .unwrap();
        objects.publish_manifest(&child_id, &hashes).await.unwrap();
        assert!(hub_data.join("blobs").join(parent_id).is_file());
    }
}
