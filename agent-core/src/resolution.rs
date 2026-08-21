//! Guarded automatic conflict resolution.
//!
//! Owns the private, bounded, schema-versioned `ResolutionJob` store, the
//! immutable create-new candidate output beneath the protected workspace
//! `orchestrator/` boundary, candidate submission validation, and guarded
//! publication. Prepare/submit never touch the worktree, conflict registry,
//! artifacts, or head; apply revalidates every identity field and candidate
//! descriptor immediately before a single CAS and discards the plan on CAS
//! loss, restarting complete validation (never a path-removal retry).
//!
//! Losing state never authorizes a publication: corrupt or unsupported state
//! fails closed.

use crate::conflict_artifacts::{conflict_identity_from_edit, IdentityBinding};
use crate::ctx::SyncCtx;
use crate::durable::DurableJson;
use crate::integrator::{designate_conflict_owner, DesignationRefusalKind};
use crate::lock::SyncLock;
use crate::snapshot::{ResolutionPublication, SnapshotEngine, StalePublication};
use crate::state::ResolutionMethod;
use crate::work::WorkStore;
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::integrator_contract::VerificationCheck;
use feanorfs_common::resolution_contract::{
    validate_designation_evidence, validate_human_resolution_answer, HumanResolutionAnswer,
    HumanResolutionOption, OwnerDesignationEvidence, OwnerDesignationMethod,
};
use feanorfs_common::{
    compute_conflict_identity_fingerprint, validate_conflict_identity, validate_resolution_job,
    validate_resolution_result, CandidateDescriptor, PreventionReason, ResolutionJob,
    ResolutionOutcome, ResolutionResult, ResolutionStaleKind, VerificationStatus,
    VerificationSummary, WorkTaskState, EXECUTABLE_MODE, RESOLUTION_SCHEMA_VERSION,
    RESOLUTION_VERIFICATION_POLICY_ID,
};
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const RESOLUTION_STORE_SCHEMA_VERSION: u32 = 1;
const RESOLUTION_STORE_FILE: &str = "resolution-state.json";
const RESOLUTION_JOB_FILE: &str = "job.json";
const RESOLUTION_MAX_JOBS: usize = 64;
const RESOLUTION_MAX_APPLY_REVALIDATIONS: usize = 3;
/// Bounded durable journal written before the guarded CAS so a crash can be
/// recovered (the journal names the planned new head).
const PUBLICATION_PENDING_JOURNAL_FILE: &str = "publication-pending.json";
/// Byte bound of the serialized `publication-pending` journal (the embedded
/// result is already bounded by `validate_resolution_result`).
const RESOLUTION_MAX_JOURNAL_BYTES: usize = 32 * 1024;

/// Typed rejection of one resolution engine operation. Wrapped in
/// `anyhow::Error` (downcastable); never a bare string.
#[derive(Debug)]
pub enum ResolutionOpError {
    UnknownJob(String),
    JobNotActive {
        job_id: String,
        state: ResolutionAssignmentState,
    },
    ResultAlreadySubmitted(String),
    CandidateAlreadyExists(String),
    CandidateTooLarge {
        job_id: String,
        size: u64,
    },
    DuplicateFingerprintJob(String),
    ActiveJobLimitExceeded,
    NoOutstandingQuestion(String),
    StaleQuestionGeneration {
        job_id: String,
        expected: u32,
        received: u32,
    },
    AnswerBindingMismatch {
        job_id: String,
        detail: String,
    },
    NotSubmittedWithoutCandidate(String),
}

impl std::fmt::Display for ResolutionOpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownJob(job_id) => write!(formatter, "unknown resolution job {job_id}"),
            Self::JobNotActive { job_id, state } => write!(
                formatter,
                "resolution job {job_id} is not active (state {state:?}); operation refused"
            ),
            Self::ResultAlreadySubmitted(job_id) => {
                write!(formatter, "resolution job {job_id} already has a submitted result")
            }
            Self::CandidateAlreadyExists(path) => write!(
                formatter,
                "candidate file {path} already exists; the destination is create-new"
            ),
            Self::CandidateTooLarge { job_id, size } => write!(
                formatter,
                "candidate for {job_id} is {size} bytes, exceeding the {} byte bound",
                feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES
            ),
            Self::DuplicateFingerprintJob(fingerprint) => write!(
                formatter,
                "another non-terminal resolution job already exists for conflict fingerprint \
                 {fingerprint}; duplicate preparation refused"
            ),
            Self::ActiveJobLimitExceeded => write!(
                formatter,
                "resolution store holds {RESOLUTION_MAX_JOBS} non-terminal jobs; preparation refused"
            ),
            Self::NoOutstandingQuestion(job_id) => {
                write!(formatter, "resolution job {job_id} has no outstanding human question")
            }
            Self::StaleQuestionGeneration {
                job_id,
                expected,
                received,
            } => write!(
                formatter,
                "answer for {job_id} references stale question generation {received} \
                 (current {expected}); the escalation was superseded"
            ),
            Self::AnswerBindingMismatch { job_id, detail } => {
                write!(formatter, "answer for {job_id} does not bind the stored assignment: {detail}")
            }
            Self::NotSubmittedWithoutCandidate(job_id) => write!(
                formatter,
                "resolution job {job_id} is not in a submitted-without-candidate state; \
                 put_candidate refused"
            ),
        }
    }
}

impl std::error::Error for ResolutionOpError {}

/// Assignment lifecycle of one resolution job (durable and explicit).
///
/// `Active` is the only working state (it may carry a submitted result);
/// `PublicationUncertain` marks an apply whose CAS may have won; every other
/// state is terminal and never transitions again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionAssignmentState {
    Active,
    /// A guarded apply is in flight: the durable journal exists and the CAS
    /// may or may not have won. Recovered on the next store load; never
    /// evicted by trim.
    PublicationUncertain,
    Revoked,
    Superseded,
    Completed,
    /// Recovery failed closed: the CAS did not win (or the journal is
    /// missing/corrupt), so nothing was published and the conflict survives.
    Stale,
    /// A human answered `Defer` (or `defer_resolution` was called); the
    /// conflict is preserved for later manual action.
    Deferred,
    /// A human chose `KeepUnresolved`; the conflict stays unresolved.
    KeepUnresolved,
}

impl ResolutionAssignmentState {
    /// Whether the assignment is terminal: it never transitions again and
    /// trim may evict it.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Revoked
                | Self::Superseded
                | Self::Completed
                | Self::Stale
                | Self::Deferred
                | Self::KeepUnresolved
        )
    }
}

/// Enforced assignment-state transition map. Every state mutation in the
/// engine routes through here so a buggy or corrupt caller can never reach a
/// state that contradicts the durable evidence:
///
/// ```text
/// Active               → Active (result recorded / answer routed)
///                      → PublicationUncertain (guarded apply in flight)
///                      → Revoked | Superseded (assignment cancelled)
///                      → Deferred | KeepUnresolved (human answer / defer)
/// PublicationUncertain → Completed (journal + bookkeeping prove the CAS won)
///                      → Stale (recovery fail-closed: the CAS did not win)
///                      → Active (in-process CAS failure revert, journal
///                                already deleted)
/// Revoked | Superseded | Completed | Deferred | KeepUnresolved | Stale
///                      → terminal: no outgoing transitions
/// ```
///
/// `Completed` is only ever reached through `PublicationUncertain`: the
/// guarded apply marks the record uncertain before its single CAS and only
/// completes after the durable journal + bookkeeping confirm the CAS won
/// ("submitted-with-applied" therefore always passes through the uncertain
/// gate).
fn assert_state_transition(
    from: ResolutionAssignmentState,
    to: ResolutionAssignmentState,
) -> Result<()> {
    use ResolutionAssignmentState::*;
    let allowed = matches!(
        (from, to),
        (Active, Active)
            | (Active, PublicationUncertain)
            | (Active, Revoked)
            | (Active, Superseded)
            | (Active, Deferred)
            | (Active, KeepUnresolved)
            | (PublicationUncertain, Completed)
            | (PublicationUncertain, Stale)
            | (PublicationUncertain, Active)
    );
    ensure!(
        allowed,
        "invalid resolution assignment state transition {from:?} → {to:?}"
    );
    Ok(())
}

/// One persisted resolution job record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedResolutionJob {
    pub schema_version: u32,
    pub job: ResolutionJob,
    pub assignment_state: ResolutionAssignmentState,
    pub created_at_ms: i64,
    /// Verification evidence time recorded at submit (freshness bound).
    pub verified_at_ms: Option<i64>,
    /// At most one submitted result (replay is rejected).
    pub result: Option<ResolutionResult>,
    /// Monotonic per-fingerprint question generation of the escalation this
    /// job carries (0 when no question was ever recorded). Every human
    /// answer must reference the exact generation.
    #[serde(default)]
    pub question_generation: u32,
}

/// Durable resolution state file (schema-versioned, advisory lock, atomic
/// replacement, bounded).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolutionStateFile {
    pub schema_version: u32,
    pub jobs: Vec<PersistedResolutionJob>,
}

impl ResolutionStateFile {
    fn fresh() -> Self {
        Self {
            schema_version: RESOLUTION_STORE_SCHEMA_VERSION,
            jobs: Vec::new(),
        }
    }

    /// Evicts only TERMINAL records (oldest first) so the store stays
    /// bounded without ever dropping an in-flight, uncertain, or working
    /// assignment. `Active` and `PublicationUncertain` records are never
    /// evicted; the non-terminal count is bounded by prepare instead.
    fn trim(&mut self) {
        if self.jobs.len() <= RESOLUTION_MAX_JOBS {
            return;
        }
        let mut terminal: Vec<PersistedResolutionJob> = self
            .jobs
            .iter()
            .filter(|record| record.assignment_state.is_terminal())
            .cloned()
            .collect();
        terminal.sort_by_key(|record| record.created_at_ms);
        terminal.reverse();
        let non_terminal = self.jobs.len() - terminal.len();
        let keep = RESOLUTION_MAX_JOBS.saturating_sub(non_terminal);
        terminal.truncate(keep);
        let mut kept: Vec<PersistedResolutionJob> = self
            .jobs
            .iter()
            .filter(|record| !record.assignment_state.is_terminal())
            .cloned()
            .collect();
        kept.extend(terminal);
        self.jobs = kept;
    }
}

/// Crash-safe resolution job store.
pub struct ResolutionStore {
    inner: DurableJson<ResolutionStateFile>,
}

impl ResolutionStore {
    /// Opens (creating when absent) the orchestrator resolution store for a
    /// workspace. Corrupt or unsupported-schema state fails closed.
    pub fn open(base: &Path) -> Result<Self> {
        let dir = crate::workspace_layout::ensure_workspace_state(base)?.join("orchestrator");
        let inner = DurableJson::open(&dir, RESOLUTION_STORE_FILE, ResolutionStateFile::fresh())?;
        inner.with_read(|state| {
            ensure!(
                state.schema_version == RESOLUTION_STORE_SCHEMA_VERSION,
                "unsupported resolution store schema {} (expected {RESOLUTION_STORE_SCHEMA_VERSION}); \
                 do not infer resolution state from artifacts alone",
                state.schema_version
            );
            Ok(())
        })?;
        Ok(Self { inner })
    }

    pub fn load(&self) -> Result<ResolutionStateFile> {
        self.inner.with_read(|state| {
            ensure!(
                state.schema_version == RESOLUTION_STORE_SCHEMA_VERSION,
                "unsupported resolution store schema {}",
                state.schema_version
            );
            // Fails closed on any corrupt or mixed-version record: a job or
            // stored result that no longer validates is never trusted.
            for record in &state.jobs {
                validate_resolution_job(&record.job).with_context(|| {
                    format!("corrupt resolution job record {}", record.job.job_id)
                })?;
                if let Some(result) = &record.result {
                    validate_resolution_result(result).with_context(|| {
                        format!("corrupt resolution result record {}", record.job.job_id)
                    })?;
                }
            }
            Ok(state.clone())
        })
    }

    pub fn update(
        &self,
        f: impl FnOnce(&mut ResolutionStateFile) -> Result<()>,
    ) -> Result<ResolutionStateFile> {
        self.inner.with_write(|state| {
            ensure!(
                state.schema_version == RESOLUTION_STORE_SCHEMA_VERSION,
                "unsupported resolution store schema {}",
                state.schema_version
            );
            f(state)?;
            state.schema_version = RESOLUTION_STORE_SCHEMA_VERSION;
            state.trim();
            Ok(())
        })?;
        self.load()
    }

    fn load_job(&self, job_id: &str) -> Result<PersistedResolutionJob> {
        self.load()?
            .jobs
            .into_iter()
            .find(|record| record.job.job_id == job_id)
            .with_context(|| format!("unknown resolution job {job_id}"))
    }

    fn find_mut<'a>(
        state: &'a mut ResolutionStateFile,
        job_id: &str,
    ) -> Result<&'a mut PersistedResolutionJob> {
        state
            .jobs
            .iter_mut()
            .find(|record| record.job.job_id == job_id)
            .with_context(|| format!("unknown resolution job {job_id}"))
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        self.inner.state_path.as_path()
    }
}

/// Canonical portable relative path (under the protected state root) of the
/// immutable candidate file for one job/attempt.
#[must_use]
pub fn candidate_path_for(job_id: &str, attempt: u32) -> String {
    format!("orchestrator/resolution/jobs/{job_id}/candidate-{attempt}.bin")
}

/// Directory holding one job's immutable files (job.json + candidate).
pub fn resolution_jobs_dir(base: &Path) -> Result<PathBuf> {
    Ok(crate::workspace_layout::ensure_workspace_state(base)?.join("orchestrator/resolution/jobs"))
}

fn job_dir(base: &Path, job_id: &str) -> Result<PathBuf> {
    Ok(resolution_jobs_dir(base)?.join(job_id))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Outcome of one guarded apply pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ResolutionApplyOutcome {
    /// Published; the returned id is the new workspace head.
    Published { head: String },
    /// Typed stale/invalid outcome; the current conflict survives unchanged.
    Stale {
        kind: ResolutionStaleKind,
        diagnostics: Vec<String>,
    },
}

/// Prepares one automatic resolution job for the exact current conflict at
/// `path`.
///
/// Requires a real current conflict in the workspace head and a typed
/// prevention-exhausted/violated reason; refuses anything else. Runs under
/// the existing sync ownership.
///
/// # Errors
/// Returns an error for no current conflict, missing prevention reason,
/// designation refusal, or invalid state.
pub async fn prepare_resolution_job(
    ctx: &SyncCtx<'_>,
    path: &str,
    prevention: PreventionReason,
) -> Result<ResolutionJob> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    prepare_resolution_job_guarded(ctx, path, prevention).await
}

