use crate::fs_util::atomic_write_durable;
use crate::paths::validate_name;
use crate::{ObjectStore, SwapHeadResult, SyncCtx};
use anyhow::{bail, Context, Result};
use feanorfs_common::{
    flat_to_tree_with_conflicts, is_valid_hash, ConcurrentEdit, FileState, Snapshot,
};
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;

const MAX_HEAD_RETRIES: usize = 8;
const WORKSPACE_REF: &str = "refs/workspace";
const LAST_SYNCED_REF: &str = "refs/last-synced";

/// Typed stale/invalid publication refusal.
#[derive(Debug)]
pub struct StalePublication {
    pub kind: feanorfs_common::ResolutionStaleKind,
    pub detail: String,
}

impl std::fmt::Display for StalePublication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for StalePublication {}

/// Typed lost-CAS signal: the head moved between revalidation and swap, so
/// the caller must discard the plan and restart ALL validation.
#[derive(Debug)]
pub struct LostCas {
    pub current_head: String,
}

impl std::fmt::Display for LostCas {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "workspace head changed during guarded publication (now {})",
            self.current_head
        )
    }
}

impl std::error::Error for LostCas {}

#[cfg(test)]
thread_local! {
    static INJECT_LOST_CAS: std::cell::RefCell<Option<(u32, Option<String>)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
/// Injects `times` lost-CAS outcomes into the next guarded publications.
/// When a replacement head is supplied, the injection first publishes that
/// head (modeling a head that changed during the CAS window) and then
/// reports the CAS as lost with it.
pub fn inject_lost_cas(times: u32, replacement_head: Option<String>) {
    INJECT_LOST_CAS.with(|cell| *cell.borrow_mut() = Some((times, replacement_head)));
}

/// Test-only crash points inside the guarded-publication flow. Injecting one
/// simulates process death at that exact boundary (the preceding write has
/// already committed), so recovery converges on the next store load.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestPublishCrashPoint {
    /// After the journal + `PublicationUncertain` record, before the CAS.
    BeforeCas,
    /// After the CAS won, before any bookkeeping write.
    AfterCas,
    /// After the history record, before the registry resolve.
    AfterHistory,
    /// After the registry resolve, before the Completed write.
    AfterRegistry,
    /// After the Completed write, before artifact/journal cleanup.
    AfterCompleted,
}

