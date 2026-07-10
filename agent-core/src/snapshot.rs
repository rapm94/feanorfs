use crate::fs_util::atomic_write;
use crate::paths::validate_name;
use crate::{ObjectStore, SwapHeadResult, SyncCtx};
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    flat_to_tree_with_conflicts, is_valid_hash, ConcurrentEdit, FileState, Snapshot,
};
use std::collections::HashMap;
use std::io::ErrorKind;

const MAX_HEAD_RETRIES: usize = 8;
const WORKSPACE_REF: &str = ".feanorfs/refs/workspace";
const LAST_SYNCED_REF: &str = ".feanorfs/refs/last-synced";

pub(crate) struct SnapshotInput<'a> {
    pub files: &'a HashMap<String, FileState>,
    pub conflicts: &'a [ConcurrentEdit],
    pub parents: Vec<String>,
    pub author: &'a str,
    pub message: Option<String>,
}

/// Workspace snapshot operations over encrypted immutable objects and one CAS head.
pub struct SnapshotEngine<'ctx, 'a> {
    pub(crate) ctx: &'ctx SyncCtx<'a>,
    pub(crate) objects: ObjectStore<'ctx, 'a>,
}

impl<'ctx, 'a> SnapshotEngine<'ctx, 'a> {
    /// Binds snapshot operations to one workspace sync context.
    #[must_use]
    pub const fn new(ctx: &'ctx SyncCtx<'a>) -> Self {
        Self {
            ctx,
            objects: ObjectStore::new(ctx),
        }
    }

    /// Publishes a flat server view unless the current head already represents it.
    ///
    /// # Errors
    /// Returns an error for object failures or repeated concurrent head changes.
    pub async fn publish_server_view(
        &self,
        files: &HashMap<String, FileState>,
        author: &str,
    ) -> Result<String> {
        let mut expected = self.ctx.api.get_head(self.ctx.workspace_id()).await?;
        for _ in 0..MAX_HEAD_RETRIES {
            if let Some(current) = &expected {
                let current_files = self.load_files(current).await?;
                if same_view(&current_files, files) {
                    return Ok(current.clone());
                }
            }
            let id = self
                .write(SnapshotInput {
                    files,
                    conflicts: &[],
                    parents: expected.iter().cloned().collect(),
                    author,
                    message: None,
                })
                .await?;
            match self
                .ctx
                .api
                .swap_head(self.ctx.workspace_id(), expected.as_deref(), &id)
                .await?
            {
                SwapHeadResult::Swapped => return Ok(id),
                SwapHeadResult::Conflict(current) => expected = current,
            }
        }
        bail!("workspace head changed too many times while publishing snapshot")
    }

    /// Publishes a re-encrypted root without retaining an unreadable old-key parent.
    pub async fn publish_rekeyed_view(
        &self,
        files: &HashMap<String, FileState>,
        author: &str,
    ) -> Result<String> {
        let expected = self.ctx.api.get_head(self.ctx.workspace_id()).await?;
        let id = self
            .write(SnapshotInput {
                files,
                conflicts: &[],
                parents: Vec::new(),
                author,
                message: Some("rekey workspace".to_string()),
            })
            .await?;
        match self
            .ctx
            .api
            .swap_head(self.ctx.workspace_id(), expected.as_deref(), &id)
            .await?
        {
            SwapHeadResult::Swapped => Ok(id),
            SwapHeadResult::Conflict(_) => {
                bail!("workspace changed during rekey migration; retry from a fresh pull")
            }
        }
    }

    /// Loads one snapshot object.
    ///
    /// # Errors
    /// Returns an error when the snapshot cannot be fetched or decoded.
    pub async fn load_snapshot(&self, id: &str) -> Result<Snapshot> {
        self.objects.get_snapshot(id).await
    }

    /// Loads the visible flat file view for one snapshot.
    ///
    /// # Errors
    /// Returns an error when the snapshot or any tree object is unavailable.
    pub async fn load_files(&self, id: &str) -> Result<HashMap<String, FileState>> {
        let snapshot = self.load_snapshot(id).await?;
        self.objects.get_flat_tree(&snapshot.root).await
    }

    pub(crate) async fn load_state(&self, id: &str) -> Result<crate::objects::LoadedTree> {
        let snapshot = self.load_snapshot(id).await?;
        self.objects.get_tree_state(&snapshot.root).await
    }

    /// Records the current working-copy view unless its root is unchanged.
    ///
    /// # Errors
    /// Returns an error when existing refs or encrypted objects cannot be read or written.
    pub async fn snapshot_local_view(
        &self,
        files: &HashMap<String, FileState>,
        author: &str,
    ) -> Result<String> {
        self.record_ref_view(WORKSPACE_REF, files, author).await
    }

    /// Records the last-agreed sync view as one snapshot id.
    ///
    /// # Errors
    /// Returns an error when existing refs or encrypted objects cannot be read or written.
    pub async fn record_last_synced(
        &self,
        files: &HashMap<String, FileState>,
        author: &str,
    ) -> Result<String> {
        self.record_ref_view(LAST_SYNCED_REF, files, author).await
    }

    /// Loads the last-agreed sync view, or an empty view before first sync.
    ///
    /// # Errors
    /// Returns an error when the ref or its encrypted object closure is corrupt.
    pub async fn load_last_synced(&self) -> Result<HashMap<String, FileState>> {
        match self.read_ref(LAST_SYNCED_REF).await? {
            Some(id) => self.load_files(&id).await,
            None => Ok(HashMap::new()),
        }
    }

