//! Execution session and terminal-state transitions.

use crate::agent::continuous::conflict_failure;
use anyhow::{ensure, Context, Result};
use feanorfs_common::{
    is_valid_hash, AgentInboxResult, AgentMessage, AgentMessageKind, AGENT_INBOX_MAX_LIMIT,
};
use std::path::Path;

use super::contract::{
    AcceptedWorkDescriptor, RunnerAdmission, RunnerAttention, RunnerExecutionMode, RunnerLaunch,
    RunnerStatus, RunnerWorkWait,
};
use super::ownership::RunnerLifetimeLock;
use super::store::{
    now_ms, touch_completed, validate_execution_mode, validate_message, validate_session_id,
    ActivePhase, LastTerminal, RunnerStore, MAX_FAILURES, MAX_PROCESS_START_ID_BYTES,
};
/// Owns the exact configured runner lease for one launcher lifecycle.
#[derive(Debug)]
#[must_use = "dropping the execution session releases the runner lifetime lease"]
pub struct RunnerExecutionSession<'store> {
    pub(super) store: &'store RunnerStore,
    lease: RunnerLifetimeLock,
    mode: RunnerExecutionMode,
}

impl RunnerStore {
    /// Acquires the exact configuration lease used by every execution-state
    /// transition. Reacquiring after an interrupted active session marks that
    /// request ambiguous before returning.
    pub fn execution_session(
        &self,
        base: &Path,
        mode: RunnerExecutionMode,
    ) -> Result<RunnerExecutionSession<'_>> {
        let lease = RunnerLifetimeLock::try_acquire_exact_store(base, self)?
            .context("this runner store is not the configured agent for the workspace")?;
        let state = self.load()?;
        validate_execution_mode(mode, state.runtime.enabled)?;
        let session = RunnerExecutionSession {
            store: self,
            lease,
            mode,
        };
        session.store.checkpoint_startup_locked()?;
        Ok(session)
    }

    #[cfg(test)]
    pub(super) fn mark_spawned(
        &self,
        message_id: &str,
        pid: u32,
        process_start_id: &str,
    ) -> Result<RunnerStatus> {
        self.mark_spawned_locked(message_id, pid, process_start_id)
    }

    #[cfg(test)]
    pub(super) fn observe_terminals(
        &self,
        request: &AgentMessage,
        messages: &[AgentMessage],
    ) -> Result<Option<RunnerStatus>> {
        self.observe_terminals_locked(request, messages, Some(&request.about_snapshot))
    }

    #[cfg(test)]
    pub(super) fn checkpoint_startup(&self) -> Result<RunnerStatus> {
        self.checkpoint_startup_locked()
    }

    #[cfg(test)]
    pub(super) fn record_delivery_unknown(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<RunnerStatus> {
        self.record_delivery_unknown_locked(message_id, session_id)
    }

    fn mark_spawned_locked(
        &self,
        message_id: &str,
        pid: u32,
        process_start_id: &str,
    ) -> Result<RunnerStatus> {
        ensure!(
            is_valid_hash(message_id),
            "invalid runner request message id"
        );
        ensure!(pid > 0, "runner child pid must be non-zero");
        ensure!(
            !process_start_id.is_empty()
                && process_start_id.len() <= MAX_PROCESS_START_ID_BYTES
                && !process_start_id.chars().any(char::is_control),
            "runner process start identifier is invalid or too large"
        );
        self.update(|state| {
            let active = state
                .runtime
                .active
                .as_mut()
                .context("runner has no active request")?;
            ensure!(
                active.message_id == message_id,
                "active runner request changed"
            );
            ensure!(
                active.phase == ActivePhase::Launching,
                "runner request is not in the launching phase"
            );
            let now = now_ms();
            active.phase = ActivePhase::Running;
            active.pid = Some(pid);
            active.process_start_id = Some(process_start_id.to_string());
            active.spawned_at_ms = Some(now);
            state.runtime.updated_at_ms = now;
            Ok(())
        })?;
        self.status()
    }

    fn observe_terminals_locked(
        &self,
        request: &AgentMessage,
        messages: &[AgentMessage],
        expected_result_snapshot: Option<&str>,
    ) -> Result<Option<RunnerStatus>> {
        validate_message(request)?;
        if let Some(snapshot) = expected_result_snapshot {
            ensure!(
                is_valid_hash(snapshot),
                "runner settled snapshot must be a full snapshot id"
            );
        }
        ensure!(
            messages.len() <= AGENT_INBOX_MAX_LIMIT,
            "runner terminal observation exceeds the inbox bound"
        );
        for message in messages {
            validate_message(message)?;
        }
        let mut completed = false;
        self.update(|state| {
            let active = state
                .runtime
                .active
                .as_ref()
                .context("runner has no active request to correlate a terminal")?;
            ensure!(
                request.message_id == active.message_id,
                "runner terminal observation request does not match the active request"
            );
            ensure!(
                request.to == state.config.agent && request.kind == AgentMessageKind::Request,
                "runner terminal observation requires the active direct request for the configured agent"
            );
            let Some(terminal) = messages.iter().find(|message| {
                message.from == state.config.agent
                    && message.to == request.from
                    && message.reply_to.as_deref() == Some(active.message_id.as_str())
                    && match message.kind {
                        AgentMessageKind::Blocked => true,
                        AgentMessageKind::Result => expected_result_snapshot
                            .is_some_and(|snapshot| message.about_snapshot == snapshot),
                        AgentMessageKind::Request | AgentMessageKind::Status => false,
                    }
            }) else {
                return Ok(());
            };
            let request_id = active.message_id.clone();
            state.runtime.last_terminal = Some(LastTerminal {
                kind: terminal.kind,
                message_id: terminal.message_id.clone(),
                observed_at_ms: now_ms(),
            });
            touch_completed(&mut state.runtime.completed_ids, &request_id);
            state
                .runtime
                .pending
                .retain(|request| request != &request_id);
            state.runtime.active = None;
            state.runtime.work_wait = None;
            state.runtime.scope_change_request = None;
            if state.runtime.pending.is_empty() {
                if let Some(cursor) = state.runtime.staged_cursor.take() {
                    state.runtime.committed_cursor = cursor;
                }
            }
            state.runtime.updated_at_ms = now_ms();
            completed = true;
            Ok(())
        })?;
        if completed {
            Ok(Some(self.status()?))
        } else {
            Ok(None)
        }
    }

    #[cfg(test)]
    pub(super) fn record_inbox_failure(&self) -> Result<RunnerStatus> {
        self.record_inbox_failure_locked()
    }

    fn record_inbox_failure_locked(&self) -> Result<RunnerStatus> {
        self.update(|state| {
            state.runtime.inbox_failure_count = state
                .runtime
                .inbox_failure_count
                .saturating_add(1)
                .min(MAX_FAILURES);
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    #[cfg(test)]
    pub(super) fn record_inbox_recovery(&self) -> Result<RunnerStatus> {
        self.record_inbox_recovery_locked()
    }

    fn record_inbox_recovery_locked(&self) -> Result<RunnerStatus> {
        self.update(|state| {
            state.runtime.inbox_failure_count = 0;
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    /// Stops before launching a child when refresh preparation cannot be
    /// completed locally. The queued IDs and inbox checkpoints remain intact
    /// for explicit operator inspection and discard.
    fn record_preparation_failed_locked(&self, mode: RunnerExecutionMode) -> Result<RunnerStatus> {
        self.update(|state| {
            validate_execution_mode(mode, state.runtime.enabled)?;
            ensure!(
                !state.runtime.pending.is_empty(),
                "runner has no pending request to preserve after refresh preparation failed"
            );
            ensure!(
                state.runtime.active.is_none(),
                "runner has an active request and cannot record a refresh preparation failure"
            );
            if state.runtime.attention.is_some() {
                return Err(conflict_failure("runner already needs attention"));
            }
            state.runtime.attention = Some(RunnerAttention::PreparationFailed);
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    /// Fails closed after any restart that finds a possibly executed request.
    fn checkpoint_startup_locked(&self) -> Result<RunnerStatus> {
        self.update(|state| {
            if let Some(active) = &mut state.runtime.active {
                if matches!(active.phase, ActivePhase::Launching | ActivePhase::Running) {
                    active.phase = ActivePhase::Ambiguous;
                    state.runtime.attention = Some(RunnerAttention::AmbiguousExecution);
                    state.runtime.updated_at_ms = now_ms();
                }
            }
            Ok(())
        })?;
        self.status()
    }

    /// Marks a correlated launcher session ambiguous when its terminal signal
    /// could not be observed or published. The active request remains pinned
    /// in place until an explicit disabled reset discards it.
    fn record_delivery_unknown_locked(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<RunnerStatus> {
        ensure!(
            is_valid_hash(message_id),
            "invalid runner request message id"
        );
        validate_session_id(session_id)?;
        self.update(|state| {
            ensure!(
                matches!(
                    state.runtime.attention,
                    None | Some(RunnerAttention::AmbiguousExecution)
                        | Some(RunnerAttention::DeliveryUnknown)
                ),
                "runner already needs unrelated attention"
            );
            let active = state
                .runtime
                .active
                .as_mut()
                .context("runner has no active request")?;
            ensure!(
                active.message_id == message_id && active.session_id == session_id,
                "runner launcher session does not match the active request"
            );
            active.phase = ActivePhase::Ambiguous;
            state.runtime.attention = Some(RunnerAttention::DeliveryUnknown);
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }
}

impl RunnerExecutionSession<'_> {
    fn validate_lease(&self) -> Result<()> {
        self.lease.ensure_store(self.store)
    }

    pub(crate) fn ensure_matches(&self, base: &Path, agent: &str) -> Result<()> {
        self.validate_lease()?;
        self.lease.ensure_matches(base, agent)
    }

    /// Admits one inbox batch only when this exact execution mode remains
    /// enabled at the admission state update's linearization point.
    pub fn admit_inbox(&self, result: &AgentInboxResult) -> Result<RunnerAdmission> {
        self.validate_lease()?;
        self.store.admit_inbox_locked(self.mode, result)
    }

    /// Records a failed inbox poll without changing task lifecycle state.
    pub fn record_inbox_failure(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.record_inbox_failure_locked()
    }

    /// Clears inbox failure accounting without changing task lifecycle state.
    pub fn record_inbox_recovery(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.record_inbox_recovery_locked()
    }

    /// Records a body-free refresh preparation failure before a child launch.
    /// Only this generation-bound execution session may make the transition.
    pub fn record_preparation_failed(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.record_preparation_failed_locked(self.mode)
    }

    /// Persists the pre-spawn checkpoint while retaining the lifetime lease.
    pub fn begin_next(&self, reply_cursor: &str) -> Result<RunnerLaunch> {
        self.validate_lease()?;
        self.store.begin_next_locked(self.mode, reply_cursor, None)
    }

    /// Persists the pre-spawn checkpoint bound to one accepted-work
    /// descriptor. Enforced runners require a valid descriptor matching the
    /// next pending request; non-enforced runners reject it.
    pub fn begin_next_admitted(
        &self,
        reply_cursor: &str,
        accepted_work: AcceptedWorkDescriptor,
    ) -> Result<RunnerLaunch> {
        self.validate_lease()?;
        self.store
            .begin_next_locked(self.mode, reply_cursor, Some(accepted_work))
    }

    /// Records a typed admission/scope wait for the next pending request.
    pub fn record_work_wait(&self, wait: &RunnerWorkWait) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.record_work_wait_locked(self.mode, wait)
    }

    /// Reads the accepted-work descriptor bound to the active request.
    pub fn active_accepted_work(&self) -> Result<Option<AcceptedWorkDescriptor>> {
        self.validate_lease()?;
        self.store.active_accepted_work_locked()
    }

    /// Records the correlated child process after a successful spawn.
    pub fn mark_spawned(
        &self,
        message_id: &str,
        pid: u32,
        process_start_id: &str,
    ) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store
            .mark_spawned_locked(message_id, pid, process_start_id)
    }

    /// Completes the active direct request only after observing a terminal
    /// signal from the configured agent to its original sender.
    ///
    /// `request` is intentionally supplied in-memory rather than persisted so
    /// runner state remains body-free.
    pub fn observe_terminals(
        &self,
        request: &AgentMessage,
        messages: &[AgentMessage],
    ) -> Result<Option<RunnerStatus>> {
        self.observe_terminals_at_snapshot(request, messages, Some(&request.about_snapshot))
    }

    /// Correlates a result only when it names the exact final settled
    /// snapshot. `None` rejects results while still allowing a correlated
    /// blocked terminal to close the request.
    pub fn observe_terminals_at_snapshot(
        &self,
        request: &AgentMessage,
        messages: &[AgentMessage],
        expected_result_snapshot: Option<&str>,
    ) -> Result<Option<RunnerStatus>> {
        self.validate_lease()?;
        self.store
            .observe_terminals_locked(request, messages, expected_result_snapshot)
    }

    /// Marks an interrupted active launcher lifecycle ambiguous.
    pub fn checkpoint_startup(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.checkpoint_startup_locked()
    }

    /// Pins a launcher session whose terminal delivery could not be established.
    pub fn record_delivery_unknown(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store
            .record_delivery_unknown_locked(message_id, session_id)
    }
}
