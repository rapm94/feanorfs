//! Configured-process invocation I/O and completion deadlines.

use super::super::process_tree;
#[cfg(unix)]
use super::super::process_tree::ProcessGroup;
use super::super::process_tree::{ChildReaper, ReadyChildReaper, CHILD_REAPER};
use super::cycle::{should_stop, CONTROL_POLL};
#[cfg(all(unix, not(test)))]
use anyhow::Context as _;
use feanorfs_agent_core::{
    RunnerConfig, RunnerExecutionMode, RunnerExecutionSession, RunnerLaunch, RunnerStore,
};
use std::future::Future;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

const TERMINATE_GRACE: Duration = Duration::from_secs(2);
const POST_KILL_REAP_GRACE: Duration = Duration::from_secs(1);
pub(super) const MAX_INVOCATION_BYTES: usize =
    feanorfs_common::AGENT_MESSAGE_MAX_ENCODED_BYTES + 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessOutcome {
    StartFailure,
    StdinFailure,
    Exited,
    Timeout,
    Cancellation,
}

impl ProcessOutcome {
    pub(super) const fn blocked_body(self) -> &'static str {
        match self {
            Self::StartFailure => "runner blocked: process start failed",
            Self::StdinFailure => "runner blocked: invocation delivery failed",
            Self::Exited => "runner blocked: process exited without a correlated terminal",
            Self::Timeout => "runner blocked: execution timed out",
            Self::Cancellation => "runner blocked: execution cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvocationWrite {
    Written,
    Failed,
    TimedOut,
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_configured_process(
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

pub(super) async fn wait_for_child_until(
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

pub(super) enum PostKillWait {
    Reaped,
    TimedOut,
    WaitError(std::io::Error),
}

pub(super) fn spawn_managed_child(
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

pub(super) struct ManagedChild {
    pub(super) child: Option<tokio::process::Child>,
    reaper: ReadyChildReaper,
    #[cfg(unix)]
    process_group: Option<ProcessGroup>,
    #[cfg(windows)]
    pub(super) process_tree: Option<process_tree::ProcessTree>,
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

    pub(super) fn id(&self) -> Option<u32> {
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

pub(super) async fn post_kill_wait<F>(future: F, duration: Duration) -> PostKillWait
where
    F: Future<Output = std::io::Result<ExitStatus>>,
{
    match tokio::time::timeout(duration, future).await {
        Ok(Ok(_)) => PostKillWait::Reaped,
        Ok(Err(error)) => PostKillWait::WaitError(error),
        Err(_) => PostKillWait::TimedOut,
    }
}

pub(super) fn finish_post_kill_wait(
    child: tokio::process::Child,
    outcome: PostKillWait,
    reaper: ReadyChildReaper,
) {
    match outcome {
        PostKillWait::Reaped => {}
        PostKillWait::TimedOut => {
            let _ = reaper.enqueue(child);
        }
        PostKillWait::WaitError(error) => {
            tracing::warn!("agent-runner child wait failed; retaining it for reaping: {error}");
            let _ = reaper.enqueue(child);
        }
    }
}