#[cfg(test)]
thread_local! {
    static INJECT_PUBLISH_CRASH: std::cell::RefCell<Option<TestPublishCrashPoint>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
/// Injects one simulated crash at `point` into the next guarded publication.
pub(crate) fn inject_publish_crash(point: TestPublishCrashPoint) {
    INJECT_PUBLISH_CRASH.with(|cell| *cell.borrow_mut() = Some(point));
}

#[cfg(test)]
/// Clears any injected publish crash (call in test teardown).
pub(crate) fn clear_publish_crash() {
    INJECT_PUBLISH_CRASH.with(|cell| *cell.borrow_mut() = None);
}

#[cfg(test)]
/// Consumes the injected crash at `point`: returns `true` (and clears it)
/// exactly when a crash was injected there — the caller then stops as if the
/// process died.
pub(crate) fn consume_publish_crash(point: TestPublishCrashPoint) -> bool {
    INJECT_PUBLISH_CRASH.with(|cell| {
        if *cell.borrow() == Some(point) {
            *cell.borrow_mut() = None;
            true
        } else {
            false
        }
    })
}

/// One validated guarded-publication plan.
pub(crate) struct ResolutionPublication {
    /// Revalidated full identity (base fields from the current head,
    /// automatic fields from the job).
    pub identity: feanorfs_common::ConflictIdentity,
    /// Exact full fingerprint of `identity`.
    pub fingerprint: String,
    /// Plaintext candidate descriptor; `None` for `no_change_required`.
    pub candidate: Option<feanorfs_common::CandidateDescriptor>,
    /// Canonical relative path of the immutable candidate file when the
    /// engine must re-read/rehash/upload it (automatic path).
    pub candidate_file: Option<String>,
    /// Server view for the conflict path when the content was already
    /// uploaded (manual path); ignored when `candidate_file` is set.
    pub manual_state: Option<FileState>,
    /// Additional paths to apply alongside the conflict path.
    pub additional: Vec<(String, FileState)>,
    /// Revalidated head to CAS from (exactly one attempt).
    pub expected_head: String,
    pub author: String,
}

/// One fully computed guarded-publication plan: the pre-CAS head and the
/// snapshot id the CAS will produce. A caller journals [`Self::candidate_id`]
/// durably BEFORE the CAS so a crash mid-publication can later confirm
/// whether the CAS won (current head == planned candidate id).
pub(crate) struct PlannedResolutionPublication {
    pub expected_head: String,
    pub candidate_id: String,
}

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

    pub(crate) async fn load_files_local(&self, id: &str) -> Result<HashMap<String, FileState>> {
        let snapshot = self.objects.get_snapshot_local(id).await?;
        self.objects.get_flat_tree_local(&snapshot.root).await
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
        additional_paths: &[String],
        author: &str,
    ) -> Result<String> {
        self.resolve_conflicts(&[path.to_string()], files, additional_paths, author)
            .await
    }

    pub(crate) async fn resolve_conflicts(
        &self,
        paths: &[String],
        files: &HashMap<String, FileState>,
        additional_paths: &[String],
        author: &str,
    ) -> Result<String> {
        if paths.is_empty() {
            bail!("at least one conflict path is required");
        }
        let resolved: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let Some(mut expected) = self.ctx.api.get_head(self.ctx.workspace_id()).await? else {
            bail!("workspace head disappeared during conflict resolution");
        };
        for _ in 0..MAX_HEAD_RETRIES {
            let snapshot = self.load_snapshot(&expected).await?;
            let mut state = self.objects.get_tree_state(&snapshot.root).await?;
            state
                .conflicts
                .retain(|conflict| !resolved.contains(conflict.path.as_str()));
            for path in paths.iter().chain(additional_paths) {
                match files.get(path).filter(|file| !file.deleted) {
                    Some(file) => {
                        state.files.insert(path.clone(), file.clone());
                    }
                    None => {
                        state.files.remove(path);
                    }
                }
            }
            let message = if paths.len() == 1 {
                format!("resolve {}", paths[0])
            } else {
                format!("resolve {} conflicts", paths.len())
            };
            let candidate = self
                .write(SnapshotInput {
                    files: &state.files,
                    conflicts: &state.conflicts,
                    parents: vec![expected.clone()],
                    author,
                    message: Some(message),
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

    /// Guarded publication of exactly one resolution candidate.
    ///
    /// Reloads the head/tree/exact conflict from `plan.expected_head`,
    /// recomputes the full identity/fingerprint, descriptor-opens and
    /// rehashes the immutable candidate (uploading it sealed), builds a
    /// snapshot that changes exactly the candidate/additional paths and
    /// removes exactly the matching conflict identity, and performs ONE CAS
    /// from the revalidated head.
    ///
    /// On CAS loss the caller must discard the plan and restart all
    /// validation: this method never retries with a bare `expected` update
    /// while keeping a path-removal set.
    ///
    /// # Errors
    /// Returns [`StalePublication`] for any revalidation mismatch and
    /// [`LostCas`] when the head moved during the single CAS attempt.
    pub(crate) async fn publish_resolution(&self, plan: ResolutionPublication) -> Result<String> {
        let planned = self.plan_resolution_publication(plan).await?;
        self.commit_resolution_publication(&planned).await
    }

    /// Computes the full guarded-publication plan without swapping the head:
    /// validates the head, re-reads/rehashes/uploads the candidate, and
    /// writes the candidate snapshot. The returned id is the head the CAS
    /// will produce, so a caller can durably journal it BEFORE the atomic
    /// swap. Re-running the plan after a lost CAS re-uploads idempotently
    /// (content-addressed objects) and produces a fresh snapshot id.
    ///
    /// # Errors
    /// Returns [`StalePublication`] for any revalidation mismatch.
    pub(crate) async fn plan_resolution_publication(
        &self,
        plan: ResolutionPublication,
    ) -> Result<PlannedResolutionPublication> {
        use crate::conflict_artifacts::leg_descriptor;
        use feanorfs_common::{compute_conflict_identity_fingerprint, ResolutionStaleKind};

        let Some(head) = self.ctx.api.get_head(self.ctx.workspace_id()).await? else {
            return Err(anyhow::Error::new(StalePublication {
                kind: ResolutionStaleKind::HeadChanged,
                detail: "workspace head disappeared during guarded publication".to_string(),
            }));
        };
        if head != plan.expected_head {
            return Err(anyhow::Error::new(LostCas { current_head: head }));
        }
        let snapshot = self.load_snapshot(&head).await?;
        let state = self.objects.get_tree_state(&snapshot.root).await?;
        let Some(conflict) = state
            .conflicts
            .iter()
            .find(|candidate| candidate.path == plan.identity.path)
        else {
            return Err(anyhow::Error::new(StalePublication {
                kind: ResolutionStaleKind::ConflictMissing,
                detail: format!(
                    "conflict at '{}' no longer exists in the current head",
                    plan.identity.path
                ),
            }));
        };
        let legs_equal = |left: &Option<FileState>,
                          right: &feanorfs_common::ConflictLegDescriptor| {
            &leg_descriptor(left.as_ref()) == right
        };
        if !(legs_equal(&conflict.base, &plan.identity.base)
            && legs_equal(&conflict.ours, &plan.identity.ours)
            && legs_equal(&conflict.theirs, &plan.identity.theirs))
        {
            return Err(anyhow::Error::new(StalePublication {
                kind: ResolutionStaleKind::LegsChanged,
                detail: format!(
                    "conflict at '{}' changed legs in the current head",
                    plan.identity.path
                ),
            }));
        }
        let mut identity = plan.identity.clone();
        identity.current_snapshot = head.clone();
        identity.about_snapshot = head.clone();
        identity.tree_root = snapshot.root.clone();
        identity.base = leg_descriptor(conflict.base.as_ref());
        identity.ours = leg_descriptor(conflict.ours.as_ref());
        identity.theirs = leg_descriptor(conflict.theirs.as_ref());
        identity.kind =
            feanorfs_common::derive_conflict_kind(&identity.base, &identity.ours, &identity.theirs);
        if identity != plan.identity {
            return Err(anyhow::Error::new(StalePublication {
                kind: ResolutionStaleKind::IdentityMismatch,
                detail: format!(
                    "recomputed identity for '{}' no longer matches the job",
                    plan.identity.path
                ),
            }));
        }
        if compute_conflict_identity_fingerprint(&identity) != plan.fingerprint {
            return Err(anyhow::Error::new(StalePublication {
                kind: ResolutionStaleKind::IdentityMismatch,
                detail: format!(
                    "recomputed fingerprint for '{}' no longer matches the job",
                    plan.identity.path
                ),
            }));
        }

        // Descriptor-open + rehash the immutable candidate; upload it sealed.
        let mut candidate_state: Option<FileState> = plan.manual_state.clone();
        if let Some(candidate) = &plan.candidate {
            if let Some(relative) = &plan.candidate_file {
                let bytes = read_candidate_file(self.ctx, relative, candidate).await?;
                if candidate.deleted {
                    candidate_state = None;
                } else {
                    let (hash, packed) =
                        crate::crypto::seal(&bytes, self.ctx.password_str(), &plan.identity.path)?;
                    self.ctx
                        .api
                        .upload_object(self.ctx.workspace_id(), &hash, packed)
                        .await?;
                    candidate_state = Some(FileState {
                        path: plan.identity.path.clone(),
                        hash,
                        size: bytes.len() as u64,
                        mtime: chrono::Utc::now().timestamp_millis(),
                        deleted: false,
                        mode: candidate.mode,
                    });
                }
            } else if candidate.deleted {
                candidate_state = None;
            }
        }

        let mut files = state.files.clone();
        if plan.candidate.is_some() {
            match &candidate_state {
                Some(file) => {
                    files.insert(plan.identity.path.clone(), file.clone());
                }
                None => {
                    files.remove(&plan.identity.path);
                }
            }
        }
        for (additional_path, file) in &plan.additional {
            files.insert(additional_path.clone(), file.clone());
        }
        let mut conflicts = state.conflicts.clone();
        conflicts.retain(|candidate| candidate.path != plan.identity.path);

        let message = format!("resolve {}", plan.identity.path);
        let candidate_id = self
            .write(SnapshotInput {
                files: &files,
                conflicts: &conflicts,
                parents: vec![head.clone()],
                author: &plan.author,
                message: Some(message),
            })
            .await?;
        Ok(PlannedResolutionPublication {
            expected_head: head,
            candidate_id,
        })
    }

    /// Single CAS of a previously planned publication: re-checks the head is
    /// still the expected one, then swaps to the planned candidate snapshot.
    /// On CAS loss the caller must discard the plan and restart ALL
    /// validation.
    ///
    /// # Errors
    /// Returns [`LostCas`] when the head moved before the swap.
    pub(crate) async fn commit_resolution_publication(
        &self,
        planned: &PlannedResolutionPublication,
    ) -> Result<String> {
        use feanorfs_common::ResolutionStaleKind;

        let head = planned.expected_head.clone();
        #[cfg(test)]
        {
            let (inject_times, inject_replacement) = INJECT_LOST_CAS.with(|cell| {
                let mut cell = cell.borrow_mut();
                if let Some((times, replacement)) = cell.as_mut() {
                    if *times > 0 {
                        *times -= 1;
                        return (*times + 1, replacement.clone());
                    }
                }
                (0, None)
            });
            if inject_times > 0 {
                let current_head = if let Some(replacement) = inject_replacement {
                    let _ = self
                        .ctx
                        .api
                        .swap_head(self.ctx.workspace_id(), Some(&head), &replacement)
                        .await;
                    replacement
                } else {
                    head.clone()
                };
                return Err(anyhow::Error::new(LostCas { current_head }));
            }
        }
        match self
            .ctx
            .api
            .swap_head(self.ctx.workspace_id(), Some(&head), &planned.candidate_id)
            .await?
        {
            SwapHeadResult::Swapped => Ok(planned.candidate_id.clone()),
            SwapHeadResult::Conflict(Some(current)) => Err(anyhow::Error::new(LostCas {
                current_head: current,
            })),
            SwapHeadResult::Conflict(None) => Err(anyhow::Error::new(StalePublication {
                kind: ResolutionStaleKind::HeadChanged,
                detail: "workspace head disappeared during the single guarded CAS".to_string(),
            })),
        }
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
        let hashes = self
            .objects
            .snapshot_reachability(&id, upload_manifest)
            .await?;
        if upload_manifest {
            self.objects.publish_manifest(&id, &hashes).await?;
        }
        self.objects.cache_manifest(&id, &hashes).await?;
        Ok(id)
    }

    pub(crate) async fn read_agent_base(&self, name: &str) -> Result<String> {
        validate_name(name)?;
        self.read_ref(&format!("agents/{name}/state/base-snapshot"))
            .await?
            .with_context(|| format!("agent {name} has no base snapshot ref"))
    }

    pub(crate) async fn write_agent_base(&self, name: &str, id: &str) -> Result<()> {
        validate_name(name)?;
        if !is_valid_hash(id) {
            bail!("invalid agent base snapshot id");
        }
        let state = self.ctx.state_dir()?;
        atomic_write_durable(
            &state,
            &format!("agents/{name}/state/base-snapshot"),
            id.as_bytes(),
        )
        .await
    }

    pub(crate) async fn record_committed_refs(&self, id: &str) -> Result<()> {
        if !is_valid_hash(id) {
            bail!("invalid committed snapshot id");
        }
        let state = self.ctx.state_dir()?;
        atomic_write_durable(&state, WORKSPACE_REF, id.as_bytes()).await?;
        atomic_write_durable(&state, LAST_SYNCED_REF, id.as_bytes()).await
    }

    pub(crate) async fn record_last_synced_ref(&self, id: &str) -> Result<()> {
        if !is_valid_hash(id) {
            bail!("invalid last-synced snapshot id");
        }
        let state = self.ctx.state_dir()?;
        atomic_write_durable(&state, LAST_SYNCED_REF, id.as_bytes()).await
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
        let state = self.ctx.state_dir()?;
        atomic_write_durable(&state, reference, id.as_bytes()).await?;
        Ok(id)
    }

    async fn read_ref(&self, reference: &str) -> Result<Option<String>> {
        let path = self.ctx.state_dir()?.join(reference);
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

/// Descriptor-opens the immutable candidate beneath the protected state
/// root, rejects symlinks/reparse aliases, and verifies the bounded content
/// against the plaintext descriptor. Shared by candidate submission and
/// guarded publication.
///
/// Every open failure is classified by downcasting the typed
/// [`crate::workspace_read::CandidateOpenError`] produced by the no-follow
/// open — never by inspecting error text: missing → `CandidateMissing`,
/// permission denied → `CandidatePermissionDenied`, symlink/reparse →
/// `CandidateSymlink`, not-a-regular-file → `CandidateInvalidType`, and any
/// other I/O failure → `CandidateIoError`.
pub(crate) async fn read_candidate_file(
    ctx: &SyncCtx<'_>,
    relative: &str,
    candidate: &feanorfs_common::CandidateDescriptor,
) -> Result<Vec<u8>> {
    use crate::workspace_read::CandidateOpenError;
    use feanorfs_common::ResolutionStaleKind;

    fn stale(kind: ResolutionStaleKind, detail: String) -> anyhow::Error {
        anyhow::Error::new(StalePublication { kind, detail })
    }

    /// Maps one typed open failure to the exact candidate stale kind.
    fn open_stale_kind(error: &CandidateOpenError) -> anyhow::Error {
        let detail = error.to_string();
        match error {
            CandidateOpenError::NotFound(_) => stale(ResolutionStaleKind::CandidateMissing, detail),
            CandidateOpenError::PermissionDenied(_) => {
                stale(ResolutionStaleKind::CandidatePermissionDenied, detail)
            }
            CandidateOpenError::Symlink(_) => stale(ResolutionStaleKind::CandidateSymlink, detail),
            CandidateOpenError::InvalidType(_) => {
                stale(ResolutionStaleKind::CandidateInvalidType, detail)
            }
            CandidateOpenError::Io(_) => stale(ResolutionStaleKind::CandidateIoError, detail),
        }
    }

    fn open_error_stale(error: &anyhow::Error, relative: &str) -> anyhow::Error {
        match error.downcast_ref::<CandidateOpenError>() {
            Some(typed) => open_stale_kind(typed),
            // Path-validation and other non-typed failures count as
            // "other I/O" for the typed candidate classification.
            None => stale(
                ResolutionStaleKind::CandidateIoError,
                format!("candidate file {relative} cannot be opened: {error}"),
            ),
        }
    }

    let state_root = ctx.state_dir()?;
    let root = crate::workspace_read::WorkspaceReadRoot::open(&state_root)
        .map_err(|error| open_error_stale(&error, relative))?;
    if candidate.deleted {
        // A deletion candidate may carry an empty marker or nothing. The
        // absent case is ONLY a genuine not-found; permission, symlink,
        // type, and other I/O failures are reported with their typed kind.
        match root.open_regular(relative) {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                use std::io::Read as _;
                file.read_to_end(&mut bytes)
                    .context("read deleted candidate marker")?;
                if !bytes.is_empty() {
                    return Err(stale(
                        ResolutionStaleKind::CandidateSizeMismatch,
                        format!("deleted candidate {} must be empty or absent", relative),
                    ));
                }
                Ok(bytes)
            }
            Err(error) => match error.downcast_ref::<CandidateOpenError>() {
                Some(CandidateOpenError::NotFound(_)) => Ok(Vec::new()),
                Some(typed) => Err(open_stale_kind(typed)),
                None => Err(open_error_stale(&error, relative)),
            },
        }
    } else {
        let file = root
            .open_regular(relative)
            .map_err(|error| open_error_stale(&error, relative))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("candidate metadata {relative}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            // The immutable candidate must be engine-exclusive: a hard-linked
            // alias (nlink > 1) means another name can mutate the same inode,
            // so it is rejected typed.
            if metadata.nlink() > 1 {
                return Err(stale(
                    ResolutionStaleKind::CandidateIoError,
                    format!(
                        "candidate {relative} is hard-linked (nlink {}); immutable candidates \
                         must be singly-linked (aliasing rejected)",
                        metadata.nlink()
                    ),
                ));
            }
        }
        if metadata.len() > feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES {
            return Err(stale(
                ResolutionStaleKind::CandidateSizeMismatch,
                format!(
                    "candidate {relative} exceeds the {} byte bound",
                    feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES
                ),
            ));
        }
        let portable = portable_file_mode(&metadata);
        if portable != candidate.mode {
            return Err(stale(
                ResolutionStaleKind::CandidateModeMismatch,
                format!(
                    "candidate {relative} mode {portable} does not match descriptor {}",
                    candidate.mode
                ),
            ));
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(usize::MAX)
                .min(8 * 1024 * 1024),
        );
        {
            use tokio::io::AsyncReadExt as _;
            let mut file = tokio::fs::File::from_std(file);
            let limit = feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES
                .checked_add(1)
                .context("candidate read limit overflow")?;
            let mut bounded = (&mut file).take(limit);
            bounded.read_to_end(&mut bytes).await?;
        }
        if bytes.len() as u64 != metadata.len() {
            return Err(stale(
                ResolutionStaleKind::CandidateSizeMismatch,
                format!("candidate {relative} changed while it was being read"),
            ));
        }
        let observed = feanorfs_common::hash_bytes(&bytes);
        if observed != candidate.hash {
            return Err(stale(
                ResolutionStaleKind::CandidateHashMismatch,
                format!(
                    "candidate {relative} hashes to {observed}, descriptor expects {}",
                    candidate.hash
                ),
            ));
        }
        Ok(bytes)
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

pub(crate) fn portable_file_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 != 0 {
            feanorfs_common::EXECUTABLE_MODE
        } else {
            0
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}
