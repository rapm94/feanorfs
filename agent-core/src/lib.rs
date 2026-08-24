//! Embeddable agent workspace isolation — blocking API over async internals.
//!
//! Rust consumers call [`Runtime`] / [`Workspace`] directly. FFI and Node bindings
//! serialize the same JSON shapes documented in `docs/agent-api.md`.

#[cfg(test)]
feanorfs_test_support::isolate_test_process!();

pub mod agent;
pub mod api;
pub mod conflict_artifacts;
pub mod conflicts;
pub mod crypto;
pub mod ctx;
mod durable;
pub mod fs_util;
pub mod head;
pub mod history;
pub mod hub;
mod hub_state;
pub mod integrator;
pub mod large_file;
pub mod local;
pub mod lock;
pub mod mesh;
pub mod messages;
mod object_gc;
pub mod objects;
pub mod paths;
mod prepared_tree;
pub mod resolution;
pub mod resolution_protocol;
mod signal_index;
pub mod snapshot;
mod snapshot_diff;
mod state;
pub mod sync_pass;
mod tree_reconcile;
pub mod tunnel;
mod upload_registry;
pub mod work;
pub mod workspace_layout;
pub mod workspace_read;
pub mod workspace_state_registry;

pub use agent::{
    build_status, check_agent, classify_continuous_error, clean_agent, commit_agent, land_agent,
    land_agent_continuous, land_agent_continuous_scoped, land_agent_guarded,
    land_agent_guarded_scoped, land_agent_runner_owned, land_agent_runner_owned_scoped,
    list_agents, live_continuous_status, live_reconciliation_health, partition_agent_scope,
    probe_agent_state, read_continuous_status, refresh_agent, refresh_agent_continuous,
    refresh_agent_guarded, refresh_agent_runner_owned, refresh_agent_with_options,
    remove_configured, resolve_request_admission, runner_process_metadata, runner_status,
    spawn_agent, verify_agent_worktree, write_continuous_status, AcceptedWorkDescriptor,
    ContinuousErrorClass, ContinuousOwnerLock, ContinuousProbe, LiveReconciliationHealth,
    RefreshOptions, RunnerAdmission, RunnerAdmissionReject, RunnerAttention, RunnerConfig,
    RunnerExecutionMode, RunnerExecutionSession, RunnerInvocation, RunnerLaunch, RunnerOwnership,
    RunnerPhase, RunnerProcessMetadata, RunnerScopeMode, RunnerStatus, RunnerStore, RunnerWorkWait,
    RunnerWorkWaitKind, ScopeChangePublishState, ScopeChangeRequestKey,
    ACCEPTED_WORK_SCHEMA_VERSION,
};
pub use api::{ApiClient, MIN_SUPPORTED_SERVER_VERSION};
pub use conflict_artifacts::{resolve_artifact, ArtifactRole};
pub use conflicts::{resolve_conflict, ResolveKeep};
pub use ctx::SyncCtx;
pub use feanorfs_common::{
    decode_invite, encode_invite, looks_like_invite, AgentCheckResult, AgentCleanResult,
    AgentCommitResult, AgentLandResult, AgentListEntry, AgentListOfflineResult, AgentListResult,
    AgentRefreshResult, ConcurrentEdit, ConflictKind, ConflictRecord, FileState, RelayConfig,
    SpawnResult, WorkspaceInvite, INVITE_PREFIX,
};
pub use head::{
    wait_for_head_change, HeadObservation, HeadObserver, HeadWaitOutcome, SwapHeadResult,
    MAX_HEAD_WAIT_MS,
};
pub use history::{log, undo};
pub use hub::LocalHub;
pub use integrator::{
    designate_conflict_owner, DesignationRefusal, DesignationRefusalKind, OwnerDesignation,
    OwnerDesignationEvidence, OwnerDesignationMethod,
};
pub use integrator::{
    integrator_assign, integrator_observe, integrator_resume, integrator_revoke, integrator_status,
    materialize_conflicts, IntegratorObserveOptions, IntegratorStateFile, IntegratorStore,
    PersistedIntegratorAssignment,
};
pub use local::{
    load_config, load_global_config, load_workspace_id, load_workspace_id_from_state, save_config,
    save_config_secure, save_global_config, save_global_config_secure, validate_e2ee_key, ClientDb,
    Config, CredentialProtection, GlobalConfig, LOCAL_HUB_URL,
};
pub use messages::{inbox, send_message, signals_since};
pub use objects::ObjectStore;
pub use paths::legacy_policy_for_config;
pub use paths::{agent_dir, agent_runner_dir, agents_dir, conflicts_dir, validate_name};
pub use resolution::{
    answer_resolution, apply_resolution_job, candidate_path_for, defer_resolution,
    materialize_resolution_legs, prepare_resolution_job, put_resolution_candidate,
    recover_uncertain_publications, resolution_status, revoke_resolution_assignment,
    submit_resolution_result, PersistedResolutionJob, ResolutionApplyOutcome,
    ResolutionAssignmentState, ResolutionJobStatus, ResolutionOpError, ResolutionStateFile,
    ResolutionStatusProjection, ResolutionStore,
};
pub use workspace_state_registry::{
    retire_workspace_state, sweep_retired_state, RetirementSweep, TombstoneRecord,
};

