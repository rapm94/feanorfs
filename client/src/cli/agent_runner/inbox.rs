//! Bounded inbox reads and durable batch admission for the runner worker.

use super::cycle::{stopped_after_state_error, CycleOutcome};
use anyhow::Context as _;
use feanorfs_agent_core::{
    resolve_request_admission, AcceptedWorkDescriptor, RunnerAdmissionReject, RunnerConfig,
    RunnerExecutionMode, RunnerExecutionSession, RunnerStore, RunnerWorkWaitKind,
};
use feanorfs_common::{AgentInboxQuery, WorkStatusInput, AGENT_INBOX_MAX_LIMIT};
use std::path::Path;

pub(super) fn admit_batch(
    session: &RunnerExecutionSession<'_>,
    store: &RunnerStore,
    mode: RunnerExecutionMode,
    result: &feanorfs_common::AgentInboxResult,
) -> anyhow::Result<Option<CycleOutcome>> {
    match session.admit_inbox(result) {
        Ok(admission) if admission.needs_attention => Ok(Some(CycleOutcome::NeedsAttention)),
        Ok(_) => Ok(None),
        Err(_) if stopped_after_state_error(store, mode)? => Ok(Some(CycleOutcome::Stop)),
        Err(error) => Err(error).context("durably admit runner inbox batch"),
    }
}

/// Outcome of resolving the next pending request against the reducer.
#[allow(clippy::large_enum_variant)]
pub(super) enum NextAdmission {
    /// Enforced: the request maps to one accepted intent ready to launch.
    Launch(AcceptedWorkDescriptor),
    /// Legacy/advisory: admission is not gated.
    NotApplicable,
    /// Enforced: typed rejection; the runner waits (never launches).
    Wait {
        message_id: String,
        reason: RunnerAdmissionReject,
    },
    /// The work projection could not be read (transport); retry later.
    Unavailable,
}

pub(super) fn wait_kind_for(reason: RunnerAdmissionReject) -> RunnerWorkWaitKind {
    match reason {
        RunnerAdmissionReject::UnsettledDependency => RunnerWorkWaitKind::DependencyBlocked,
        RunnerAdmissionReject::ProjectionIncomplete => RunnerWorkWaitKind::ProjectionIncomplete,
        RunnerAdmissionReject::ScopeAmendmentPending => RunnerWorkWaitKind::ScopeAmendmentRequested,
        _ => RunnerWorkWaitKind::WaitingAcceptance,
    }
}

/// Canonical scope fingerprint (paths + concerns, NUL-joined, Blake3) used
/// by both the controller's dedup record and this admission gate. Must stay
/// byte-identical to `agent_live.rs`'s computation.
fn canonical_scope_fingerprint(scope: &feanorfs_common::work_contract::WorkScope) -> String {
    let mut entries = scope.paths.clone();
    entries.sort();
    entries.dedup();
    let mut concerns = scope.concerns.clone();
    concerns.sort();
    concerns.dedup();
    blake3::hash(format!("{}\u{0}{}", entries.join("\u{0}"), concerns.join("\u{0}")).as_bytes())
        .to_hex()
        .to_string()
}

/// Scope-amendment wait gate: while a confirmed scope-change request waits
/// for an amendment, re-admission of the same intent is blocked unless the
/// projection shows the accepted scope changed (then the wait is cleared and
/// the amended scope is admitted) or the intent is gone/superseded (then the
/// normal admission path produces its typed wait).
fn scope_amendment_gate(
    store: &RunnerStore,
    projection: &feanorfs_common::WorkStatusResult,
    pending_request_id: &str,
) -> Option<RunnerAdmissionReject> {
    // The confirmed scope-change record is the authority: while it exists
    // for this exact intent, re-admission is blocked even before (or while)
    // the typed wait is recorded — the wait record requires an idle runner
    // and may not exist yet.
    let key = store.scope_change_request_key_locked().ok().flatten()?;
    if key.intent_message_id != pending_request_id
        || !matches!(
            key.publish_state,
            feanorfs_agent_core::ScopeChangePublishState::Confirmed
        )
    {
        return None;
    }
    if key.scope_fingerprint.is_empty() {
        // Legacy record without a canonical scope component: fail closed by
        // keeping the wait; it can only be released by a scope change, which
        // admission cannot prove here.
        return Some(RunnerAdmissionReject::ScopeAmendmentPending);
    }
    let current = projection.tasks.iter().find_map(|task| {
        task.proposals.iter().find_map(|proposal| {
            (proposal.intent_message_id == pending_request_id)
                .then(|| canonical_scope_fingerprint(&proposal.accepted_scope))
        })
    })?;
    if current == key.scope_fingerprint {
        Some(RunnerAdmissionReject::ScopeAmendmentPending)
    } else {
        // The accepted scope changed: release the wait so the amended
        // scope is admitted below.
        let _ = store.clear_scope_amendment_wait_locked();
        None
    }
}