/// Guarded preparation for callers that already hold sync/runner ownership.
pub(crate) async fn prepare_resolution_job_guarded(
    ctx: &SyncCtx<'_>,
    path: &str,
    prevention: PreventionReason,
) -> Result<ResolutionJob> {
    ensure!(
        ctx.format_version() >= 3,
        "{}",
        crate::agent::continuous::unsupported_schema_failure(
            "automatic resolution requires format v3; run `feanorfs migrate` first"
        )
    );
    ensure!(
        !prevention.detail().trim().is_empty(),
        "automatic job preparation requires a typed prevention-exhausted/violated reason"
    );
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        bail!("workspace head disappeared during automatic job preparation");
    };
    let engine = SnapshotEngine::new(ctx);
    let snapshot = engine.load_snapshot(&head).await?;
    let state = engine.objects.get_tree_state(&snapshot.root).await?;
    let conflict = state
        .conflicts
        .iter()
        .find(|candidate| candidate.path == path)
        .with_context(|| {
            format!(
                "no current conflict at '{path}' in the workspace head; \
                 automatic job preparation refused"
            )
        })?;
    ensure!(
        ctx.db.is_conflict_fingerprinted(path).await?,
        "conflict at '{path}' is a legacy unfingerprinted record (no valid identity \
         sidecar beside its artifacts); it can be resolved manually but can never \
         enter automatic prepare/apply"
    );

    let designation = designate_conflict_owner(ctx, path)
        .await
        .map_err(|refusal| match refusal.kind {
            DesignationRefusalKind::ProjectionIncomplete => {
                crate::agent::continuous::unsupported_schema_failure(refusal.detail)
            }
            DesignationRefusalKind::NoRoster
            | DesignationRefusalKind::NoCapableRoster
            | DesignationRefusalKind::ActiveAssignmentExists => anyhow::Error::new(refusal),
        })?;
    let task_id = designation
        .task_id
        .clone()
        .context("owner designation must bind a canonical task before job preparation")?;

    let binding = IdentityBinding {
        task_id: Some(&task_id),
        intent_message_ids: &designation.intent_message_ids,
        assignment_id: Some(&designation.assignment_id),
        attempt: Some(designation.attempt),
        designated_owner: Some(&designation.owner),
        verification_policy: Some(RESOLUTION_VERIFICATION_POLICY_ID),
    };
    let identity = conflict_identity_from_edit(
        ctx.workspace_id(),
        &head,
        &head,
        &snapshot.root,
        conflict,
        feanorfs_common::ConflictKind::EditEdit,
        &binding,
    );
    let fingerprint = compute_conflict_identity_fingerprint(&identity);
    validate_conflict_identity(&identity)?;

    // Causal references: the accepted intent ids bound by the designation
    // plus every applied-ancestry message id touching the path (source ids,
    // causal bases, superseded decisions of accepted path-covering
    // proposals). Sorted, unique, bounded — never empty while accepted
    // coverage exists.
    let ancestry_ids = path_ancestry_message_ids(ctx, path)?;
    let accepted_intents = if designation.intent_message_ids.is_empty() {
        // Fallback designations bind no intents; derive the accepted
        // path-covering intents so the job stays fully provable.
        ancestry_ids.clone()
    } else {
        designation.intent_message_ids.clone()
    };
    let designation_evidence = OwnerDesignationEvidence {
        method: match designation.method {
            crate::integrator::OwnerDesignationMethod::CausalEligible => {
                OwnerDesignationMethod::CausalEligible
            }
            crate::integrator::OwnerDesignationMethod::IntegratorFallback => {
                OwnerDesignationMethod::IntegratorFallback
            }
        },
        nonce: designation.evidence.nonce.clone(),
        roster_fingerprint: designation.evidence.roster_fingerprint.clone(),
        eligible: designation.evidence.eligible.clone(),
        ranked: designation.evidence.ranked.clone(),
        reasoning: designation.evidence.reasoning.clone(),
        attempt: designation.attempt,
    };
    validate_designation_evidence(&designation_evidence)?;

    let job_id = feanorfs_common::generate_assignment_id()?;
    let candidate_destination = candidate_path_for(&job_id, designation.attempt);
    let artifacts = artifact_descriptors(ctx, path, &conflict_dir_for(ctx, path).await?);
    let last_resort_reason = prevention.detail().to_string();
    let job = ResolutionJob {
        schema_version: RESOLUTION_SCHEMA_VERSION,
        job_id,
        task_id,
        assignment_id: designation.assignment_id.clone(),
        attempt: designation.attempt,
        workspace_id: ctx.workspace_id().to_string(),
        owner: designation.owner.clone(),
        conflict: identity.clone(),
        conflict_fingerprint: fingerprint.clone(),
        current_snapshot: head.clone(),
        about_snapshot: head.clone(),
        tree_root: snapshot.root.clone(),
        accepted_intents,
        causal_refs: ancestry_ids,
        artifacts,
        candidate_destination: feanorfs_common::CandidateDestination {
            path: candidate_destination,
            create_new: true,
        },
        allowed_output_paths: vec![path.to_string()],
        verification: feanorfs_common::VerificationPolicyRef {
            policy_id: RESOLUTION_VERIFICATION_POLICY_ID.to_string(),
            command_config_ref: feanorfs_common::RESOLUTION_VERIFICATION_CONFIG_REF.to_string(),
            timeout_ms: feanorfs_common::RESOLUTION_DEFAULT_VERIFICATION_TIMEOUT_MS,
            freshness_required: true,
        },
        prevention,
        last_resort_reason,
        designation: designation_evidence,
    };
    validate_resolution_job(&job)?;

    // The immutable job.json must be durable BEFORE the store record so the
    // projection can never show a job without its durable immutable file.
    let dir = job_dir(ctx.base, &job.job_id)?;
    tokio::fs::create_dir_all(&dir).await?;
    let payload = serde_json::to_vec(&job).context("serialize resolution job")?;
    crate::fs_util::atomic_write_durable(&dir, RESOLUTION_JOB_FILE, &payload).await?;

    let record = PersistedResolutionJob {
        schema_version: RESOLUTION_STORE_SCHEMA_VERSION,
        job: job.clone(),
        assignment_state: ResolutionAssignmentState::Active,
        created_at_ms: now_ms(),
        verified_at_ms: None,
        result: None,
        question_generation: 0,
    };
    let store = ResolutionStore::open(ctx.base)?;
    store.update(|state| {
        ensure!(
            !state
                .jobs
                .iter()
                .any(|existing| existing.job.job_id == job.job_id),
            "resolution job {} already exists",
            job.job_id
        );
        // One active job per fingerprint: another NON-TERMINAL job for the
        // same conflict fingerprint (Active with or without a submitted
        // result, including requires_human escalations, or an in-flight
        // PublicationUncertain apply) refuses a second preparation.
        if state.jobs.iter().any(|existing| {
            existing.job.conflict_fingerprint == job.conflict_fingerprint
                && !existing.assignment_state.is_terminal()
        }) {
            return Err(anyhow::Error::new(
                ResolutionOpError::DuplicateFingerprintJob(job.conflict_fingerprint.clone()),
            ));
        }
        let non_terminal = state
            .jobs
            .iter()
            .filter(|existing| !existing.assignment_state.is_terminal())
            .count();
        if non_terminal >= RESOLUTION_MAX_JOBS {
            return Err(anyhow::Error::new(
                ResolutionOpError::ActiveJobLimitExceeded,
            ));
        }
        state.jobs.push(record);
        Ok(())
    })?;
    Ok(job)
}

/// Sorted unique message ids of the accepted ancestry touching `path`: every
/// accepted proposal whose scope covers the path contributes its intent
/// message id, its source message id, its causal base, and its superseded
/// decision ids. Bounded by [`feanorfs_common::RESOLUTION_MAX_CAUSAL_REFS`].
fn path_ancestry_message_ids(ctx: &SyncCtx<'_>, path: &str) -> Result<Vec<String>> {
    let state = WorkStore::open(ctx.base)?.load()?;
    let mut ids: Vec<String> = Vec::new();
    for task in &state.tasks {
        for proposal in &task.proposals {
            if proposal.state != WorkTaskState::Accepted {
                continue;
            }
            if !feanorfs_common::work_contract::scope_covers_path(&proposal.scope, path) {
                continue;
            }
            ids.push(proposal.intent_message_id.clone());
            if let Some(base) = &proposal.causal_base {
                ids.push(base.clone());
            }
            if proposal.source_message_id != proposal.intent_message_id {
                ids.push(proposal.source_message_id.clone());
            }
            ids.extend(proposal.superseded_decisions.iter().cloned());
        }
    }
    ids.sort();
    ids.dedup();
    ensure!(
        ids.len() <= feanorfs_common::RESOLUTION_MAX_CAUSAL_REFS,
        "causal reference set for '{path}' exceeds the {} bound",
        feanorfs_common::RESOLUTION_MAX_CAUSAL_REFS
    );
    Ok(ids)
}

async fn conflict_dir_for(ctx: &SyncCtx<'_>, path: &str) -> Result<PathBuf> {
    Ok(ctx
        .db
        .get_conflict_record(path)
        .await?
        .map(|record| PathBuf::from(record.conflict_dir))
        .unwrap_or_default())
}

fn artifact_descriptors(
    ctx: &SyncCtx<'_>,
    path: &str,
    conflict_dir: &Path,
) -> Vec<feanorfs_common::ArtifactDescriptor> {
    let state_root = ctx.state_dir().ok();
    let role_path = |suffix: &str| {
        let absolute = conflict_dir.join(format!("{path}{suffix}"));
        match &state_root {
            Some(root) => absolute
                .strip_prefix(root)
                .map(|relative| relative.to_string_lossy().into_owned())
                .unwrap_or_default(),
            None => String::new(),
        }
    };
    vec![
        feanorfs_common::ArtifactDescriptor {
            role: feanorfs_common::ArtifactRoleName::Original,
            path: role_path(".original"),
        },
        feanorfs_common::ArtifactDescriptor {
            role: feanorfs_common::ArtifactRoleName::Local,
            path: role_path(".local"),
        },
        feanorfs_common::ArtifactDescriptor {
            role: feanorfs_common::ArtifactRoleName::Cloud,
            path: role_path(".cloud"),
        },
    ]
}

/// Submits one closed resolution result for `job_id`.
///
/// Validates result schema/bounds, assignment/attempt/owner/fingerprint, and
/// the candidate descriptor against the immutable candidate file
/// (descriptor-open, symlink-reject). For `candidate_ready` the engine then
/// EXECUTES the fixed inline verification policy itself (bytes match, output
/// path allowed, size bound, descriptor consistency) and replaces the
/// result's verification summary with the recorded evidence; a candidate
/// that fails is rejected with a typed error. For `requires_human` the
/// engine assigns the monotonic per-fingerprint question generation.
/// Submission never modifies the worktree, conflict registry, artifacts, or
/// head — only the private resolution store records the result.
///
/// # Errors
/// Returns an error for replay, unknown jobs, revoked assignments, or
/// candidate mismatches.
pub async fn submit_resolution_result(
    ctx: &SyncCtx<'_>,
    job_id: &str,
    result: ResolutionResult,
) -> Result<ResolutionResult> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    validate_resolution_result(&result)?;
    ensure!(
        result.job_id == job_id,
        "result job id {} does not match the submission target {job_id}",
        result.job_id
    );
    let store = ResolutionStore::open(ctx.base)?;
    let job = store.load_job(job_id)?;
    ensure!(
        job.assignment_state == ResolutionAssignmentState::Active,
        "resolution assignment {job_id} is not active; submission rejected"
    );
    ensure!(
        job.result.is_none(),
        "resolution job {job_id} already has a submitted result; replay rejected"
    );
    ensure!(
        result.assignment_id == job.job.assignment_id
            && result.attempt == job.job.attempt
            && result.owner == job.job.owner
            && result.conflict_fingerprint == job.job.conflict_fingerprint,
        "result does not match the job's assignment/attempt/owner/fingerprint"
    );
    let mut result = result;
    match result.outcome {
        ResolutionOutcome::CandidateReady => {
            let candidate = result
                .candidate
                .as_ref()
                .context("candidate_ready result requires a candidate descriptor")?;
            ensure!(
                candidate.path == job.job.candidate_destination.path,
                "candidate path {} must equal the job's engine-owned destination {}",
                candidate.path,
                job.job.candidate_destination.path
            );
            // The engine executes the fixed inline policy itself: read the
            // immutable file (descriptor-open, symlink-reject) and record
            // real evidence. A failing candidate is rejected typed.
            let (_bytes, summary) = verify_candidate_for_submit(ctx, &job, candidate).await?;
            result.verification = summary;
        }
        ResolutionOutcome::RequiresHuman => {
            // Store gains question state: the engine assigns the monotonic
            // per-fingerprint generation and records it with the result.
            let generation =
                next_question_generation(&store.load()?, &job.job.conflict_fingerprint);
            result.question_generation = generation;
        }
        ResolutionOutcome::NoChangeRequired
        | ResolutionOutcome::Blocked
        | ResolutionOutcome::Failed
        | ResolutionOutcome::Stale => {
            result.verification = inline_verify_no_candidate(&job.job, &result);
        }
    }

    let mut record = job;
    record.verified_at_ms = Some(now_ms());
    record.result = Some(result.clone());
    if result.outcome == ResolutionOutcome::RequiresHuman {
        record.question_generation = result.question_generation;
    }
    let store = ResolutionStore::open(ctx.base)?;
    store.update(|state| {
        let existing = ResolutionStore::find_mut(state, job_id)?;
        assert_state_transition(existing.assignment_state, ResolutionAssignmentState::Active)?;
        *existing = record;
        Ok(())
    })?;
    Ok(result)
}

/// Revokes (or marks superseded) one resolution assignment. The conflict,
/// its artifacts, and any candidate stay untouched; apply afterwards returns
/// a typed stale outcome.
///
/// # Errors
/// Returns an error for unknown jobs or terminal assignments.
pub async fn revoke_resolution_assignment(
    ctx: &SyncCtx<'_>,
    job_id: &str,
    superseded: bool,
) -> Result<()> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    let store = ResolutionStore::open(ctx.base)?;
    store.update(|state| {
        let record = ResolutionStore::find_mut(state, job_id)?;
        let next = if superseded {
            ResolutionAssignmentState::Superseded
        } else {
            ResolutionAssignmentState::Revoked
        };
        assert_state_transition(record.assignment_state, next)?;
        record.assignment_state = next;
        Ok(())
    })?;
    Ok(())
}

