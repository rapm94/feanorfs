//! Terminal head/message send and delivery-unknown handling.

use super::super::agent_live::{LiveFinalOutcome, RunnerControllerHandle};
use super::cycle::{should_stop, wait_interruptible, CycleOutcome, CONTROL_POLL, RUNNER_BACKOFF};
use super::process::ProcessOutcome;
use feanorfs_agent_core::messages::HeadConditionalSendResult;
use feanorfs_agent_core::{RunnerExecutionMode, RunnerExecutionSession, RunnerLaunch, RunnerStore};
use feanorfs_common::{
    AgentInboxQuery, AgentMessage, AgentMessageInput, AgentMessageKind, AGENT_INBOX_MAX_LIMIT,
};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

const TERMINAL_READ_ATTEMPTS: u32 = 5;
pub(super) const FALLBACK_CAS_ATTEMPTS: usize = 4;
const CANCELLATION_COMPLETION_GRACE: Duration = Duration::from_secs(2);

pub(super) enum TerminalReadOutcome {
    Batch(feanorfs_common::AgentInboxResult),
    NeedsAttention,
}

enum RemoteOutcome<T> {
    Completed(T),
    Interrupted,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_request(
    workspace_root: &Path,
    workspace_config: &feanorfs_client::Config,
    db: &feanorfs_client::ClientDb,
    api: &feanorfs_client::ApiClient,
    store: &RunnerStore,
    session: &RunnerExecutionSession<'_>,
    agent: &str,
    launch: &RunnerLaunch,
    request: &AgentMessage,
    outcome: ProcessOutcome,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    controller: &RunnerControllerHandle,
) -> anyhow::Result<CycleOutcome> {
    let cancellation_deadline = match cancellation_completion_deadline(
        store,
        outcome,
        mode,
        shutdown,
        CANCELLATION_COMPLETION_GRACE,
    ) {
        Ok(deadline) => deadline,
        Err(_) => return delivery_unknown(session, launch),
    };
    // Reconcile the final file generation before the terminal reply so a
    // result can name the exact settled snapshot that was produced. The
    // flush never replays the model request itself.
    let live = match controller.flush_final().await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                ?error,
                "final continuous flush failed before terminal reply"
            );
            LiveFinalOutcome {
                settled: false,
                offline: true,
                attention: None,
                settled_snapshot: None,
                deferred_count: 0,
                child_exit: None,
            }
        }
    };
    let ctx = match feanorfs_client::SyncCtx::from_config(api, db, workspace_root, workspace_config)
    {
        Ok(ctx) => ctx,
        Err(_) => return delivery_unknown(session, launch),
    };
    let expected_result_snapshot = if live.settled && !live.offline && live.attention.is_none() {
        live.settled_snapshot.clone()
    } else {
        None
    };
    complete_request_with_remote(
        store,
        session,
        agent,
        launch,
        request,
        outcome,
        mode,
        shutdown,
        cancellation_deadline,
        expected_result_snapshot,
        || {
            feanorfs_agent_core::inbox(
                &ctx,
                AgentInboxQuery {
                    recipient: request.from.clone(),
                    after: Some(launch.reply_cursor.clone()),
                    limit: AGENT_INBOX_MAX_LIMIT,
                },
            )
        },
        |expected_head, input| {
            let ctx = &ctx;
            async move {
                feanorfs_agent_core::messages::send_message_if_head(ctx, &expected_head, input)
                    .await
            }
        },
    )
    .await
}

