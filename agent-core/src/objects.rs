use crate::ctx::SyncCtx;
use crate::fs_util::atomic_write_visible;
use crate::prepared_tree::{PreparedTreeBundle, OBJECT_DOMAIN};
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{
    canonical_manifest_hashes, hash_bytes, is_safe_rel_path, is_valid_hash, pack_bytes,
    unpack_bytes_with_policy, ConcurrentEdit, FileState, LegacyPolicy, Snapshot, Tree, TreeBundle,
    TreeEntryKind, MANIFEST_MAX_ENTRIES, MAX_CANONICAL_OBJECT_BYTES, MAX_ENCRYPTED_OBJECT_BYTES,
    MAX_TREE_DEPTH, MAX_TREE_OBJECTS, MAX_TREE_OUTPUT_PATHS, MAX_TREE_PATH_BYTES_TOTAL,
    MAX_TREE_WORK_ITEMS,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use tokio::fs;

/// Encrypted immutable tree/snapshot store backed by FeanorFS CAS.
pub struct ObjectStore<'ctx, 'a> {
    ctx: &'ctx SyncCtx<'a>,
}

#[derive(Clone)]
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
        if let Some(tree) = self.ctx.memoized_tree(root) {
            return Ok(LoadedTree::clone(&tree));
        }
        let tree = self
            .get_tree_state_from(root, ObjectReadSource::CacheOrHub)
            .await?;
        self.ctx.memoize_tree(root, &tree);
        Ok(tree)
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

    /// Every object a snapshot keeps live on the hub: itself, its tree
    /// closure, and its ancestors' closures down to the recorded history floor.
    ///
    /// An ancestor whose closure the hub already accepted contributes that
    /// stored list, so the walk costs what changed since then instead of all
    /// history. Ancestors the hub already pruned (absent from the cache and 404
    /// on the hub) are skipped instead of failing the walk, and history older
    /// than the traversal limits is left out; the starting snapshot and its own
    /// tree must still be complete.
    pub(crate) async fn snapshot_reachability(
        &self,
        id: &str,
        expand_chunked_files: bool,
    ) -> Result<Vec<String>> {
        let floor = read_history_floor(&self.ctx.state_dir()?).await?;
        self.reachability_within(id, expand_chunked_files, &floor, REACHABILITY_LIMITS)
            .await
    }

    async fn reachability_within(
        &self,
        id: &str,
        expand_chunked_files: bool,
        floor: &HashSet<String>,
        limits: ReachabilityLimits,
    ) -> Result<Vec<String>> {
        let extend = ReachabilityWalk {
            expand_chunked_files,
            floor,
            limits,
            ancestor_entries: limits.entries,
            reuse_accepted: true,
        };
        if let Some(hashes) = self.walk_reachability(id, extend).await? {
            return Ok(hashes);
        }
        // Accepted history no longer fits beside this snapshot. Rebuild it
        // newest first into half the manifest, so later commits extend it
        // again instead of rebuilding every time.
        let rebuild = ReachabilityWalk {
            ancestor_entries: limits.entries / 2,
            reuse_accepted: false,
            ..extend
        };
        self.walk_reachability(id, rebuild)
            .await?
            .context("rebuilt snapshot reachability did not fit")
    }

    /// Returns `None` only when reused ancestor closures leave no room for the
    /// rest of the walk.
    ///
    /// Every tree and file id in the result has its complete closure in the
    /// result too: an ancestor that cannot be walked completely contributes
    /// only its snapshot id. Both the subtree pruning below and later walks
    /// that extend this closure rely on that.
    async fn walk_reachability(
        &self,
        id: &str,
        walk: ReachabilityWalk<'_>,
    ) -> Result<Option<Vec<String>>> {
        let limits = walk.limits;
        let mut found = Reachable::new(limits);
        let mut snapshots = Vec::new();
        let mut seen = HashSet::new();
        let mut pending = VecDeque::from([id.to_string()]);
        let mut trimmed = false;
        while let Some(snapshot_id) = pending.pop_front() {
            if !seen.insert(snapshot_id.clone()) {
                continue;
            }
            let required = snapshot_id == id;
            let at_floor = walk.floor.contains(&snapshot_id);
            if walk.reuse_accepted && !required && !at_floor {
                if let Some(closure) = self.accepted_closure(&snapshot_id).await? {
                    found.hashes.extend(closure);
                    if found.hashes.len() > limits.entries {
                        return Ok(None);
                    }
                    continue;
                }
            }
            let mut loaded = self.get_snapshot(&snapshot_id).await;
            if !required && loaded.as_ref().is_err_and(is_pruned) {
                self.confirm_hub_serves(id).await?;
                loaded = self.get_snapshot(&snapshot_id).await;
            }
            let snapshot = match loaded {
                Ok(snapshot) => snapshot,
                Err(error) if !required && is_pruned(&error) => continue,
                Err(error) => return Err(error),
            };
            if !found.charge_work(snapshot.parents.len().saturating_add(1)) {
                trimmed = true;
                break;
            }
            if !at_floor {
                pending.extend(snapshot.parents);
            }
            snapshots.push((snapshot_id, snapshot.root));
        }

        let reused = !found.hashes.is_empty();
        let mut snapshots = snapshots.into_iter();
        let (_, root) = snapshots
            .next()
            .context("snapshot exceeds the reachability limits")?;
        if !found.insert(id)?
            || !self
                .add_tree(&mut found, &root, walk.expand_chunked_files)
                .await?
        {
            if reused && found.is_full() {
                return Ok(None);
            }
            bail!("snapshot tree exceeds the reachability limits");
        }

        // Ancestor ids come before their trees, so history and signals stay
        // walkable when the trees of the oldest snapshots are left out.
        found.begin(walk.ancestor_entries);
        let mut roots = Vec::new();
        for (snapshot_id, root) in snapshots {
            if !found.insert(&snapshot_id)? {
                if reused {
                    return Ok(None);
                }
                trimmed = true;
                break;
            }
            roots.push(root);
        }
        for root in roots {
            found.begin(walk.ancestor_entries);
            let mut added = self
                .add_tree(&mut found, &root, walk.expand_chunked_files)
                .await;
            if added.as_ref().is_err_and(is_pruned) {
                found.roll_back();
                self.confirm_hub_serves(id).await?;
                added = self
                    .add_tree(&mut found, &root, walk.expand_chunked_files)
                    .await;
            }
            match added {
                Ok(true) => {}
                Ok(false) if reused && found.is_full() => return Ok(None),
                Ok(false) => {
                    found.roll_back();
                    trimmed = true;
                    break;
                }
                Err(error) if is_pruned(&error) => found.roll_back(),
                Err(error) => return Err(error),
            }
        }
        if trimmed {
            tracing::warn!(
                "Snapshot history exceeds the manifest limits; the oldest snapshots are no longer kept on the hub past its retention window"
            );
        }
        let mut hashes = found.hashes.into_iter().collect::<Vec<_>>();
        hashes.sort_unstable();
        Ok(Some(hashes))
    }

    /// A proxy in front of a restarting hub also answers 404, and leaving
    /// history out of a published list is permanent. An ancestor therefore
    /// counts as pruned only while the hub serves `id`, which this client has
    /// just uploaded; otherwise the walk fails and a later pass retries it.
    async fn confirm_hub_serves(&self, id: &str) -> Result<()> {
        let ciphertext = self
            .ctx
            .api
            .download_file_bounded(id, MAX_ENCRYPTED_OBJECT_BYTES)
            .await
            .context("hub does not serve the snapshot being published")?;
        ensure!(
            hash_bytes(&ciphertext) == id,
            "downloaded object hash mismatch for {id}"
        );
        Ok(())
    }

    /// Adds one tree closure, skipping every subtree and file already found.
    /// Returns `false` when a limit is reached; the caller rolls back.
    async fn add_tree(
        &self,
        found: &mut Reachable,
        root: &str,
        expand_chunked_files: bool,
    ) -> Result<bool> {
        let mut pending = vec![(root.to_string(), String::new())];
        while let Some((tree_id, prefix)) = pending.pop() {
            if found.hashes.contains(&tree_id) {
                continue;
            }
            let tree = self.get_tree(&tree_id).await?;
            if !found.charge_work(tree.entries.len().saturating_add(1))
                || !found.insert(&tree_id)?
            {
                return Ok(false);
            }
            for entry in tree.entries {
                let legs = match entry.kind {
                    TreeEntryKind::Dir => {
                        if !found.hashes.contains(&entry.hash) {
                            let Some(path) = found.child_path(&prefix, &entry.name)? else {
                                return Ok(false);
                            };
                            pending.push((entry.hash, path));
                        }
                        continue;
                    }
                    TreeEntryKind::File => vec![(entry.hash, Some(entry.size))],
                    TreeEntryKind::Conflict {
                        base, ours, theirs, ..
                    } => {
                        let mut legs = vec![entry.hash.clone()];
                        legs.extend(base);
                        legs.extend(ours);
                        legs.extend(theirs);
                        legs.sort_unstable();
                        legs.dedup();
                        legs.into_iter()
                            .map(|leg| {
                                let size = (leg == entry.hash).then_some(entry.size);
                                (leg, size)
                            })
                            .collect()
                    }
                };
                for (leg, size) in legs {
                    if found.hashes.contains(&leg) {
                        continue;
                    }
                    let Some(path) = found.child_path(&prefix, &entry.name)? else {
                        return Ok(false);
                    };
                    if !found.insert(&leg)? {
                        return Ok(false);
                    }
                    if !expand_chunked_files {
                        continue;
                    }
                    let chunks =
                        crate::large_file::reachable_chunks(self.ctx, &path, &leg, size).await?;
                    if !found.charge_work(chunks.len()) {
                        return Ok(false);
                    }
                    for chunk in &chunks {
                        if !found.insert(chunk)? {
                            return Ok(false);
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    /// The complete closure the hub accepted for `id`, when this client
    /// published it and the stored copy is intact.
    async fn accepted_closure(&self, id: &str) -> Result<Option<Vec<String>>> {
        if !is_valid_hash(id) {
            bail!("invalid snapshot id {id:?}");
        }
        let path = self.ctx.state_dir()?.join("manifests").join(id);
        let file = match fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read accepted closure"),
        };
        let mut bytes = Vec::new();
        use tokio::io::AsyncReadExt as _;
        file.take(MAX_CLOSURE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .context("read accepted closure")?;
        if bytes.len() as u64 > MAX_CLOSURE_BYTES {
            return Ok(None);
        }
        Ok(parse_accepted_closure(id, &bytes))
    }

    /// Publishes a snapshot's reachability manifest and returns the hash list
    /// the hub accepted.
    ///
    /// Histories first published under per-snapshot manifests lost older
    /// ancestor blobs to hub GC, so their full closure can never be accepted.
    /// When the hub still rejects it after `publish_manifest` re-uploaded every
    /// cached object, retry once with history cut at this snapshot's parents.
    /// Acceptance proves only that older history was missing, so the parents
    /// become the durable history floor and later walks stop there.
    pub(crate) async fn publish_reachability(&self, id: &str) -> Result<Vec<String>> {
        let hashes = self.snapshot_reachability(id, true).await?;
        let error = match self.publish_manifest(id, &hashes).await {
            Ok(()) => return Ok(hashes),
            Err(error) => error,
        };
        if crate::api::api_failure_kind(&error)
            != Some(crate::api::ApiFailureKind::ManifestReferencesMissingBlob)
        {
            return Err(error);
        }
        let state = self.ctx.state_dir()?;
        let parents = self.get_snapshot(id).await?.parents;
        let mut floor = read_history_floor(&state).await?;
        if parents.iter().all(|parent| floor.contains(parent)) {
            return Err(error);
        }
        floor.extend(parents.iter().cloned());
        let cut = ReachabilityWalk {
            expand_chunked_files: true,
            floor: &floor,
            limits: REACHABILITY_LIMITS,
            ancestor_entries: REACHABILITY_LIMITS.entries,
            reuse_accepted: false,
        };
        let shallow = self
            .walk_reachability(id, cut)
            .await?
            .context("cut snapshot reachability did not fit")?;
        // The failed attempt already re-uploaded every cached object, and the
        // cut closure is a subset of it, so a plain upload suffices here.
        self.ctx
            .api
            .upload_manifest(self.ctx.workspace_id(), id, &shallow)
            .await
            .context("hub rejected the manifest even with history cut at the previous head")?;
        let _ = crate::upload_registry::record_many(&state, &shallow).await;
        record_history_floor(&state, &parents).await?;
        tracing::warn!(
            "Hub no longer stores part of this workspace's older history; snapshot manifests now stop at {} recorded ancestor(s)",
            parents.len()
        );
        Ok(shallow)
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

    /// Stores a snapshot's object list for local GC. A list the hub accepted
    /// as a complete closure (`published`) is marked so later walks extend it;
    /// local-only lists, lists from older releases, and lists built without
    /// chunk expansion stay unmarked.
    pub(crate) async fn cache_manifest(
        &self,
        id: &str,
        hashes: &[String],
        published: bool,
    ) -> Result<()> {
        let canonical = feanorfs_common::canonical_manifest_hash_list(id, hashes)?;
        let mut manifest = canonical.join("\n");
        manifest.push('\n');
        if published && self.ctx.policy == LegacyPolicy::Reject {
            let checksum = hash_bytes(manifest.as_bytes());
            manifest.insert_str(0, &format!("{CLOSURE_HEADER}{checksum}\n"));
        }
        let state = self.ctx.state_dir()?;
        atomic_write_visible(&state, &format!("manifests/{id}"), manifest.as_bytes()).await?;
        crate::object_gc::prune(self.ctx.base).await
    }

    async fn put_bytes(&self, bytes: &[u8]) -> Result<String> {
        if bytes.len() > MAX_CANONICAL_OBJECT_BYTES {
            bail!("canonical object exceeds 16 MiB limit");
        }
        let ciphertext = pack_bytes(bytes, self.ctx.password_str(), OBJECT_DOMAIN)?;
        let id = hash_bytes(&ciphertext);
        // Tree/snapshot ids are deterministic: unchanged subtrees keep their
        // id across syncs, so neither rewrite an intact cache entry nor
        // re-upload ciphertext the hub already accepted on every pass.
        if cached_object(self.ctx, &id, MAX_ENCRYPTED_OBJECT_BYTES)
            .await?
            .is_none()
        {
            self.cache(&id, &ciphertext).await?;
        }
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
    // One small local read; a hop to the blocking pool costs more than it.
    let file = match std::fs::File::open(&cache_path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open cached object"),
    };
    let length = file.metadata().context("read object-cache metadata")?.len();
    let mut bytes = Vec::new();
    if length <= max_bytes as u64 {
        use std::io::Read as _;
        bytes.reserve_exact(length as usize);
        file.take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .context("read cached object")?;
    }
    if length > max_bytes as u64 || bytes.len() > max_bytes || hash_bytes(&bytes) != id {
        let _ = std::fs::remove_file(&cache_path);
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Aggregate bounds of one reachability walk.
#[derive(Clone, Copy)]
struct ReachabilityLimits {
    entries: usize,
    work_items: usize,
    path_bytes: usize,
}

/// History re-reads every old version of a folder, so its work bound is eight
/// times that of a single tree.
const REACHABILITY_LIMITS: ReachabilityLimits = ReachabilityLimits {
    entries: MANIFEST_MAX_ENTRIES,
    work_items: 8 * MAX_TREE_WORK_ITEMS,
    path_bytes: MAX_TREE_PATH_BYTES_TOTAL,
};

#[derive(Clone, Copy)]
struct ReachabilityWalk<'a> {
    expand_chunked_files: bool,
    floor: &'a HashSet<String>,
    limits: ReachabilityLimits,
    /// Entry limit once ancestors are added; the starting snapshot may always
    /// use the whole manifest.
    ancestor_entries: usize,
    reuse_accepted: bool,
}

/// Object ids found so far, plus those the current snapshot added, so one that
/// cannot be walked completely is undone.
struct Reachable {
    hashes: HashSet<String>,
    added: Vec<String>,
    limits: ReachabilityLimits,
    entry_limit: usize,
    work_items: usize,
    path_bytes: usize,
}

impl Reachable {
    fn new(limits: ReachabilityLimits) -> Self {
        Self {
            hashes: HashSet::new(),
            added: Vec::new(),
            limits,
            entry_limit: limits.entries,
            work_items: 0,
            path_bytes: 0,
        }
    }

    fn begin(&mut self, entry_limit: usize) {
        self.added.clear();
        self.entry_limit = entry_limit;
    }

    fn roll_back(&mut self) {
        for hash in self.added.drain(..) {
            self.hashes.remove(&hash);
        }
    }

    fn is_full(&self) -> bool {
        self.hashes.len() >= self.entry_limit
    }

    fn insert(&mut self, hash: &str) -> Result<bool> {
        if !is_valid_hash(hash) {
            bail!("snapshot reachability contains an invalid object id");
        }
        if self.hashes.contains(hash) {
            return Ok(true);
        }
        if self.is_full() {
            return Ok(false);
        }
        self.hashes.insert(hash.to_string());
        self.added.push(hash.to_string());
        Ok(true)
    }

    fn charge_work(&mut self, items: usize) -> bool {
        self.work_items = self.work_items.saturating_add(items);
        self.work_items <= self.limits.work_items
    }

    fn child_path(&mut self, prefix: &str, name: &str) -> Result<Option<String>> {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if !is_safe_rel_path(&path) || path.split('/').count() > MAX_TREE_DEPTH {
            bail!("reachable encrypted tree produced an unsafe path");
        }
        self.path_bytes = self.path_bytes.saturating_add(path.len());
        Ok((self.path_bytes <= self.limits.path_bytes).then_some(path))
    }
}

/// First line of a stored manifest that is a complete closure the hub
/// accepted: this marker plus the BLAKE3 of the list below it.
const CLOSURE_HEADER: &str = "ffclosure1:";
const MAX_CLOSURE_BYTES: u64 = (MANIFEST_MAX_ENTRIES as u64 + 2) * 65;

fn parse_accepted_closure(id: &str, bytes: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (checksum, list) = text.strip_prefix(CLOSURE_HEADER)?.split_once('\n')?;
    if hash_bytes(list.as_bytes()) != checksum {
        return None;
    }
    canonical_manifest_hashes(id, list).ok()
}

/// Ancestors whose objects the hub no longer stores (404) and that are absent
/// from the local cache: history pruned under an older manifest policy.
fn is_pruned(error: &anyhow::Error) -> bool {
    crate::api::request_error_status(error) == Some(http::StatusCode::NOT_FOUND)
}

/// Snapshot ids whose ancestors the hub no longer retains; reachability walks
/// keep each floor snapshot's own closure but never descend past it.
const HISTORY_FLOOR: &str = "history-floor";
/// Each recorded cut adds the parents of one snapshot. Forgetting an old entry
/// only costs one more rejected manifest before the cut is re-recorded.
const MAX_HISTORY_FLOOR_IDS: usize = 256;
const MAX_HISTORY_FLOOR_BYTES: u64 = (MAX_HISTORY_FLOOR_IDS as u64) * 65;

async fn read_history_floor_lines(state: &std::path::Path) -> Result<Vec<String>> {
    let file = match fs::File::open(state.join(HISTORY_FLOOR)).await {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read history floor"),
    };
    let mut text = String::new();
    use tokio::io::AsyncReadExt as _;
    file.take(MAX_HISTORY_FLOOR_BYTES)
        .read_to_string(&mut text)
        .await
        .context("read history floor")?;
    Ok(text
        .lines()
        .filter(|line| is_valid_hash(line))
        .map(str::to_string)
        .collect())
}

async fn read_history_floor(state: &std::path::Path) -> Result<HashSet<String>> {
    Ok(read_history_floor_lines(state).await?.into_iter().collect())
}

async fn record_history_floor(state: &std::path::Path, ids: &[String]) -> Result<()> {
    let mut lines = read_history_floor_lines(state).await?;
    lines.retain(|line| !ids.contains(line));
    lines.extend(ids.iter().filter(|id| is_valid_hash(id)).cloned());
    let keep_from = lines.len().saturating_sub(MAX_HISTORY_FLOOR_IDS);
    let mut text = lines[keep_from..].join("\n");
    text.push('\n');
    crate::fs_util::atomic_write_durable(state, HISTORY_FLOOR, text.as_bytes()).await
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

    struct PrunedHistory {
        _dir: tempfile::TempDir,
        base: std::path::PathBuf,
        hub_data: std::path::PathBuf,
        api: ApiClient,
        db: ClientDb,
    }

    impl PrunedHistory {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().join("ws");
            std::fs::create_dir_all(&base).unwrap();
            let state = crate::workspace_layout::ensure_workspace_state(&base).unwrap();
            let hub_data = dir.path().join("hub-data");
            let hub = LocalHub::open(hub_data.clone(), None).await.unwrap();
            Self {
                _dir: dir,
                base,
                hub_data,
                api: ApiClient::local(hub, None),
                db: ClientDb::new(state).await.unwrap(),
            }
        }

        fn ctx(&self) -> SyncCtx<'_> {
            SyncCtx::new(
                &self.api,
                &self.db,
                &self.base,
                "test-ws",
                Some(TEST_PASSWORD),
                feanorfs_common::LegacyPolicy::Reject,
            )
        }

        fn forget(&self, ctx: &SyncCtx<'_>, hash: &str) {
            let _ = std::fs::remove_file(self.hub_data.join("blobs").join(hash));
            let _ = std::fs::remove_file(ctx.state_dir().unwrap().join("objects").join(hash));
        }
    }

    async fn file_blob(ctx: &SyncCtx<'_>, content: &[u8]) -> String {
        let hash = feanorfs_common::hash_bytes(content);
        ctx.api
            .upload_object(ctx.workspace_id(), &hash, content.to_vec())
            .await
            .unwrap();
        hash
    }

    async fn snapshot_with(
        objects: &ObjectStore<'_, '_>,
        files: &[(&str, &str)],
        parents: Vec<String>,
    ) -> (String, String) {
        let files = files
            .iter()
            .map(|(path, hash)| {
                (
                    (*path).to_string(),
                    FileState {
                        path: (*path).to_string(),
                        hash: (*hash).to_string(),
                        size: 4,
                        mtime: 0,
                        deleted: false,
                        mode: 0,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let bundle = feanorfs_common::flat_to_tree_with_conflicts(&files, &[]).unwrap();
        let root = objects.put_bundle(&bundle).await.unwrap();
        let id = objects
            .put_snapshot(&Snapshot {
                root: root.clone(),
                parents,
                author: "test".into(),
                created_at_ms: 0,
                message: None,
            })
            .await
            .unwrap();
        (id, root)
    }

    #[tokio::test]
    async fn reachability_ends_at_pruned_ancestors_but_requires_the_start_snapshot() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let old = file_blob(&ctx, b"old!").await;
        let (grandparent, _) = snapshot_with(&objects, &[("a.txt", &old)], vec![]).await;
        let (parent, _) = snapshot_with(&objects, &[], vec![grandparent.clone()]).await;
        let (child, _) = snapshot_with(&objects, &[], vec![parent.clone()]).await;
        harness.forget(&ctx, &grandparent);

        let hashes = objects.snapshot_reachability(&child, true).await.unwrap();
        assert!(hashes.contains(&child) && hashes.contains(&parent));
        assert!(!hashes.contains(&grandparent) && !hashes.contains(&old));

        harness.forget(&ctx, &child);
        assert!(objects.snapshot_reachability(&child, true).await.is_err());
    }

    #[tokio::test]
    async fn rejected_history_is_cut_at_the_parents_and_remembered() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let old = file_blob(&ctx, b"old!").await;
        let new = file_blob(&ctx, b"new!").await;
        let (grandparent, _) = snapshot_with(&objects, &[("a.txt", &old)], vec![]).await;
        let (parent, parent_root) =
            snapshot_with(&objects, &[("a.txt", &new)], vec![grandparent.clone()]).await;
        objects.publish_reachability(&parent).await.unwrap();
        let (child, _) = snapshot_with(&objects, &[("a.txt", &new)], vec![parent.clone()]).await;
        // Hub GC under the old per-snapshot manifests dropped the old file
        // version; the ancestor trees themselves are still cached locally.
        harness.forget(&ctx, &old);

        let accepted = objects.publish_reachability(&child).await.unwrap();
        for kept in [&child, &parent, &parent_root, &new] {
            assert!(accepted.contains(kept));
        }
        assert!(!accepted.contains(&grandparent) && !accepted.contains(&old));
        assert_eq!(
            objects.snapshot_reachability(&child, true).await.unwrap(),
            accepted
        );

        // A floor already covering the parents does not hide a missing
        // current blob: the rejection surfaces instead of cutting further.
        harness.forget(&ctx, &new);
        let error = objects.publish_reachability(&child).await.unwrap_err();
        assert_eq!(
            crate::api::api_failure_kind(&error),
            Some(crate::api::ApiFailureKind::ManifestReferencesMissingBlob)
        );
    }

    async fn publish(objects: &ObjectStore<'_, '_>, id: &str) -> Vec<String> {
        let hashes = objects.publish_reachability(id).await.unwrap();
        objects.cache_manifest(id, &hashes, true).await.unwrap();
        hashes
    }

    async fn full_walk(objects: &ObjectStore<'_, '_>, id: &str) -> Vec<String> {
        let floor = HashSet::new();
        let walk = ReachabilityWalk {
            expand_chunked_files: true,
            floor: &floor,
            limits: REACHABILITY_LIMITS,
            ancestor_entries: REACHABILITY_LIMITS.entries,
            reuse_accepted: false,
        };
        objects.walk_reachability(id, walk).await.unwrap().unwrap()
    }

    /// Pruning at known ids is only sound while every listed tree has all its
    /// children listed too.
    async fn assert_tree_closed(objects: &ObjectStore<'_, '_>, hashes: &[String]) {
        for id in hashes {
            let Ok(tree) = objects.get_tree(id).await else {
                continue;
            };
            for entry in tree.entries {
                assert!(hashes.contains(&entry.hash), "{id} lost {}", entry.name);
            }
        }
    }

    /// One wide folder where every commit changes one file.
    async fn wide_commit(
        ctx: &SyncCtx<'_>,
        objects: &ObjectStore<'_, '_>,
        edit: usize,
        parents: Vec<String>,
    ) -> (String, String) {
        const WIDTH: usize = 12;
        let mut files = Vec::new();
        for index in 0..WIDTH {
            let last_edit = if index == edit % WIDTH { edit } else { 0 };
            let content = format!("file {index} edit {last_edit}");
            files.push((
                format!("wide/{index}.txt"),
                file_blob(ctx, content.as_bytes()).await,
            ));
        }
        let files = files
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect::<Vec<_>>();
        snapshot_with(objects, &files, parents).await
    }

    #[tokio::test]
    async fn accepted_closures_are_extended_without_walking_older_history() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let (grandparent, old_root) = wide_commit(&ctx, &objects, 1, vec![]).await;
        publish(&objects, &grandparent).await;
        let (parent, _) = wide_commit(&ctx, &objects, 2, vec![grandparent.clone()]).await;
        let accepted = publish(&objects, &parent).await;
        assert_eq!(accepted, full_walk(&objects, &parent).await);

        let (child, _) = wide_commit(&ctx, &objects, 3, vec![parent.clone()]).await;
        let complete = full_walk(&objects, &child).await;
        // Older history is not read again: it stays listed with its objects gone.
        harness.forget(&ctx, &grandparent);
        harness.forget(&ctx, &old_root);
        let extended = objects.snapshot_reachability(&child, true).await.unwrap();
        assert_eq!(extended, complete);

        // A signal shares its parent's tree and adds nothing but itself.
        let root = objects.get_snapshot(&child).await.unwrap().root;
        objects
            .cache_manifest(&child, &extended, true)
            .await
            .unwrap();
        let signal = objects
            .put_snapshot(&Snapshot {
                root,
                parents: vec![child.clone()],
                author: "agent".into(),
                created_at_ms: 1,
                message: None,
            })
            .await
            .unwrap();
        let signalled = objects.snapshot_reachability(&signal, true).await.unwrap();
        assert_eq!(signalled.len(), extended.len() + 1);
    }

    #[tokio::test]
    async fn only_intact_published_closures_are_extended() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let (id, _) = wide_commit(&ctx, &objects, 1, vec![]).await;
        let hashes = full_walk(&objects, &id).await;

        assert_eq!(objects.accepted_closure(&id).await.unwrap(), None);
        // Local-only lists and lists from older releases carry no marker.
        objects.cache_manifest(&id, &hashes, false).await.unwrap();
        assert_eq!(objects.accepted_closure(&id).await.unwrap(), None);
        objects.cache_manifest(&id, &hashes, true).await.unwrap();
        assert_eq!(
            objects.accepted_closure(&id).await.unwrap(),
            Some(hashes.clone())
        );

        let path = ctx.state_dir().unwrap().join("manifests").join(&id);
        let stored = std::fs::read_to_string(&path).unwrap();
        let shortened = stored.trim_end().rsplit_once('\n').unwrap().0;
        std::fs::write(&path, format!("{shortened}\n")).unwrap();
        assert_eq!(objects.accepted_closure(&id).await.unwrap(), None);
        // A list stored for one snapshot never stands in for another.
        let other = feanorfs_common::hash_bytes(b"other");
        std::fs::write(path.with_file_name(&other), &stored).unwrap();
        assert_eq!(objects.accepted_closure(&other).await.unwrap(), None);
    }

    /// Editing one file in a wide folder used to stop publishing for good once
    /// all historical folder listings together passed the walk limits.
    #[tokio::test]
    async fn repeated_edits_in_a_wide_folder_keep_publishing_whole_history() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let floor = HashSet::new();
        // Room for about three full tree walks; history grows to 30.
        let limits = ReachabilityLimits {
            work_items: 50,
            ..REACHABILITY_LIMITS
        };
        let mut parents = Vec::new();
        for edit in 1..=30 {
            let (id, _) = wide_commit(&ctx, &objects, edit, parents).await;
            let hashes = objects
                .reachability_within(&id, true, &floor, limits)
                .await
                .unwrap();
            objects.cache_manifest(&id, &hashes, true).await.unwrap();
            assert_eq!(hashes, full_walk(&objects, &id).await);
            parents = vec![id];
        }
    }

    #[tokio::test]
    async fn unpublished_history_over_the_limits_keeps_the_newest_snapshots() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let mut commits = Vec::new();
        for edit in 1..=10 {
            let parents = commits
                .last()
                .map(|(id, _): &(String, String)| vec![id.clone()])
                .unwrap_or_default();
            commits.push(wide_commit(&ctx, &objects, edit, parents).await);
        }
        let (head, head_root) = commits.last().unwrap();
        let limits = ReachabilityLimits {
            work_items: 65,
            ..REACHABILITY_LIMITS
        };
        let hashes = objects
            .reachability_within(head, true, &HashSet::new(), limits)
            .await
            .unwrap();
        assert!(hashes.contains(head) && hashes.contains(head_root));
        assert!(hashes.contains(&commits[8].1));
        assert!(!hashes.contains(&commits[0].1));
        for (id, _) in &commits {
            assert!(hashes.contains(id), "history must stay walkable");
        }
        assert_tree_closed(&objects, &hashes).await;

        // The starting snapshot itself is never published incomplete.
        let too_small = ReachabilityLimits {
            work_items: 12,
            ..REACHABILITY_LIMITS
        };
        assert!(objects
            .reachability_within(head, true, &HashSet::new(), too_small)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_full_manifest_is_rebuilt_with_room_to_grow() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let floor = HashSet::new();
        let limits = ReachabilityLimits {
            entries: 60,
            ..REACHABILITY_LIMITS
        };
        let mut parents = Vec::new();
        let mut previous = 0;
        let mut rebuilds = 0;
        for edit in 1..=40 {
            let (id, root) = wide_commit(&ctx, &objects, edit, parents).await;
            let hashes = objects
                .reachability_within(&id, true, &floor, limits)
                .await
                .unwrap();
            assert!(hashes.len() <= limits.entries);
            assert!(hashes.contains(&id) && hashes.contains(&root));
            assert_tree_closed(&objects, &hashes).await;
            if hashes.len() < previous {
                rebuilds += 1;
                assert!(hashes.len() <= limits.entries / 2);
            }
            previous = hashes.len();
            objects.cache_manifest(&id, &hashes, true).await.unwrap();
            parents = vec![id];
        }
        assert!((1..=6).contains(&rebuilds), "{rebuilds} rebuilds");
    }

    /// A 404 that the hub itself did not decide (a proxy answering while the
    /// hub restarts) must fail the walk, not drop history for good.
    #[tokio::test]
    async fn missing_history_is_skipped_only_while_the_hub_serves_this_snapshot() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let (grandparent, _) = wide_commit(&ctx, &objects, 1, vec![]).await;
        let (parent, parent_root) = wide_commit(&ctx, &objects, 2, vec![grandparent.clone()]).await;
        let (child, _) = wide_commit(&ctx, &objects, 3, vec![parent.clone()]).await;
        for missing in [&grandparent, &parent_root] {
            harness.forget(&ctx, missing);
            let served = harness.hub_data.join("blobs").join(&child);
            let blob = std::fs::read(&served).unwrap();
            std::fs::remove_file(&served).unwrap();
            assert!(objects.snapshot_reachability(&child, true).await.is_err());
            std::fs::write(&served, blob).unwrap();
            assert!(objects.snapshot_reachability(&child, true).await.is_ok());
        }
    }

    /// An ancestor missing part of its tree must not leave a half-listed
    /// folder behind: a later snapshot reusing that folder would be published
    /// without the missing part.
    #[tokio::test]
    async fn a_partly_pruned_ancestor_contributes_only_its_id() {
        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let (parent, parent_root) = wide_commit(&ctx, &objects, 1, vec![]).await;
        let folder = objects.get_tree(&parent_root).await.unwrap().entries[0]
            .hash
            .clone();
        let (child, _) = wide_commit(&ctx, &objects, 2, vec![parent.clone()]).await;
        harness.forget(&ctx, &folder);

        let hashes = objects.snapshot_reachability(&child, true).await.unwrap();
        assert!(hashes.contains(&parent));
        assert!(!hashes.contains(&parent_root) && !hashes.contains(&folder));
        assert_tree_closed(&objects, &hashes).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unchanged_objects_are_not_rewritten() {
        use std::os::unix::fs::MetadataExt as _;

        let harness = PrunedHistory::new().await;
        let ctx = harness.ctx();
        let objects = ObjectStore::new(&ctx);
        let (_, root) = wide_commit(&ctx, &objects, 1, vec![]).await;
        let path = ctx.state_dir().unwrap().join("objects").join(&root);
        let written = std::fs::metadata(&path).unwrap().ino();
        wide_commit(&ctx, &objects, 1, vec![]).await;
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), written);

        // A damaged entry is still replaced.
        std::fs::write(&path, b"damaged").unwrap();
        wide_commit(&ctx, &objects, 1, vec![]).await;
        assert_eq!(hash_bytes(&std::fs::read(&path).unwrap()), root);
    }
}