/// Next monotonic question generation for one conflict fingerprint: the
/// maximum generation already recorded for the fingerprint plus one. The
/// counter only resets when every record for the fingerprint is evicted by
/// trim (bounded store).
fn next_question_generation(state: &ResolutionStateFile, fingerprint: &str) -> u32 {
    state
        .jobs
        .iter()
        .filter(|record| record.job.conflict_fingerprint == fingerprint)
        .map(|record| record.question_generation)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// Engine execution of the fixed inline verification policy for a
/// `candidate_ready` result: reads the immutable candidate file (typed open
/// plus rehash) and records real evidence, returning the verified bytes and
/// the evidence summary. A typed stale error rejects any failed check.
async fn verify_candidate_for_submit(
    ctx: &SyncCtx<'_>,
    job: &PersistedResolutionJob,
    candidate: &CandidateDescriptor,
) -> Result<(Vec<u8>, VerificationSummary)> {
    let bytes = crate::snapshot::read_candidate_file(ctx, &candidate.path, candidate).await?;
    let summary = inline_verify_candidate(&job.job, &bytes, candidate)?;
    Ok((bytes, summary))
}

/// The honest verifiable check set the engine records for a candidate:
/// descriptor bytes match, the output path is allowed, the size is bounded,
/// and the descriptor is consistent with the engine-owned destination.
fn inline_verify_candidate(
    job: &ResolutionJob,
    bytes: &[u8],
    candidate: &CandidateDescriptor,
) -> Result<VerificationSummary> {
    let observed_hash = feanorfs_common::hash_bytes(bytes);
    let path_allowed = job
        .allowed_output_paths
        .iter()
        .any(|allowed| allowed == &job.conflict.path);
    let checks = if candidate.deleted {
        // A deletion candidate carries no bytes: the marker file must be
        // empty or absent, and the descriptor (already validated) must be
        // consistent with the engine-owned destination.
        vec![
            VerificationCheck {
                name: "candidate_bytes_match_descriptor".to_string(),
                passed: bytes.is_empty() && candidate.hash.is_empty() && candidate.size == 0,
                detail: Some(format!(
                    "deleted candidate observed {} bytes; descriptor carries no hash/size",
                    bytes.len()
                )),
            },
            VerificationCheck {
                name: "candidate_within_allowed_output_paths".to_string(),
                passed: path_allowed,
                detail: Some(format!(
                    "output path '{}' within allowed outputs [{}]",
                    job.conflict.path,
                    job.allowed_output_paths.join(", ")
                )),
            },
            VerificationCheck {
                name: "candidate_size_bounded".to_string(),
                passed: bytes.is_empty(),
                detail: Some("deleted candidate must carry no bytes".to_string()),
            },
            VerificationCheck {
                name: "candidate_descriptor_consistent".to_string(),
                passed: candidate.path == job.candidate_destination.path,
                detail: Some(format!(
                    "descriptor path {} equals the engine destination {}",
                    candidate.path, job.candidate_destination.path
                )),
            },
        ]
    } else {
        let bytes_match = bytes.len() == candidate.size as usize && observed_hash == candidate.hash;
        let size_bounded = bytes.len() as u64 <= feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES;
        let descriptor_consistent = candidate.path == job.candidate_destination.path
            && (candidate.mode == 0 || candidate.mode == EXECUTABLE_MODE)
            && !candidate.deleted;
        vec![
            VerificationCheck {
                name: "candidate_bytes_match_descriptor".to_string(),
                passed: bytes_match,
                detail: Some(format!(
                    "observed {} bytes hashing {observed_hash}, descriptor {} bytes hashing {}",
                    bytes.len(),
                    candidate.size,
                    candidate.hash
                )),
            },
            VerificationCheck {
                name: "candidate_within_allowed_output_paths".to_string(),
                passed: path_allowed,
                detail: Some(format!(
                    "output path '{}' within allowed outputs [{}]",
                    job.conflict.path,
                    job.allowed_output_paths.join(", ")
                )),
            },
            VerificationCheck {
                name: "candidate_size_bounded".to_string(),
                passed: size_bounded,
                detail: Some(format!(
                    "{} bytes within the {} byte bound",
                    bytes.len(),
                    feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES
                )),
            },
            VerificationCheck {
                name: "candidate_descriptor_consistent".to_string(),
                passed: descriptor_consistent,
                detail: Some(format!(
                    "descriptor path {} equals the engine destination {}",
                    candidate.path, job.candidate_destination.path
                )),
            },
        ]
    };
    let passed = checks.iter().all(|check| check.passed);
    let summary = VerificationSummary {
        status: if passed {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        },
        summary: "engine inline verification policy executed at submit".to_string(),
        policy_id: Some(RESOLUTION_VERIFICATION_POLICY_ID.to_string()),
        policy_version: 1,
        tool_ref: None,
        input_hashes: vec![job.job_id.clone(), job.conflict_fingerprint.clone()],
        output_hash: Some(observed_hash),
        checks,
    };
    if !passed {
        let kind = if candidate.deleted {
            ResolutionStaleKind::CandidateSizeMismatch
        } else if !summary.checks[0].passed {
            ResolutionStaleKind::CandidateHashMismatch
        } else if !summary.checks[2].passed {
            ResolutionStaleKind::CandidateSizeMismatch
        } else {
            ResolutionStaleKind::CandidatePathMismatch
        };
        return Err(anyhow::Error::new(StalePublication {
            kind,
            detail: format!("candidate failed the engine inline verification policy: {summary:?}"),
        }));
    }
    Ok(summary)
}

/// Engine evidence for outcomes that carry no candidate bytes (the policy
/// asserts that no candidate was required by the outcome).
fn inline_verify_no_candidate(
    job: &ResolutionJob,
    result: &ResolutionResult,
) -> VerificationSummary {
    VerificationSummary {
        status: result.verification.status,
        summary: "engine inline verification policy executed at submit (no candidate bytes)"
            .to_string(),
        policy_id: Some(RESOLUTION_VERIFICATION_POLICY_ID.to_string()),
        policy_version: 1,
        tool_ref: None,
        input_hashes: vec![job.job_id.clone(), job.conflict_fingerprint.clone()],
        output_hash: None,
        checks: vec![VerificationCheck {
            name: "no_candidate_required".to_string(),
            passed: true,
            detail: Some(format!(
                "outcome {:?} carries no candidate bytes",
                result.outcome
            )),
        }],
    }
}

/// Applies one submitted resolution result with guarded publication.
///
/// Each attempt revalidates the assignment state, verification freshness,
/// the conflict registry record, every identity field, and the candidate
/// descriptor immediately before a single CAS; a lost CAS discards the plan
/// and restarts ALL validation. Before the CAS a durable `publication-pending`
/// journal is written and the record moves to `PublicationUncertain`, so a
/// crash mid-publication converges on the next store load (bookkeeping is
/// idempotent). Cleanup happens only after confirmed success.
///
/// # Errors
/// Returns a typed stale/invalid outcome via [`ResolutionApplyOutcome::Stale`]
/// when any identity/candidate field changed, and an error for unknown jobs
/// or missing results.
pub async fn apply_resolution_job(
    ctx: &SyncCtx<'_>,
    job_id: &str,
) -> Result<ResolutionApplyOutcome> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    let mut last_error: Option<anyhow::Error> = None;
    for _ in 0..RESOLUTION_MAX_APPLY_REVALIDATIONS {
        let store = ResolutionStore::open(ctx.base)?;
        let job = store.load_job(job_id)?;
        let result = job
            .result
            .as_ref()
            .context("resolution job has no submitted result; apply before submit is refused")?;
        ensure!(
            matches!(
                result.outcome,
                ResolutionOutcome::CandidateReady | ResolutionOutcome::NoChangeRequired
            ),
            "apply requires a candidate_ready or no_change_required result; got {:?}",
            result.outcome
        );
        match publish_once(ctx, job_id).await {
            Ok(head) => {
                complete_publication(ctx, job_id).await?;
                return Ok(ResolutionApplyOutcome::Published { head });
            }
            Err(error) => {
                if let Some(stale) = error.downcast_ref::<StalePublication>() {
                    return Ok(ResolutionApplyOutcome::Stale {
                        kind: stale.kind,
                        diagnostics: vec![stale.detail.clone()],
                    });
                }
                if error.downcast_ref::<crate::snapshot::LostCas>().is_some() {
                    // Discard the plan and restart ALL validation.
                    last_error = Some(error);
                    continue;
                }
                return Err(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        crate::agent::continuous::retryable_volatility_failure(
            "workspace head changed too many times during guarded publication",
        )
    }))
}

/// One guarded CAS attempt for `job_id`.
///
/// Re-loads the record so EVERY attempt re-checks the assignment state
/// (must still be active for this attempt), verification freshness, and the
/// conflict registry record (still pending with a matching fingerprinted
/// sidecar) before building the publication. Then: plan → durable journal →
/// record `PublicationUncertain` (durable) → single CAS. On any in-process
/// failure nothing was published: the journal is deleted and the record is
/// reverted to `Active` so the retry loop (or a later apply) starts clean.
/// A crash after the journal/state but before the CAS is recovered on the
/// next store load (fails closed to `Stale` when the CAS did not win).
async fn publish_once(ctx: &SyncCtx<'_>, job_id: &str) -> Result<String> {
    let store = ResolutionStore::open(ctx.base)?;
    let job = store.load_job(job_id)?;
    let result = job
        .result
        .as_ref()
        .context("resolution job has no submitted result; apply before submit is refused")?;

    // Per-attempt assignment state re-check.
    if job.assignment_state != ResolutionAssignmentState::Active {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::AssignmentRevoked,
            detail: format!(
                "resolution assignment {job_id} is {:?}; apply refused",
                job.assignment_state
            ),
        }));
    }

    // Per-attempt verification freshness re-check (not once before the loop).
    if job.job.verification.freshness_required {
        let verified_at = job
            .verified_at_ms
            .context("fresh verification evidence is required but was never recorded")?;
        let elapsed = now_ms().saturating_sub(verified_at);
        if u64::try_from(elapsed).unwrap_or(u64::MAX) > job.job.verification.timeout_ms {
            return Err(anyhow::Error::new(StalePublication {
                kind: ResolutionStaleKind::VerificationExpired,
                detail: format!(
                    "verification evidence expired after {} ms (bound {})",
                    elapsed, job.job.verification.timeout_ms
                ),
            }));
        }
    }

    // Per-attempt registry re-verification: the record must still be pending
    // AND still fingerprinted — its fingerprint-keyed identity sidecar must
    // still exist beside the artifacts and still match the record (a
    // downgraded or mismatched sidecar means the automatic path is gone).
    let conflict_path = job.job.conflict.path.clone();
    if ctx.db.get_conflict_record(&conflict_path).await?.is_none() {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::ConflictMissing,
            detail: format!(
                "conflict registry record for '{}' is no longer pending",
                conflict_path
            ),
        }));
    }
    if !ctx.db.is_conflict_fingerprinted(&conflict_path).await? {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::IdentityMismatch,
            detail: format!(
                "fingerprinted identity sidecar beside '{conflict_path}' no longer matches \
                 the registry record"
            ),
        }));
    }

    let head = ctx.api.get_head(ctx.workspace_id()).await?;
    let Some(head) = head else {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::HeadChanged,
            detail: "workspace head disappeared during guarded publication".to_string(),
        }));
    };
    if head != job.job.current_snapshot {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::HeadChanged,
            detail: format!(
                "workspace head changed since preparation (expected {}, found {head})",
                job.job.current_snapshot
            ),
        }));
    }
    let engine = SnapshotEngine::new(ctx);
    let snapshot = engine.load_snapshot(&head).await?;
    let state = engine.objects.get_tree_state(&snapshot.root).await?;
    let Some(conflict) = state
        .conflicts
        .iter()
        .find(|candidate| candidate.path == conflict_path)
    else {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::ConflictMissing,
            detail: format!("conflict at '{conflict_path}' no longer exists in the current head"),
        }));
    };
    let binding = IdentityBinding {
        task_id: job.job.conflict.task_id.as_deref(),
        intent_message_ids: &job.job.conflict.intent_message_ids,
        assignment_id: job.job.conflict.assignment_id.as_deref(),
        attempt: job.job.conflict.attempt,
        designated_owner: job.job.conflict.designated_owner.as_deref(),
        verification_policy: job.job.conflict.verification_policy.as_deref(),
    };
    let recomputed = conflict_identity_from_edit(
        ctx.workspace_id(),
        &head,
        &head,
        &snapshot.root,
        conflict,
        feanorfs_common::ConflictKind::EditEdit,
        &binding,
    );
    if recomputed != job.job.conflict {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::IdentityMismatch,
            detail: format!("recomputed identity for '{conflict_path}' no longer matches the job"),
        }));
    }
    if compute_conflict_identity_fingerprint(&recomputed) != job.job.conflict_fingerprint {
        return Err(anyhow::Error::new(StalePublication {
            kind: ResolutionStaleKind::IdentityMismatch,
            detail: format!(
                "recomputed fingerprint for '{conflict_path}' no longer matches the job"
            ),
        }));
    }

    let plan = ResolutionPublication {
        identity: job.job.conflict.clone(),
        fingerprint: job.job.conflict_fingerprint.clone(),
        candidate: result.candidate.clone(),
        candidate_file: (result.outcome == ResolutionOutcome::CandidateReady)
            .then(|| job.job.candidate_destination.path.clone()),
        manual_state: None,
        additional: Vec::new(),
        expected_head: head,
        author: job.job.owner.clone(),
    };
    let planned = engine.plan_resolution_publication(plan).await?;

    // Durable journal + uncertain state BEFORE the single CAS.
    let dir = job_dir(ctx.base, job_id)?;
    let journal = PublicationPendingJournal {
        job_id: job_id.to_string(),
        expected_head: planned.expected_head.clone(),
        planned_head: planned.candidate_id.clone(),
        result: result.clone(),
    };
    write_publication_journal(&dir, &journal).await?;
    store.update(|state| {
        let record = ResolutionStore::find_mut(state, job_id)?;
        assert_state_transition(
            record.assignment_state,
            ResolutionAssignmentState::PublicationUncertain,
        )?;
        record.assignment_state = ResolutionAssignmentState::PublicationUncertain;
        Ok(())
    })?;

    #[cfg(test)]
    {
        use crate::snapshot::{consume_publish_crash, TestPublishCrashPoint};
        if consume_publish_crash(TestPublishCrashPoint::BeforeCas) {
            return Err(anyhow::anyhow!(
                "simulated crash before the guarded CAS (state: publication_uncertain)"
            ));
        }
    }

    match engine.commit_resolution_publication(&planned).await {
        Ok(head) => Ok(head),
        Err(error) => {
            // In-process failure: the CAS did not win (or never ran), so
            // nothing was published. Drop the journal and revert to Active
            // so a retry (or a later apply) starts from a clean pre-CAS
            // state. A crash before this revert is recovered on load.
            let _ = tokio::fs::remove_file(dir.join(PUBLICATION_PENDING_JOURNAL_FILE)).await;
            if let Err(revert_error) = store.update(|state| {
                let record = ResolutionStore::find_mut(state, job_id)?;
                assert_state_transition(
                    record.assignment_state,
                    ResolutionAssignmentState::Active,
                )?;
                record.assignment_state = ResolutionAssignmentState::Active;
                Ok(())
            }) {
                tracing::warn!(
                    "failed to revert publication-uncertain state for {job_id}: {revert_error}"
                );
            }
            Err(error)
        }
    }
}

/// Durable bounded `publication-pending` journal written before the CAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationPendingJournal {
    job_id: String,
    /// Head the publication was planned against (pre-CAS).
    expected_head: String,
    /// Planned new head: the snapshot id the CAS was about to produce. On
    /// recovery, `head == planned_head` proves the CAS won.
    planned_head: String,
    result: ResolutionResult,
}

async fn write_publication_journal(dir: &Path, journal: &PublicationPendingJournal) -> Result<()> {
    let payload = serde_json::to_vec(journal).context("serialize publication-pending journal")?;
    ensure!(
        payload.len() <= RESOLUTION_MAX_JOURNAL_BYTES,
        "publication-pending journal exceeds its {} byte bound",
        RESOLUTION_MAX_JOURNAL_BYTES
    );
    crate::fs_util::atomic_write_durable(dir, PUBLICATION_PENDING_JOURNAL_FILE, &payload).await
}

async fn read_publication_journal(dir: &Path) -> Result<Option<PublicationPendingJournal>> {
    let path = dir.join(PUBLICATION_PENDING_JOURNAL_FILE);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read publication-pending journal {}", path.display()))
        }
    };
    ensure!(
        bytes.len() <= RESOLUTION_MAX_JOURNAL_BYTES,
        "publication-pending journal exceeds its {} byte bound",
        RESOLUTION_MAX_JOURNAL_BYTES
    );
    let journal: PublicationPendingJournal =
        serde_json::from_slice(&bytes).context("parse publication-pending journal")?;
    Ok(Some(journal))
}

/// Post-CAS bookkeeping + terminal completion. Idempotent: the history
/// record is appended only when no identical record exists, the registry
/// resolve is a removal, artifact cleanup tolerates absence, and the
/// Completed transition is validated. Runs on the apply success path and on
/// crash recovery, so a publication is completed EXACTLY once.
async fn complete_publication(ctx: &SyncCtx<'_>, job_id: &str) -> Result<()> {
    let store = ResolutionStore::open(ctx.base)?;
    let job = store.load_job(job_id)?;
    if job.assignment_state != ResolutionAssignmentState::PublicationUncertain {
        ensure!(
            job.assignment_state == ResolutionAssignmentState::Completed,
            "resolution job {job_id} cannot complete from {:?}",
            job.assignment_state
        );
        return Ok(());
    }
    let result = job
        .result
        .as_ref()
        .context("resolution job has no submitted result to complete")?;

    #[cfg(test)]
    {
        use crate::snapshot::{consume_publish_crash, TestPublishCrashPoint};
        if consume_publish_crash(TestPublishCrashPoint::AfterCas) {
            return Err(anyhow::anyhow!(
                "simulated crash after the CAS, before bookkeeping"
            ));
        }
    }

    let path = job.job.conflict.path.clone();
    let candidate_hash = result
        .candidate
        .as_ref()
        .filter(|_| result.outcome == ResolutionOutcome::CandidateReady)
        .map(|candidate| candidate.hash.as_str());
    let captured_registry = ctx.db.get_conflict_record(&path).await?;

    record_conflict_resolution_once(ctx, &job, candidate_hash).await?;

    #[cfg(test)]
    {
        use crate::snapshot::{consume_publish_crash, TestPublishCrashPoint};
        if consume_publish_crash(TestPublishCrashPoint::AfterHistory) {
            return Err(anyhow::anyhow!(
                "simulated crash after the history record, before the registry resolve"
            ));
        }
    }

    ctx.db.resolve_conflict_path(&path).await?;

    #[cfg(test)]
    {
        use crate::snapshot::{consume_publish_crash, TestPublishCrashPoint};
        if consume_publish_crash(TestPublishCrashPoint::AfterRegistry) {
            return Err(anyhow::anyhow!(
                "simulated crash after the registry resolve, before Completed"
            ));
        }
    }

    cleanup_conflict_artifacts(ctx, &job, captured_registry.as_ref()).await;
    store.update(|state| {
        let record = ResolutionStore::find_mut(state, job_id)?;
        assert_state_transition(
            record.assignment_state,
            ResolutionAssignmentState::Completed,
        )?;
        record.assignment_state = ResolutionAssignmentState::Completed;
        Ok(())
    })?;

    #[cfg(test)]
    {
        use crate::snapshot::{consume_publish_crash, TestPublishCrashPoint};
        if consume_publish_crash(TestPublishCrashPoint::AfterCompleted) {
            return Err(anyhow::anyhow!(
                "simulated crash after Completed, before artifact/journal cleanup"
            ));
        }
    }

    remove_job_dir(ctx, &job).await;
    Ok(())
}