pub use resolution_protocol::{
    resolution_protocol_status, send_human_answer, send_resolution_assignment,
    send_resolution_result, send_resolution_revoke, ProtocolAssignmentState,
    ResolutionProtocolEntryStatus, ResolutionProtocolStatus,
};
pub use snapshot::SnapshotEngine;
pub use snapshot_diff::TreeDiffStats;
pub use work::{
    work_amend, work_block, work_complete, work_decide, work_propose, work_settle, work_status,
    work_yield,
};
pub use workspace_layout::{
    ensure_workspace_state, global_state_root, maintain_workspace_state, workspace_is_configured,
    workspace_state_id, workspace_state_path,
};

use anyhow::{Context, Result};
#[doc(hidden)]
pub use hub_state::{
    MigrationHubFence, MigrationHubFile, MigrationHubManifest, MigrationHubState,
    MigrationHubWorkspace,
};
#[doc(hidden)]
pub use local::ClientDb as _ClientDb;
pub use state::{ConflictRecordStatus, ResolutionMethod};
#[doc(hidden)]
pub use state::{
    MigrationAccessEntry, MigrationCacheEntry, MigrationConflictRecord,
    MigrationConflictResolution, MigrationLocalState,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Shared Tokio runtime for blocking SDK callers.
pub struct Runtime {
    inner: Option<tokio::runtime::Runtime>,
}

impl Runtime {
    /// Build a multi-thread Tokio runtime for agent operations.
    pub fn new() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: Some(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .context("failed to build Tokio runtime")?,
            ),
        }))
    }

    /// Run an async future to completion on this runtime.
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + Send,
        F::Output: Send,
    {
        let runtime = self
            .inner
            .as_ref()
            .expect("runtime is available until drop");
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| match scope.spawn(|| runtime.block_on(fut)).join() {
                Ok(output) => output,
                Err(payload) => std::panic::resume_unwind(payload),
            })
        } else {
            runtime.block_on(fut)
        }
    }

    /// Open a workspace rooted at `path` (state is resolved under `~/.feanorfs`).
    pub fn open_workspace(self: &Arc<Self>, path: impl AsRef<Path>) -> Result<Workspace> {
        Workspace::open(self, path.as_ref())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let Some(runtime) = self.inner.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            let _ = std::thread::spawn(move || drop(runtime)).join();
        } else {
            drop(runtime);
        }
    }
}

/// Options for [`Workspace::spawn`].
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub no_sync: bool,
    pub replace: bool,
}

/// Options for [`Workspace::land`].
#[derive(Debug, Clone, Default)]
pub struct LandOptions {
    pub clean: bool,
    pub propose: bool,
}

/// A configured FeanorFS workspace with agent operations.
pub struct Workspace {
    root: PathBuf,
    rt: Arc<Runtime>,
    config: Config,
    db: ClientDb,
    api: ApiClient,
}

