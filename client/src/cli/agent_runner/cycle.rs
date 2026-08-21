//! Continuous cycle, admission gate, and backoff for the runner worker.

use super::super::agent_live::{spawn_runner_controller, RunnerControllerHandle};
use super::inbox::{
    admit_batch, read_runner_inbox, resolve_next_admission, wait_kind_for, NextAdmission,
};
use super::process::{run_configured_process, ProcessOutcome, MAX_INVOCATION_BYTES};
use super::remote::complete_request;
use super::render::{report_attention, require_canonical_workspace};
use anyhow::{ensure, Context as _};
use feanorfs_agent_core::{
    refresh_agent_guarded, RunnerConfig, RunnerExecutionMode, RunnerExecutionSession,
    RunnerInvocation, RunnerOwnership, RunnerStore, RunnerWorkWait,
};
use feanorfs_client::backoff::{BackoffGrowth, BackoffReset, ExponentialBackoff};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

const IDLE_POLL: Duration = Duration::from_millis(500);
pub(super) const CONTROL_POLL: Duration = Duration::from_millis(100);
/// Inbox/transport retry backoff: base 1 s doubling from the second failure,
/// 60 s cap, immediate reset. Sequence (failures 0..): 0, 1, 2, 4, 8, 16, 32,
/// 60, 60, ... One call site additionally caps the delay at 8 s.
pub(super) const RUNNER_BACKOFF: ExponentialBackoff =
    ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60))
        .with_growth(BackoffGrowth::DoublesFromSecondFailure)
        .with_reset(BackoffReset::Immediate);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CycleOutcome {
    Idle,
    Completed,
    RemoteUnavailable,
    Stop,
    NeedsAttention,
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
    // Continuous reconciliation runs for the configured agent for this
    // worker's lifetime; the runner lifetime lease remains the authority and
    // the controller revalidates its ownership token before every mutation.
    let controller = spawn_runner_controller(
        &workspace_root,
        &runner_config.agent,
        RunnerOwnership::from_session(&session),
        shutdown.clone(),
    )
    .await
    .context("start continuous reconciliation controller")?;
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
                        RUNNER_BACKOFF.delay(status.inbox_failure_count),
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
            &controller,
        )
        .await?;

        match outcome {
            CycleOutcome::Idle | CycleOutcome::Completed => {
                if store.status()?.inbox_failure_count > 0 {
                    session.record_inbox_recovery()?;
                }
                if outcome == CycleOutcome::Idle
                    && wait_for_head_wakeup(&controller, &store, mode, &shutdown).await?
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
                    RUNNER_BACKOFF.delay(status.inbox_failure_count),
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
    controller: &RunnerControllerHandle,
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

    // Enforcement gate: resolve the next pending request to its accepted
    // intent through the reducer projection (recomputed after refresh and
    // inbox re-read). No process is launched for any invalid state; a typed
    // work wait keeps the runner live until the next head change.
    let admission = match resolve_next_admission(
        workspace_root,
        workspace_config,
        db,
        api,
        store,
        &current,
        runner_config,
    )
    .await?
    {
        NextAdmission::Launch(descriptor) => Some(descriptor),
        NextAdmission::NotApplicable => None,
        NextAdmission::Wait { message_id, reason } => {
            session.record_work_wait(&RunnerWorkWait {
                kind: wait_kind_for(reason),
                message_id,
                reason: Some(reason),
                out_of_scope_count: 0,
                observed_at_ms: chrono::Utc::now().timestamp_millis(),
            })?;
            tracing::warn!(
                reason = reason.as_str(),
                "agent runner waits for accepted work before launching"
            );
            return Ok(CycleOutcome::Idle);
        }
        NextAdmission::Unavailable => return Ok(CycleOutcome::RemoteUnavailable),
    };
    let launch = match admission {
        Some(descriptor) => match session.begin_next_admitted(&current.cursor, descriptor) {
            Ok(launch) => launch,
            Err(_) if stopped_after_state_error(store, mode)? => return Ok(CycleOutcome::Stop),
            Err(error) => return Err(error).context("checkpoint the next admitted runner request"),
        },
        None => match session.begin_next(&current.cursor) {
            Ok(launch) => launch,
            Err(_) if stopped_after_state_error(store, mode)? => return Ok(CycleOutcome::Stop),
            Err(error) => return Err(error).context("checkpoint the next runner request"),
        },
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
                controller,
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
        controller,
    )
    .await
}

pub(super) async fn refresh_before_launch<T>(
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

pub(super) fn stopped_after_state_error(
    store: &RunnerStore,
    mode: RunnerExecutionMode,
) -> anyhow::Result<bool> {
    Ok(mode == RunnerExecutionMode::Supervised && !store.status()?.enabled)
}

pub(super) fn should_stop(
    store: &RunnerStore,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    if *shutdown.borrow() {
        return Ok(true);
    }
    Ok(mode == RunnerExecutionMode::Supervised && !store.status()?.enabled)
}

pub(super) async fn wait_interruptible(
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

/// Waits for the continuous controller to observe an opaque head change
/// (files or signals). Returns `true` on shutdown. The local control poll
/// only re-checks runner state — never the network inbox.
pub(super) async fn wait_for_head_wakeup(
    controller: &RunnerControllerHandle,
    store: &RunnerStore,
    mode: RunnerExecutionMode,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    let mut generation = controller.head_generation.clone();
    let mut shutdown = shutdown.clone();
    loop {
        tokio::select! {
            changed = generation.changed() => {
                if changed.is_err() {
                    if *shutdown.borrow() {
                        return Ok(true);
                    }
                    return Err(controller.stopped_error(
                        "continuous controller stopped while the runner was waiting",
                    ));
                }
                generation.borrow_and_update();
                return Ok(false);
            }
            _ = shutdown.changed() => return Ok(true),
            _ = tokio::time::sleep(IDLE_POLL) => {
                if should_stop(store, mode, &shutdown)? {
                    return Ok(true);
                }
            }
        }
    }
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
