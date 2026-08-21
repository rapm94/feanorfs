//! Public runner configuration, status, and admission types.

pub(super) use crate::agent::scope::{
    message_fingerprint, validate_accepted_work, AcceptedWorkDescriptor, RunnerAdmissionReject,
};
use crate::paths::validate_name;
use anyhow::{ensure, Result};
use feanorfs_common::{AgentMessage, AgentMessageKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::ownership::RunnerIdentity;
use super::session::RunnerExecutionSession;
use super::store::{validate_message, validate_session_id, RunnerStore};

const RUNNER_INVOCATION_SCHEMA_VERSION: u32 = 2;
/// Trusted runner configuration. Unlike [`RunnerStatus`], this exposes argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub agent: String,
    pub program: PathBuf,
    pub fixed_args: Vec<String>,
    pub timeout_secs: u64,
    pub configured_at_ms: i64,
    /// Accepted-work enforcement level. Pre-enforcement state migrates to
    /// [`RunnerScopeMode::LegacyUnenforced`]; enforced runners never claim
    /// accepted scope they did not verify.
    #[serde(default)]
    pub scope_mode: RunnerScopeMode,
}

/// Accepted-work enforcement level for one configured runner.
///
/// The level is persisted with the runner configuration (schema 3) and is
/// never inferred from message bodies or other state: a legacy runner stays
/// explicitly `legacy_unenforced` and can never masquerade as enforced
/// coordination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum RunnerScopeMode {
    /// Pre-enforcement configuration. Admission is not gated and automatic
    /// land remains all-path. Never reports accepted scope.
    #[default]
    /// Pre-enforcement default: never silently claim accepted scope.
    LegacyUnenforced,
    /// Acceptance is computed and reported through status (advisory) but
    /// never gates launch or land.
    Advisory,
    /// Launch requires an accepted intent bound to the request, and every
    /// automatic land applies the accepted scope guard.
    Enforced,
}

impl RunnerScopeMode {
    /// Whether this level gates launch and land.
    #[must_use]
    pub const fn is_enforced(self) -> bool {
        matches!(self, Self::Enforced)
    }

    /// Whether this level participates in work projection at all.
    #[must_use]
    pub const fn participates(self) -> bool {
        matches!(self, Self::Advisory | Self::Enforced)
    }

    /// Stable wire string for this level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyUnenforced => "legacy_unenforced",
            Self::Advisory => "advisory",
            Self::Enforced => "enforced",
        }
    }
}

/// Publish lifecycle of the one runner scope-change request record.
///
/// The dedup record is persisted **before** the request message is sent
/// (crash-durable), so a restart between persist and send must never
/// republish. `PublishPending` marks the crash window; a restart that finds
/// it flips the record to `AwaitingConfirmation` and stops before any new
/// request, because the send outcome is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ScopeChangePublishState {
    /// Persisted before the request was sent; the send has not completed.
    #[default]
    PublishPending,
    /// The send succeeded and the record carries the returned message id.
    Confirmed,
    /// A restart found a publish-pending record: the send outcome is
    /// unknown; nothing may be republished or begun.
    AwaitingConfirmation,
}

impl ScopeChangePublishState {
    /// Stable wire string for this publish state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PublishPending => "publish_pending",
            Self::Confirmed => "confirmed",
            Self::AwaitingConfirmation => "awaiting_confirmation",
        }
    }
}

/// Bounded read-only key of the one persisted scope-change request record.
/// The pre-publish gate compares the full tuple (task, intent, fingerprint)
/// — never the fingerprint alone — and fails closed on a publish-pending
/// record whose send outcome is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeChangeRequestKey {
    pub task_id: String,
    pub intent_message_id: String,
    /// Bounded fingerprint of the out-of-scope scope/operation set already
    /// reported; republishing is skipped while it is unchanged.
    pub paths_fingerprint: String,
    /// Canonical scope component (paths + concerns) at publish time; the
    /// admission gate releases the scope-amendment wait only when it
    /// changes. Empty for legacy records.
    pub scope_fingerprint: String,
    pub publish_state: ScopeChangePublishState,
}

/// Selects which lifecycle is allowed to consume the next queued request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerExecutionMode {
    /// A persisted, enabled runner owned by the background supervisor.
    Supervised,
    /// An explicit terminal-owned run while background execution is disabled.
    Foreground,
}

/// Bounded typed reason a runner is waiting on accepted work before the next
/// launch (admission) or before the next automatic publication (scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerWorkWaitKind {
    /// Waiting for an accepted intent for the next queued request.
    WaitingAcceptance,
    /// A dependency task is not settled/completed.
    DependencyBlocked,
    /// The reducer projection is incomplete (cursor reset); acceptance
    /// cannot be proven.
    ProjectionIncomplete,
    /// Out-of-scope local work found; a scope amendment was requested.
    ScopeAmendmentRequested,
}