/// Appends the typed history record exactly once: skips when an identical
/// record (path, candidate method, resolver, source hash) already exists,
/// which is what makes recovery idempotent.
async fn record_conflict_resolution_once(
    ctx: &SyncCtx<'_>,
    job: &PersistedResolutionJob,
    candidate_hash: Option<&str>,
) -> Result<()> {
    let path = job.job.conflict.path.clone();
    let resolver = job.job.owner.clone();
    let already = ctx
        .db
        .list_conflict_resolutions()
        .await?
        .iter()
        .any(|record| {
            record.path == path
                && record.method == "candidate"
                && record.resolver == resolver
                && record.source_file_hash.as_deref() == candidate_hash
        });
    if !already {
        ctx.db
            .record_conflict_resolution(
                &path,
                ResolutionMethod::Candidate,
                candidate_hash,
                &resolver,
            )
            .await?;
    }
    Ok(())
}

/// Best-effort removal of the materialized conflict artifacts once the
/// registry record is gone. Failures never undo a confirmed publication.
async fn cleanup_conflict_artifacts(
    ctx: &SyncCtx<'_>,
    job: &PersistedResolutionJob,
    captured: Option<&feanorfs_common::ConflictRecord>,
) {
    let path = job.job.conflict.path.clone();
    let Some(record) = captured else {
        return;
    };
    let conflict_dir = PathBuf::from(&record.conflict_dir);
    for artifact in [
        conflict_dir.join(format!("{path}.original")),
        conflict_dir.join(format!("{path}.local")),
        conflict_dir.join(format!("{path}.cloud")),
    ] {
        if artifact.is_file() {
            if let Err(error) = tokio::fs::remove_file(&artifact).await {
                tracing::warn!(
                    "failed to clean conflict artifact {}: {error}",
                    artifact.display()
                );
            }
        }
    }
    if let Ok(count) = ctx.db.count_pending_in_dir(&record.conflict_dir).await {
        if count == 0 && conflict_dir.is_dir() {
            if let Err(error) = tokio::fs::remove_dir_all(&conflict_dir).await {
                tracing::warn!(
                    "failed to clean conflict directory {}: {error}",
                    conflict_dir.display()
                );
            }
        }
    }
}

/// Removes the immutable job directory (job.json, candidate, journal) after
/// the record is durably Completed. Failures are logged; a surviving
/// directory is swept by the next recovery.
async fn remove_job_dir(ctx: &SyncCtx<'_>, job: &PersistedResolutionJob) {
    let dir = job_dir(ctx.base, &job.job.job_id);
    if let Ok(dir) = dir {
        if dir.is_dir() {
            if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
                tracing::warn!(
                    "failed to clean resolution job directory {}: {error}",
                    dir.display()
                );
            }
        }
    }
}

/// Converges every `PublicationUncertain` record left by a crash and sweeps
/// orphaned journals/job directories. Call at the start of every engine
/// operation (the store-load hook): a job whose journal proves the CAS won
/// (current head == planned head) completes its bookkeeping and converges to
/// `Completed`; anything else fails closed to the terminal `Stale` state and
/// the conflict survives for manual action. Idempotent and bounded; returns
/// the number of uncertain records fully recovered.
pub async fn recover_uncertain_publications(ctx: &SyncCtx<'_>) -> Result<usize> {
    let store = ResolutionStore::open(ctx.base)?;
    let state = store.load()?;
    let uncertain: Vec<String> = state
        .jobs
        .iter()
        .filter(|record| record.assignment_state == ResolutionAssignmentState::PublicationUncertain)
        .map(|record| record.job.job_id.clone())
        .collect();
    let sweep: Vec<String> = state
        .jobs
        .iter()
        .filter(|record| record.assignment_state != ResolutionAssignmentState::PublicationUncertain)
        .map(|record| record.job.job_id.clone())
        .collect();
    if uncertain.is_empty() && sweep.is_empty() {
        return Ok(0);
    }
    let _lock = SyncLock::acquire(ctx.base)?;
    let mut recovered = 0;
    for job_id in uncertain {
        if recover_one_uncertain(ctx, &job_id).await? {
            recovered += 1;
        }
    }
    // Sweep: a journal surviving beside a terminal/working record is either
    // a crash between Completed and cleanup (remove the job dir) or a stale
    // pre-CAS journal (remove just the journal).
    for job_id in sweep {
        let dir = job_dir(ctx.base, &job_id)?;
        if !dir.join(PUBLICATION_PENDING_JOURNAL_FILE).is_file() {
            continue;
        }
        let job = store.load_job(&job_id)?;
        if job.assignment_state == ResolutionAssignmentState::Completed {
            remove_job_dir(ctx, &job).await;
        } else {
            let _ = tokio::fs::remove_file(dir.join(PUBLICATION_PENDING_JOURNAL_FILE)).await;
        }
    }
    Ok(recovered)
}

/// Recovers one `PublicationUncertain` record (called under the sync lock).
async fn recover_one_uncertain(ctx: &SyncCtx<'_>, job_id: &str) -> Result<bool> {
    let store = ResolutionStore::open(ctx.base)?;
    let job = store.load_job(job_id)?;
    if job.assignment_state != ResolutionAssignmentState::PublicationUncertain {
        return Ok(false);
    }
    let dir = job_dir(ctx.base, job_id)?;
    let head = ctx.api.get_head(ctx.workspace_id()).await?;
    // Unreadable/corrupt journal: the CAS proof is gone — fail closed.
    let journal = read_publication_journal(&dir).await.ok().flatten();
    let Some(head) = head else {
        // Head disappeared: the CAS outcome cannot be confirmed — fail closed.
        fail_closed_recovery(ctx, job_id).await?;
        return Ok(true);
    };
    let Some(journal) = journal else {
        fail_closed_recovery(ctx, job_id).await?;
        return Ok(true);
    };
    // A journal must bind the exact same job; anything else is corruption.
    if journal.job_id != job_id || journal.result.job_id != job_id {
        fail_closed_recovery(ctx, job_id).await?;
        return Ok(true);
    }
    if head == journal.planned_head {
        // The CAS won (the planned new head is the current head): re-run the
        // idempotent bookkeeping and converge to Completed.
        complete_publication(ctx, job_id).await?;
        Ok(true)
    } else {
        // The CAS did not win (crash before it, or it lost): nothing was
        // published — fail closed so the conflict survives for manual action.
        fail_closed_recovery(ctx, job_id).await?;
        Ok(true)
    }
}

/// Terminal fail-closed recovery for a `PublicationUncertain` record whose
/// CAS cannot be confirmed: mark `Stale`, drop the journal, preserve the
/// conflict.
async fn fail_closed_recovery(ctx: &SyncCtx<'_>, job_id: &str) -> Result<()> {
    let store = ResolutionStore::open(ctx.base)?;
    store.update(|state| {
        let record = ResolutionStore::find_mut(state, job_id)?;
        if record.assignment_state == ResolutionAssignmentState::PublicationUncertain {
            assert_state_transition(record.assignment_state, ResolutionAssignmentState::Stale)?;
            record.assignment_state = ResolutionAssignmentState::Stale;
        }
        Ok(())
    })?;
    let dir = job_dir(ctx.base, job_id)?;
    let _ = tokio::fs::remove_file(dir.join(PUBLICATION_PENDING_JOURNAL_FILE)).await;
    Ok(())
}

/// Writes the immutable engine-owned candidate file for one job from a
/// bounded byte stream (create-new, no-follow, fsync'd) and returns its
/// plaintext descriptor (path/hash/size/mode).
///
/// Allowed exactly while the job is `Active` and has either no submitted
/// result or a `requires_human` escalation (a submitted-without-candidate
/// state): the engine-owned candidate destination is created once and never
/// replaced. Rejects unknown/inactive jobs, an already-existing candidate,
/// a job with a candidate-bearing result, and oversized streams — all typed.
pub async fn put_resolution_candidate(
    ctx: &SyncCtx<'_>,
    job_id: &str,
    bytes: &[u8],
) -> Result<CandidateDescriptor> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    let store = ResolutionStore::open(ctx.base)?;
    let job = store.load_job(job_id)?;
    if job.assignment_state != ResolutionAssignmentState::Active {
        return Err(anyhow::Error::new(ResolutionOpError::JobNotActive {
            job_id: job_id.to_string(),
            state: job.assignment_state,
        }));
    }
    match &job.result {
        None => {}
        Some(result) if result.outcome == ResolutionOutcome::RequiresHuman => {}
        Some(_) => {
            return Err(anyhow::Error::new(
                ResolutionOpError::NotSubmittedWithoutCandidate(job_id.to_string()),
            ));
        }
    }
    if bytes.len() as u64 > feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES {
        return Err(anyhow::Error::new(ResolutionOpError::CandidateTooLarge {
            job_id: job_id.to_string(),
            size: bytes.len() as u64,
        }));
    }

    let state_root = ctx.state_dir()?;
    let relative = job.job.candidate_destination.path.clone();
    let absolute = state_root.join(&relative);
    if let Some(parent) = absolute.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file = open_create_new_no_follow(&absolute).map_err(|error| {
        if error.kind() == ErrorKind::AlreadyExists {
            anyhow::Error::new(ResolutionOpError::CandidateAlreadyExists(relative.clone()))
        } else {
            error.into()
        }
    })?;

    let metadata = {
        use tokio::io::AsyncWriteExt as _;
        let mut file = tokio::fs::File::from_std(file);
        // Bounded streaming write: never trust a caller length beyond the
        // engine bound (the up-front check already rejected oversized
        // streams; the loop re-enforces it per chunk).
        let mut remaining = feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES;
        for chunk in bytes.chunks(64 * 1024) {
            ensure!(
                remaining >= chunk.len() as u64,
                "candidate stream exceeds the {} byte bound",
                feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES
            );
            file.write_all(chunk).await?;
            remaining -= chunk.len() as u64;
        }
        file.sync_all().await?;
        let metadata = file.metadata().await?;
        let mode = crate::snapshot::portable_file_mode(&metadata);
        (metadata, mode)
    };
    if let Some(parent) = absolute.parent() {
        sync_candidate_parent_dir(parent).await?;
    }
    let (_, mode) = metadata;
    let hash = feanorfs_common::hash_bytes(bytes);
    Ok(CandidateDescriptor {
        path: relative,
        hash,
        size: bytes.len() as u64,
        mode,
        deleted: false,
    })
}

/// Materializes the authenticated base/ours/theirs legs of one job into the
/// engine-owned job directory, so the designated agent on another machine can
/// reconstruct the context it did not observe live without depending on
/// shared local filesystem paths.
///
/// Each present leg is downloaded by its encrypted blob hash, re-hashed
/// before decrypt, decrypted with the path-bound workspace key, verified
/// against the descriptor's plaintext size, and written create-new, no-follow
/// with fsync. Deleted or absent legs produce no file.
///
/// # Errors
/// Returns an error for unknown/non-active jobs, unsafe destinations, or any
/// download/integrity failure.
pub async fn materialize_resolution_legs(
    ctx: &SyncCtx<'_>,
    job_id: &str,
) -> Result<Vec<(feanorfs_common::ArtifactRoleName, PathBuf)>> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    let store = ResolutionStore::open(ctx.base)?;
    let job = store.load_job(job_id)?;
    if job.assignment_state != ResolutionAssignmentState::Active {
        return Err(anyhow::Error::new(ResolutionOpError::JobNotActive {
            job_id: job_id.to_string(),
            state: job.assignment_state,
        }));
    }
    let conflict = &job.job.conflict;
    let legs = [
        (feanorfs_common::ArtifactRoleName::Original, &conflict.base),
        (feanorfs_common::ArtifactRoleName::Local, &conflict.ours),
        (feanorfs_common::ArtifactRoleName::Cloud, &conflict.theirs),
    ];
    let state_root = ctx.state_dir()?;
    let job_dir = state_root
        .join("orchestrator")
        .join("resolution")
        .join("jobs")
        .join(&job.job.job_id);
    tokio::fs::create_dir_all(&job_dir).await?;
    let mut materialized = Vec::new();
    for (role, leg) in legs {
        if leg.deleted || !leg.present {
            continue;
        }
        let destination = job_dir.join(format!("leg-{}.bin", role.as_str()));
        // The head's conflict triple records base/ours sizes as 0 (only the
        // live theirs leg carries a size); 0 means "not recorded" and skips
        // the size check while the AEAD + ciphertext hash still bind the
        // exact blob.
        let plaintext =
            crate::large_file::read_bytes(ctx, &conflict.path, &leg.hash, leg.size).await?;
        if leg.size != 0 && plaintext.len() as u64 != leg.size {
            anyhow::bail!(
                "materialized leg {:?} size mismatch: got {}, expected {}",
                role,
                plaintext.len(),
                leg.size
            );
        }
        let file = open_create_new_no_follow(&destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("resolution leg {:?} already materialized", role)
            } else {
                anyhow::Error::new(error)
            }
        })?;
        {
            use tokio::io::AsyncWriteExt as _;
            let mut file = tokio::fs::File::from_std(file);
            file.write_all(&plaintext).await?;
            file.sync_all().await?;
        }
        if let Some(parent) = destination.parent() {
            sync_candidate_parent_dir(parent).await?;
        }
        materialized.push((role, destination));
    }
    Ok(materialized)
}

/// Create-new + no-follow open of the candidate destination.
#[cfg(unix)]
fn open_create_new_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_create_new_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(0x0020_0000 | 0x0200_0000) // FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_create_new_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Fsync the candidate's parent directory so the create-new entry survives
/// power loss (Unix only; the file itself is always fsynced).
async fn sync_candidate_parent_dir(parent: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let display = parent.to_path_buf();
        let sync_path = display.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::File::open(&sync_path)?.sync_all()
        })
        .await
        .map_err(|join| anyhow::anyhow!("candidate parent sync task failed: {join}"))?
        .map_err(|error| crate::durable::durability_uncertain(&display, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}

/// Records a typed human answer bound to one exact escalation. Validates the
/// answer against the stored
/// job/assignment/attempt/fingerprint and the exact question generation
/// (stale generations and duplicate answers are rejected typed).
/// `Defer`/`KeepUnresolved` record terminal states without publication;
/// `SubmitCandidate` routes through the SAME candidate validation as
/// `submit_resolution_result` (engine inline verification) and records a
/// `candidate_ready` result that a later guarded apply publishes.
pub async fn answer_resolution(
    ctx: &SyncCtx<'_>,
    answer: HumanResolutionAnswer,
) -> Result<HumanResolutionAnswer> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    validate_human_resolution_answer(&answer)?;
    let store = ResolutionStore::open(ctx.base)?;
    let job = store.load_job(&answer.job_id)?;
    if job.assignment_state != ResolutionAssignmentState::Active {
        return Err(anyhow::Error::new(ResolutionOpError::JobNotActive {
            job_id: answer.job_id.clone(),
            state: job.assignment_state,
        }));
    }
    if !(answer.assignment_id == job.job.assignment_id
        && answer.attempt == job.job.attempt
        && answer.conflict_fingerprint == job.job.conflict_fingerprint)
    {
        return Err(anyhow::Error::new(
            ResolutionOpError::AnswerBindingMismatch {
                job_id: answer.job_id.clone(),
                detail: format!(
                    "answer binds assignment '{}' attempt {} fingerprint '{}', but the job holds \
                 assignment '{}' attempt {} fingerprint '{}'",
                    answer.assignment_id,
                    answer.attempt,
                    answer.conflict_fingerprint,
                    job.job.assignment_id,
                    job.job.attempt,
                    job.job.conflict_fingerprint
                ),
            },
        ));
    }
    let stored = job
        .result
        .as_ref()
        .context("resolution job has no submitted result; there is no question to answer")?;
    if stored.outcome != ResolutionOutcome::RequiresHuman {
        return Err(anyhow::Error::new(
            ResolutionOpError::NoOutstandingQuestion(answer.job_id.clone()),
        ));
    }
    if answer.question_generation != job.question_generation {
        return Err(anyhow::Error::new(
            ResolutionOpError::StaleQuestionGeneration {
                job_id: answer.job_id.clone(),
                expected: job.question_generation,
                received: answer.question_generation,
            },
        ));
    }

    match answer.chosen_option {
        HumanResolutionOption::Defer => {
            store.update(|state| {
                let record = ResolutionStore::find_mut(state, &answer.job_id)?;
                assert_state_transition(
                    record.assignment_state,
                    ResolutionAssignmentState::Deferred,
                )?;
                record.assignment_state = ResolutionAssignmentState::Deferred;
                Ok(())
            })?;
        }
        HumanResolutionOption::KeepUnresolved => {
            store.update(|state| {
                let record = ResolutionStore::find_mut(state, &answer.job_id)?;
                assert_state_transition(
                    record.assignment_state,
                    ResolutionAssignmentState::KeepUnresolved,
                )?;
                record.assignment_state = ResolutionAssignmentState::KeepUnresolved;
                Ok(())
            })?;
        }
        HumanResolutionOption::SubmitCandidate => {
            let candidate = answer
                .candidate
                .as_ref()
                .context("submit_candidate answer must carry a candidate descriptor")?;
            ensure!(
                candidate.path == job.job.candidate_destination.path,
                "candidate path {} must equal the job's engine-owned destination {}",
                candidate.path,
                job.job.candidate_destination.path
            );
            let (_bytes, summary) = verify_candidate_for_submit(ctx, &job, candidate).await?;
            let result = ResolutionResult {
                schema_version: RESOLUTION_SCHEMA_VERSION,
                outcome: ResolutionOutcome::CandidateReady,
                job_id: job.job.job_id.clone(),
                assignment_id: job.job.assignment_id.clone(),
                attempt: job.job.attempt,
                owner: job.job.owner.clone(),
                conflict_fingerprint: job.job.conflict_fingerprint.clone(),
                candidate: Some(candidate.clone()),
                verification: summary,
                diagnostics: vec![],
                question: None,
                human_reason: None,
                question_generation: 0,
                safe_options: vec![],
            };
            validate_resolution_result(&result)?;
            let mut record = job;
            record.verified_at_ms = Some(now_ms());
            record.result = Some(result.clone());
            let store = ResolutionStore::open(ctx.base)?;
            store.update(|state| {
                let existing = ResolutionStore::find_mut(state, &answer.job_id)?;
                assert_state_transition(
                    existing.assignment_state,
                    ResolutionAssignmentState::Active,
                )?;
                *existing = record;
                Ok(())
            })?;
        }
    }
    Ok(answer)
}