impl Workspace {
    /// Load config, cache DB, and transport for a workspace directory.
    pub fn open(rt: &Arc<Runtime>, root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        let config = load_config(&root)?;
        let state = ensure_workspace_state(&root)?;
        let db = rt.block_on(ClientDb::new(state))?;
        let api = rt.block_on(ApiClient::from_config(&root, &config))?;
        rt.block_on(api.ensure_server_compatible())?;
        Ok(Self {
            root,
            rt: Arc::clone(rt),
            config,
            db,
            api,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    fn password(&self) -> Option<&str> {
        self.config.encryption_password.as_deref()
    }

    fn workspace_id(&self) -> &str {
        &self.config.workspace_id
    }

    /// List agent workspace names from global workspace state.
    pub fn list(&self) -> Result<Vec<String>> {
        self.rt.block_on(list_agents(&self.root, &self.db))
    }

    /// Return the absolute worktree path for an existing agent.
    pub fn agent_path(&self, name: &str) -> Result<PathBuf> {
        let path = agent_dir(&self.root, name)?;
        if !path.is_dir() {
            anyhow::bail!("agent workspace '{name}' not found");
        }
        Ok(path)
    }

    /// Spawn an isolated agent workspace.
    pub fn spawn(&self, name: &str, opts: SpawnOptions) -> Result<SpawnResult> {
        let files_copied = self.rt.block_on(spawn_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
            opts.no_sync,
            opts.replace,
        ))?;
        Ok(SpawnResult {
            agent: name.to_string(),
            files_copied,
        })
    }

    /// Read-only preview of one agent's changes and conflicts.
    pub fn status(&self, name: &str) -> Result<AgentCheckResult> {
        self.rt.block_on(check_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
        ))
    }

    /// Pull cloud changes into the agent for paths the agent has not edited.
    pub fn refresh(&self, name: &str) -> Result<AgentRefreshResult> {
        self.rt.block_on(refresh_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
        ))
    }