impl RunnerWorkWaitKind {
    /// Stable wire string for this wait kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WaitingAcceptance => "waiting_acceptance",
            Self::DependencyBlocked => "dependency_blocked",
            Self::ProjectionIncomplete => "projection_incomplete",
            Self::ScopeAmendmentRequested => "scope_amendment_requested",
        }
    }
}

/// Bounded, typed waiting state for one runner request. Deliberately not a
/// [`RunnerAttention`] reason: waiting keeps the runner live and re-attempts
/// on head change, while attention stops the runner until explicit action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerWorkWait {
    pub kind: RunnerWorkWaitKind,
    /// Request message id the wait applies to (the next pending request).
    pub message_id: String,
    /// Typed admission rejection, when this wait came from admission.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<RunnerAdmissionReject>,
    /// Bounded count of out-of-scope local paths (constant-cost projection;
    /// full paths are fetched on explicit action).
    pub out_of_scope_count: u32,
    pub observed_at_ms: i64,
}

/// Stable, non-secret runner phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPhase {
    Idle,
    Launching,
    Running,
    NeedsAttention,
}

/// Categorical fail-closed reason; bodies and process output are never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerAttention {
    CursorReset,
    PendingOverflow,
    AmbiguousExecution,
    DeliveryUnknown,
    PreparationFailed,
}

/// Public, redacted runner status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerStatus {
    pub configured: bool,
    pub enabled: bool,
    pub agent: String,
    pub phase: RunnerPhase,
    pub pending_count: usize,
    pub active_message_id: Option<String>,
    pub active_session_id: Option<String>,
    pub active_started_at_ms: Option<i64>,
    pub active_spawned_at_ms: Option<i64>,
    pub last_terminal_kind: Option<AgentMessageKind>,
    pub last_terminal_message_id: Option<String>,
    pub attention: Option<RunnerAttention>,
    pub scope_mode: RunnerScopeMode,
    /// Bounded typed waiting state (admission or scope amendment). Never a
    /// reason to stop; the runner re-attempts on the next head change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_wait: Option<RunnerWorkWait>,
    pub updated_at_ms: i64,
    pub inbox_failure_count: u32,
}

/// Read-only identity of the configured process group for an active request.
///
/// This is intentionally separate from [`RunnerStatus`]: process metadata is
/// used only for exact local orphan cleanup and is never part of public status
/// output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerProcessMetadata {
    pub pid: u32,
    pub process_start_id: String,
}

/// Result of admitting one inbox read without retaining any message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerAdmission {
    pub admitted: usize,
    pub ignored: usize,
    pub cursor_advanced: bool,
    pub needs_attention: bool,
}

/// Persisted pre-spawn checkpoint returned to the process launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerLaunch {
    pub message_id: String,
    pub session_id: String,
    pub reply_cursor: String,
    /// Bound accepted work for enforced launches; `None` for legacy/advisory
    /// launches and never silently claimed otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_work: Option<AcceptedWorkDescriptor>,
}

/// Bounded stdin document for the configured child. This type is never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInvocation {
    pub schema_version: u32,
    pub session_id: String,
    pub agent: String,
    pub message: AgentMessage,
    /// Bound accepted work identity for the executed request; `None` for
    /// legacy/advisory executions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_work: Option<AcceptedWorkDescriptor>,
}

impl RunnerInvocation {
    pub fn new(launch: &RunnerLaunch, agent: &str, message: AgentMessage) -> Result<Self> {
        validate_name(agent)?;
        validate_session_id(&launch.session_id)?;
        validate_message(&message)?;
        ensure!(
            message.message_id == launch.message_id,
            "runner invocation message does not match the active checkpoint"
        );
        ensure!(
            message.to == agent && message.kind == AgentMessageKind::Request,
            "runner invocation must contain a direct request for the configured agent"
        );
        if let Some(accepted_work) = &launch.accepted_work {
            validate_accepted_work(accepted_work, agent, &launch.message_id)?;
            ensure!(
                accepted_work.message_fingerprint == message_fingerprint(&message),
                "runner invocation accepted work does not match the request message"
            );
        }
        Ok(Self {
            schema_version: RUNNER_INVOCATION_SCHEMA_VERSION,
            session_id: launch.session_id.clone(),
            agent: agent.to_string(),
            message,
            accepted_work: launch.accepted_work.clone(),
        })
    }
}
/// Owned, secret-free identity token for the exact configured runner lease.
///
/// The continuous controller task cannot borrow the worker's
/// [`RunnerExecutionSession`] (it is not `'static`), so it carries this
/// token and revalidates it against the durable store before each mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerOwnership {
    identity: RunnerIdentity,
}

impl RunnerOwnership {
    /// Captures the exact identity bound by the caller's execution session.
    pub fn from_session(session: &RunnerExecutionSession<'_>) -> Self {
        Self {
            identity: session.store.identity.clone(),
        }
    }

    /// Fail-closed revalidation against the current configured runner.
    pub fn verify(&self, base: &Path, agent: &str) -> Result<()> {
        ensure!(
            self.identity.agent == agent,
            "runner ownership token does not own agent '{agent}'"
        );
        let store = RunnerStore::open_configured(base)?;
        ensure!(
            self.identity == store.identity,
            "runner ownership token does not match the configured runner"
        );
        Ok(())
    }

