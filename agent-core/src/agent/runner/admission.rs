//! Inbox and work-scope admission for one configured runner.

use crate::agent::continuous::conflict_failure;
use anyhow::{ensure, Context as _, Result};
use feanorfs_common::{AgentInboxResult, AgentMessageKind};
use std::collections::HashSet;

use super::contract::{
    AcceptedWorkDescriptor, RunnerAdmission, RunnerAttention, RunnerExecutionMode, RunnerStatus,
    RunnerWorkWait, RunnerWorkWaitKind, ScopeChangePublishState, ScopeChangeRequestKey,
};
use super::store::{
    now_ms, validate_execution_mode, validate_inbox, validate_message_id, RunnerStore,
    ScopeChangeRequestState, MAX_PENDING,
};
#[cfg(test)]
use super::test_hooks::pause_inbox_admission_if_requested;
impl RunnerStore {
    /// Read-only accepted-work descriptor bound to the active request.
    pub fn active_accepted_work(&self) -> Result<Option<AcceptedWorkDescriptor>> {
        self.active_accepted_work_locked()
    }

    #[cfg(test)]
    pub(super) fn admit_inbox(&self, result: &AgentInboxResult) -> Result<RunnerAdmission> {
        let mode = if self.load()?.runtime.enabled {
            RunnerExecutionMode::Supervised
        } else {
            RunnerExecutionMode::Foreground
        };
        self.admit_inbox_locked(mode, result)
    }

