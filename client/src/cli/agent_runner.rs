//! Shared execution loop for configured unattended agent runners.

use anyhow::{ensure, Context as _};
use feanorfs_agent_core::messages::HeadConditionalSendResult;
use feanorfs_agent_core::{
    refresh_agent_guarded, RunnerAttention, RunnerConfig, RunnerExecutionMode,
    RunnerExecutionSession, RunnerInvocation, RunnerLaunch, RunnerPhase, RunnerStore,
};
use feanorfs_common::{
    AgentInboxQuery, AgentMessage, AgentMessageInput, AgentMessageKind, AGENT_INBOX_MAX_LIMIT,
    AGENT_MESSAGE_MAX_ENCODED_BYTES,
};
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

use super::process_tree;
#[cfg(unix)]
use super::process_tree::ProcessGroup;

const IDLE_POLL: Duration = Duration::from_millis(500);
const CONTROL_POLL: Duration = Duration::from_millis(100);
const TERMINATE_GRACE: Duration = Duration::from_secs(2);
const POST_KILL_REAP_GRACE: Duration = Duration::from_secs(1);
const DETACHED_REAP_POLL: Duration = Duration::from_millis(100);
const CANCELLATION_COMPLETION_GRACE: Duration = Duration::from_secs(2);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const TERMINAL_READ_ATTEMPTS: u32 = 5;
const FALLBACK_CAS_ATTEMPTS: usize = 4;
const MAX_INVOCATION_BYTES: usize = AGENT_MESSAGE_MAX_ENCODED_BYTES + 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleOutcome {
    Idle,
    Completed,
    RemoteUnavailable,
    Stop,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOutcome {
    StartFailure,
    StdinFailure,
    Exited,
    Timeout,
    Cancellation,
}

enum TerminalReadOutcome {
    Batch(feanorfs_common::AgentInboxResult),
    NeedsAttention,
}

enum RemoteOutcome<T> {
    Completed(T),
    Interrupted,
}

impl ProcessOutcome {
    const fn blocked_body(self) -> &'static str {
        match self {
            Self::StartFailure => "runner blocked: process start failed",
            Self::StdinFailure => "runner blocked: invocation delivery failed",
            Self::Exited => "runner blocked: process exited without a correlated terminal",
            Self::Timeout => "runner blocked: execution timed out",
            Self::Cancellation => "runner blocked: execution cancelled",
        }
    }
}