    /// Scope mode of the configured runner this token owns.
    pub fn scope_mode(&self, base: &Path, agent: &str) -> Result<RunnerScopeMode> {
        self.verify(base, agent)?;
        Ok(RunnerStore::open_configured(base)?.config()?.scope_mode)
    }

    /// Accepted-work descriptor bound to the active request, when the
    /// configured runner is enforced and executing accepted work.
    pub fn active_accepted_work(
        &self,
        base: &Path,
        agent: &str,
    ) -> Result<Option<AcceptedWorkDescriptor>> {
        self.verify(base, agent)?;
        RunnerStore::open_configured(base)?.active_accepted_work()
    }

    /// Records a typed scope/amendment wait against the runner this token
    /// owns. Fail-closed: the wait applies only when the token still matches
    /// the configured runner.
    pub fn record_work_wait(&self, base: &Path, agent: &str, wait: &RunnerWorkWait) -> Result<()> {
        self.verify(base, agent)?;
        let store = RunnerStore::open_configured(base)?;
        store.record_work_wait_locked(RunnerExecutionMode::Supervised, wait)?;
        Ok(())
    }

    /// Publishes the deduplicated scope-change request record when the
    /// out-of-scope path set changed since the last publication. Returns
    /// `true` when a new record was persisted.
    pub fn record_scope_change_request(
        &self,
        base: &Path,
        agent: &str,
        message_id: &str,
        task_id: &str,
        intent_message_id: &str,
        paths_fingerprint: &str,
    ) -> Result<bool> {
        self.verify(base, agent)?;
        let store = RunnerStore::open_configured(base)?;
        store.record_scope_change_request_locked(
            message_id,
            task_id,
            intent_message_id,
            paths_fingerprint,
        )
    }

    /// Current persisted scope-change path fingerprint, when one was
    /// published. Used to deduplicate before sending.
    pub fn scope_change_fingerprint(&self, base: &Path, agent: &str) -> Result<Option<String>> {
        self.verify(base, agent)?;
        let store = RunnerStore::open_configured(base)?;
        let state = store.load()?;
        Ok(state
            .runtime
            .scope_change_request
            .map(|record| record.paths_fingerprint))
    }

    /// Full dedup tuple (task, intent, fingerprint) plus publish lifecycle of
    /// the one persisted scope-change request record. The pre-publish gate
    /// compares this whole tuple — never the fingerprint alone — and fails
    /// closed when the record is publish-pending.
    pub fn scope_change_request_key(
        &self,
        base: &Path,
        agent: &str,
    ) -> Result<Option<ScopeChangeRequestKey>> {
        self.verify(base, agent)?;
        RunnerStore::open_configured(base)?.scope_change_request_key_locked()
    }

    /// Persists the publish-pending scope-change request record (tuple +
    /// fingerprint, **no** message id yet) BEFORE the request message is
    /// sent. Returns `true` only when the (task, intent, fingerprint) tuple
    /// changed; identical tuples are not re-begun.
    pub fn begin_scope_change_request(
        &self,
        base: &Path,
        agent: &str,
        task_id: &str,
        intent_message_id: &str,
        paths_fingerprint: &str,
        scope_fingerprint: &str,
    ) -> Result<bool> {
        self.verify(base, agent)?;
        RunnerStore::open_configured(base)?.begin_scope_change_request_locked(
            task_id,
            intent_message_id,
            paths_fingerprint,
            scope_fingerprint,
        )
    }

    /// Fills the returned message id into the publish-pending record after a
    /// successful send. Fails when no record exists or it is already
    /// confirmed.
    pub fn complete_scope_change_request(
        &self,
        base: &Path,
        agent: &str,
        message_id: &str,
    ) -> Result<()> {
        self.verify(base, agent)?;
        RunnerStore::open_configured(base)?.complete_scope_change_request_locked(message_id)
    }

    /// Marks a publish-pending record awaiting confirmation after a restart:
    /// the send outcome is unknown; nothing may be republished or begun.
    pub fn mark_scope_change_awaiting_confirmation(&self, base: &Path, agent: &str) -> Result<()> {
        self.verify(base, agent)?;
        RunnerStore::open_configured(base)?.mark_scope_change_awaiting_confirmation_locked()
    }

    /// Clears the scope-amendment wait so admission re-resolves the pending
    /// request after an amendment changed the accepted scope.
    pub fn clear_scope_amendment_wait(&self, base: &Path, agent: &str) -> Result<()> {
        self.verify(base, agent)?;
        RunnerStore::open_configured(base)?.clear_scope_amendment_wait_locked()
    }

    /// Abandons a scope-change request whose send failed (clears the record
    /// and its wait) so the next cycle retries from a clean state.
    pub fn abandon_scope_change_request(&self, base: &Path, agent: &str) -> Result<()> {
        self.verify(base, agent)?;
        RunnerStore::open_configured(base)?.abandon_scope_change_request_locked()
    }
}