pub(super) fn cancellation_completion_deadline(
    store: &RunnerStore,
    outcome: ProcessOutcome,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    grace: Duration,
) -> anyhow::Result<Option<tokio::time::Instant>> {
    if outcome != ProcessOutcome::Cancellation || !should_stop(store, mode, shutdown)? {
        return Ok(None);
    }
    Ok(Some(tokio::time::Instant::now() + grace))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_request_with_remote<Read, ReadFuture, Send, SendFuture>(
    store: &RunnerStore,
    session: &RunnerExecutionSession<'_>,
    agent: &str,
    launch: &RunnerLaunch,
    request: &AgentMessage,
    outcome: ProcessOutcome,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    cancellation_deadline: Option<tokio::time::Instant>,
    expected_result_snapshot: Option<String>,
    mut read: Read,
    mut send: Send,
) -> anyhow::Result<CycleOutcome>
where
    Read: FnMut() -> ReadFuture,
    ReadFuture: Future<Output = anyhow::Result<feanorfs_common::AgentInboxResult>>,
    Send: FnMut(String, AgentMessageInput) -> SendFuture,
    SendFuture: Future<Output = anyhow::Result<HeadConditionalSendResult>>,
{
    let terminal_read = read_terminal_batch(
        store,
        session,
        launch,
        mode,
        shutdown,
        cancellation_deadline,
        &mut read,
    )
    .await?;
    let TerminalReadOutcome::Batch(terminal_read) = terminal_read else {
        return Ok(CycleOutcome::NeedsAttention);
    };
    if terminal_read.cursor_reset {
        return delivery_unknown(session, launch);
    }
    match session.observe_terminals_at_snapshot(
        request,
        &terminal_read.messages,
        expected_result_snapshot.as_deref(),
    ) {
        Ok(Some(_)) => return Ok(CycleOutcome::Completed),
        Ok(None) => {}
        Err(_) => return delivery_unknown(session, launch),
    }

    let body = outcome.blocked_body().to_string();
    // A fallback is always blocked. It names the exact final settled snapshot
    // only when reconciliation proved one; otherwise it retains the request's
    // snapshot without claiming that later file work settled.
    let about = expected_result_snapshot
        .clone()
        .unwrap_or_else(|| request.about_snapshot.clone());
    let fallback = AgentMessageInput {
        to: request.from.clone(),
        kind: AgentMessageKind::Blocked,
        body: body.clone(),
        about_snapshot: Some(about),
        reply_to: Some(request.message_id.clone()),
        from: Some(agent.to_string()),
    };
    let mut expected_head = terminal_read.cursor;
    for attempt in 1..=FALLBACK_CAS_ATTEMPTS {
        if expected_head.is_empty() {
            return delivery_unknown(session, launch);
        }
        let publication = send(expected_head.clone(), fallback.clone());
        match await_remote(store, mode, shutdown, cancellation_deadline, publication).await {
            RemoteOutcome::Completed(Ok(HeadConditionalSendResult::Sent(sent))) => {
                let published = AgentMessage {
                    message_id: sent.message_id,
                    from: agent.to_string(),
                    to: request.from.clone(),
                    kind: AgentMessageKind::Blocked,
                    body,
                    about_snapshot: sent.about_snapshot,
                    reply_to: Some(request.message_id.clone()),
                    created_at_ms: chrono::Utc::now().timestamp_millis(),
                };
                return match session.observe_terminals_at_snapshot(
                    request,
                    &[published],
                    expected_result_snapshot.as_deref(),
                ) {
                    Ok(Some(_)) => Ok(CycleOutcome::Completed),
                    Ok(None) | Err(_) => delivery_unknown(session, launch),
                };
            }
            RemoteOutcome::Completed(Ok(HeadConditionalSendResult::Conflict(_))) => {}
            RemoteOutcome::Completed(Err(_)) | RemoteOutcome::Interrupted => {
                return delivery_unknown(session, launch);
            }
        }

        let reread = read_terminal_batch(
            store,
            session,
            launch,
            mode,
            shutdown,
            cancellation_deadline,
            &mut read,
        )
        .await?;
        let TerminalReadOutcome::Batch(reread) = reread else {
            return Ok(CycleOutcome::NeedsAttention);
        };
        if reread.cursor_reset {
            return delivery_unknown(session, launch);
        }
        match session.observe_terminals_at_snapshot(
            request,
            &reread.messages,
            expected_result_snapshot.as_deref(),
        ) {
            Ok(Some(_)) => return Ok(CycleOutcome::Completed),
            Ok(None) => {}
            Err(_) => return delivery_unknown(session, launch),
        }
        if attempt == FALLBACK_CAS_ATTEMPTS
            || reread.cursor.is_empty()
            || reread.cursor == expected_head
        {
            return delivery_unknown(session, launch);
        }
        expected_head = reread.cursor;
    }
    unreachable!("fallback CAS attempts are non-zero")
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn read_terminal_batch<Read, ReadFuture>(
    store: &RunnerStore,
    session: &RunnerExecutionSession<'_>,
    launch: &RunnerLaunch,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    cancellation_deadline: Option<tokio::time::Instant>,
    mut read: Read,
) -> anyhow::Result<TerminalReadOutcome>
where
    Read: FnMut() -> ReadFuture,
    ReadFuture: Future<Output = anyhow::Result<feanorfs_common::AgentInboxResult>>,
{
    for attempt in 1..=TERMINAL_READ_ATTEMPTS {
        match await_remote(store, mode, shutdown, cancellation_deadline, read()).await {
            RemoteOutcome::Interrupted => {
                delivery_unknown(session, launch)?;
                return Ok(TerminalReadOutcome::NeedsAttention);
            }
            RemoteOutcome::Completed(Ok(result)) => {
                if session.record_inbox_recovery().is_err() {
                    delivery_unknown(session, launch)?;
                    return Ok(TerminalReadOutcome::NeedsAttention);
                }
                return Ok(TerminalReadOutcome::Batch(result));
            }
            RemoteOutcome::Completed(Err(_)) if cancellation_deadline.is_some() => {
                if session.record_inbox_failure().is_err() {
                    delivery_unknown(session, launch)?;
                    return Ok(TerminalReadOutcome::NeedsAttention);
                }
                delivery_unknown(session, launch)?;
                return Ok(TerminalReadOutcome::NeedsAttention);
            }
            RemoteOutcome::Completed(Err(_)) if attempt < TERMINAL_READ_ATTEMPTS => {
                let status = match session.record_inbox_failure() {
                    Ok(status) => status,
                    Err(_) => {
                        delivery_unknown(session, launch)?;
                        return Ok(TerminalReadOutcome::NeedsAttention);
                    }
                };
                if wait_interruptible(
                    store,
                    mode,
                    shutdown,
                    RUNNER_BACKOFF
                        .delay(status.inbox_failure_count)
                        .min(Duration::from_secs(8)),
                )
                .await
                .unwrap_or(true)
                {
                    delivery_unknown(session, launch)?;
                    return Ok(TerminalReadOutcome::NeedsAttention);
                }
            }
            RemoteOutcome::Completed(Err(_)) => {
                if session.record_inbox_failure().is_err() {
                    delivery_unknown(session, launch)?;
                    return Ok(TerminalReadOutcome::NeedsAttention);
                }
                delivery_unknown(session, launch)?;
                return Ok(TerminalReadOutcome::NeedsAttention);
            }
        }
    }
    unreachable!("terminal read attempts are non-zero")
}

async fn await_remote<T>(
    store: &RunnerStore,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    cancellation_deadline: Option<tokio::time::Instant>,
    remote: impl Future<Output = T>,
) -> RemoteOutcome<T> {
    let mut remote = Box::pin(remote);
    loop {
        if let Some(deadline) = cancellation_deadline {
            if tokio::time::Instant::now() >= deadline {
                return RemoteOutcome::Interrupted;
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return RemoteOutcome::Interrupted,
                result = &mut remote => return RemoteOutcome::Completed(result),
            }
        }
        if should_stop(store, mode, shutdown).unwrap_or(true) {
            return RemoteOutcome::Interrupted;
        }
        tokio::select! {
            result = &mut remote => return RemoteOutcome::Completed(result),
            _ = tokio::time::sleep(CONTROL_POLL) => {}
        }
    }
}

fn delivery_unknown(
    session: &RunnerExecutionSession<'_>,
    launch: &RunnerLaunch,
) -> anyhow::Result<CycleOutcome> {
    session.record_delivery_unknown(&launch.message_id, &launch.session_id)?;
    tracing::error!("agent runner stopped because terminal delivery could not be established");
    Ok(CycleOutcome::NeedsAttention)
}