/// Recomputes admission for the next pending request after refresh and inbox
/// re-read. For enforced runners this is the sole gate before any process
/// launch; legacy/advisory runners return [`NextAdmission::NotApplicable`].
pub(super) async fn resolve_next_admission(
    workspace_root: &Path,
    workspace_config: &feanorfs_client::Config,
    db: &feanorfs_client::ClientDb,
    api: &feanorfs_client::ApiClient,
    store: &RunnerStore,
    current: &feanorfs_common::AgentInboxResult,
    runner_config: &RunnerConfig,
) -> anyhow::Result<NextAdmission> {
    if !runner_config.scope_mode.is_enforced() {
        return Ok(NextAdmission::NotApplicable);
    }
    let Some(next_id) = store.next_pending_message_id()? else {
        return Ok(NextAdmission::NotApplicable);
    };
    let Some(request) = current
        .messages
        .iter()
        .find(|message| message.message_id == next_id)
    else {
        // The admitted request body is unavailable in the re-read (only
        // reachable after a bounded cursor reset that should have entered
        // attention first). Fail closed: never launch without the message.
        anyhow::bail!("admitted runner request body is unavailable for admission");
    };
    let ctx = match feanorfs_client::SyncCtx::from_config(api, db, workspace_root, workspace_config)
    {
        Ok(ctx) => ctx,
        Err(error) => return Err(error).context("build work projection context"),
    };
    let projection = match feanorfs_agent_core::work::work_status(&ctx, WorkStatusInput::default())
        .await
    {
        Ok(projection) => projection,
        Err(error) if feanorfs_client::api::is_retryable_transport_error(&error) => {
            return Ok(NextAdmission::Unavailable);
        }
        Err(error) => return Err(error).context("observe work projection before runner admission"),
    };
    let work_state = feanorfs_agent_core::work::WorkStore::open(workspace_root)?
        .load()
        .context("read work reducer state before runner admission")?;
    if let Some(reason) = scope_amendment_gate(store, &projection, &next_id) {
        return Ok(NextAdmission::Wait {
            message_id: next_id,
            reason,
        });
    }
    match resolve_request_admission(&projection, &work_state, request, &runner_config.agent) {
        Ok(Some(descriptor)) => Ok(NextAdmission::Launch(descriptor)),
        Ok(None) => Ok(NextAdmission::Wait {
            message_id: next_id,
            reason: RunnerAdmissionReject::RequestWithoutIntent,
        }),
        Err(reason) => Ok(NextAdmission::Wait {
            message_id: next_id,
            reason,
        }),
    }
}

pub(super) async fn read_runner_inbox(
    workspace_root: &Path,
    workspace_config: &feanorfs_client::Config,
    db: &feanorfs_client::ClientDb,
    api: &feanorfs_client::ApiClient,
    store: &RunnerStore,
) -> anyhow::Result<feanorfs_common::AgentInboxResult> {
    let runner = store.config()?;
    let cursor = store.committed_cursor()?;
    let ctx = feanorfs_client::SyncCtx::from_config(api, db, workspace_root, workspace_config)?;
    feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: runner.agent,
            after: Some(cursor),
            limit: AGENT_INBOX_MAX_LIMIT,
        },
    )
    .await
}