    pub(super) fn admit_inbox_locked(
        &self,
        mode: RunnerExecutionMode,
        result: &AgentInboxResult,
    ) -> Result<RunnerAdmission> {
        validate_inbox(result)?;
        #[cfg(test)]
        pause_inbox_admission_if_requested(&self.identity);
        let mut admission = RunnerAdmission {
            admitted: 0,
            ignored: result.messages.len(),
            cursor_advanced: false,
            needs_attention: false,
        };
        self.update(|state| {
            validate_execution_mode(mode, state.runtime.enabled)?;
            if result.cursor_reset {
                state.runtime.attention = Some(RunnerAttention::CursorReset);
                state.runtime.updated_at_ms = now_ms();
                admission.needs_attention = true;
                return Ok(());
            }
            if state.runtime.attention.is_some() {
                return Err(conflict_failure(
                    "runner needs attention before more inbox messages can be admitted",
                ));
            }

            let mut known: HashSet<String> = state.runtime.pending.iter().cloned().collect();
            known.extend(state.runtime.completed_ids.iter().cloned());
            if let Some(active) = &state.runtime.active {
                known.insert(active.message_id.clone());
            }
            let mut accepted = result
                .messages
                .iter()
                .filter(|message| {
                    message.to == state.config.agent
                        && message.kind == AgentMessageKind::Request
                        && known.insert(message.message_id.clone())
                })
                .collect::<Vec<_>>();
            if state.runtime.pending.len().saturating_add(accepted.len()) > MAX_PENDING {
                state.runtime.attention = Some(RunnerAttention::PendingOverflow);
                state.runtime.updated_at_ms = now_ms();
                admission.needs_attention = true;
                return Ok(());
            }

            accepted.sort_by(|left, right| {
                left.created_at_ms
                    .cmp(&right.created_at_ms)
                    .then_with(|| left.message_id.cmp(&right.message_id))
            });
            admission.admitted = accepted.len();
            admission.ignored = result.messages.len().saturating_sub(accepted.len());
            state.runtime.pending.extend(
                accepted
                    .into_iter()
                    .map(|message| message.message_id.clone()),
            );
            if state.runtime.pending.is_empty() && state.runtime.active.is_none() {
                admission.cursor_advanced = state.runtime.committed_cursor != result.cursor;
                state.runtime.committed_cursor.clone_from(&result.cursor);
                state.runtime.staged_cursor = None;
            } else {
                state.runtime.staged_cursor = Some(result.cursor.clone());
            }
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        Ok(admission)
    }

    /// Records a typed admission/scope wait for the next pending request.
    /// Waiting is not attention: the runner stays live and re-attempts on
    /// the next head change.
    pub(super) fn record_work_wait_locked(
        &self,
        mode: RunnerExecutionMode,
        wait: &RunnerWorkWait,
    ) -> Result<RunnerStatus> {
        validate_execution_mode(mode, self.load_unlocked()?.runtime.enabled)?;
        validate_message_id(&wait.message_id)?;
        self.update(|state| {
            validate_execution_mode(mode, state.runtime.enabled)?;
            ensure!(
                state.runtime.active.is_none(),
                "runner has an active request and cannot record a work wait"
            );
            ensure!(
                state
                    .runtime
                    .pending
                    .first()
                    .is_some_and(|pending| pending == &wait.message_id),
                "runner work wait does not match the next pending request"
            );
            if state.runtime.attention.is_some() {
                return Err(conflict_failure("runner already needs attention"));
            }
            state.runtime.work_wait = Some(wait.clone());
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    /// Reads the accepted-work descriptor bound to the active request, if
    /// the runner is enforced and currently executing accepted work.
    pub(super) fn active_accepted_work_locked(&self) -> Result<Option<AcceptedWorkDescriptor>> {
        let state = self.load_unlocked()?;
        if !state.config.scope_mode.is_enforced() {
            return Ok(None);
        }
        Ok(state.runtime.active.and_then(|active| active.accepted_work))
    }

    /// Persists one deduplicated scope-change request record. Returns `true`
    /// only when the (task, intent, path fingerprint) changed; identical
    /// requests are not republished.
    pub(super) fn record_scope_change_request_locked(
        &self,
        message_id: &str,
        task_id: &str,
        intent_message_id: &str,
        paths_fingerprint: &str,
    ) -> Result<bool> {
        validate_message_id(message_id)?;
        validate_message_id(intent_message_id)?;
        ensure!(
            !paths_fingerprint.is_empty()
                && paths_fingerprint.len() <= 128
                && paths_fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
            "runner scope-change request fingerprint is invalid"
        );
        let mut published = false;
        self.update(|state| {
            let current = state.runtime.scope_change_request.as_ref();
            let already = current.is_some_and(|record| {
                record.task_id == task_id
                    && record.intent_message_id == intent_message_id
                    && record.paths_fingerprint == paths_fingerprint
            });
            if !already {
                state.runtime.scope_change_request = Some(ScopeChangeRequestState {
                    message_id: Some(message_id.to_string()),
                    task_id: task_id.to_string(),
                    intent_message_id: intent_message_id.to_string(),
                    paths_fingerprint: paths_fingerprint.to_string(),
                    scope_fingerprint: String::new(),
                    publish_state: ScopeChangePublishState::Confirmed,
                    observed_at_ms: now_ms(),
                });
                published = true;
            }
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        Ok(published)
    }

    /// Persists the publish-pending scope-change request record (tuple +
    /// fingerprint, **no** message id yet) BEFORE the request message is
    /// sent. Returns `true` only when the (task, intent, fingerprint) tuple
    /// changed; identical tuples are not re-begun.
    pub(super) fn begin_scope_change_request_locked(
        &self,
        task_id: &str,
        intent_message_id: &str,
        paths_fingerprint: &str,
        scope_fingerprint: &str,
    ) -> Result<bool> {
        validate_message_id(intent_message_id)?;
        ensure!(
            feanorfs_common::is_valid_task_id(task_id),
            "runner scope-change request has an invalid task id"
        );
        ensure!(
            !paths_fingerprint.is_empty()
                && paths_fingerprint.len() <= 128
                && paths_fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
            "runner scope-change request fingerprint is invalid"
        );
        ensure!(
            scope_fingerprint.is_empty()
                || (scope_fingerprint.len() <= 128
                    && scope_fingerprint.chars().all(|c| c.is_ascii_hexdigit())),
            "runner scope-change scope fingerprint is invalid"
        );
        let mut published = false;
        self.update(|state| {
            let current = state.runtime.scope_change_request.as_ref();
            ensure!(
                !current.is_some_and(|record| {
                    record.publish_state == ScopeChangePublishState::AwaitingConfirmation
                }),
                "runner scope-change request is awaiting confirmation; refusing a new request"
            );
            let already = current.is_some_and(|record| {
                record.task_id == task_id
                    && record.intent_message_id == intent_message_id
                    && record.paths_fingerprint == paths_fingerprint
            });
            if !already {
                state.runtime.scope_change_request = Some(ScopeChangeRequestState {
                    message_id: None,
                    task_id: task_id.to_string(),
                    intent_message_id: intent_message_id.to_string(),
                    paths_fingerprint: paths_fingerprint.to_string(),
                    scope_fingerprint: scope_fingerprint.to_string(),
                    publish_state: ScopeChangePublishState::PublishPending,
                    observed_at_ms: now_ms(),
                });
                published = true;
            }
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        Ok(published)
    }

    /// Abandons a scope-change request whose send failed: clears the
    /// publish-pending record and its scope-amendment wait together so the
    /// next cycle can retry the request from a clean state.
    pub fn abandon_scope_change_request_locked(&self) -> Result<()> {
        self.update(|state| {
            state.runtime.scope_change_request = None;
            if state
                .runtime
                .work_wait
                .as_ref()
                .is_some_and(|wait| wait.kind == RunnerWorkWaitKind::ScopeAmendmentRequested)
            {
                state.runtime.work_wait = None;
            }
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        Ok(())
    }

    /// Clears a scope-amendment wait (and leaves the confirmed request
    /// record in place) so admission re-resolves the pending request after
    /// an amendment changed the accepted scope.
    pub fn clear_scope_amendment_wait_locked(&self) -> Result<()> {
        self.update(|state| {
            if state
                .runtime
                .work_wait
                .as_ref()
                .is_some_and(|wait| wait.kind == RunnerWorkWaitKind::ScopeAmendmentRequested)
            {
                state.runtime.work_wait = None;
                state.runtime.updated_at_ms = now_ms();
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Fills the returned message id into the publish-pending record after a
    /// successful send. Fails when no record exists or it is already
    /// confirmed.
    pub(super) fn complete_scope_change_request_locked(&self, message_id: &str) -> Result<()> {
        validate_message_id(message_id)?;
        self.update(|state| {
            let record = state
                .runtime
                .scope_change_request
                .as_mut()
                .context("runner has no scope-change request record to complete")?;
            ensure!(
                record.message_id.is_none(),
                "runner scope-change request is already confirmed"
            );
            record.message_id = Some(message_id.to_string());
            record.publish_state = ScopeChangePublishState::Confirmed;
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        Ok(())
    }

    /// Marks a publish-pending record awaiting confirmation after a restart:
    /// the send outcome is unknown; nothing may be republished or begun.
    pub(super) fn mark_scope_change_awaiting_confirmation_locked(&self) -> Result<()> {
        self.update(|state| {
            let Some(record) = state.runtime.scope_change_request.as_mut() else {
                return Ok(());
            };
            if record.publish_state == ScopeChangePublishState::PublishPending {
                record.publish_state = ScopeChangePublishState::AwaitingConfirmation;
                state.runtime.updated_at_ms = now_ms();
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Full dedup tuple (task, intent, fingerprint) plus publish lifecycle of
    /// the one persisted scope-change request record.
    pub fn scope_change_request_key_locked(&self) -> Result<Option<ScopeChangeRequestKey>> {
        let state = self.load_unlocked()?;
        Ok(state
            .runtime
            .scope_change_request
            .map(|record| ScopeChangeRequestKey {
                task_id: record.task_id,
                intent_message_id: record.intent_message_id,
                paths_fingerprint: record.paths_fingerprint,
                scope_fingerprint: record.scope_fingerprint,
                publish_state: record.publish_state,
            }))
    }
}