    pub(crate) async fn last_synced_id(&self) -> Result<Option<String>> {
        self.read_ref(LAST_SYNCED_REF).await
    }

    pub(crate) async fn resolve_conflict(
        &self,
        path: &str,
        files: &HashMap<String, FileState>,
        author: &str,
    ) -> Result<String> {
        let Some(mut expected) = self.ctx.api.get_head(self.ctx.workspace_id()).await? else {
            return self.publish_server_view(files, author).await;
        };
        for _ in 0..MAX_HEAD_RETRIES {
            let snapshot = self.load_snapshot(&expected).await?;
            let mut state = self.objects.get_tree_state(&snapshot.root).await?;
            state.conflicts.retain(|conflict| conflict.path != path);
            let candidate = self
                .write(SnapshotInput {
                    files,
                    conflicts: &state.conflicts,
                    parents: vec![expected.clone()],
                    author,
                    message: Some(format!("resolve {path}")),
                })
                .await?;
            match self
                .ctx
                .api
                .swap_head(self.ctx.workspace_id(), Some(&expected), &candidate)
                .await?
            {
                SwapHeadResult::Swapped => return Ok(candidate),
                SwapHeadResult::Conflict(Some(current)) => expected = current,
                SwapHeadResult::Conflict(None) => {
                    bail!("workspace head disappeared during conflict resolution")
                }
            }
        }
        bail!("workspace head changed too many times during conflict resolution")
    }

    pub(crate) async fn write(&self, input: SnapshotInput<'_>) -> Result<String> {
        self.write_inner(input, true).await
    }

    pub(crate) async fn write_local(&self, input: SnapshotInput<'_>) -> Result<String> {
        self.write_inner(input, false).await
    }

    async fn write_inner(&self, input: SnapshotInput<'_>, upload_manifest: bool) -> Result<String> {
        let bundle = flat_to_tree_with_conflicts(input.files, input.conflicts)?;
        let root = self.objects.put_bundle(&bundle).await?;
        let id = self
            .objects
            .put_snapshot(&Snapshot {
                root,
                parents: input.parents,
                author: input.author.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                message: input.message,
            })
            .await?;
        let hashes = self.objects.snapshot_reachability(&id).await?;
        if upload_manifest {
            self.ctx
                .api
                .upload_manifest(self.ctx.workspace_id(), &id, &hashes)
                .await?;
        }
        self.objects.cache_manifest(&id, &hashes).await?;
        Ok(id)
    }

    pub(crate) async fn read_agent_base(&self, name: &str) -> Result<String> {
        validate_name(name)?;
        self.read_ref(&format!(".feanorfs/agents/{name}/.feanorfs/base-snapshot"))
            .await?
            .with_context(|| format!("agent {name} has no base snapshot ref"))
    }

    pub(crate) async fn write_agent_base(&self, name: &str, id: &str) -> Result<()> {
        validate_name(name)?;
        if !is_valid_hash(id) {
            bail!("invalid agent base snapshot id");
        }
        atomic_write(
            self.ctx.base,
            &format!(".feanorfs/agents/{name}/.feanorfs/base-snapshot"),
            id.as_bytes(),
        )
        .await
    }

    pub(crate) async fn record_committed_refs(&self, id: &str) -> Result<()> {
        if !is_valid_hash(id) {
            bail!("invalid committed snapshot id");
        }
        atomic_write(self.ctx.base, WORKSPACE_REF, id.as_bytes()).await?;
        atomic_write(self.ctx.base, LAST_SYNCED_REF, id.as_bytes()).await
    }

    pub(crate) async fn record_last_synced_ref(&self, id: &str) -> Result<()> {
        if !is_valid_hash(id) {
            bail!("invalid last-synced snapshot id");
        }
        atomic_write(self.ctx.base, LAST_SYNCED_REF, id.as_bytes()).await
    }

    async fn record_ref_view(
        &self,
        reference: &str,
        files: &HashMap<String, FileState>,
        author: &str,
    ) -> Result<String> {
        let parent = self.read_ref(reference).await?;
        if let Some(current) = &parent {
            if same_view(&self.load_files(current).await?, files) {
                return Ok(current.clone());
            }
        }
        let id = self
            .write_local(SnapshotInput {
                files,
                conflicts: &[],
                parents: parent.into_iter().collect(),
                author,
                message: None,
            })
            .await?;
        atomic_write(self.ctx.base, reference, id.as_bytes()).await?;
        Ok(id)
    }

    async fn read_ref(&self, reference: &str) -> Result<Option<String>> {
        let path = self.ctx.base.join(reference);
        let value = match tokio::fs::read_to_string(&path).await {
            Ok(value) => value,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read ref {}", path.display()))
            }
        };
        let id = value.trim();
        if !is_valid_hash(id) {
            bail!("invalid snapshot ref at {}", path.display());
        }
        Ok(Some(id.to_string()))
    }
}

fn same_view(left: &HashMap<String, FileState>, right: &HashMap<String, FileState>) -> bool {
    left.len() == right.len()
        && left.iter().all(|(path, state)| {
            right.get(path).is_some_and(|other| {
                state.hash == other.hash
                    && state.size == other.size
                    && state.deleted == other.deleted
                    && state.mode == other.mode
            })
        })
}