/// Runs the sole already-configured agent runner for `workspace_root`.
///
/// The caller supplies a canonical workspace root. Foreground and supervised
/// entry points intentionally share this loop so they cannot drift on process,
/// cursor, or fail-closed delivery behavior.
pub(crate) async fn run_worker(
    workspace_root: &Path,
    mode: RunnerExecutionMode,
) -> anyhow::Result<()> {
    let workspace_root = require_canonical_workspace(workspace_root)?;
    let config = feanorfs_client::load_config(&workspace_root)
        .context("open configured runner workspace")?;
    ensure!(
        config.format_version == 3,
        "agent runners require an already configured format-v3 workspace"
    );

    let store = RunnerStore::open_configured(&workspace_root)
        .context("open the configured agent runner")?;
    let runner_config = store.config()?;
    let session = store
        .execution_session(&workspace_root, mode)
        .context("acquire the configured agent runner execution lease")?;
    if report_attention(&store)? {
        return Ok(());
    }

    let agent_dir = feanorfs_agent_core::agent_dir(&workspace_root, &runner_config.agent)?
        .canonicalize()
        .context("canonicalize configured agent worktree")?;
    ensure!(agent_dir.is_absolute(), "agent worktree must be absolute");
    let db = feanorfs_client::open_client_db(&workspace_root).await?;
    let (shutdown, shutdown_task) = shutdown_channel()?;
    let _shutdown_task = AbortTask(shutdown_task);
    let mut api = None;

    loop {
        if should_stop(&store, mode, &shutdown)? {
            return Ok(());
        }
        if report_attention(&store)? {
            return Ok(());
        }

        if api.is_none() {
            match feanorfs_client::open_api_client(&workspace_root, &config).await {
                Ok(opened) => api = Some(opened),
                Err(_) => {
                    let status = session.record_inbox_failure()?;
                    if wait_interruptible(
                        &store,
                        mode,
                        &shutdown,
                        backoff_duration(status.inbox_failure_count),
                    )
                    .await?
                    {
                        return Ok(());
                    }
                    continue;
                }
            }
        }

        let outcome = run_cycle(
            &workspace_root,
            &agent_dir,
            &config,
            &db,
            api.as_ref().expect("runner API initialized"),
            &store,
            &session,
            &runner_config,
            mode,
            &shutdown,
        )
        .await?;

        match outcome {
            CycleOutcome::Idle | CycleOutcome::Completed => {
                if store.status()?.inbox_failure_count > 0 {
                    session.record_inbox_recovery()?;
                }
                if outcome == CycleOutcome::Idle
                    && wait_interruptible(&store, mode, &shutdown, IDLE_POLL).await?
                {
                    return Ok(());
                }
            }
            CycleOutcome::RemoteUnavailable => {
                api = None;
                let status = session.record_inbox_failure()?;
                if wait_interruptible(
                    &store,
                    mode,
                    &shutdown,
                    backoff_duration(status.inbox_failure_count),
                )
                .await?
                {
                    return Ok(());
                }
            }
            CycleOutcome::Stop => return Ok(()),
            CycleOutcome::NeedsAttention => {
                report_attention(&store)?;
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_cycle(
    workspace_root: &Path,
    agent_dir: &Path,
    workspace_config: &feanorfs_client::Config,
    db: &feanorfs_client::ClientDb,
    api: &feanorfs_client::ApiClient,
    store: &RunnerStore,
    session: &RunnerExecutionSession<'_>,
    runner_config: &RunnerConfig,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<CycleOutcome> {
    let first = match read_runner_inbox(workspace_root, workspace_config, db, api, store).await {
        Ok(result) => result,
        Err(_) => return Ok(CycleOutcome::RemoteUnavailable),
    };
    if let Some(outcome) = admit_batch(session, store, mode, &first)? {
        return Ok(outcome);
    }
    if store.status()?.pending_count == 0 {
        return Ok(CycleOutcome::Idle);
    }

    if should_stop(store, mode, shutdown)? {
        return Ok(CycleOutcome::Stop);
    }
    if let Some(outcome) = refresh_before_launch(
        session,
        refresh_agent_guarded(
            workspace_root,
            db,
            api,
            &workspace_config.workspace_id,
            &runner_config.agent,
            workspace_config.encryption_password.as_deref(),
            session,
        ),
    )
    .await?
    {
        return Ok(outcome);
    }

    // Refresh can overlap new signals. Re-read from the still-committed cursor
    // and durably admit the complete bounded batch before selecting one task.
    let current = match read_runner_inbox(workspace_root, workspace_config, db, api, store).await {
        Ok(result) => result,
        Err(_) => return Ok(CycleOutcome::RemoteUnavailable),
    };
    if let Some(outcome) = admit_batch(session, store, mode, &current)? {
        return Ok(outcome);
    }
    if should_stop(store, mode, shutdown)? {
        return Ok(CycleOutcome::Stop);
    }

    let launch = match session.begin_next(&current.cursor) {
        Ok(launch) => launch,
        Err(_) if stopped_after_state_error(store, mode)? => return Ok(CycleOutcome::Stop),
        Err(error) => return Err(error).context("checkpoint the next runner request"),
    };
    let Some(request) = current
        .messages
        .into_iter()
        .find(|message| message.message_id == launch.message_id)
    else {
        session.record_delivery_unknown(&launch.message_id, &launch.session_id)?;
        tracing::error!("agent runner stopped because an admitted request body was unavailable");
        return Ok(CycleOutcome::NeedsAttention);
    };
    let invocation = match RunnerInvocation::new(&launch, &runner_config.agent, request.clone())
        .and_then(|invocation| {
            let bytes = serde_json::to_vec(&invocation).context("serialize runner invocation")?;
            ensure!(
                bytes.len() <= MAX_INVOCATION_BYTES,
                "runner invocation exceeds its size bound"
            );
            Ok(bytes)
        }) {
        Ok(invocation) => invocation,
        Err(_) => {
            return complete_request(
                workspace_root,
                workspace_config,
                db,
                api,
                store,
                session,
                &runner_config.agent,
                &launch,
                &request,
                ProcessOutcome::StartFailure,
                mode,
                shutdown,
            )
            .await;
        }
    };

    let process_outcome = run_configured_process(
        workspace_root,
        agent_dir,
        store,
        session,
        runner_config,
        &launch,
        &invocation,
        mode,
        shutdown,
    )
    .await?;
    complete_request(
        workspace_root,
        workspace_config,
        db,
        api,
        store,
        session,
        &runner_config.agent,
        &launch,
        &request,
        process_outcome,
        mode,
        shutdown,
    )
    .await
}

async fn refresh_before_launch<T>(
    session: &RunnerExecutionSession<'_>,
    refresh: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<Option<CycleOutcome>> {
    match refresh.await {
        Ok(_) => Ok(None),
        Err(error) if feanorfs_client::api::is_retryable_transport_error(&error) => {
            Ok(Some(CycleOutcome::RemoteUnavailable))
        }
        Err(error) => {
            tracing::error!(error = ?error, "agent runner stopped because refresh preparation failed");
            session
                .record_preparation_failed()
                .context("record runner refresh preparation failure")?;
            Ok(Some(CycleOutcome::NeedsAttention))
        }
    }
}

fn admit_batch(
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

async fn read_runner_inbox(
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

#[allow(clippy::too_many_arguments)]
async fn run_configured_process(
    workspace_root: &Path,
    agent_dir: &Path,
    store: &RunnerStore,
    session: &RunnerExecutionSession<'_>,
    config: &RunnerConfig,
    launch: &RunnerLaunch,
    invocation: &[u8],
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<ProcessOutcome> {
    #[cfg(all(unix, not(test)))]
    let wrapper_program = std::env::current_exe().context("locate feanorfs startup wrapper")?;
    #[cfg(all(unix, not(test)))]
    let mut command = tokio::process::Command::new(wrapper_program);
    #[cfg(any(not(unix), test))]
    let mut command = tokio::process::Command::new(&config.program);
    command
        .current_dir(agent_dir)
        .env("FEANORFS_AGENT", &config.agent)
        .env("FEANORFS_AGENT_DIR", agent_dir)
        .env("FEANORFS_WORKSPACE_ROOT", workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if process_tree::configure_process_group(&mut command).is_err() {
        return Ok(ProcessOutcome::StartFailure);
    }
    #[cfg(not(test))]
    let mut startup_gate = match process_tree::StartupGate::prepare(&mut command) {
        Ok(gate) => gate,
        Err(_) => return Ok(ProcessOutcome::StartFailure),
    };
    #[cfg(test)]
    let mut startup_gate = process_tree::StartupGate::disabled();
    #[cfg(all(unix, not(test)))]
    {
        command.args([
            std::ffi::OsString::from("service"),
            std::ffi::OsString::from("exec-gate"),
            std::ffi::OsString::from(startup_gate.release_fd().to_string()),
            config.program.as_os_str().to_owned(),
            std::ffi::OsString::from("--"),
        ]);
        command.args(&config.fixed_args);
    }
    #[cfg(any(not(unix), test))]
    command.args(&config.fixed_args);

    let child = match spawn_managed_child(&CHILD_REAPER, || command.spawn()) {
        Ok(child) => child,
        Err(_) => return Ok(ProcessOutcome::StartFailure),
    };
    let mut child = child;
    let Some(pid) = child.id() else {
        child.force_reap().await;
        return Ok(ProcessOutcome::StartFailure);
    };
    let process_start_id = process_tree::process_start_identifier(pid, &launch.session_id);
    if !process_tree::process_start_matches(pid, &process_start_id) {
        // A missing or malformed native identity is not a recoverable launch
        // condition. Terminate/reap the newly adopted tree before returning;
        // no process metadata is published for an unowned/ambiguous child.
        child.terminate().await;
        return Ok(ProcessOutcome::StartFailure);
    }
    match should_stop(store, mode, shutdown) {
        Ok(true) | Err(_) => {
            drop(startup_gate);
            child.terminate().await;
            return Ok(ProcessOutcome::Cancellation);
        }
        Ok(false) => {}
    }
    if session
        .mark_spawned(&launch.message_id, pid, &process_start_id)
        .is_err()
    {
        drop(startup_gate);
        child.terminate().await;
        return Ok(ProcessOutcome::StartFailure);
    }
    #[cfg(unix)]
    let release_result = startup_gate.release();
    #[cfg(windows)]
    let release_result = startup_gate.release(child.process_tree.as_ref(), child.child.as_ref());
    #[cfg(not(any(unix, windows)))]
    let release_result = startup_gate.release();
    if release_result.is_err() {
        drop(startup_gate);
        child.terminate().await;
        return Ok(ProcessOutcome::StartFailure);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.timeout_secs);
    let Some(stdin) = child.take_stdin() else {
        child.terminate().await;
        return Ok(ProcessOutcome::StdinFailure);
    };
    let write_outcome = write_invocation_until(stdin, invocation, deadline, || {
        should_stop(store, mode, shutdown)
    })
    .await;
    match write_outcome {
        InvocationWrite::Written => {}
        InvocationWrite::Failed => {
            child.terminate().await;
            return Ok(ProcessOutcome::StdinFailure);
        }
        InvocationWrite::TimedOut => {
            child.terminate().await;
            return Ok(ProcessOutcome::Timeout);
        }
        InvocationWrite::Cancelled => {
            child.terminate().await;
            return Ok(ProcessOutcome::Cancellation);
        }
    }

    wait_for_child_until(&mut child, deadline, || should_stop(store, mode, shutdown)).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvocationWrite {
    Written,
    Failed,
    TimedOut,
    Cancelled,
}

async fn write_invocation_until(
    mut stdin: tokio::process::ChildStdin,
    invocation: &[u8],
    deadline: tokio::time::Instant,
    mut cancelled: impl FnMut() -> anyhow::Result<bool>,
) -> InvocationWrite {
    let mut write = Box::pin(async move {
        stdin.write_all(invocation).await?;
        stdin.shutdown().await
    });
    loop {
        match cancelled() {
            Ok(true) | Err(_) => return InvocationWrite::Cancelled,
            Ok(false) => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return InvocationWrite::TimedOut;
        }
        let poll_until = deadline.min(now + CONTROL_POLL);
        tokio::select! {
            result = &mut write => {
                return if result.is_ok() {
                    InvocationWrite::Written
                } else {
                    InvocationWrite::Failed
                };
            }
            _ = tokio::time::sleep_until(poll_until) => {}
        }
    }
}

async fn wait_for_child_until(
    child: &mut ManagedChild,
    deadline: tokio::time::Instant,
    mut cancelled: impl FnMut() -> anyhow::Result<bool>,
) -> anyhow::Result<ProcessOutcome> {
    loop {
        match cancelled() {
            Ok(true) => {
                child.terminate().await;
                return Ok(ProcessOutcome::Cancellation);
            }
            Err(_) => {
                child.terminate().await;
                return Ok(ProcessOutcome::Cancellation);
            }
            Ok(false) => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            child.terminate().await;
            return Ok(ProcessOutcome::Timeout);
        }
        let poll_until = deadline.min(now + CONTROL_POLL);
        match child.poll_until(poll_until).await {
            Ok(Some(_)) => {
                child.cleanup_process_tree_after_exit().await;
                return Ok(ProcessOutcome::Exited);
            }
            Ok(None) => {}
            Err(_) => {
                child.force_reap().await;
                return Ok(ProcessOutcome::Exited);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn complete_request(
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
    let ctx = match feanorfs_client::SyncCtx::from_config(api, db, workspace_root, workspace_config)
    {
        Ok(ctx) => ctx,
        Err(_) => return delivery_unknown(session, launch),
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

fn cancellation_completion_deadline(
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
async fn complete_request_with_remote<Read, ReadFuture, Send, SendFuture>(
    store: &RunnerStore,
    session: &RunnerExecutionSession<'_>,
    agent: &str,
    launch: &RunnerLaunch,
    request: &AgentMessage,
    outcome: ProcessOutcome,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    cancellation_deadline: Option<tokio::time::Instant>,
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
    match session.observe_terminals(request, &terminal_read.messages) {
        Ok(Some(_)) => return Ok(CycleOutcome::Completed),
        Ok(None) => {}
        Err(_) => return delivery_unknown(session, launch),
    }

    let body = outcome.blocked_body().to_string();
    let fallback = AgentMessageInput {
        to: request.from.clone(),
        kind: AgentMessageKind::Blocked,
        body: body.clone(),
        about_snapshot: Some(request.about_snapshot.clone()),
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
                return match session.observe_terminals(request, &[published]) {
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
        match session.observe_terminals(request, &reread.messages) {
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
async fn read_terminal_batch<Read, ReadFuture>(
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
                    backoff_duration(status.inbox_failure_count).min(Duration::from_secs(8)),
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

fn require_canonical_workspace(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(path.is_absolute(), "runner workspace path must be absolute");
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize runner workspace {}", path.display()))?;
    ensure!(
        canonical == path,
        "runner workspace path must already be canonical: {}",
        path.display()
    );
    ensure!(canonical.is_dir(), "runner workspace must be a directory");
    Ok(canonical)
}

fn report_attention(store: &RunnerStore) -> anyhow::Result<bool> {
    let status = store.status()?;
    if status.phase != RunnerPhase::NeedsAttention {
        return Ok(false);
    }
    match status.attention {
        Some(RunnerAttention::CursorReset) => {
            tracing::error!("agent runner stopped: inbox cursor needs attention")
        }
        Some(RunnerAttention::PendingOverflow) => {
            tracing::error!("agent runner stopped: pending queue needs attention")
        }
        Some(RunnerAttention::AmbiguousExecution) => {
            tracing::error!("agent runner stopped: a prior execution is ambiguous")
        }
        Some(RunnerAttention::DeliveryUnknown) => {
            tracing::error!("agent runner stopped: terminal delivery is unknown")
        }
        Some(RunnerAttention::PreparationFailed) => {
            tracing::error!("agent runner stopped: refresh preparation failed")
        }
        None => tracing::error!("agent runner stopped: runner state needs attention"),
    }
    Ok(true)
}

fn stopped_after_state_error(
    store: &RunnerStore,
    mode: RunnerExecutionMode,
) -> anyhow::Result<bool> {
    Ok(mode == RunnerExecutionMode::Supervised && !store.status()?.enabled)
}

fn should_stop(
    store: &RunnerStore,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    if *shutdown.borrow() {
        return Ok(true);
    }
    Ok(mode == RunnerExecutionMode::Supervised && !store.status()?.enabled)
}

async fn wait_interruptible(
    store: &RunnerStore,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    duration: Duration,
) -> anyhow::Result<bool> {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if should_stop(store, mode, shutdown)? {
            return Ok(true);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        tokio::time::sleep_until(deadline.min(now + CONTROL_POLL)).await;
    }
}

fn backoff_duration(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let shift = failures.saturating_sub(1).min(6);
    BACKOFF_BASE.saturating_mul(1_u32 << shift).min(BACKOFF_MAX)
}

fn shutdown_channel() -> anyhow::Result<(
    tokio::sync::watch::Receiver<bool>,
    tokio::task::JoinHandle<()>,
)> {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install agent runner SIGTERM handler")?;
    #[cfg(windows)]
    let mut ctrl_break =
        tokio::signal::windows::ctrl_break().context("install agent runner Ctrl+Break handler")?;
    let task = tokio::spawn(async move {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        #[cfg(windows)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = ctrl_break.recv() => {}
        }
        #[cfg(all(not(unix), not(windows)))]
        let _ = tokio::signal::ctrl_c().await;
        let _ = sender.send(true);
    });
    Ok((receiver, task))
}

struct AbortTask(tokio::task::JoinHandle<()>);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum PostKillWait {
    Reaped,
    TimedOut,
    WaitError(std::io::Error),
}

struct ChildReaper {
    pending: Mutex<VecDeque<tokio::process::Child>>,
    wake: Condvar,
    initialization: Mutex<()>,
    ready: AtomicBool,
    processing: AtomicBool,
    #[cfg(test)]
    forced_start_failures: AtomicUsize,
    #[cfg(test)]
    forced_try_wait_errors: AtomicUsize,
    #[cfg(test)]
    forced_try_wait_panics: AtomicUsize,
    #[cfg(test)]
    coordinator_starts: AtomicUsize,
    #[cfg(test)]
    panic_recoveries: AtomicUsize,
    #[cfg(test)]
    transfers: AtomicUsize,
    #[cfg(test)]
    error_requeues: AtomicUsize,
}

static CHILD_REAPER: ChildReaper = ChildReaper::new();

#[derive(Clone, Copy)]
struct ReadyChildReaper(&'static ChildReaper);

impl ChildReaper {
    const fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            initialization: Mutex::new(()),
            ready: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            #[cfg(test)]
            forced_start_failures: AtomicUsize::new(0),
            #[cfg(test)]
            forced_try_wait_errors: AtomicUsize::new(0),
            #[cfg(test)]
            forced_try_wait_panics: AtomicUsize::new(0),
            #[cfg(test)]
            coordinator_starts: AtomicUsize::new(0),
            #[cfg(test)]
            panic_recoveries: AtomicUsize::new(0),
            #[cfg(test)]
            transfers: AtomicUsize::new(0),
            #[cfg(test)]
            error_requeues: AtomicUsize::new(0),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, VecDeque<tokio::process::Child>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ensure_ready(&'static self) -> std::io::Result<ReadyChildReaper> {
        if !self.ready.load(AtomicOrdering::Acquire) {
            let _initialization = self
                .initialization
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !self.ready.load(AtomicOrdering::Acquire) {
                self.spawn_thread()?;
                self.ready.store(true, AtomicOrdering::Release);
            }
        }
        Ok(ReadyChildReaper(self))
    }

    fn spawn_thread(&'static self) -> std::io::Result<()> {
        #[cfg(test)]
        if take_test_counter(&self.forced_start_failures) {
            return Err(std::io::Error::other(
                "injected reaper coordinator start failure",
            ));
        }
        let thread = std::thread::Builder::new()
            .name("feanorfs-runner-reaper".to_string())
            .spawn(move || self.run())?;
        #[cfg(test)]
        self.coordinator_starts.fetch_add(1, AtomicOrdering::SeqCst);
        drop(thread);
        Ok(())
    }

    fn run(&'static self) -> ! {
        loop {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reap_one())).is_err() {
                #[cfg(test)]
                self.panic_recoveries.fetch_add(1, AtomicOrdering::SeqCst);
                tracing::warn!("agent-runner reaper recovered from child-processing panic");
                std::thread::sleep(DETACHED_REAP_POLL);
            }
        }
    }

    fn reap_one(&'static self) {
        let mut child = self.wait_for_child();
        let retry = match self.try_wait(child.child_mut()) {
            Ok(Some(_)) => {
                child.reaped();
                false
            }
            Ok(None) => true,
            Err(_) => {
                #[cfg(test)]
                self.error_requeues.fetch_add(1, AtomicOrdering::SeqCst);
                true
            }
        };
        if retry {
            drop(child);
            std::thread::sleep(DETACHED_REAP_POLL);
        }
    }

    fn wait_for_child(&'static self) -> ReaperChildGuard {
        let mut pending = self.pending();
        let child = loop {
            if let Some(child) = pending.pop_front() {
                break child;
            }
            pending = self
                .wake
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        };
        self.processing.store(true, AtomicOrdering::Release);
        ReaperChildGuard::new(self, child)
    }

    fn try_wait(&self, child: &mut tokio::process::Child) -> std::io::Result<Option<ExitStatus>> {
        #[cfg(test)]
        if take_test_counter(&self.forced_try_wait_panics) {
            panic!("injected reaper wait panic");
        }
        #[cfg(test)]
        if take_test_counter(&self.forced_try_wait_errors) {
            return Err(std::io::Error::other("injected reaper wait failure"));
        }
        child.try_wait()
    }

    #[cfg(test)]
    fn fail_next_start(&self) {
        self.forced_start_failures
            .fetch_add(1, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    fn coordinator_start_count(&self) -> usize {
        self.coordinator_starts.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn fail_next_try_wait(&self) {
        self.forced_try_wait_errors
            .fetch_add(1, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    fn panic_next_try_wait(&self) {
        self.forced_try_wait_panics
            .fetch_add(1, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    fn transfer_count(&self) -> usize {
        self.transfers.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn error_requeue_count(&self) -> usize {
        self.error_requeues.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn panic_recovery_count(&self) -> usize {
        self.panic_recoveries.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    fn is_idle(&self) -> bool {
        let pending = self.pending();
        let empty = pending.is_empty();
        let processing = self.processing.load(AtomicOrdering::Acquire);
        drop(pending);
        empty && !processing
    }
}

impl ReadyChildReaper {
    fn enqueue(self, child: tokio::process::Child) {
        let mut pending = self.0.pending();
        pending.push_back(child);
        #[cfg(test)]
        self.0.transfers.fetch_add(1, AtomicOrdering::SeqCst);
        drop(pending);
        self.0.wake.notify_one();
    }
}

struct ReaperChildGuard {
    reaper: &'static ChildReaper,
    child: Option<tokio::process::Child>,
}

impl ReaperChildGuard {
    fn new(reaper: &'static ChildReaper, child: tokio::process::Child) -> Self {
        Self {
            reaper,
            child: Some(child),
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child.as_mut().expect("reaper child is present")
    }

    fn reaped(&mut self) {
        self.child = None;
        self.reaper.processing.store(false, AtomicOrdering::Release);
    }
}

impl Drop for ReaperChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let mut pending = self.reaper.pending();
            pending.push_back(child);
            self.reaper.processing.store(false, AtomicOrdering::Release);
            drop(pending);
            self.reaper.wake.notify_one();
        }
    }
}

#[cfg(test)]
fn take_test_counter(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(AtomicOrdering::SeqCst, AtomicOrdering::SeqCst, |count| {
            count.checked_sub(1)
        })
        .is_ok()
}

fn spawn_managed_child(
    reaper: &'static ChildReaper,
    spawn: impl FnOnce() -> std::io::Result<tokio::process::Child>,
) -> std::io::Result<ManagedChild> {
    let reaper = reaper.ensure_ready()?;
    let child = spawn()?;
    match ManagedChild::try_new(child, reaper) {
        Ok(child) => Ok(child),
        Err((error, child)) => {
            let mut child = *child;
            // Adoption failure is fail-closed. The child was never published
            // as running; terminate it and retain the Tokio handle until the
            // persistent reaper observes kernel exit.
            let _ = child.start_kill();
            reaper.enqueue(child);
            Err(error)
        }
    }
}

struct ManagedChild {
    child: Option<tokio::process::Child>,
    reaper: ReadyChildReaper,
    #[cfg(unix)]
    process_group: Option<ProcessGroup>,
    #[cfg(windows)]
    process_tree: Option<process_tree::ProcessTree>,
}

#[cfg(windows)]
fn assert_send<T: Send>() {}

#[cfg(windows)]
const _: fn() = assert_send::<ManagedChild>;

impl ManagedChild {
    fn try_new(
        child: tokio::process::Child,
        reaper: ReadyChildReaper,
    ) -> Result<Self, (std::io::Error, Box<tokio::process::Child>)> {
        #[cfg(unix)]
        let process_group = child.id().map(ProcessGroup::for_child);
        #[cfg(windows)]
        let process_tree = match process_tree::ProcessTree::adopt_child(&child) {
            Ok(tree) => Some(tree),
            Err(error) => return Err((error, Box::new(child))),
        };
        Ok(Self {
            child: Some(child),
            reaper,
            #[cfg(unix)]
            process_group,
            #[cfg(windows)]
            process_tree,
        })
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(tokio::process::Child::id)
    }

    fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut().and_then(|child| child.stdin.take())
    }

    async fn poll_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> std::io::Result<Option<ExitStatus>> {
        let child = self.child.as_mut().expect("managed child is present");
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                self.child = None;
                Ok(Some(status))
            }
            _ = tokio::time::sleep_until(deadline) => Ok(None),
        }
    }

    async fn terminate(&mut self) {
        if self.child.is_none() {
            return;
        }
        self.signal_terminate();
        let deadline = tokio::time::Instant::now() + TERMINATE_GRACE;
        if matches!(self.poll_until(deadline).await, Ok(Some(_))) {
            self.signal_kill();
            self.cleanup_process_tree_after_exit().await;
            return;
        }
        self.force_reap().await;
    }

    async fn force_reap(&mut self) {
        self.signal_kill();
        #[cfg(unix)]
        let group_deadline = tokio::time::Instant::now() + POST_KILL_REAP_GRACE;
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let outcome = post_kill_wait(child.wait(), POST_KILL_REAP_GRACE).await;
            finish_post_kill_wait(child, outcome, self.reaper);
        }
        #[cfg(unix)]
        let _ = self.wait_for_process_group_exit(group_deadline).await;
        #[cfg(windows)]
        self.process_tree.take();
    }

    async fn cleanup_process_tree_after_exit(&mut self) {
        #[cfg(unix)]
        {
            if !self.process_group_exists() {
                return;
            }
            self.signal_terminate();
            if self
                .wait_for_process_group_exit(tokio::time::Instant::now() + TERMINATE_GRACE)
                .await
            {
                return;
            }
            self.signal_kill();
            let _ = self
                .wait_for_process_group_exit(tokio::time::Instant::now() + TERMINATE_GRACE)
                .await;
        }
        #[cfg(windows)]
        {
            // A direct child may exit while descendants remain. There is no
            // safe Windows PID/group scan; terminate and close the retained
            // Job Object instead, which covers every admitted descendant.
            if let Some(tree) = self.process_tree.take() {
                let _ = tree.force_termination();
                drop(tree);
            }
        }
        #[cfg(not(any(unix, windows)))]
        let _ = self;
    }

    #[cfg(unix)]
    async fn wait_for_process_group_exit(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            if !self.process_group_exists() {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::sleep_until(deadline.min(now + CONTROL_POLL)).await;
        }
    }

    #[cfg(unix)]
    fn process_group_exists(&self) -> bool {
        self.process_group
            .as_ref()
            .is_some_and(ProcessGroup::exists)
    }

    fn signal_terminate(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.process_group.as_ref() {
            let _ = group.request_termination();
        }
        #[cfg(windows)]
        if let Some(tree) = self.process_tree.as_ref() {
            let _ = tree.request_termination();
        }
        #[cfg(not(any(unix, windows)))]
        {
            if let Some(child) = self.child.as_mut() {
                let _ = child.start_kill();
            }
        }
    }

    fn signal_kill(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.process_group.as_ref() {
            let _ = group.force_termination();
        }
        #[cfg(windows)]
        if let Some(tree) = self.process_tree.as_ref() {
            let _ = tree.force_termination();
        }
        #[cfg(not(any(unix, windows)))]
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        self.signal_kill();
        #[cfg(windows)]
        self.process_tree.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            self.reaper.enqueue(child);
        }
    }
}

async fn post_kill_wait<F>(future: F, duration: Duration) -> PostKillWait
where
    F: Future<Output = std::io::Result<ExitStatus>>,
{
    match tokio::time::timeout(duration, future).await {
        Ok(Ok(_)) => PostKillWait::Reaped,
        Ok(Err(error)) => PostKillWait::WaitError(error),
        Err(_) => PostKillWait::TimedOut,
    }
}

fn finish_post_kill_wait(
    child: tokio::process::Child,
    outcome: PostKillWait,
    reaper: ReadyChildReaper,
) {
    match outcome {
        PostKillWait::Reaped => {}
        PostKillWait::TimedOut => reaper.enqueue(child),
        PostKillWait::WaitError(error) => {
            tracing::warn!("agent-runner child wait failed; retaining it for reaping: {error}");
            reaper.enqueue(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn id(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn test_reaper() -> &'static ChildReaper {
        Box::leak(Box::new(ChildReaper::new()))
    }

    fn reap_child_command() -> tokio::process::Command {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_reap_helper",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        process_tree::configure_process_group(&mut command).unwrap();
        command
    }

    fn spawn_reap_child(_reaper: ReadyChildReaper) -> tokio::process::Child {
        reap_child_command().spawn().unwrap()
    }

    fn spawn_managed_reap_child(reaper: &'static ChildReaper) -> ManagedChild {
        let mut command = reap_child_command();
        spawn_managed_child(reaper, || command.spawn()).unwrap()
    }

    #[cfg(windows)]
    fn release_suspended_child(child: &ManagedChild) {
        let tree = child
            .process_tree
            .as_ref()
            .expect("managed child has a Windows Job Object");
        let process = child
            .child
            .as_ref()
            .expect("managed child retains its process handle");
        tree.release_child(process)
            .expect("release adopted suspended child");
    }

    #[cfg(any(unix, windows))]
    async fn wait_for_descendant_pid(path: &Path, timeout: Duration) -> u32 {
        tokio::time::timeout(timeout, async {
            loop {
                if let Ok(value) = std::fs::read_to_string(path) {
                    if let Ok(pid) = value.parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant became ready")
    }

    async fn wait_for_reaper_idle(reaper: &'static ChildReaper) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !reaper.is_idle() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child reaper became idle");
    }

    fn setup_active_runner() -> (tempfile::TempDir, RunnerStore) {
        let base = tempfile::tempdir().unwrap();
        feanorfs_client::save_config(
            base.path(),
            &feanorfs_client::Config {
                server_url: "http://127.0.0.1:1".into(),
                workspace_id: "runner-test".into(),
                encryption_password: Some("e".repeat(64)),
                server_password: None,
                tls_ca_pem: None,
                format_version: 3,
                hub_local: false,
                relay: None,
            },
        )
        .unwrap();
        let worktree = feanorfs_agent_core::agent_dir(base.path(), "worker").unwrap();
        let agent_root = worktree.parent().unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(agent_root.join("state")).unwrap();
        std::fs::write(agent_root.join("state/base-snapshot"), id('f')).unwrap();
        let program = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let store =
            RunnerStore::configure(base.path(), "worker", &program, Vec::new(), 60, &id('a'))
                .unwrap();
        store.set_enabled(true).unwrap();
        (base, store)
    }

    fn begin_active_request(session: &RunnerExecutionSession<'_>) -> (RunnerLaunch, AgentMessage) {
        let request = AgentMessage {
            message_id: id('1'),
            from: "requester".into(),
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "private request".into(),
            about_snapshot: id('f'),
            reply_to: None,
            created_at_ms: 1,
        };
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('b'),
                cursor_reset: false,
                messages: vec![request.clone()],
            })
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, std::process::id(), "test-process")
            .unwrap();
        (launch, request)
    }

    fn terminal_for(
        request: &AgentMessage,
        message_id: String,
        kind: AgentMessageKind,
    ) -> AgentMessage {
        AgentMessage {
            message_id,
            from: "worker".into(),
            to: request.from.clone(),
            kind,
            body: "legitimate terminal".into(),
            about_snapshot: request.about_snapshot.clone(),
            reply_to: Some(request.message_id.clone()),
            created_at_ms: 2,
        }
    }

    fn assert_delivery_unknown(store: &RunnerStore, launch: &RunnerLaunch, enabled: bool) {
        let status = store.status().unwrap();
        assert_eq!(status.enabled, enabled);
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::DeliveryUnknown));
        assert_eq!(status.pending_count, 1);
        assert_eq!(
            status.active_message_id.as_deref(),
            Some(launch.message_id.as_str())
        );
        assert_eq!(
            status.active_session_id.as_deref(),
            Some(launch.session_id.as_str())
        );
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
    }

    async fn wait_for_first_terminal_failure(store: &RunnerStore) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.status().unwrap().inbox_failure_count == 1 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal read entered retry backoff");
    }

    #[test]
    fn backoff_is_deterministic_and_bounded() {
        assert_eq!(backoff_duration(0), Duration::ZERO);
        assert_eq!(backoff_duration(1), Duration::from_secs(1));
        assert_eq!(backoff_duration(2), Duration::from_secs(2));
        assert_eq!(backoff_duration(7), Duration::from_secs(60));
        assert_eq!(backoff_duration(u32::MAX), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn transport_refresh_failure_keeps_the_remote_retry_path() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let request = AgentMessage {
            message_id: id('1'),
            from: "requester".into(),
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "private request".into(),
            about_snapshot: id('f'),
            reply_to: None,
            created_at_ms: 1,
        };
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('b'),
                cursor_reset: false,
                messages: vec![request],
            })
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let api = feanorfs_client::ApiClient::new(&format!("http://{address}"), None);

        let outcome = refresh_before_launch(&session, api.get_head("runner-test"))
            .await
            .unwrap();

        assert_eq!(outcome, Some(CycleOutcome::RemoteUnavailable));
        let status = store.status().unwrap();
        assert_eq!(status.pending_count, 1);
        assert!(status.active_message_id.is_none());
        assert!(status.attention.is_none());
        let retry = session.record_inbox_failure().unwrap();
        assert_eq!(
            backoff_duration(retry.inbox_failure_count),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn local_refresh_failure_stops_before_child_launch_and_requires_reset() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let request = AgentMessage {
            message_id: id('1'),
            from: "requester".into(),
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "private request".into(),
            about_snapshot: id('f'),
            reply_to: None,
            created_at_ms: 1,
        };
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('b'),
                cursor_reset: false,
                messages: vec![request],
            })
            .unwrap();

        let outcome = refresh_before_launch(
            &session,
            std::future::ready(Err::<(), _>(anyhow::anyhow!(
                "injected local runner refresh failure: private details"
            ))),
        )
        .await
        .unwrap();

        assert_eq!(outcome, Some(CycleOutcome::NeedsAttention));
        let status = store.status().unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::PreparationFailed));
        assert_eq!(status.pending_count, 1);
        assert!(status.active_message_id.is_none());
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let persisted = std::fs::read_to_string(store.path()).unwrap();
        assert!(persisted.contains("preparation_failed"));
        assert!(!persisted.contains("injected local runner refresh failure"));
        assert!(!persisted.contains("private details"));
        assert!(session.begin_next(&id('b')).is_err());

        store.set_enabled(false).unwrap();
        drop(session);
        let reset = store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(reset.phase, RunnerPhase::Idle);
        assert!(reset.attention.is_none());
        assert_eq!(reset.pending_count, 0);
    }

    #[tokio::test]
    async fn supervised_disable_interrupts_terminal_retry_backoff() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, _request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let read_attempts = Arc::clone(&attempts);
        let read = read_terminal_batch(
            &store,
            &session,
            &launch,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            move || {
                read_attempts.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<feanorfs_common::AgentInboxResult, _>(anyhow::anyhow!("hub offline"))
                }
            },
        );
        let disable = async {
            wait_for_first_terminal_failure(&store).await;
            store.set_enabled(false).unwrap();
        };
        let (result, ()) = tokio::join!(read, disable);

        assert!(matches!(
            result.unwrap(),
            TerminalReadOutcome::NeedsAttention
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_delivery_unknown(&store, &launch, false);
    }

    #[tokio::test]
    async fn shutdown_signal_interrupts_terminal_retry_backoff() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, _request) = begin_active_request(&session);
        let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let read_attempts = Arc::clone(&attempts);
        let read = read_terminal_batch(
            &store,
            &session,
            &launch,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            move || {
                read_attempts.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<feanorfs_common::AgentInboxResult, _>(anyhow::anyhow!("hub offline"))
                }
            },
        );
        let interrupt = async {
            wait_for_first_terminal_failure(&store).await;
            shutdown_sender.send(true).unwrap();
        };
        let (result, ()) = tokio::join!(read, interrupt);

        assert!(matches!(
            result.unwrap(),
            TerminalReadOutcome::NeedsAttention
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_delivery_unknown(&store, &launch, true);
    }

    #[tokio::test]
    async fn pre_cancelled_terminal_read_is_bounded_by_completion_deadline() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        store.set_enabled(false).unwrap();
        let deadline = cancellation_completion_deadline(
            &store,
            ProcessOutcome::Cancellation,
            RunnerExecutionMode::Supervised,
            &shutdown,
            Duration::from_millis(50),
        )
        .unwrap()
        .expect("disabled cancellation receives a completion deadline");
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let send_count = Arc::clone(&send_attempts);

        // The pending remote can complete only through the cancellation
        // deadline. The outer timeout is a hang guard; a tighter wall-clock
        // assertion would measure executor scheduling rather than behavior.
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            complete_request_with_remote(
                &store,
                &session,
                "worker",
                &launch,
                &request,
                ProcessOutcome::Cancellation,
                RunnerExecutionMode::Supervised,
                &shutdown,
                Some(deadline),
                std::future::pending::<anyhow::Result<feanorfs_common::AgentInboxResult>>,
                move |_, _| {
                    send_count.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<anyhow::Result<HeadConditionalSendResult>>()
                },
            ),
        )
        .await
        .expect("pre-cancelled terminal read respects its deadline")
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_eq!(send_attempts.load(Ordering::SeqCst), 0);
        assert_delivery_unknown(&store, &launch, false);
    }

    #[tokio::test]
    async fn pre_cancelled_read_and_send_share_one_completion_deadline() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        store.set_enabled(false).unwrap();
        let deadline = cancellation_completion_deadline(
            &store,
            ProcessOutcome::Cancellation,
            RunnerExecutionMode::Supervised,
            &shutdown,
            Duration::from_millis(500),
        )
        .unwrap()
        .expect("disabled cancellation receives a completion deadline");
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let send_count = Arc::clone(&send_attempts);

        let result = tokio::time::timeout_at(
            deadline + Duration::from_millis(100),
            complete_request_with_remote(
                &store,
                &session,
                "worker",
                &launch,
                &request,
                ProcessOutcome::Cancellation,
                RunnerExecutionMode::Supervised,
                &shutdown,
                Some(deadline),
                || async {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Ok(feanorfs_common::AgentInboxResult {
                        cursor: id('c'),
                        cursor_reset: false,
                        messages: Vec::new(),
                    })
                },
                move |_, _| {
                    send_count.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<anyhow::Result<HeadConditionalSendResult>>()
                },
            ),
        )
        .await
        .expect("fallback publication cannot reset the shared deadline")
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_eq!(send_attempts.load(Ordering::SeqCst), 1);
        assert_delivery_unknown(&store, &launch, false);
    }

    #[tokio::test]
    async fn conflicting_fallback_rereads_and_accepts_concurrent_terminal() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let first_head = id('c');
        let terminal_head = id('d');
        let terminal = terminal_for(&request, terminal_head.clone(), AgentMessageKind::Result);
        let read_attempts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&read_attempts);
        let expected_heads = Arc::new(Mutex::new(Vec::new()));
        let sent_heads = Arc::clone(&expected_heads);

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            move || {
                let attempt = reads.fetch_add(1, Ordering::SeqCst);
                let batch = match attempt {
                    0 => feanorfs_common::AgentInboxResult {
                        cursor: first_head.clone(),
                        cursor_reset: false,
                        messages: Vec::new(),
                    },
                    1 => feanorfs_common::AgentInboxResult {
                        cursor: terminal_head.clone(),
                        cursor_reset: false,
                        messages: vec![terminal.clone()],
                    },
                    _ => panic!("unexpected terminal reread"),
                };
                std::future::ready(Ok(batch))
            },
            move |expected, input| {
                sent_heads.lock().unwrap().push(expected);
                assert_eq!(input.kind, AgentMessageKind::Blocked);
                std::future::ready(Ok(HeadConditionalSendResult::Conflict(Some(id('d')))))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::Completed);
        assert_eq!(read_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(*expected_heads.lock().unwrap(), vec![id('c')]);
        let status = store.status().unwrap();
        assert_eq!(status.phase, RunnerPhase::Idle);
        assert_eq!(status.last_terminal_kind, Some(AgentMessageKind::Result));
    }

    #[tokio::test]
    async fn unrelated_head_conflict_retries_against_reread_head() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let read_attempts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&read_attempts);
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let sends = Arc::clone(&send_attempts);
        let expected_heads = Arc::new(Mutex::new(Vec::new()));
        let sent_heads = Arc::clone(&expected_heads);
        let about_snapshot = request.about_snapshot.clone();

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            move || {
                let cursor = match reads.fetch_add(1, Ordering::SeqCst) {
                    0 => id('c'),
                    1 => id('d'),
                    _ => panic!("unexpected terminal reread"),
                };
                std::future::ready(Ok(feanorfs_common::AgentInboxResult {
                    cursor,
                    cursor_reset: false,
                    messages: Vec::new(),
                }))
            },
            move |expected, _| {
                sent_heads.lock().unwrap().push(expected);
                let outcome = match sends.fetch_add(1, Ordering::SeqCst) {
                    0 => HeadConditionalSendResult::Conflict(Some(id('d'))),
                    1 => HeadConditionalSendResult::Sent(feanorfs_common::AgentSendResult {
                        message_id: id('e'),
                        about_snapshot: about_snapshot.clone(),
                    }),
                    _ => panic!("unexpected fallback retry"),
                };
                std::future::ready(Ok(outcome))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::Completed);
        assert_eq!(read_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(send_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(*expected_heads.lock().unwrap(), vec![id('c'), id('d')]);
        assert_eq!(
            store.status().unwrap().last_terminal_kind,
            Some(AgentMessageKind::Blocked)
        );
    }

    #[tokio::test]
    async fn fallback_conflict_retries_are_bounded() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let read_attempts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&read_attempts);
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let sends = Arc::clone(&send_attempts);
        let cursors = [id('c'), id('d'), id('e'), id('f'), id('7')];

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            move || {
                let attempt = reads.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(feanorfs_common::AgentInboxResult {
                    cursor: cursors[attempt].clone(),
                    cursor_reset: false,
                    messages: Vec::new(),
                }))
            },
            move |_, _| {
                sends.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(HeadConditionalSendResult::Conflict(Some(id('8')))))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_eq!(send_attempts.load(Ordering::SeqCst), FALLBACK_CAS_ATTEMPTS);
        assert_eq!(
            read_attempts.load(Ordering::SeqCst),
            FALLBACK_CAS_ATTEMPTS + 1
        );
        assert_delivery_unknown(&store, &launch, true);
    }

    #[tokio::test]
    async fn uncertain_fallback_publication_records_delivery_unknown() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            || {
                std::future::ready(Ok(feanorfs_common::AgentInboxResult {
                    cursor: id('c'),
                    cursor_reset: false,
                    messages: Vec::new(),
                }))
            },
            |_, _| {
                std::future::ready(Err(anyhow::anyhow!(
                    "CAS response lost after request transmission"
                )))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_delivery_unknown(&store, &launch, true);
    }

    #[tokio::test]
    async fn unresolved_post_kill_wait_is_bounded() {
        // A genuinely uninterruptible child is not portable or reliable in a
        // unit test; a pending future deterministically exercises the same
        // timeout decision that transfers the child to the detached reaper.
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            post_kill_wait(
                std::future::pending::<std::io::Result<ExitStatus>>(),
                Duration::from_millis(20),
            ),
        )
        .await
        .expect("bounded reap wait returned");
        assert!(matches!(outcome, PostKillWait::TimedOut));
    }

    #[test]
    fn reaper_initialization_failure_prevents_process_spawn() {
        let reaper = test_reaper();
        reaper.fail_next_start();
        let spawn_attempted = AtomicBool::new(false);

        let error = spawn_managed_child(reaper, || {
            spawn_attempted.store(true, Ordering::SeqCst);
            Err(std::io::Error::other("process spawn closure was called"))
        })
        .err()
        .expect("reaper initialization failed");

        assert!(error
            .to_string()
            .contains("injected reaper coordinator start failure"));
        assert!(!spawn_attempted.load(Ordering::SeqCst));
        assert_eq!(reaper.coordinator_start_count(), 0);
        assert!(!reaper.ready.load(AtomicOrdering::Acquire));
        assert!(reaper.is_idle());
    }

    #[tokio::test]
    async fn managed_child_drop_outside_runtime_recovers_poisoned_queue() {
        let reaper = test_reaper();
        let poisoned = std::panic::catch_unwind(|| {
            let _pending = reaper.pending.lock().unwrap();
            panic!("poison the isolated test reaper queue");
        });
        assert!(poisoned.is_err());
        let child = spawn_managed_reap_child(reaper);
        let pid = child.id().unwrap();
        std::thread::spawn(move || {
            assert!(tokio::runtime::Handle::try_current().is_err());
            drop(child);
        })
        .join()
        .unwrap();

        assert_eq!(reaper.transfer_count(), 1);
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(pid));
        assert_eq!(reaper.coordinator_start_count(), 1);
    }

    #[tokio::test]
    async fn post_kill_wait_error_retains_child_for_reaping() {
        let reaper = test_reaper();
        let ready = reaper.ensure_ready().unwrap();
        let mut child = spawn_reap_child(ready);
        let pid = child.id().unwrap();
        let _ = child.start_kill();
        let outcome = post_kill_wait(
            std::future::ready(Err(std::io::Error::other("injected child wait error"))),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(outcome, PostKillWait::WaitError(_)));

        finish_post_kill_wait(child, outcome, ready);
        assert_eq!(reaper.transfer_count(), 1);
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    }

    #[tokio::test]
    async fn persistent_reaper_wakes_after_draining_to_idle() {
        let reaper = test_reaper();
        let first = spawn_managed_reap_child(reaper);
        let first_pid = first.id().unwrap();
        drop(first);
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(first_pid));

        let second = spawn_managed_reap_child(reaper);
        let second_pid = second.id().unwrap();
        drop(second);
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(second_pid));
        assert_eq!(reaper.transfer_count(), 2);
        assert_eq!(reaper.coordinator_start_count(), 1);
    }

    #[tokio::test]
    async fn persistent_reaper_recovers_transient_try_wait_error() {
        let reaper = test_reaper();
        let ready = reaper.ensure_ready().unwrap();
        reaper.fail_next_try_wait();
        let mut child = spawn_reap_child(ready);
        let pid = child.id().unwrap();
        let _ = child.start_kill();
        ready.enqueue(child);

        tokio::time::timeout(Duration::from_secs(2), async {
            while reaper.error_requeue_count() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("transient try_wait error was requeued");
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(pid));
        assert_eq!(reaper.coordinator_start_count(), 1);
    }

    #[tokio::test]
    async fn persistent_reaper_recovers_child_processing_panic() {
        let reaper = test_reaper();
        let ready = reaper.ensure_ready().unwrap();
        reaper.panic_next_try_wait();
        let mut child = spawn_reap_child(ready);
        let pid = child.id().unwrap();
        let _ = child.start_kill();
        ready.enqueue(child);

        tokio::time::timeout(Duration::from_secs(2), async {
            while reaper.panic_recovery_count() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("panicking reaper retained the child and continued");
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(pid));
        assert_eq!(reaper.coordinator_start_count(), 1);
    }

    #[test]
    #[ignore]
    fn runner_reap_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(any(unix, target_os = "windows"))]
    #[test]
    #[ignore]
    fn runner_timeout_tree_helper() {
        let descendant_path = std::env::var_os("FEANORFS_RUNNER_DESCENDANT")
            .map(PathBuf::from)
            .expect("descendant pid path");
        let executable = std::env::current_exe().expect("test executable");
        let mut descendant = std::process::Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_timeout_descendant_helper",
                "--nocapture",
            ])
            .spawn()
            .expect("spawn descendant");
        std::fs::write(descendant_path, descendant.id().to_string())
            .expect("record descendant pid");
        std::thread::sleep(Duration::from_secs(30));
        let _ = descendant.wait();
    }

    #[cfg(any(unix, target_os = "windows"))]
    #[test]
    #[ignore]
    fn runner_timeout_descendant_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(any(unix, target_os = "windows"))]
    #[tokio::test]
    async fn timeout_kills_the_child_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let descendant_path = temp.path().join("descendant.pid");
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_timeout_tree_helper",
                "--nocapture",
            ])
            .env("FEANORFS_RUNNER_DESCENDANT", &descendant_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        process_tree::configure_process_group(&mut command).unwrap();
        let reaper = test_reaper();
        let mut child = spawn_managed_child(reaper, || command.spawn()).unwrap();
        // `configure_process_group` creates a suspended child on Windows so
        // adoption is atomic with respect to user code. These tests bypass
        // `run_configured_process`, so release the verified Job-owned child
        // explicitly before waiting for the helper's readiness marker.
        #[cfg(windows)]
        release_suspended_child(&child);
        let descendant = wait_for_descendant_pid(&descendant_path, Duration::from_secs(5)).await;
        let outcome = wait_for_child_until(
            &mut child,
            tokio::time::Instant::now() + Duration::from_millis(50),
            || Ok(false),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ProcessOutcome::Timeout);
        let dead_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while feanorfs_agent_core::lock::pid_alive(descendant)
            && tokio::time::Instant::now() < dead_deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!feanorfs_agent_core::lock::pid_alive(descendant));
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn runner_direct_exit_tree_helper() {
        let descendant_path = std::env::var_os("FEANORFS_RUNNER_DESCENDANT")
            .map(PathBuf::from)
            .expect("descendant pid path");
        let executable = std::env::current_exe().expect("test executable");
        let descendant = std::process::Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_timeout_descendant_helper",
                "--nocapture",
            ])
            .spawn()
            .expect("spawn descendant");
        std::fs::write(descendant_path, descendant.id().to_string())
            .expect("record descendant pid");
        // Returning without waiting exercises the direct-child-exit path; the
        // retained Job Object must still terminate the surviving descendant.
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn direct_child_exit_kills_job_owned_descendant() {
        let temp = tempfile::tempdir().unwrap();
        let descendant_path = temp.path().join("descendant.pid");
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_direct_exit_tree_helper",
                "--nocapture",
            ])
            .env("FEANORFS_RUNNER_DESCENDANT", &descendant_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Windows children must enter the suspended/adopted production path
        // before the helper can spawn its surviving descendant. Without this
        // call the test would exercise an unsuspended process outside the Job
        // Object ownership protocol it is intended to verify.
        process_tree::configure_process_group(&mut command).unwrap();
        let reaper = test_reaper();
        let mut child = spawn_managed_child(reaper, || command.spawn()).unwrap();
        // See the timeout test above: direct test spawning bypasses the
        // production startup gate, so the adopted suspended process must be
        // released explicitly after Job membership is verified.
        release_suspended_child(&child);
        // Observe the descendant's readiness before waiting for the helper.
        // Otherwise the direct-child-exit cleanup can close the Job Object in
        // the small window between the helper exiting and its marker write.
        let descendant = wait_for_descendant_pid(&descendant_path, Duration::from_secs(5)).await;
        let outcome = wait_for_child_until(
            &mut child,
            tokio::time::Instant::now() + Duration::from_secs(5),
            || Ok(false),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ProcessOutcome::Exited);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while feanorfs_agent_core::lock::pid_alive(descendant)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!feanorfs_agent_core::lock::pid_alive(descendant));
    }
}