/// Records the terminal `Deferred` state for one assignment without any
/// publication; the conflict is preserved for later manual action.
///
/// # Errors
/// Returns an error for unknown jobs or assignments that are not active.
pub async fn defer_resolution(ctx: &SyncCtx<'_>, job_id: &str) -> Result<()> {
    recover_uncertain_publications(ctx).await?;
    let _lock = SyncLock::acquire(ctx.base)?;
    let store = ResolutionStore::open(ctx.base)?;
    store.update(|state| {
        let record = ResolutionStore::find_mut(state, job_id)?;
        ensure!(
            record.assignment_state == ResolutionAssignmentState::Active,
            "resolution assignment {job_id} is not active; defer refused"
        );
        assert_state_transition(record.assignment_state, ResolutionAssignmentState::Deferred)?;
        record.assignment_state = ResolutionAssignmentState::Deferred;
        Ok(())
    })?;
    Ok(())
}

/// Pure helper: validates a candidate descriptor against concrete content
/// bytes (used by tests and adapters; the engine re-validates from the
/// immutable file).
#[must_use]
pub fn candidate_matches_bytes(candidate: &CandidateDescriptor, bytes: &[u8]) -> bool {
    if candidate.deleted {
        return bytes.is_empty();
    }
    candidate.size == bytes.len() as u64 && feanorfs_common::hash_bytes(bytes) == candidate.hash
}

/// Bounded metadata-only status projection of one resolution job
/// (ids/state/counts only; never paths, identities, or bodies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionJobStatus {
    /// 128-bit engine job id (32 lowercase hex chars).
    pub job_id: String,
    /// 128-bit assignment id (32 lowercase hex chars).
    pub assignment_id: String,
    pub attempt: u32,
    pub owner: String,
    /// Exact conflict identity fingerprint this job is bound to.
    pub conflict_fingerprint: String,
    pub assignment_state: ResolutionAssignmentState,
    /// Outcome of the submitted result, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ResolutionOutcome>,
    /// Monotonic per-fingerprint question generation of the escalation this
    /// job carries (0 when no question was ever recorded). Every human
    /// answer must reference the exact generation.
    pub question_generation: u32,
    pub created_at_ms: i64,
    /// Verification evidence time recorded at submit (freshness bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at_ms: Option<i64>,
}

/// Bounded metadata-only projection of the whole resolution store
/// (ids/state/counts only; never paths or bodies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionStatusProjection {
    pub schema_version: u32,
    pub jobs: Vec<ResolutionJobStatus>,
}