    /// Integrate agent work into the main workspace.
    pub fn land(&self, name: &str, opts: LandOptions) -> Result<AgentLandResult> {
        self.rt.block_on(land_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
            opts.clean,
            opts.propose,
        ))
    }

    /// Remove an agent workspace and its snapshot rows.
    pub fn clean(&self, name: &str) -> Result<AgentCleanResult> {
        self.rt.block_on(clean_agent(&self.root, &self.db, name))?;
        Ok(AgentCleanResult {
            cleaned: name.to_string(),
        })
    }

    /// Resolve a pending workspace conflict after reconciliation.
    pub fn resolve(&self, path: &str, keep: ResolveKeep, file_source: Option<&Path>) -> Result<()> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolve_conflict(&ctx, path, keep, file_source))
    }

    /// Lists reachable workspace snapshots, newest first.
    pub fn log(&self, limit: usize) -> Result<feanorfs_common::LogResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(history::log(&ctx, limit))
    }

    /// Restores a reachable snapshot as a new snapshot on current head.
    pub fn undo(&self, snapshot_id: &str) -> Result<feanorfs_common::UndoResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(history::undo(&ctx, snapshot_id))
    }

    /// Publishes one encrypted agent signal as a no-file-change snapshot.
    pub fn send_message(
        &self,
        input: feanorfs_common::AgentMessageInput,
    ) -> Result<feanorfs_common::AgentSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(messages::send_message(&ctx, input))
    }

    /// Reads signals addressed to a recipient from reachable snapshot history.
    pub fn inbox(
        &self,
        query: feanorfs_common::AgentInboxQuery,
    ) -> Result<feanorfs_common::AgentInboxResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(messages::inbox(&ctx, query))
    }

    /// Assigns one bounded batch to a randomly ranked integrator.
    pub fn integrator_assign(
        &self,
        input: feanorfs_common::IntegratorAssignInput,
    ) -> Result<feanorfs_common::IntegratorAssignResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(integrator::integrator_assign(&ctx, input))
    }

    /// Reads the current integrator assignment status.
    pub fn integrator_status(
        &self,
        assignment_id: Option<&str>,
    ) -> Result<feanorfs_common::IntegratorStatusResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(integrator::integrator_status(&ctx, assignment_id))
    }

    /// Explicitly revokes the active integrator assignment.
    pub fn integrator_revoke(
        &self,
        assignment_id: &str,
        reason: &str,
    ) -> Result<feanorfs_common::IntegratorStatusResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(integrator::integrator_revoke(&ctx, assignment_id, reason))
    }

    /// Resumes dispatcher observation after a restart (crash-safe).
    pub fn integrator_resume(
        &self,
        options: IntegratorObserveOptions,
    ) -> Result<feanorfs_common::IntegratorObserveResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(integrator::integrator_resume(&ctx, options))
    }

    /// Materializes the encrypted conflict triple for a snapshot (read-only).
    pub fn materialize_conflicts(
        &self,
        about_snapshot: &str,
        paths: &[String],
    ) -> Result<feanorfs_common::ConflictMaterializeResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(integrator::materialize_conflicts(
            &ctx,
            about_snapshot,
            paths,
        ))
    }

    /// Proposes one `ffwork1` work intent (sends an encrypted signal).
    pub fn work_propose(
        &self,
        input: feanorfs_common::WorkProposeInput,
    ) -> Result<feanorfs_common::WorkSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_propose(&ctx, input))
    }

    /// Sends one `ffwork1` coordinator decision for an exact proposal.
    pub fn work_decide(
        &self,
        input: feanorfs_common::WorkDecideInput,
    ) -> Result<feanorfs_common::WorkSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_decide(&ctx, input))
    }

    /// Sends one `ffwork1` scope amendment against an accepted intent.
    pub fn work_amend(
        &self,
        input: feanorfs_common::WorkAmendInput,
    ) -> Result<feanorfs_common::WorkSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_amend(&ctx, input))
    }

    /// Sends one `ffwork1` explicit yield relinquishing accepted overlap.
    pub fn work_yield(
        &self,
        input: feanorfs_common::WorkYieldInput,
    ) -> Result<feanorfs_common::WorkSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_yield(&ctx, input))
    }

    /// Sends one `ffwork1` settled profile with verification evidence.
    pub fn work_settle(
        &self,
        input: feanorfs_common::WorkSettleInput,
    ) -> Result<feanorfs_common::WorkSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_settle(&ctx, input))
    }

    /// Sends one `ffwork1` terminal completion.
    pub fn work_complete(
        &self,
        input: feanorfs_common::WorkCompleteInput,
    ) -> Result<feanorfs_common::WorkSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_complete(&ctx, input))
    }

    /// Sends one `ffwork1` terminal blocker.
    pub fn work_block(
        &self,
        input: feanorfs_common::WorkBlockInput,
    ) -> Result<feanorfs_common::WorkSendResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_block(&ctx, input))
    }

    /// Observes signals through the `ffwork1` reducer and reports the
    /// bounded projection (cursor-reset rebuilds are marked incomplete).
    pub fn work_status(
        &self,
        input: feanorfs_common::WorkStatusInput,
    ) -> Result<feanorfs_common::WorkStatusResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(work::work_status(&ctx, input))
    }

    /// Prepares one automatic resolution job for the exact current conflict
    /// at `path`. Requires a real current conflict and a
    /// typed prevention-exhausted/violated reason; refuses anything else.
    /// Prepare never mutates the worktree, conflict registry, artifacts, or
    /// head.
    pub fn resolution_prepare(
        &self,
        path: &str,
        prevention: feanorfs_common::PreventionReason,
    ) -> Result<feanorfs_common::ResolutionJob> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::prepare_resolution_job(&ctx, path, prevention))
    }

    /// Submits one resolution result for an exact job. Submission NEVER
    /// applies: it validates result schema/bounds, assignment/attempt/owner/
    /// fingerprint, and the immutable candidate, then records the result
    /// without mutating the worktree, registry, artifacts, or head. Apply is
    /// a separate explicit operation.
    pub fn resolution_submit(
        &self,
        job_id: &str,
        result: feanorfs_common::ResolutionResult,
    ) -> Result<feanorfs_common::ResolutionResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::submit_resolution_result(&ctx, job_id, result))
    }

    /// Applies one submitted resolution result with guarded publication
    /// by revalidating every identity field and the candidate
    /// descriptor immediately before a single CAS; a lost CAS restarts
    /// complete validation. The current conflict survives unchanged for any
    /// typed stale outcome.
    pub fn resolution_apply(&self, job_id: &str) -> Result<resolution::ResolutionApplyOutcome> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::apply_resolution_job(&ctx, job_id))
    }

    /// Reads the bounded resolution status projection (ids/state/counts
    /// only; never paths or bodies). Read-only and constant-cost; first
    /// converges any crash-left publication-uncertain records.
    pub fn resolution_status(
        &self,
        job_id: Option<&str>,
    ) -> Result<resolution::ResolutionStatusProjection> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::resolution_status(&ctx, job_id))
    }

    /// Writes the immutable engine-owned candidate file for one job from a
    /// bounded byte stream (create-new, no-follow, fsync'd) and returns its
    /// plaintext descriptor. Allowed while the job is active and carries no
    /// candidate-bearing result.
    pub fn resolution_put_candidate(
        &self,
        job_id: &str,
        bytes: &[u8],
    ) -> Result<feanorfs_common::CandidateDescriptor> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::put_resolution_candidate(&ctx, job_id, bytes))
    }

    /// Records a typed human answer bound to one exact escalation. Defer and
    /// keep_unresolved record terminal states without publication;
    /// submit_candidate records a `candidate_ready` result that a later
    /// guarded apply publishes.
    pub fn resolution_answer(
        &self,
        answer: feanorfs_common::resolution_contract::HumanResolutionAnswer,
    ) -> Result<feanorfs_common::resolution_contract::HumanResolutionAnswer> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::answer_resolution(&ctx, answer))
    }

    /// Records the terminal `Deferred` state for one assignment without any
    /// publication; the conflict is preserved for later manual action.
    pub fn resolution_defer(&self, job_id: &str) -> Result<()> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(resolution::defer_resolution(&ctx, job_id))
    }

    /// Materializes the authenticated base/ours/theirs legs of one job into
    /// the engine-owned job directory (create-new, no-follow, fsync'd) so a
    /// designated machine can reconstruct the conflict context by ID and
    /// fingerprint.
    pub fn resolution_materialize_legs(
        &self,
        job_id: &str,
    ) -> Result<Vec<(feanorfs_common::ArtifactRoleName, std::path::PathBuf)>> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::materialize_resolution_legs(&ctx, job_id))
    }

    /// Observes the encrypted signal stream through the deterministic
    /// `ffres1` reducer and returns the bounded metadata-only projection.
    /// `rebuild` resets the cursor and re-observes the bounded window.
    pub fn resolution_protocol_status(
        &self,
        rebuild: bool,
    ) -> Result<resolution_protocol::ResolutionProtocolStatus> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution_protocol::resolution_protocol_status(
                &ctx, rebuild,
            ))
    }

    /// Publishes the `ffres1` assignment profile (with the complete
    /// immutable job) for one locally prepared job.
    pub fn resolution_assign(&self, job_id: &str) -> Result<String> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution_protocol::send_resolution_assignment(
                &ctx, job_id,
            ))
    }

    /// Publishes the `ffres1` result profile for one locally submitted job.
    pub fn resolution_reply(&self, job_id: &str) -> Result<String> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution_protocol::send_resolution_result(&ctx, job_id))
    }

    /// Publishes the `ffres1` revoke/supersede profile for one local job.
    pub fn resolution_revoke(&self, job_id: &str, superseded: bool) -> Result<String> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution_protocol::send_resolution_revoke(
                &ctx, job_id, superseded,
            ))
    }

    /// Publishes one typed human answer as an `ffres1` profile.
    pub fn resolution_publish_answer(
        &self,
        answer: &feanorfs_common::resolution_contract::HumanResolutionAnswer,
    ) -> Result<String> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution_protocol::send_human_answer(&ctx, answer))
    }

    /// Converges every crash-left publication-uncertain record (bookkeeping
    /// completes when the CAS won; otherwise the record fails closed to
    /// stale). Returns the number of uncertain records recovered.
    pub fn resolution_recover(&self) -> Result<usize> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolution::recover_uncertain_publications(&ctx))
    }
}

#[cfg(test)]
mod blocking_runtime_tests {
    use super::Runtime;

    #[test]
    fn blocking_runtime_can_run_and_drop_inside_an_async_runtime() {
        let outer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        outer.block_on(async {
            let runtime = Runtime::new().unwrap();
            assert_eq!(runtime.block_on(async { 42 }), 42);
            drop(runtime);
        });
    }
}