/// Reads the bounded resolution status projection. `job_id` restricts the
/// projection to one job; unknown ids yield an empty projection. First
/// converges any crash-left `PublicationUncertain` records (the store-load
/// recovery hook), then reads. Never touches the worktree, registry,
/// artifacts, or head beyond that recovery.
///
/// # Errors
/// Returns an error for corrupt or unsupported-schema store state.
pub async fn resolution_status(
    ctx: &SyncCtx<'_>,
    job_id: Option<&str>,
) -> Result<ResolutionStatusProjection> {
    recover_uncertain_publications(ctx).await?;
    let store = ResolutionStore::open(ctx.base)?;
    let state = store.load()?;
    let mut jobs: Vec<ResolutionJobStatus> = state
        .jobs
        .iter()
        .filter(|record| job_id.is_none_or(|id| record.job.job_id == id))
        .map(|record| ResolutionJobStatus {
            job_id: record.job.job_id.clone(),
            assignment_id: record.job.assignment_id.clone(),
            attempt: record.job.attempt,
            owner: record.job.owner.clone(),
            conflict_fingerprint: record.job.conflict_fingerprint.clone(),
            assignment_state: record.assignment_state,
            outcome: record.result.as_ref().map(|result| result.outcome),
            question_generation: record.question_generation,
            created_at_ms: record.created_at_ms,
            verified_at_ms: record.verified_at_ms,
        })
        .collect();
    jobs.sort_by(|left, right| {
        left.created_at_ms
            .cmp(&right.created_at_ms)
            .then_with(|| left.job_id.cmp(&right.job_id))
    });
    Ok(ResolutionStatusProjection {
        schema_version: RESOLUTION_STORE_SCHEMA_VERSION,
        jobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiClient;
    use crate::durable::{set_atomic_faults, AtomicFaults};
    use crate::snapshot::{
        clear_publish_crash, inject_publish_crash, SnapshotInput, TestPublishCrashPoint,
    };
    use crate::work::{WorkProposalRecord, WorkStore, WorkTaskRecord};
    use crate::{LocalHub, SnapshotEngine, SwapHeadResult, SyncCtx};
    use feanorfs_common::resolution_contract::{
        HumanResolutionAnswer, HumanResolutionOption, HumanResolutionReason,
    };
    use feanorfs_common::{
        hash_bytes, work_contract::WorkScope, ConcurrentEdit, ConflictIdentity, ConflictKind,
        FileState, LegacyPolicy, VerificationStatus, VerificationSummary, WorkTaskState,
    };
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::sync::Arc;

    struct Harness {
        _hub_data: tempfile::TempDir,
        base: tempfile::TempDir,
        api: ApiClient,
        db: crate::local::ClientDb,
    }

    fn hex64(byte: u8) -> String {
        std::iter::repeat_n(char::from(byte), 64).collect()
    }

    async fn setup() -> Harness {
        let hub_data = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let hub = LocalHub::open(hub_data.path().to_path_buf(), None)
            .await
            .unwrap();
        let api = ApiClient::local(Arc::clone(&hub), None);
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(&state).await.unwrap();
        Harness {
            _hub_data: hub_data,
            base,
            api,
            db,
        }
    }

    impl Harness {
        fn ctx(&self) -> SyncCtx<'_> {
            SyncCtx::with_format_version(
                &self.api,
                &self.db,
                self.base.path(),
                "workspace",
                Some("shared-key"),
                LegacyPolicy::Reject,
                3,
            )
        }
    }

    fn file_state(path: &str, content: &[u8], mode: u32) -> FileState {
        FileState {
            path: path.to_string(),
            hash: hash_bytes(content),
            size: content.len() as u64,
            mtime: 1,
            deleted: false,
            mode,
        }
    }

    fn edit_edit(path: &str) -> ConcurrentEdit {
        ConcurrentEdit::new(
            path.to_string(),
            Some(file_state(path, b"base-content", 0)),
            Some(file_state(path, b"ours-content", 0)),
            Some(file_state(path, b"theirs-content", 0)),
        )
    }

    async fn publish_conflict_head(ctx: &SyncCtx<'_>, edit: &ConcurrentEdit) -> String {
        let engine = SnapshotEngine::new(ctx);
        let id = engine
            .write(SnapshotInput {
                files: &HashMap::new(),
                conflicts: std::slice::from_ref(edit),
                parents: vec![],
                author: "test",
                message: None,
            })
            .await
            .unwrap();
        let expected = ctx.api.get_head("workspace").await.unwrap();
        match ctx
            .api
            .swap_head("workspace", expected.as_deref(), &id)
            .await
            .unwrap()
        {
            SwapHeadResult::Swapped => id,
            _ => panic!("head must swap"),
        }
    }

    async fn upload_legs(ctx: &SyncCtx<'_>) {
        for content in [
            b"base-content".as_slice(),
            b"ours-content",
            b"theirs-content",
        ] {
            ctx.api
                .upload_object("workspace", &hash_bytes(content), content.to_vec())
                .await
                .unwrap();
        }
    }

    async fn register_conflict(ctx: &SyncCtx<'_>, head: &str, edit: &ConcurrentEdit) -> PathBuf {
        let engine = SnapshotEngine::new(ctx);
        let snapshot = engine.load_snapshot(head).await.unwrap();
        let identity = crate::conflict_artifacts::conflict_identity_from_edit(
            ctx.workspace_id(),
            head,
            head,
            &snapshot.root,
            edit,
            ConflictKind::EditEdit,
            &IdentityBinding::default(),
        );
        let fingerprint = compute_conflict_identity_fingerprint(&identity);
        let dir = ctx.state_dir().unwrap().join("conflicts/test-1");
        std::fs::create_dir_all(&dir).unwrap();
        ctx.db
            .upsert_conflict_fingerprinted(
                &edit.path,
                &ConflictKind::EditEdit,
                &dir.to_string_lossy(),
                1,
                &identity,
                &fingerprint,
            )
            .await
            .unwrap();
        dir
    }

    fn seed_proposal(
        agent: &str,
        path: &str,
        intent_id: &str,
        causal_base: Option<String>,
    ) -> WorkProposalRecord {
        WorkProposalRecord {
            agent: agent.to_string(),
            sequence: 1,
            intent_message_id: intent_id.to_string(),
            coordinator: Some("human".to_string()),
            causal_base,
            original_scope: WorkScope {
                paths: vec![path.to_string()],
                concerns: vec![],
                dependencies: vec![],
            },
            scope: WorkScope {
                paths: vec![path.to_string()],
                concerns: vec![],
                dependencies: vec![],
            },
            state: WorkTaskState::Accepted,
            capabilities: vec!["edit".to_string()],
            decision: None,
            superseded_decisions: vec![],
            amendments: vec![],
            accepted_overlap: vec![],
            verification: None,
            inspected_snapshot: None,
            outcome: None,
            reason: None,
            source_message_id: intent_id.to_string(),
            author_restore: None,
            updated_at_ms: 1,
        }
    }

    async fn seed_accepted(ctx: &SyncCtx<'_>, task_id: &str, proposals: Vec<WorkProposalRecord>) {
        let store = WorkStore::open(ctx.base).unwrap();
        store
            .update(|state| {
                state.incomplete = false;
                state.tasks = vec![WorkTaskRecord {
                    task_id: task_id.to_string(),
                    proposals,
                    updated_at_ms: 1,
                }];
                Ok(())
            })
            .unwrap();
    }

    fn result_for(job: &ResolutionJob, bytes: &[u8]) -> ResolutionResult {
        ResolutionResult {
            schema_version: RESOLUTION_SCHEMA_VERSION,
            outcome: ResolutionOutcome::CandidateReady,
            job_id: job.job_id.clone(),
            assignment_id: job.assignment_id.clone(),
            attempt: job.attempt,
            owner: job.owner.clone(),
            conflict_fingerprint: job.conflict_fingerprint.clone(),
            candidate: Some(CandidateDescriptor {
                path: job.candidate_destination.path.clone(),
                hash: hash_bytes(bytes),
                size: bytes.len() as u64,
                mode: 0,
                deleted: false,
            }),
            verification: VerificationSummary {
                status: VerificationStatus::Passed,
                summary: "resolver tests passed".to_string(),
                ..VerificationSummary::default()
            },
            diagnostics: vec![],
            question: None,
            human_reason: None,
            question_generation: 0,
            safe_options: vec![],
        }
    }

    fn write_candidate(job: &ResolutionJob, state_root: &Path, bytes: &[u8]) {
        let abs = state_root.join(&job.candidate_destination.path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&abs)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn stale_kind(outcome: &ResolutionApplyOutcome) -> ResolutionStaleKind {
        match outcome {
            ResolutionApplyOutcome::Stale { kind, .. } => *kind,
            other => panic!("expected stale outcome, got {other:?}"),
        }
    }

    async fn head_of(ctx: &SyncCtx<'_>) -> String {
        ctx.api.get_head("workspace").await.unwrap().unwrap()
    }

    async fn conflict_survives(ctx: &SyncCtx<'_>, head: &str, path: &str) {
        let engine = SnapshotEngine::new(ctx);
        let state = engine.load_state(head).await.unwrap();
        assert!(
            state.conflicts.iter().any(|conflict| conflict.path == path),
            "conflict at {path} must survive unchanged"
        );
    }

    const PATH: &str = "src/main.rs";
    const CANDIDATE: &[u8] = b"reconciled content";

    async fn prepared_job(ctx: &SyncCtx<'_>, path: &str) -> (String, ResolutionJob) {
        upload_legs(ctx).await;
        let edit = edit_edit(path);
        let head = publish_conflict_head(ctx, &edit).await;
        register_conflict(ctx, &head, &edit).await;
        seed_accepted(
            ctx,
            "parser-impl",
            vec![seed_proposal("agent-a", path, &hex64(b'a'), None)],
        )
        .await;
        let job = prepare_resolution_job(
            ctx,
            path,
            PreventionReason::Exhausted {
                detail: "no bounded prevention path remains".to_string(),
            },
        )
        .await
        .unwrap();
        (head, job)
    }

    #[tokio::test]
    async fn prepare_submit_apply_publishes_exact_candidate() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        assert_eq!(job.owner, "agent-a");
        assert_eq!(job.conflict.path, PATH);
        assert!(job.conflict.is_automatic());
        assert!(job.conflict_fingerprint.len() == 64);
        assert!(!ctx.base.join(PATH).exists());

        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        let result = result_for(&job, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result)
            .await
            .unwrap();

        // Submission never mutates worktree, conflict registry, artifacts, or head.
        assert!(!ctx.base.join(PATH).exists());
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
        assert_eq!(head_of(&ctx).await, head);
        assert!(state_root.join(&job.candidate_destination.path).is_file());

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        let ResolutionApplyOutcome::Published { head: new_head } = outcome else {
            panic!("expected published, got {outcome:?}");
        };
        assert_ne!(new_head, head);

        let engine = SnapshotEngine::new(&ctx);
        let state = engine.load_state(&new_head).await.unwrap();
        assert!(state.conflicts.iter().all(|conflict| conflict.path != PATH));
        assert!(state.files.contains_key(PATH));
        assert_eq!(state.files[PATH].size, CANDIDATE.len() as u64);
        assert!(!state.files[PATH].deleted);

        // Confirmed success recorded typed history and cleaned the job.
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_none());
        let history = ctx.db.list_conflict_resolutions().await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].method, "candidate");
        assert_eq!(history[0].resolver, "agent-a");
        assert_eq!(
            history[0].source_file_hash.as_deref(),
            Some(hash_bytes(CANDIDATE).as_str())
        );
        let store = ResolutionStore::open(ctx.base).unwrap();
        let record = store.load_job(&job.job_id).unwrap();
        assert_eq!(
            record.assignment_state,
            ResolutionAssignmentState::Completed
        );
        // The immutable job directory was cleaned up after confirmed success.
        assert!(!state_root
            .join("orchestrator/resolution/jobs")
            .join(&job.job_id)
            .exists());
        // Worktree stays untouched by automatic resolution.
        assert!(!ctx.base.join(PATH).exists());
    }

    #[tokio::test]
    async fn prepare_refuses_no_conflict_and_legacy_records() {
        let h = setup().await;
        let ctx = h.ctx();
        seed_accepted(
            &ctx,
            "parser-impl",
            vec![seed_proposal("agent-a", PATH, &hex64(b'a'), None)],
        )
        .await;
        // A head exists but carries no conflict for the path.
        let empty = SnapshotEngine::new(&ctx)
            .write(SnapshotInput {
                files: &HashMap::new(),
                conflicts: &[],
                parents: vec![],
                author: "test",
                message: None,
            })
            .await
            .unwrap();
        ctx.api.swap_head("workspace", None, &empty).await.unwrap();
        let error = prepare_resolution_job(
            &ctx,
            PATH,
            PreventionReason::Exhausted {
                detail: "no bounded prevention path remains".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("no current conflict"));

        // A legacy unfingerprinted record can never enter automatic prepare.
        upload_legs(&ctx).await;
        let edit = edit_edit(PATH);
        let head = publish_conflict_head(&ctx, &edit).await;
        let dir = ctx.state_dir().unwrap().join("conflicts/legacy");
        std::fs::create_dir_all(&dir).unwrap();
        ctx.db
            .upsert_conflict(
                PATH,
                &ConflictKind::EditEdit,
                &dir.to_string_lossy(),
                1,
                crate::state::ConflictRecordStatus::Pending,
            )
            .await
            .unwrap();
        let error = prepare_resolution_job(
            &ctx,
            PATH,
            PreventionReason::Exhausted {
                detail: "no bounded prevention path remains".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("legacy unfingerprinted record"));
        let _ = head;
    }

    #[tokio::test]
    async fn prepare_refuses_without_typed_prevention_reason() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, _job) = prepared_job(&ctx, PATH).await;
        let error = prepare_resolution_job(
            &ctx,
            PATH,
            PreventionReason::Exhausted {
                detail: "   ".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("typed prevention"));
    }

    #[tokio::test]
    async fn submit_rejects_replay_wrong_binding_and_mutated_candidate() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        let result = result_for(&job, CANDIDATE);

        // Wrong owner.
        let mut wrong_owner = result.clone();
        wrong_owner.owner = "intruder".to_string();
        assert!(submit_resolution_result(&ctx, &job.job_id, wrong_owner)
            .await
            .is_err());
        // Wrong attempt.
        let mut wrong_attempt = result.clone();
        wrong_attempt.attempt = 7;
        assert!(submit_resolution_result(&ctx, &job.job_id, wrong_attempt)
            .await
            .is_err());
        // Wrong fingerprint.
        let mut wrong_fp = result.clone();
        wrong_fp.conflict_fingerprint = hex64(b'9');
        assert!(submit_resolution_result(&ctx, &job.job_id, wrong_fp)
            .await
            .is_err());
        // Wrong assignment.
        let mut wrong_assignment = result.clone();
        wrong_assignment.assignment_id = hex64(b'8')[..32].to_string();
        assert!(
            submit_resolution_result(&ctx, &job.job_id, wrong_assignment)
                .await
                .is_err()
        );
        // Mutated candidate descriptor (hash does not match the file).
        let mut mutated = result.clone();
        mutated.candidate.as_mut().unwrap().hash = hash_bytes(b"other bytes");
        let error = submit_resolution_result(&ctx, &job.job_id, mutated)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("hash") || error.to_string().contains("Candidate"),
            "{error}"
        );
        // Candidate path mismatch.
        let mut wrong_path = result.clone();
        wrong_path.candidate.as_mut().unwrap().path = "orchestrator/other.bin".to_string();
        assert!(submit_resolution_result(&ctx, &job.job_id, wrong_path)
            .await
            .is_err());
        // Oversized result (diagnostics bound).
        let mut oversized = result.clone();
        oversized.diagnostics = vec!["d".repeat(1024); 17];
        assert!(submit_resolution_result(&ctx, &job.job_id, oversized)
            .await
            .is_err());
        // Replay after the valid submission.
        submit_resolution_result(&ctx, &job.job_id, result.clone())
            .await
            .unwrap();
        let error = submit_resolution_result(&ctx, &job.job_id, result)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("replay"));

        // Nothing was published by any of these submissions.
        assert_eq!(
            head_of(&ctx).await,
            ctx.api.get_head("workspace").await.unwrap().unwrap()
        );
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    #[tokio::test]
    async fn apply_before_submit_is_refused() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let error = apply_resolution_job(&ctx, &job.job_id).await.unwrap_err();
        assert!(error.to_string().contains("no submitted result"));
        assert_eq!(head_of(&ctx).await, head);
        conflict_survives(&ctx, &head, PATH).await;
    }

    /// Race matrix: mutating the head (signal-only, same root) between
    /// prepare and apply yields typed stale and the conflict survives.
    #[tokio::test]
    async fn race_head_mutation_is_stale_and_conflict_survives() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // Signal-only head: same root, same conflict legs, new snapshot id.
        let edit = edit_edit(PATH);
        let signal = publish_conflict_head(&ctx, &edit).await;
        ctx.api
            .swap_head("workspace", Some(&job.current_snapshot), &signal)
            .await
            .unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::HeadChanged);
        // The current conflict survives unchanged; nothing else moved.
        conflict_survives(&ctx, &signal, PATH).await;
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
        assert!(state_root.join(&job.candidate_destination.path).is_file());
        assert!(!ctx.base.join(PATH).exists());
    }

    /// Race matrix: a head whose conflict legs changed yields typed stale and
    /// the changed conflict survives untouched.
    #[tokio::test]
    async fn race_changed_conflict_head_is_stale_and_survives() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // A concurrent sync changed the ours leg.
        let mut edit = edit_edit(PATH);
        edit.ours = Some(file_state(PATH, b"newer-ours", 0));
        ctx.api
            .upload_object(
                "workspace",
                &hash_bytes(b"newer-ours"),
                b"newer-ours".to_vec(),
            )
            .await
            .unwrap();
        let changed = publish_conflict_head(&ctx, &edit).await;
        ctx.api
            .swap_head("workspace", Some(&job.current_snapshot), &changed)
            .await
            .unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::HeadChanged);
        conflict_survives(&ctx, &changed, PATH).await;
        let engine = SnapshotEngine::new(&ctx);
        let state = engine.load_state(&changed).await.unwrap();
        let conflict = state
            .conflicts
            .iter()
            .find(|conflict| conflict.path == PATH)
            .unwrap();
        assert_eq!(
            conflict.ours.as_ref().unwrap().hash,
            hash_bytes(b"newer-ours")
        );
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
    }

    /// Race matrix: the candidate bytes change after submit.
    #[tokio::test]
    async fn race_candidate_bytes_mutation_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // Mutate the immutable candidate file after submit.
        let abs = state_root.join(&job.candidate_destination.path);
        std::fs::write(&abs, b"mutated candidate bytes").unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(
            stale_kind(&outcome),
            ResolutionStaleKind::CandidateHashMismatch
        );
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
    }

    /// Race matrix: the candidate is truncated after submit.
    #[tokio::test]
    async fn race_candidate_size_mutation_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // Grow the immutable candidate beyond the engine bound (sparse file
        // keeps this instant).
        let abs = state_root.join(&job.candidate_destination.path);
        let file = std::fs::OpenOptions::new().write(true).open(&abs).unwrap();
        file.set_len(feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES + 1)
            .unwrap();
        drop(file);

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(
            stale_kind(&outcome),
            ResolutionStaleKind::CandidateSizeMismatch
        );
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    /// Race matrix: the candidate file disappears after submit.
    #[tokio::test]
    async fn race_candidate_missing_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        let abs = state_root.join(&job.candidate_destination.path);
        std::fs::remove_file(&abs).unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::CandidateMissing);
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    /// Race matrix: the candidate path resolves through a symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn race_candidate_symlink_substitution_is_stale() {
        use std::os::unix::fs::symlink;

        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        let abs = state_root.join(&job.candidate_destination.path);
        std::fs::remove_file(&abs).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside bytes").unwrap();
        symlink(outside.path().join("secret"), &abs).unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        if stale_kind(&outcome) != ResolutionStaleKind::CandidateSymlink {
            panic!("expected symlink staleness, got {outcome:?}");
        }
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    /// Race matrix: the candidate executable mode changes after submit.
    #[cfg(unix)]
    #[tokio::test]
    async fn race_candidate_mode_mutation_is_stale() {
        use std::os::unix::fs::PermissionsExt as _;

        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        let abs = state_root.join(&job.candidate_destination.path);
        std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(
            stale_kind(&outcome),
            ResolutionStaleKind::CandidateModeMismatch
        );
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    /// Race matrix: the assignment is revoked between prepare and apply.
    #[tokio::test]
    async fn race_revoked_assignment_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        revoke_resolution_assignment(&ctx, &job.job_id, false)
            .await
            .unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::AssignmentRevoked);
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
    }

    /// Race matrix: a superseded assignment is stale too.
    #[tokio::test]
    async fn race_superseded_assignment_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        revoke_resolution_assignment(&ctx, &job.job_id, true)
            .await
            .unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::AssignmentRevoked);
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    /// Race matrix: verification evidence expires before apply.
    #[tokio::test]
    async fn race_verification_expiry_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // Push the recorded verification time far into the past (simulating a
        // very slow resolver) beyond the fixed 10-minute freshness bound.
        let store = ResolutionStore::open(ctx.base).unwrap();
        store
            .update(|state| {
                let record = ResolutionStore::find_mut(state, &job.job_id)?;
                record.verified_at_ms = Some(now_ms() - (job.verification.timeout_ms as i64) - 1);
                Ok(())
            })
            .unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(
            stale_kind(&outcome),
            ResolutionStaleKind::VerificationExpired
        );
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    /// Lost CAS with an unchanged head: the plan is discarded and ALL
    /// validation restarts, then publication succeeds on the fresh pass.
    #[tokio::test]
    async fn lost_cas_restarts_validation_and_succeeds() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        crate::snapshot::inject_lost_cas(1, None);
        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        let ResolutionApplyOutcome::Published { head: new_head } = outcome else {
            panic!("expected published after lost-CAS restart, got {outcome:?}");
        };
        assert_ne!(new_head, head);
        let state = SnapshotEngine::new(&ctx)
            .load_state(&new_head)
            .await
            .unwrap();
        assert!(state.conflicts.iter().all(|conflict| conflict.path != PATH));
    }

    /// Race matrix: lost CAS followed by a signal-only head with the same
    /// root → typed stale; the current conflict survives unchanged.
    #[tokio::test]
    async fn lost_cas_then_signal_only_head_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // The signal-only head appears during the CAS window.
        let edit = edit_edit(PATH);
        let signal = publish_conflict_head(&ctx, &edit).await;
        crate::snapshot::inject_lost_cas(1, Some(signal.clone()));

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::HeadChanged);
        conflict_survives(&ctx, &signal, PATH).await;
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
        assert!(!ctx.base.join(PATH).exists());
    }

    /// Race matrix: lost CAS followed by a changed conflict → typed stale;
    /// the changed conflict survives untouched.
    #[tokio::test]
    async fn lost_cas_then_changed_conflict_is_stale() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        let mut edit = edit_edit(PATH);
        edit.theirs = Some(file_state(PATH, b"changed-theirs", 0));
        ctx.api
            .upload_object(
                "workspace",
                &hash_bytes(b"changed-theirs"),
                b"changed-theirs".to_vec(),
            )
            .await
            .unwrap();
        let changed = publish_conflict_head(&ctx, &edit).await;
        crate::snapshot::inject_lost_cas(1, Some(changed.clone()));

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::HeadChanged);
        conflict_survives(&ctx, &changed, PATH).await;
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
    }

    /// Crash windows: before submit nothing changed; after submit only the
    /// private store changed; after a crash mid-apply the conflict survives.
    #[tokio::test]
    async fn crash_windows_leave_consistent_state() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();

        // Crash before submit: job persisted, nothing else moved.
        assert!(state_root
            .join(&job.candidate_destination.path)
            .parent()
            .unwrap()
            .is_dir());
        let store = ResolutionStore::open(ctx.base).unwrap();
        assert!(store.load_job(&job.job_id).unwrap().result.is_none());
        assert_eq!(head_of(&ctx).await, head);
        conflict_survives(&ctx, &head, PATH).await;
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());

        // Crash after submit: result recorded, head/conflict/worktree intact.
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        assert_eq!(head_of(&ctx).await, head);
        conflict_survives(&ctx, &head, PATH).await;
        assert!(!ctx.base.join(PATH).exists());

        // Crash before CAS: nothing published.
        assert!(store.load_job(&job.job_id).unwrap().result.is_some());
        assert_eq!(head_of(&ctx).await, head);

        // Crash during cleanup: publication confirmed; the job directory may
        // remain but the conflict is resolved and the head moved.
        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert!(matches!(outcome, ResolutionApplyOutcome::Published { .. }));
        assert_ne!(head_of(&ctx).await, head);
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_none());
    }

    /// Cleanup failure after confirmed success never undoes the publication:
    /// a crash after Completed leaves the job directory (with journal) which
    /// recovery sweeps, and a permission-blocked removal only logs a warning
    /// while the Completed state and the publication stand.
    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_failure_after_success_keeps_publication() {
        use std::os::unix::fs::PermissionsExt as _;

        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // Crash after Completed, before artifact/journal cleanup.
        inject_publish_crash(TestPublishCrashPoint::AfterCompleted);
        let error = apply_resolution_job(&ctx, &job.job_id).await.unwrap_err();
        clear_publish_crash();
        assert!(error.to_string().contains("simulated crash"));
        assert_ne!(head_of(&ctx).await, head);

        // Make the job directory unremovable so the recovery sweep's cleanup
        // fails; recovery must still converge (publication stands). The
        // record is already Completed after the crash, so recovery only
        // sweeps the directory.
        let job_dir = state_root
            .join("orchestrator/resolution/jobs")
            .join(&job.job_id);
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        recover_uncertain_publications(&ctx).await.unwrap();

        let state = SnapshotEngine::new(&ctx)
            .load_state(&head_of(&ctx).await)
            .await
            .unwrap();
        assert!(state.conflicts.iter().all(|conflict| conflict.path != PATH));
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_none());
        let store = ResolutionStore::open(ctx.base).unwrap();
        assert_eq!(
            store.load_job(&job.job_id).unwrap().assignment_state,
            ResolutionAssignmentState::Completed
        );
        // The unremovable directory survives; the next recovery with write
        // permission sweeps it.
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        recover_uncertain_publications(&ctx).await.unwrap();
        assert!(!job_dir.exists());
    }

    /// Direct guarded-publication unit checks for the deeper typed stale
    /// kinds (same head, mutated inputs).
    #[tokio::test]
    async fn publish_rejects_leg_changes_and_identity_mismatch() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let engine = SnapshotEngine::new(&ctx);
        let snapshot = engine.load_snapshot(&head).await.unwrap();
        let state = engine.objects.get_tree_state(&snapshot.root).await.unwrap();
        let conflict = state
            .conflicts
            .iter()
            .find(|conflict| conflict.path == PATH)
            .unwrap();

        let base_plan = |identity: ConflictIdentity| ResolutionPublication {
            identity: identity.clone(),
            fingerprint: compute_conflict_identity_fingerprint(&identity),
            candidate: None,
            candidate_file: None,
            manual_state: None,
            additional: vec![],
            expected_head: head.clone(),
            author: "test".to_string(),
        };

        // Mutated ours leg.
        let mut identity = crate::conflict_artifacts::conflict_identity_from_edit(
            ctx.workspace_id(),
            &head,
            &head,
            &snapshot.root,
            conflict,
            ConflictKind::EditEdit,
            &IdentityBinding::default(),
        );
        identity.ours.hash = hex64(b'9');
        let error = engine
            .publish_resolution(base_plan(identity))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StalePublication>().unwrap().kind,
            ResolutionStaleKind::LegsChanged
        );

        // Missing conflict (path differs from the head).
        let mut identity = crate::conflict_artifacts::conflict_identity_from_edit(
            ctx.workspace_id(),
            &head,
            &head,
            &snapshot.root,
            conflict,
            ConflictKind::EditEdit,
            &IdentityBinding::default(),
        );
        identity.path = "src/other.rs".to_string();
        let error = engine
            .publish_resolution(base_plan(identity))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StalePublication>().unwrap().kind,
            ResolutionStaleKind::ConflictMissing
        );

        // Identity mismatch: the inspected snapshot field no longer matches
        // the head (legs are equal, so only the full identity/fingerprint
        // recomputation can catch it).
        let mut identity = crate::conflict_artifacts::conflict_identity_from_edit(
            ctx.workspace_id(),
            &head,
            &head,
            &snapshot.root,
            conflict,
            ConflictKind::EditEdit,
            &IdentityBinding::default(),
        );
        identity.about_snapshot = hex64(b'f');
        let error = engine
            .publish_resolution(base_plan(identity))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StalePublication>().unwrap().kind,
            ResolutionStaleKind::IdentityMismatch
        );

        // Stale expected head → lost CAS (single attempt, never a retry).
        let identity = crate::conflict_artifacts::conflict_identity_from_edit(
            ctx.workspace_id(),
            &head,
            &head,
            &snapshot.root,
            conflict,
            ConflictKind::EditEdit,
            &IdentityBinding::default(),
        );
        let mut plan = base_plan(identity);
        plan.expected_head = hex64(b'f');
        let error = engine.publish_resolution(plan).await.unwrap_err();
        assert!(error.downcast_ref::<crate::snapshot::LostCas>().is_some());

        // The conflict is untouched by every refused publication.
        conflict_survives(&ctx, &head, PATH).await;
        let _ = job;
    }

    /// No-change-required results remove the conflict without touching files.
    #[tokio::test]
    async fn no_change_required_removes_only_the_conflict() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let mut result = result_for(&job, CANDIDATE);
        result.outcome = ResolutionOutcome::NoChangeRequired;
        result.candidate = None;
        submit_resolution_result(&ctx, &job.job_id, result)
            .await
            .unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        let ResolutionApplyOutcome::Published { head: new_head } = outcome else {
            panic!("expected published, got {outcome:?}");
        };
        assert_ne!(new_head, head);
        let state = SnapshotEngine::new(&ctx)
            .load_state(&new_head)
            .await
            .unwrap();
        assert!(state.conflicts.iter().all(|conflict| conflict.path != PATH));
        // Exactly the matching conflict identity was removed; the file entry
        // survives as an ordinary regular file (no content change).
        assert!(
            state.files.contains_key(PATH),
            "no_change_required must keep the visible file"
        );
    }

    /// Helper: one `requires_human` result for a job (the engine assigns the
    /// question generation at submit).
    fn requires_human_result_for(job: &ResolutionJob) -> ResolutionResult {
        ResolutionResult {
            schema_version: RESOLUTION_SCHEMA_VERSION,
            outcome: ResolutionOutcome::RequiresHuman,
            job_id: job.job_id.clone(),
            assignment_id: job.assignment_id.clone(),
            attempt: job.attempt,
            owner: job.owner.clone(),
            conflict_fingerprint: job.conflict_fingerprint.clone(),
            candidate: None,
            verification: VerificationSummary {
                status: VerificationStatus::Unknown,
                summary: "resolver asks the human".to_string(),
                ..VerificationSummary::default()
            },
            diagnostics: vec![],
            question: Some("which side is canonical?".to_string()),
            human_reason: Some(HumanResolutionReason::SemanticAmbiguity),
            question_generation: 0,
            safe_options: vec![
                HumanResolutionOption::KeepUnresolved,
                HumanResolutionOption::Defer,
                HumanResolutionOption::SubmitCandidate,
            ],
        }
    }

    /// Helper: one typed answer for a job's question.
    fn answer_for(
        job: &ResolutionJob,
        generation: u32,
        option: HumanResolutionOption,
    ) -> HumanResolutionAnswer {
        HumanResolutionAnswer {
            schema_version: RESOLUTION_SCHEMA_VERSION,
            job_id: job.job_id.clone(),
            assignment_id: job.assignment_id.clone(),
            attempt: job.attempt,
            conflict_fingerprint: job.conflict_fingerprint.clone(),
            question_generation: generation,
            chosen_option: option,
            candidate: None,
            verification: None,
        }
    }

    fn op_error(error: &anyhow::Error) -> &ResolutionOpError {
        error
            .downcast_ref::<ResolutionOpError>()
            .unwrap_or_else(|| panic!("expected a typed ResolutionOpError, got {error}"))
    }

    /// put_resolution_candidate is bounded, create-new, rehashed, and typed:
    /// a second put for the same destination is refused, an oversized stream
    /// is refused, an inactive job is refused, and the returned descriptor
    /// drives a guarded publication that publishes exactly those bytes.
    #[tokio::test]
    async fn put_candidate_bounds_create_new_rehash_and_submit() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();

        let descriptor = put_resolution_candidate(&ctx, &job.job_id, CANDIDATE)
            .await
            .unwrap();
        assert_eq!(descriptor.path, job.candidate_destination.path);
        assert_eq!(descriptor.hash, hash_bytes(CANDIDATE));
        assert_eq!(descriptor.size, CANDIDATE.len() as u64);
        assert!(!descriptor.deleted);
        // The file was written create-new, no-follow, and fsynced.
        let abs = state_root.join(&descriptor.path);
        assert_eq!(std::fs::read(&abs).unwrap(), CANDIDATE);

        // Create-new: a second put is refused typed.
        let error = put_resolution_candidate(&ctx, &job.job_id, CANDIDATE)
            .await
            .unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::CandidateAlreadyExists(_)
        ));

        // Oversized streams are refused typed.
        let oversized = vec![0u8; feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES as usize + 1];
        let error = put_resolution_candidate(&ctx, &job.job_id, &oversized)
            .await
            .unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::CandidateTooLarge { .. }
        ));

        // Submit with the engine-returned descriptor, then publish exactly
        // those bytes.
        let mut result = result_for(&job, CANDIDATE);
        result.candidate = Some(descriptor.clone());
        submit_resolution_result(&ctx, &job.job_id, result)
            .await
            .unwrap();

        // A job whose result already carries a candidate is no longer a
        // submitted-without-candidate state: put is refused typed.
        let error = put_resolution_candidate(&ctx, &job.job_id, CANDIDATE)
            .await
            .unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::NotSubmittedWithoutCandidate(_)
        ));

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        let ResolutionApplyOutcome::Published { head: new_head } = outcome else {
            panic!("expected published, got {outcome:?}");
        };
        let state = SnapshotEngine::new(&ctx)
            .load_state(&new_head)
            .await
            .unwrap();
        assert!(state.conflicts.iter().all(|conflict| conflict.path != PATH));
        assert_eq!(state.files[PATH].size, CANDIDATE.len() as u64);
        assert!(!state.files[PATH].deleted);

        // An inactive job refuses put typed.
        let (_, other) = prepared_job(&ctx, "src/other.rs").await;
        revoke_resolution_assignment(&ctx, &other.job_id, false)
            .await
            .unwrap();
        let error = put_resolution_candidate(&ctx, &other.job_id, CANDIDATE)
            .await
            .unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::JobNotActive { .. }
        ));
    }

    /// The engine executes the fixed inline verification policy at submit
    /// and records real evidence; a candidate that fails is rejected typed.
    #[tokio::test]
    async fn submit_engine_verification_records_evidence_and_rejects() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        write_candidate(&job, &ctx.state_dir().unwrap(), CANDIDATE);

        let result = result_for(&job, CANDIDATE);
        let stored = submit_resolution_result(&ctx, &job.job_id, result)
            .await
            .unwrap();
        let evidence = &stored.verification;
        assert_eq!(evidence.status, VerificationStatus::Passed);
        assert_eq!(
            evidence.policy_id.as_deref(),
            Some(feanorfs_common::RESOLUTION_VERIFICATION_POLICY_ID)
        );
        assert_eq!(evidence.policy_version, 1);
        assert_eq!(
            evidence.input_hashes,
            vec![job.job_id.clone(), job.conflict_fingerprint.clone()]
        );
        assert_eq!(
            evidence.output_hash.as_deref(),
            Some(hash_bytes(CANDIDATE).as_str())
        );
        let names: Vec<&str> = evidence
            .checks
            .iter()
            .map(|check| check.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "candidate_bytes_match_descriptor",
                "candidate_within_allowed_output_paths",
                "candidate_size_bounded",
                "candidate_descriptor_consistent",
            ]
        );
        assert!(evidence.checks.iter().all(|check| check.passed));

        // A result whose descriptor claims different bytes is rejected typed
        // (the engine re-verifies from the immutable file).
        let (_head, job2) = prepared_job(&ctx, "src/other.rs").await;
        write_candidate(&job2, &ctx.state_dir().unwrap(), CANDIDATE);
        let mut forged = result_for(&job2, CANDIDATE);
        forged.candidate.as_mut().unwrap().hash = hash_bytes(b"other bytes");
        let error = submit_resolution_result(&ctx, &job2.job_id, forged)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<StalePublication>()
                .map(|stale| stale.kind),
            Some(ResolutionStaleKind::CandidateHashMismatch)
        );
    }

    /// The deleted-marker path returns CandidateMissing only for genuine
    /// absence: a permission-denied marker is typed CandidatePermissionDenied.
    #[cfg(unix)]
    #[tokio::test]
    async fn deleted_candidate_marker_typed_absence_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();

        // A deletion candidate with no marker file at all is genuinely
        // absent and submits fine.
        let mut deleted = result_for(&job, b"");
        deleted.outcome = ResolutionOutcome::CandidateReady;
        deleted.candidate = Some(CandidateDescriptor {
            path: job.candidate_destination.path.clone(),
            hash: String::new(),
            size: 0,
            mode: 0,
            deleted: true,
        });
        submit_resolution_result(&ctx, &job.job_id, deleted.clone())
            .await
            .unwrap();

        // A permission-denied marker must NOT be treated as absence: with
        // the job directory unwritable, opening the (absent) destination
        // marker fails EACCES, which maps typed to CandidatePermissionDenied.
        let (_, job2) = prepared_job(&ctx, "src/other.rs").await;
        let mut deleted2 = result_for(&job2, b"");
        deleted2.outcome = ResolutionOutcome::CandidateReady;
        deleted2.candidate = Some(CandidateDescriptor {
            path: job2.candidate_destination.path.clone(),
            hash: String::new(),
            size: 0,
            mode: 0,
            deleted: true,
        });
        let job_dir = state_root
            .join("orchestrator/resolution/jobs")
            .join(&job2.job_id);
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let error = submit_resolution_result(&ctx, &job2.job_id, deleted2)
            .await
            .unwrap_err();
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            error
                .downcast_ref::<StalePublication>()
                .map(|stale| stale.kind),
            Some(ResolutionStaleKind::CandidatePermissionDenied)
        );
    }

    /// Permission-denied, invalid-type, and I/O candidate failures map to
    /// the NEW typed stale kinds (no error-text matching).
    #[cfg(unix)]
    #[tokio::test]
    async fn candidate_open_failures_are_typed_stale_kinds() {
        use std::os::unix::fs::PermissionsExt as _;

        let h = setup().await;
        let ctx = h.ctx();
        let state_root = ctx.state_dir().unwrap();

        // Permission denied on the job directory.
        let (_head, job) = prepared_job(&ctx, PATH).await;
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        let job_dir = state_root
            .join("orchestrator/resolution/jobs")
            .join(&job.job_id);
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            stale_kind(&outcome),
            ResolutionStaleKind::CandidatePermissionDenied
        );
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;

        // Not a regular file: the candidate path is a directory.
        let (_head, job) = prepared_job(&ctx, "src/other.rs").await;
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        let abs = state_root.join(&job.candidate_destination.path);
        std::fs::remove_file(&abs).unwrap();
        std::fs::create_dir(&abs).unwrap();
        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(
            stale_kind(&outcome),
            ResolutionStaleKind::CandidateInvalidType
        );
        conflict_survives(&ctx, &head_of(&ctx).await, "src/other.rs").await;

        // Other I/O: the shared candidate reader must classify ANY non-
        // missing/permission/symlink/type open failure as CandidateIoError.
        // A path component beyond the portable bound is refused by the
        // no-follow open machinery without text matching (overlong
        // components can never come from a validated descriptor, so this
        // exercises the mapping directly at the shared reader).
        let (_head, job) = prepared_job(&ctx, "src/socket.rs").await;
        write_candidate(&job, &state_root, CANDIDATE);
        let descriptor = result_for(&job, CANDIDATE).candidate.unwrap();
        let overlong = format!(
            "orchestrator/resolution/jobs/{}/candidate-0.bin",
            "a".repeat(512)
        );
        let error = crate::snapshot::read_candidate_file(&ctx, &overlong, &descriptor)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<StalePublication>()
                .map(|stale| stale.kind),
            Some(ResolutionStaleKind::CandidateIoError),
            "{error}"
        );

        // The same job still resolves normally through the real path.
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert!(matches!(outcome, ResolutionApplyOutcome::Published { .. }));
    }

    /// A hard-linked alias of the immutable candidate is rejected: the
    /// candidate must stay singly-linked so no second name can mutate it.
    #[cfg(unix)]
    #[tokio::test]
    async fn hard_link_aliasing_is_rejected() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        let abs = state_root.join(&job.candidate_destination.path);
        let alias = abs.with_extension("alias");
        std::fs::hard_link(&abs, &alias).unwrap();

        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert_eq!(stale_kind(&outcome), ResolutionStaleKind::CandidateIoError);
        conflict_survives(&ctx, &head_of(&ctx).await, PATH).await;
    }

    /// The store record is only visible after the immutable job.json is
    /// durable: a fault before the store write leaves NO record (and no
    /// projection can show a job without its durable file).
    #[tokio::test]
    async fn store_visible_only_after_job_json_durable() {
        let h = setup().await;
        let ctx = h.ctx();
        upload_legs(&ctx).await;
        let edit = edit_edit(PATH);
        let head = publish_conflict_head(&ctx, &edit).await;
        register_conflict(&ctx, &head, &edit).await;
        seed_accepted(
            &ctx,
            "parser-impl",
            vec![seed_proposal("agent-a", PATH, &hex64(b'a'), None)],
        )
        .await;

        // Pre-initialize the store file so the fault below hits ONLY the
        // record write (not the store's first-open default initialization).
        let store = ResolutionStore::open(ctx.base).unwrap();
        store.load().unwrap();

        set_atomic_faults(AtomicFaults {
            fail_before_commit: true,
            fail_after_commit: false,
        });
        let error = prepare_resolution_job(
            &ctx,
            PATH,
            PreventionReason::Exhausted {
                detail: "no bounded prevention path remains".to_string(),
            },
        )
        .await
        .unwrap_err();
        set_atomic_faults(AtomicFaults::default());
        assert!(error.to_string().contains("injected pre-commit fault"));

        // The durable job.json was written BEFORE the store write failed.
        let jobs_dir = ctx
            .state_dir()
            .unwrap()
            .join("orchestrator/resolution/jobs");
        let orphans: Vec<_> = std::fs::read_dir(&jobs_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].path().join(RESOLUTION_JOB_FILE).is_file());

        // The projection never shows a job without its durable immutable
        // file: no store record exists.
        let store = ResolutionStore::open(ctx.base).unwrap();
        assert!(store.load().unwrap().jobs.is_empty());
    }

    /// Duplicate preparation for the same conflict fingerprint is refused
    /// (typed), while a TERMINAL job for the fingerprint allows a fresh
    /// preparation.
    #[tokio::test]
    async fn duplicate_prepare_same_fingerprint_refused_and_terminal_allows_retry() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;

        // Second preparation for the same fingerprint is refused typed (the
        // designation gate refuses non-terminal assignments for the path).
        let error = prepare_resolution_job(
            &ctx,
            PATH,
            PreventionReason::Exhausted {
                detail: "no bounded prevention path remains".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::integrator::DesignationRefusal>()
                .is_some()
                || error
                    .downcast_ref::<ResolutionOpError>()
                    .is_some_and(|op| matches!(op, ResolutionOpError::DuplicateFingerprintJob(_))),
            "expected a typed duplicate-preparation refusal, got {error}"
        );
        let store = ResolutionStore::open(ctx.base).unwrap();
        assert_eq!(store.load().unwrap().jobs.len(), 1);

        // A terminal job for the fingerprint allows a fresh preparation.
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        let outcome = apply_resolution_job(&ctx, &job.job_id).await.unwrap();
        assert!(matches!(outcome, ResolutionApplyOutcome::Published { .. }));
        assert_eq!(
            store.load_job(&job.job_id).unwrap().assignment_state,
            ResolutionAssignmentState::Completed
        );

        // Re-create the conflict and prepare again: the completed job is
        // terminal, so a new active job for the fingerprint is allowed.
        upload_legs(&ctx).await;
        let edit = edit_edit(PATH);
        let new_head = publish_conflict_head(&ctx, &edit).await;
        register_conflict(&ctx, &new_head, &edit).await;
        let job2 = prepare_resolution_job(
            &ctx,
            PATH,
            PreventionReason::Exhausted {
                detail: "no bounded prevention path remains".to_string(),
            },
        )
        .await
        .unwrap();
        // The fingerprint's automatic block binds a fresh assignment id, so
        // the two jobs' fingerprints differ by design; the path and conflict
        // legs are identical.
        assert_eq!(job2.conflict.path, job.conflict.path);
        assert_ne!(job2.job_id, job.job_id);
        assert_ne!(job2.conflict_fingerprint, job.conflict_fingerprint);
        let store = ResolutionStore::open(ctx.base).unwrap();
        assert_eq!(store.load().unwrap().jobs.len(), 2);
        assert_eq!(
            store.load_job(&job.job_id).unwrap().assignment_state,
            ResolutionAssignmentState::Completed
        );
        assert_eq!(
            store.load_job(&job2.job_id).unwrap().assignment_state,
            ResolutionAssignmentState::Active
        );
        let _ = head;
    }

    /// trim evicts ONLY terminal records: Active and PublicationUncertain
    /// records always survive, and the store stays bounded.
    #[tokio::test]
    async fn trim_preserves_non_terminal_records() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let store = ResolutionStore::open(ctx.base).unwrap();
        let base = store.load_job(&job.job_id).unwrap();

        let mut terminal = Vec::new();
        for index in 0..66u8 {
            let mut record = base.clone();
            record.job.job_id = format!("{index:032x}");
            record.assignment_state = ResolutionAssignmentState::Completed;
            record.created_at_ms = 2 + i64::from(index);
            terminal.push(record);
        }
        let mut uncertain = base.clone();
        uncertain.job.job_id = hex64(b'f')[..32].to_string();
        uncertain.assignment_state = ResolutionAssignmentState::PublicationUncertain;
        uncertain.created_at_ms = 1000;

        store
            .update(|state| {
                state.jobs.push(uncertain);
                state.jobs.append(&mut terminal);
                Ok(())
            })
            .unwrap();

        let trimmed = store.load().unwrap();
        assert_eq!(trimmed.jobs.len(), RESOLUTION_MAX_JOBS);
        // Both non-terminal records survive.
        assert!(trimmed.jobs.iter().any(|record| {
            record.assignment_state == ResolutionAssignmentState::PublicationUncertain
        }));
        assert!(trimmed
            .jobs
            .iter()
            .any(|record| record.assignment_state == ResolutionAssignmentState::Active));
        // The OLDEST terminal records were evicted; the newest survive.
        assert!(!trimmed.jobs.iter().any(|record| record.created_at_ms == 2));
        assert!(trimmed.jobs.iter().any(|record| record.created_at_ms == 67));
    }

    /// Concurrent submit + revoke + apply have exactly one winner.
    /// `tokio::join!` polls in declaration order, so both interleavings are
    /// deterministic: apply-first publishes (and revoke fails on the sync
    /// lock), revoke-first leaves the assignment revoked and apply returns
    /// the typed stale outcome.
    #[tokio::test]
    async fn concurrent_submit_revoke_apply_single_winner() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        // Apply wins: it publishes exactly once; the concurrent revoke fails
        // (sync-lock contention or a terminal state transition).
        let (apply, revoke) = tokio::join!(
            apply_resolution_job(&ctx, &job.job_id),
            revoke_resolution_assignment(&ctx, &job.job_id, false)
        );
        assert!(
            matches!(apply, Ok(ResolutionApplyOutcome::Published { .. })),
            "apply must win when polled first, got {apply:?}"
        );
        assert!(
            revoke.is_err(),
            "revoke must lose when apply is polled first"
        );
        assert_eq!(
            ResolutionStore::open(ctx.base)
                .unwrap()
                .load_job(&job.job_id)
                .unwrap()
                .assignment_state,
            ResolutionAssignmentState::Completed
        );
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_none());
        let history = ctx.db.list_conflict_resolutions().await.unwrap();
        assert_eq!(history.len(), 1);
        assert_ne!(head_of(&ctx).await, head);

        // Revoke wins (deterministic outcome: revoke before apply → apply
        // stale): the conflict survives and apply reports the typed stale
        // assignment-revoked outcome.
        let (_, job) = prepared_job(&ctx, PATH).await;
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();
        let (revoke, apply) = tokio::join!(
            revoke_resolution_assignment(&ctx, &job.job_id, true),
            apply_resolution_job(&ctx, &job.job_id)
        );
        assert!(
            revoke.is_ok(),
            "revoke must win when polled first: {revoke:?}"
        );
        assert!(matches!(
            apply,
            Ok(ResolutionApplyOutcome::Stale {
                kind: ResolutionStaleKind::AssignmentRevoked,
                ..
            })
        ));
        assert_eq!(
            ResolutionStore::open(ctx.base)
                .unwrap()
                .load_job(&job.job_id)
                .unwrap()
                .assignment_state,
            ResolutionAssignmentState::Superseded
        );
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
    }

    /// A simulated post-CAS crash at EVERY bookkeeping boundary converges to
    /// Completed on the next load, with the conflict resolved exactly once.
    #[tokio::test]
    async fn post_cas_crash_boundaries_converge_to_completed_once() {
        let h = setup().await;
        let ctx = h.ctx();
        let state_root = ctx.state_dir().unwrap();
        for point in [
            TestPublishCrashPoint::AfterCas,
            TestPublishCrashPoint::AfterHistory,
            TestPublishCrashPoint::AfterRegistry,
            TestPublishCrashPoint::AfterCompleted,
        ] {
            let (head, job) = prepared_job(&ctx, PATH).await;
            // Distinct candidate bytes per iteration so the idempotent
            // history once-check can never confuse one resolution with an
            // earlier iteration's (each job resolves exactly once).
            let candidate = format!("reconciled-{point:?}").into_bytes();
            write_candidate(&job, &state_root, &candidate);
            submit_resolution_result(&ctx, &job.job_id, result_for(&job, &candidate))
                .await
                .unwrap();
            let history_before = ctx.db.list_conflict_resolutions().await.unwrap().len();

            inject_publish_crash(point);
            let error = apply_resolution_job(&ctx, &job.job_id).await.unwrap_err();
            clear_publish_crash();
            assert!(
                error.to_string().contains("simulated crash"),
                "{point:?}: expected simulated crash, got {error}"
            );
            // The CAS had already won: the head moved and nothing was
            // double-applied.
            assert_ne!(head_of(&ctx).await, head);

            let recovered = recover_uncertain_publications(&ctx).await.unwrap();
            if point != TestPublishCrashPoint::AfterCompleted {
                // After Completed the record is already terminal; recovery
                // only sweeps the job directory and journal.
                assert!(recovered >= 1, "{point:?}");
            }

            let store = ResolutionStore::open(ctx.base).unwrap();
            assert_eq!(
                store.load_job(&job.job_id).unwrap().assignment_state,
                ResolutionAssignmentState::Completed,
                "{point:?}"
            );
            // Conflict resolved exactly once (registry gone, one history
            // record, head holds the resolution).
            assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_none());
            let history = ctx.db.list_conflict_resolutions().await.unwrap();
            assert_eq!(history.len(), history_before + 1, "{point:?}");
            let state = SnapshotEngine::new(&ctx)
                .load_state(&head_of(&ctx).await)
                .await
                .unwrap();
            assert!(state.conflicts.iter().all(|conflict| conflict.path != PATH));
            // The job directory + journal were cleaned after Completed.
            assert!(!state_root
                .join("orchestrator/resolution/jobs")
                .join(&job.job_id)
                .exists());
        }
    }

    /// A simulated crash BEFORE the CAS fails closed on recovery: nothing was
    /// published, the record becomes terminal Stale, the conflict survives,
    /// and the journal is dropped.
    #[tokio::test]
    async fn crash_before_cas_fails_closed_on_recovery() {
        let h = setup().await;
        let ctx = h.ctx();
        let (head, job) = prepared_job(&ctx, PATH).await;
        let state_root = ctx.state_dir().unwrap();
        write_candidate(&job, &state_root, CANDIDATE);
        submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
            .await
            .unwrap();

        inject_publish_crash(TestPublishCrashPoint::BeforeCas);
        let error = apply_resolution_job(&ctx, &job.job_id).await.unwrap_err();
        clear_publish_crash();
        assert!(error.to_string().contains("simulated crash"));

        // The CAS never ran: head unchanged, journal + uncertain state in
        // place, conflict intact.
        assert_eq!(head_of(&ctx).await, head);
        let store = ResolutionStore::open(ctx.base).unwrap();
        assert_eq!(
            store.load_job(&job.job_id).unwrap().assignment_state,
            ResolutionAssignmentState::PublicationUncertain
        );
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
        let journal = state_root
            .join("orchestrator/resolution/jobs")
            .join(&job.job_id)
            .join(PUBLICATION_PENDING_JOURNAL_FILE);
        assert!(journal.is_file());

        // Recovery fails closed: terminal Stale, journal dropped, conflict
        // preserved for manual action.
        let recovered = recover_uncertain_publications(&ctx).await.unwrap();
        assert!(recovered >= 1);
        let record = store.load_job(&job.job_id).unwrap();
        assert_eq!(record.assignment_state, ResolutionAssignmentState::Stale);
        assert!(!journal.exists());
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());
        assert_eq!(head_of(&ctx).await, head);
    }

    /// Human answer matrix: stale generation, wrong fingerprint, wrong
    /// assignment, and duplicate answers are rejected typed; defer preserves
    /// the conflict; submit_candidate routes through the same candidate
    /// validation and a guarded publication.
    #[tokio::test]
    async fn human_answer_matrix() {
        let h = setup().await;
        let ctx = h.ctx();
        let state_root = ctx.state_dir().unwrap();

        let (_head, job) = prepared_job(&ctx, PATH).await;
        submit_resolution_result(&ctx, &job.job_id, requires_human_result_for(&job))
            .await
            .unwrap();
        let store = ResolutionStore::open(ctx.base).unwrap();
        let record = store.load_job(&job.job_id).unwrap();
        assert_eq!(record.question_generation, 1);
        assert_eq!(record.result.as_ref().unwrap().question_generation, 1);
        assert!(record
            .result
            .as_ref()
            .unwrap()
            .safe_options
            .contains(&HumanResolutionOption::SubmitCandidate));

        // Stale generation.
        let error = answer_resolution(&ctx, answer_for(&job, 0, HumanResolutionOption::Defer))
            .await
            .unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::StaleQuestionGeneration { .. }
        ));
        // Wrong fingerprint.
        let mut answer = answer_for(&job, 1, HumanResolutionOption::Defer);
        answer.conflict_fingerprint = hex64(b'9');
        let error = answer_resolution(&ctx, answer).await.unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::AnswerBindingMismatch { .. }
        ));
        // Wrong assignment.
        let mut answer = answer_for(&job, 1, HumanResolutionOption::Defer);
        answer.assignment_id = hex64(b'8')[..32].to_string();
        let error = answer_resolution(&ctx, answer).await.unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::AnswerBindingMismatch { .. }
        ));

        // Defer records a terminal state WITHOUT publication; the conflict
        // and head survive unchanged.
        let head = head_of(&ctx).await;
        answer_resolution(&ctx, answer_for(&job, 1, HumanResolutionOption::Defer))
            .await
            .unwrap();
        assert_eq!(
            store.load_job(&job.job_id).unwrap().assignment_state,
            ResolutionAssignmentState::Deferred
        );
        assert_eq!(head_of(&ctx).await, head);
        assert!(ctx.db.get_conflict_record(PATH).await.unwrap().is_some());

        // Duplicate answers are rejected typed (the assignment is terminal).
        let error = answer_resolution(&ctx, answer_for(&job, 1, HumanResolutionOption::Defer))
            .await
            .unwrap_err();
        assert!(matches!(
            op_error(&error),
            ResolutionOpError::JobNotActive { .. }
        ));

        // defer_resolution records the Deferred terminal state too.
        let (_head, job2) = prepared_job(&ctx, "src/other.rs").await;
        submit_resolution_result(&ctx, &job2.job_id, requires_human_result_for(&job2))
            .await
            .unwrap();
        defer_resolution(&ctx, &job2.job_id).await.unwrap();
        assert_eq!(
            store.load_job(&job2.job_id).unwrap().assignment_state,
            ResolutionAssignmentState::Deferred
        );
        assert!(ctx
            .db
            .get_conflict_record("src/other.rs")
            .await
            .unwrap()
            .is_some());

        // submit_candidate goes through the SAME candidate validation as
        // submit (put_candidate is allowed in the submitted-without-
        // candidate state) and a guarded publication publishes the bytes.
        let (_head, job3) = prepared_job(&ctx, "src/human.rs").await;
        submit_resolution_result(&ctx, &job3.job_id, requires_human_result_for(&job3))
            .await
            .unwrap();
        let descriptor = put_resolution_candidate(&ctx, &job3.job_id, CANDIDATE)
            .await
            .unwrap();
        let mut answer = answer_for(&job3, 1, HumanResolutionOption::SubmitCandidate);
        answer.candidate = Some(descriptor.clone());
        answer.verification = Some(VerificationSummary {
            status: VerificationStatus::Passed,
            summary: "human verified".to_string(),
            ..VerificationSummary::default()
        });
        answer_resolution(&ctx, answer).await.unwrap();
        let record = store.load_job(&job3.job_id).unwrap();
        assert_eq!(
            record.result.as_ref().unwrap().outcome,
            ResolutionOutcome::CandidateReady
        );
        assert_eq!(
            record
                .result
                .as_ref()
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .hash,
            hash_bytes(CANDIDATE)
        );
        let outcome = apply_resolution_job(&ctx, &job3.job_id).await.unwrap();
        let ResolutionApplyOutcome::Published { head: new_head } = outcome else {
            panic!("expected published, got {outcome:?}");
        };
        let state = SnapshotEngine::new(&ctx)
            .load_state(&new_head)
            .await
            .unwrap();
        assert!(state
            .conflicts
            .iter()
            .all(|conflict| conflict.path != "src/human.rs"));
        assert_eq!(state.files["src/human.rs"].size, CANDIDATE.len() as u64);
        let _ = state_root;
    }

    /// Load revalidation fails closed: a stored record that no longer
    /// validates poisons every subsequent load.
    #[tokio::test]
    async fn store_load_revalidation_fails_closed() {
        let h = setup().await;
        let ctx = h.ctx();
        let (_head, job) = prepared_job(&ctx, PATH).await;
        let store = ResolutionStore::open(ctx.base).unwrap();
        let mut record = store.load_job(&job.job_id).unwrap();

        // A blocked result must not carry a candidate; push this invalid
        // record through update, whose load() revalidation refuses it.
        let mut invalid = result_for(&job, CANDIDATE);
        invalid.outcome = ResolutionOutcome::Blocked;
        record.result = Some(invalid);
        let error = store
            .update(|state| {
                state.jobs.push(record);
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        // And the corrupt state fails closed on every subsequent load.
        let error = store.load().unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let error = store.load_job(&job.job_id).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
    }
}
