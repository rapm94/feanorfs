//! Desired child specs and the child restart state machine.
//!
//! `desired_specs` projects the registry plus hub/tray presence into the
//! desired child set; `ManagedChild` tracks each child's exact process
//! identity, process-tree ownership, startup gate, and bounded restart
//! backoff. Termination keeps ownership until the child is reaped or handed
//! to the persistent reaper in [`crate::cli::process_tree`].

use anyhow::{ensure, Context as _};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::cli::process_tree;
use crate::cli::process_tree::{ReapTicket, CHILD_REAPER};
use feanorfs_client::workspace_path::CanonicalWorkspacePath;

use super::*;

#[derive(Debug, Clone)]
pub(super) struct ChildSpec {
    pub(super) kind: ChildKind,
    pub(super) program: PathBuf,
    pub(super) args: Vec<OsString>,
    pub(super) env: Vec<(OsString, OsString)>,
    pub(super) restart_on_zero_exit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChildKind {
    Hub,
    Tray,
    /// The canonical workspace path this child owns. Held as the exact
    /// contract-boundary type so runner ownership and status comparisons can
    /// never observe a lossy-mangled path.
    Workspace(CanonicalWorkspacePath),
    Runner(CanonicalWorkspacePath),
}

pub(super) fn workspace_child_key(workspace: &str) -> String {
    format!("{WORKSPACE_CHILD_PREFIX}{workspace}")
}

pub(super) fn runner_child_key(workspace: &str) -> String {
    format!("{RUNNER_CHILD_PREFIX}{workspace}")
}

pub(super) fn managed_tray_args() -> Vec<OsString> {
    vec![OsString::from(
        feanorfs_common::tray_contract::MANAGED_TRAY_ARG,
    )]
}

pub(super) fn managed_tray_command_line(program: &Path) -> String {
    let mut command = program.display().to_string();
    for argument in managed_tray_args() {
        command.push(' ');
        command.push_str(&argument.to_string_lossy());
    }
    command
}

pub(super) struct ManagedChild {
    pub(super) spec: ChildSpec,
    /// Target image identity captured from `spec.program` before launch.  A
    /// Unix startup gate initially runs this binary's wrapper, so status must
    /// never infer the durable worker identity from the pre-exec PID.
    pub(super) expected_executable_identity: Option<String>,
    pub(super) child: Option<tokio::process::Child>,
    /// Set when termination had to defer the kernel wait to the persistent
    /// reaper. The ManagedChild remains in the supervisor map until this
    /// ticket completes, so reconciliation cannot acknowledge a runner stop
    /// while the exact child is still owned elsewhere.
    pub(super) pending_reap: Option<ReapTicket>,
    /// A child recorded by a previous supervisor but not safely reaped at
    /// startup. PID-only orphan cleanup is deliberately represented in the
    /// same map as live children: until this ticket completes, the exact
    /// runner authority remains stopping and cannot be acknowledged or
    /// respawned.
    pub(super) pending_orphan: Option<PendingOrphanCleanup>,
    /// Native process-tree ownership for children spawned by this
    /// supervisor. Unix uses a fresh process group; Windows uses a private
    /// Job Object. Retain it through root wait and residual verification.
    pub(super) process_tree: Option<process_tree::ProcessTree>,
    /// Child has entered the trusted internal startup wrapper but has not yet
    /// been released to exec its configured worker. Keep the gate owned until
    /// the status/identity ledger is durably published.
    pub(super) startup_gate: Option<process_tree::StartupGate>,
    /// Durable identity of the exact direct child, retained after its Tokio
    /// handle is handed to the background reaper. These fields are projected
    /// into `supervisor-status.json` so a replacement supervisor can retry
    /// the same process instead of treating `pid: null` as completed.
    pub(super) owned_pid: Option<u32>,
    pub(super) owned_process_start_id: Option<String>,
    pub(super) owned_since: u64,
    pub(super) state: ChildState,
    pub(super) restarts: u32,
    pub(super) last_exit: Option<i32>,
    pub(super) backoff_until: Option<Instant>,
    pub(super) spawned_at: Option<Instant>,
}

#[cfg(windows)]
fn assert_managed_child_send<T: Send>() {}

#[cfg(windows)]
const _: fn() = assert_managed_child_send::<ManagedChild>;

impl ManagedChild {
    pub(super) fn new(spec: ChildSpec) -> Self {
        let expected_executable_identity =
            process_tree::executable_identity_for_path(&spec.program);
        Self {
            spec,
            expected_executable_identity,
            child: None,
            pending_reap: None,
            pending_orphan: None,
            process_tree: None,
            startup_gate: None,
            owned_pid: None,
            owned_process_start_id: None,
            owned_since: 0,
            state: ChildState::Stopped,
            restarts: 0,
            last_exit: None,
            backoff_until: None,
            spawned_at: None,
        }
    }
}

pub(super) async fn await_reap_ticket(ticket: &ReapTicket) {
    while !ticket.is_complete() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// True when a desired-but-missing child should be spawned on this poll.
///
/// A child that exited cleanly and opted out of restarts (e.g. the user quit
/// the tray, or the hub shut down on purpose) must stay stopped: without this
/// the desired-set diff respawns it within one poll, and a clean-exit child
/// would spin without ever engaging backoff.
pub(super) fn should_respawn(managed: &ManagedChild) -> bool {
    if managed.child.is_some()
        || managed.pending_reap.is_some()
        || managed.pending_orphan.is_some()
        || managed.process_tree.is_some()
        || managed.startup_gate.is_some()
        || managed.state == ChildState::Stopping
    {
        return false;
    }
    if managed.last_exit == Some(0) && !managed.spec.restart_on_zero_exit {
        return false;
    }
    managed
        .backoff_until
        .is_none_or(|until| Instant::now() >= until)
}

pub(super) fn desired_specs(
    registry: &SupervisorRegistry,
    tray_program: &Option<PathBuf>,
) -> anyhow::Result<BTreeMap<String, ChildSpec>> {
    let mut desired = BTreeMap::new();
    let program = std::env::current_exe().context("locate the feanorfs executable")?;

    if crate::cli::hub_service::hub_data_present() {
        let data_dir = crate::cli::hub_service::default_data_dir()?;
        desired.insert(
            HUB_CHILD_KEY.into(),
            ChildSpec {
                kind: ChildKind::Hub,
                program: program.clone(),
                args: vec![
                    OsString::from("service"),
                    OsString::from("hub-run"),
                    data_dir.into_os_string(),
                ],
                env: Vec::new(),
                restart_on_zero_exit: true,
            },
        );
    }
    // macOS and Linux run the tray inside the supervisor job; Windows keeps
    // its own scheduled task (see `install_tray_if_available`), so spawning a
    // tray child here too would run two trays on Windows. The tray binary is
    // resolved once by the supervisor, not on every 500 ms poll.
    #[cfg(not(target_os = "windows"))]
    if let Some(tray) = tray_program {
        desired.insert(
            TRAY_CHILD_KEY.into(),
            ChildSpec {
                kind: ChildKind::Tray,
                program: tray.clone(),
                args: managed_tray_args(),
                env: vec![(
                    OsString::from("FEANORFS_BIN"),
                    program.clone().into_os_string(),
                )],
                restart_on_zero_exit: false,
            },
        );
    }
    for path in &registry.workspaces {
        let workspace = path.as_path();
        if !workspace.is_dir() || !feanorfs_agent_core::workspace_is_configured(workspace) {
            continue;
        }
        desired.insert(
            workspace_child_key(path.as_str()),
            ChildSpec {
                kind: ChildKind::Workspace(path.clone()),
                program: program.clone(),
                args: vec![
                    OsString::from("service"),
                    OsString::from("run"),
                    workspace.as_os_str().to_owned(),
                ],
                env: Vec::new(),
                restart_on_zero_exit: true,
            },
        );
    }
    for path in &registry.runners {
        let workspace = path.as_path();
        if !workspace.is_dir() || !feanorfs_agent_core::workspace_is_configured(workspace) {
            continue;
        }
        if workspace.canonicalize().ok().as_deref() != Some(workspace) {
            tracing::warn!("ignoring non-canonical runner workspace in supervisor registry");
            continue;
        }
        let Ok(Some(status)) = feanorfs_agent_core::runner_status(workspace) else {
            continue;
        };
        if !status.enabled || status.phase == feanorfs_agent_core::RunnerPhase::NeedsAttention {
            continue;
        }
        desired.insert(
            runner_child_key(path.as_str()),
            ChildSpec {
                kind: ChildKind::Runner(path.clone()),
                program: program.clone(),
                args: vec![
                    OsString::from("service"),
                    OsString::from("runner-run"),
                    workspace.as_os_str().to_owned(),
                ],
                env: Vec::new(),
                restart_on_zero_exit: true,
            },
        );
    }
    Ok(desired)
}

pub(super) fn hub_config_mtime(relative: &str) -> Option<u64> {
    crate::cli::hub_service::default_data_dir()
        .ok()
        .map(|dir| dir.join(relative))
        .and_then(|path| fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
}

pub(super) struct SpawnedChild {
    pub(super) child: tokio::process::Child,
    pub(super) process_tree: process_tree::ProcessTree,
    pub(super) startup_gate: process_tree::StartupGate,
}

pub(super) fn exact_child_process_start_id(pid: u32) -> Option<String> {
    let identity = process_tree::process_start_identifier(pid, "supervisor-child");
    process_tree::process_start_matches(pid, &identity).then_some(identity)
}

pub(super) async fn spawn_child(spec: &ChildSpec) -> anyhow::Result<SpawnedChild> {
    #[cfg(all(unix, not(test)))]
    let wrapper_program = std::env::current_exe().context("locate feanorfs startup wrapper")?;
    #[cfg(all(unix, not(test)))]
    let mut command = tokio::process::Command::new(wrapper_program);
    #[cfg(any(not(unix), test))]
    let mut command = tokio::process::Command::new(&spec.program);
    command
        .envs(spec.env.iter().cloned())
        .stdin(std::process::Stdio::null());
    process_tree::configure_process_group(&mut command)
        .context("prepare supervised worker for fail-closed process ownership")?;
    #[cfg(not(test))]
    let startup_gate = process_tree::StartupGate::prepare(&mut command)
        .context("prepare durable supervised-worker startup gate")?;
    #[cfg(test)]
    let startup_gate = process_tree::StartupGate::disabled();
    #[cfg(all(unix, not(test)))]
    {
        command.args([
            OsString::from("service"),
            OsString::from("exec-gate"),
            OsString::from(startup_gate.release_fd().to_string()),
            spec.program.as_os_str().to_owned(),
            OsString::from("--"),
        ]);
        command.args(&spec.args);
    }
    #[cfg(any(not(unix), test))]
    command.args(&spec.args);
    let child = command
        .spawn()
        .with_context(|| format!("start supervised worker {}", spec.program.display()))?;
    let mut child = child;
    let process_tree = match process_tree::ProcessTree::adopt_child(&child) {
        Ok(tree) => tree,
        Err(error) => {
            drop(startup_gate);
            let _ = child.start_kill();
            match tokio::time::timeout(CHILD_REAP_GRACE, child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(wait_error)) => {
                    tracing::warn!(
                        "supervisor child adoption failed and wait failed; retaining child: {wait_error}"
                    );
                    let mut child = Some(child);
                    let ticket = CHILD_REAPER.enqueue_or_wait(&mut child).await;
                    await_reap_ticket(&ticket).await;
                }
                Err(_) => {
                    tracing::warn!(
                        "supervisor child adoption failed before bounded reap; retaining child"
                    );
                    let mut child = Some(child);
                    let ticket = CHILD_REAPER.enqueue_or_wait(&mut child).await;
                    await_reap_ticket(&ticket).await;
                }
            }
            return Err(error).context("adopt supervised worker into a private Windows Job Object");
        }
    };
    Ok(SpawnedChild {
        child,
        process_tree,
        startup_gate,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildWaitOutcome {
    Reaped,
    TimedOut,
    WaitFailed,
}

/// A forceful escalation is safe only while both the native process token and
/// the supervisor-owned executable/argv still match.
pub(super) fn force_termination_allowed(identity_current: bool, command_current: bool) -> bool {
    identity_current && command_current
}

pub(super) fn mark_finished_child_exit(managed: &mut ManagedChild) -> anyhow::Result<()> {
    managed.state = ChildState::Stopping;
    // A gate whose wrapper has already exited cannot release a target; drop
    // its endpoints so no stale capability survives into a later respawn.
    managed.startup_gate.take();
    ensure_managed_process_tree_reaped(managed)?;
    finish_runner_worker_exit(&managed.spec)?;
    managed.state = if managed.backoff_until.is_some() {
        ChildState::Backoff
    } else {
        ChildState::Stopped
    };
    managed.owned_pid = None;
    managed.owned_process_start_id = None;
    managed.owned_since = 0;
    Ok(())
}

/// Confirm that the native process-tree ownership boundary is gone before a
/// child can leave `Stopping`. On Unix the root wait is insufficient: a
/// descendant can keep the dedicated process group alive after the direct
/// child exits. Force the owned group and verify disappearance with a bounded
/// wait. Windows Job Objects are closed only after the force request (the
/// kernel ownership boundary has no PID/group scan equivalent).
fn ensure_managed_process_tree_reaped(managed: &mut ManagedChild) -> anyhow::Result<()> {
    let Some(tree) = managed.process_tree.as_ref() else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        if tree.exists() {
            let _ = tree.force_termination();
            let deadline = Instant::now() + CHILD_REAP_GRACE;
            while tree.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        ensure!(
            !tree.exists(),
            "supervised child process group remains alive after force termination"
        );
    }
    #[cfg(target_os = "windows")]
    {
        let deadline = Instant::now() + CHILD_REAP_GRACE;
        loop {
            match tree.is_empty() {
                Ok(true) => break,
                Ok(false) => {
                    let _ = tree.force_termination();
                    if Instant::now() >= deadline {
                        anyhow::bail!(
                            "supervised child Job Object still has active processes after force termination"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    anyhow::bail!(
                        "could not verify supervised child Job Object emptiness: {error}"
                    );
                }
            }
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = tree;
        anyhow::bail!("supervised child process-tree cleanup is unavailable on this platform");
    }
    managed.process_tree = None;
    Ok(())
}

/// Advance a child whose exact process handle was handed to the persistent
/// reaper.  The stopping entry remains in the supervisor map until both the
/// kernel wait and runner residual cleanup complete, which gates the durable
/// registry acknowledgement and prevents a stale desired-set diff from
/// respawning it.
pub(super) fn poll_stopping_child(managed: &mut ManagedChild) -> anyhow::Result<bool> {
    if managed.state != ChildState::Stopping {
        return Ok(false);
    }
    if let Some(ticket) = managed.pending_reap.as_ref() {
        if !ticket.is_complete() {
            return Ok(false);
        }
        managed.pending_reap = None;
    }
    if let Some(orphan) = managed.pending_orphan.as_mut() {
        retry_one_pending_orphan_cleanup(orphan);
        if !orphan.ticket.is_complete() {
            return Ok(false);
        }
        managed.pending_orphan = None;
    }
    mark_finished_child_exit(managed)?;
    Ok(true)
}

fn termination_grace() -> Duration {
    #[cfg(test)]
    {
        let millis = TEST_TERMINATION_GRACE_MILLIS.load(AtomicOrdering::Acquire);
        if millis != 0 {
            return Duration::from_millis(millis);
        }
    }
    STOP_GRACE
}

async fn reap_after_force(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> ChildWaitOutcome {
    #[cfg(test)]
    if TEST_FORCE_REAP_TIMEOUT.swap(false, AtomicOrdering::AcqRel) {
        return ChildWaitOutcome::TimedOut;
    }
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(_)) => ChildWaitOutcome::Reaped,
        Ok(Err(error)) => {
            tracing::warn!("supervisor child wait failed after force termination: {error}");
            ChildWaitOutcome::WaitFailed
        }
        Err(_) => ChildWaitOutcome::TimedOut,
    }
}

/// Terminate a managed child: request graceful termination, escalate only
/// while its exact identity remains current, and always retain ownership until
/// it is reaped or transferred to the persistent reaper.
struct ChildHandleGuard<'a> {
    managed: &'a mut ManagedChild,
    child: Option<tokio::process::Child>,
}

impl Drop for ChildHandleGuard<'_> {
    fn drop(&mut self) {
        // `terminate_child` contains cancellation points while waiting for a
        // process and while handing it to the persistent reaper. If its task
        // is cancelled at either point, put the exact Tokio handle back into
        // the ManagedChild before the outer shutdown guard recovers it. A
        // dropped Child would otherwise lose the only wait-safe ownership of
        // a still-live process.
        if let Some(child) = self.child.take() {
            self.managed.child = Some(child);
        }
    }
}

pub(super) async fn terminate_child(managed: &mut ManagedChild) -> anyhow::Result<()> {
    // A child still waiting in the internal startup wrapper must fail closed
    // when no longer desired. Closing the gate makes it exit without ever
    // executing configured worker code.
    managed.startup_gate.take();
    if managed.state == ChildState::Stopping {
        if let Some(ticket) = managed.pending_reap.as_ref() {
            if !ticket.is_complete() {
                // The reaper still owns the exact Child handle. Keep this
                // stopping entry in the map and let the next reconciliation
                // pass consume its completion signal.
                return Ok(());
            }
            managed.pending_reap = None;
        }
        if let Some(orphan) = managed.pending_orphan.as_mut() {
            retry_one_pending_orphan_cleanup(orphan);
            if !orphan.ticket.is_complete() {
                return Ok(());
            }
            managed.pending_orphan = None;
        }
        if managed.child.is_none() {
            mark_finished_child_exit(managed)?;
            return Ok(());
        }
    }
    if managed.child.is_none() {
        managed.state = ChildState::Stopped;
        return Ok(());
    }
    if managed.owned_pid.is_none() {
        let pid = managed.child.as_ref().and_then(|child| child.id());
        managed.owned_pid = pid;
        managed.owned_process_start_id = pid.and_then(exact_child_process_start_id);
        managed.owned_since = now_epoch();
    }
    let child = managed.child.take().expect("managed child checked above");
    let mut guard = ChildHandleGuard {
        managed,
        child: Some(child),
    };
    if let Some(tree) = guard.managed.process_tree.as_ref() {
        // The native tree boundary covers descendants. Keep it attached to
        // the ManagedChild until explicit disappearance has been verified;
        // dropping it merely because the direct root exits would lose that
        // ownership proof.
        let _ = tree.request_termination();
    }
    #[cfg(unix)]
    let identity = guard
        .child
        .as_ref()
        .and_then(|child| child.id().and_then(capture_owned_identity));
    #[cfg(unix)]
    if let Some(identity) = identity.as_ref() {
        let _ = identity.request_termination();
    } else {
        // This is still the direct Child handle owned by this supervisor, so
        // Tokio's handle-safe kill is preferable to leaving it unsignalled.
        // Exact identity checks remain mandatory for orphan/PID-only cleanup.
        let _ = guard
            .child
            .as_mut()
            .expect("managed child handle retained during termination")
            .start_kill();
    }
    #[cfg(not(unix))]
    {
        let _ = guard
            .child
            .as_mut()
            .expect("managed child handle retained during termination")
            .start_kill();
    }

    let deadline = Instant::now() + termination_grace();
    let mut wait_error = None;
    loop {
        match guard
            .child
            .as_mut()
            .expect("managed child handle retained during termination")
            .try_wait()
        {
            Ok(Some(_)) => {
                if let Err(error) = ensure_managed_process_tree_reaped(guard.managed) {
                    guard.managed.state = ChildState::Stopping;
                    return Err(error);
                }
                mark_finished_child_exit(guard.managed)?;
                guard.child.take();
                return wait_error.map_or(Ok(()), |error| {
                    Err(anyhow::anyhow!("supervisor child wait failed: {error}"))
                });
            }
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(None) => {
                #[cfg(unix)]
                if let Some(identity) = identity.as_ref() {
                    if force_termination_allowed(identity.is_current(), true) {
                        let _ = identity.force_termination();
                    } else {
                        tracing::warn!(
                            "leaving managed child alive because its exact identity changed before force escalation"
                        );
                    }
                } else {
                    let _ = guard
                        .child
                        .as_mut()
                        .expect("managed child handle retained during force escalation")
                        .start_kill();
                }
                #[cfg(not(unix))]
                {
                    let _ = guard
                        .child
                        .as_mut()
                        .expect("managed child handle retained during force escalation")
                        .start_kill();
                }
                if let Some(tree) = guard.managed.process_tree.as_ref() {
                    let _ = tree.force_termination();
                }
                let outcome = reap_after_force(
                    guard
                        .child
                        .as_mut()
                        .expect("managed child handle retained during force reap"),
                    CHILD_REAP_GRACE,
                )
                .await;
                match outcome {
                    ChildWaitOutcome::Reaped => {
                        if let Err(error) = ensure_managed_process_tree_reaped(guard.managed) {
                            guard.managed.state = ChildState::Stopping;
                            return Err(error);
                        }
                        mark_finished_child_exit(guard.managed)?;
                        guard.child.take();
                        return wait_error.map_or(Ok(()), |error| {
                            Err(anyhow::anyhow!("supervisor child wait failed: {error}"))
                        });
                    }
                    ChildWaitOutcome::TimedOut | ChildWaitOutcome::WaitFailed => {
                        guard.managed.state = ChildState::Stopping;
                        let ticket = CHILD_REAPER.enqueue_or_wait(&mut guard.child).await;
                        guard.managed.pending_reap = Some(ticket);
                        return Ok(());
                    }
                }
            }
            Err(error) => {
                wait_error = Some(error);
                #[cfg(unix)]
                if let Some(identity) = identity.as_ref() {
                    if force_termination_allowed(identity.is_current(), true) {
                        let _ = identity.force_termination();
                    } else {
                        tracing::warn!(
                            "leaving managed child alive because its exact identity changed after wait failure"
                        );
                    }
                } else {
                    let _ = guard
                        .child
                        .as_mut()
                        .expect("managed child handle retained after wait failure")
                        .start_kill();
                }
                #[cfg(not(unix))]
                {
                    let _ = guard
                        .child
                        .as_mut()
                        .expect("managed child handle retained after wait failure")
                        .start_kill();
                }
                if let Some(tree) = guard.managed.process_tree.as_ref() {
                    let _ = tree.force_termination();
                }
                let outcome = reap_after_force(
                    guard
                        .child
                        .as_mut()
                        .expect("managed child handle retained after wait failure"),
                    CHILD_REAP_GRACE,
                )
                .await;
                match outcome {
                    ChildWaitOutcome::Reaped => {
                        if let Err(cleanup_error) =
                            ensure_managed_process_tree_reaped(guard.managed)
                        {
                            guard.managed.state = ChildState::Stopping;
                            return Err(cleanup_error);
                        }
                        mark_finished_child_exit(guard.managed)?;
                        guard.child.take();
                        return Err(anyhow::anyhow!(
                            "supervisor child wait failed before bounded reap: {}",
                            wait_error.expect("wait error recorded")
                        ));
                    }
                    ChildWaitOutcome::TimedOut | ChildWaitOutcome::WaitFailed => {
                        guard.managed.state = ChildState::Stopping;
                        let ticket = CHILD_REAPER.enqueue_or_wait(&mut guard.child).await;
                        guard.managed.pending_reap = Some(ticket);
                        return Ok(());
                    }
                }
            }
        }
    }
}

pub(super) fn process_command_ownership_matches(
    pid: u32,
    expected_executable: &Path,
    expected_command: &str,
) -> bool {
    if !feanorfs_agent_core::lock::pid_alive(pid) {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        return process_tree::executable_matches(pid, expected_executable);
    }
    let expected = std::fs::canonicalize(expected_executable).ok();
    let executable_matches = expected.is_some()
        && process_executable(pid).and_then(|actual| std::fs::canonicalize(actual).ok())
            == expected;
    executable_matches && process_command_line(pid).as_deref() == Some(expected_command)
}

/// Revalidates a persisted executable identity and the worker's exact
/// subcommand/operand.  When an in-place upgrade unlinks the old image,
/// `ps` may append ` (deleted)` to the executable token; the native inode
/// identity remains the authority, while the suffix below continues to bind
/// every configured argv value.
fn process_command_ownership_matches_with_identity(
    pid: u32,
    expected_executable: &Path,
    expected_identity: Option<&str>,
    expected_command: &str,
) -> bool {
    if !feanorfs_agent_core::lock::pid_alive(pid) {
        return false;
    }
    if let Some(expected_identity) = expected_identity {
        if !process_tree::executable_identity_matches(pid, expected_identity) {
            return false;
        }
        let Some(command) = process_command_line(pid) else {
            return false;
        };
        if command == expected_command {
            return true;
        }
        let Some((_, arguments)) = expected_command.split_once(" service ") else {
            return false;
        };
        let suffix = format!(" service {arguments}");
        return command
            .strip_suffix(&suffix)
            .is_some_and(|prefix| !prefix.is_empty());
    }
    process_command_ownership_matches(pid, expected_executable, expected_command)
}

pub(super) fn managed_command_line(program: &Path, subcommand: &str, operand: &str) -> String {
    format!("{} service {subcommand} {operand}", program.display())
}

/// Kill a stray worker left behind by a previous supervisor or a manual
/// process.  The native start token and exact executable/argv are revalidated
/// immediately before escalation; a recycled PID is never force-killed.
pub(super) fn terminate_stray_pid(
    pid: u32,
    grace: Duration,
    expected_executable: &Path,
    expected_command: &str,
) -> bool {
    terminate_stray_pid_with_identity(pid, grace, expected_executable, None, expected_command)
}

pub(super) fn terminate_stray_pid_with_identity(
    pid: u32,
    grace: Duration,
    expected_executable: &Path,
    expected_identity: Option<&str>,
    expected_command: &str,
) -> bool {
    let Some(identity) = process_tree::ProcessIdentity::capture(pid) else {
        tracing::warn!("leaving stray pid {pid}: exact native process identity unavailable");
        return false;
    };
    if !process_command_ownership_matches_with_identity(
        pid,
        expected_executable,
        expected_identity,
        expected_command,
    ) {
        tracing::warn!("leaving stray pid {pid}: executable or argv ownership mismatch");
        return false;
    }
    if !identity.request_termination() {
        tracing::warn!("leaving stray pid {pid}: exact identity changed before TERM");
        return false;
    }
    let deadline = Instant::now() + grace;
    while identity.is_current() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let command_current = process_command_ownership_matches_with_identity(
        pid,
        expected_executable,
        expected_identity,
        expected_command,
    );
    if force_termination_allowed(identity.is_current(), command_current) {
        let _ = identity.force_termination();
    } else if identity.is_current() {
        tracing::warn!(
            "leaving stray pid {} alive because executable or argv ownership changed before KILL",
            identity.pid()
        );
    }
    let reap_deadline = Instant::now() + CHILD_REAP_GRACE;
    while identity.is_current() && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !identity.is_current()
}

/// Watchers refresh their `watch.pid` marker every 45 s while alive, so a
/// marker older than this cannot belong to a live watcher.
const STRAY_WATCHER_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// A live watcher for a workspace that is not one of our children (an orphan
/// from a previous supervisor or a manual `--foreground` process).
///
/// Only pids with a fresh marker, matching recorded process start, kernel
/// executable identity, and the exact FeanorFS argv are returned: a stale
/// marker whose pid was reused by an unrelated process must never be
/// signaled, and a user's own terminal watcher is not silently killed unless
/// it is genuinely this workspace's managed watcher.
/// True when a verified managed watcher process is running for `workspace`
/// outside the current supervisor (for example after a supervisor crash or a
/// background-item toggle in System Settings). Used by the stop paths so
/// `feanorfs stop` keeps working when the supervisor is not alive to
/// terminate its own children.
pub(crate) fn is_managed_watcher(workspace: &Path) -> anyhow::Result<bool> {
    let canonical = canonical_workspace_path(workspace)?;
    Ok(stray_workspace_watcher(canonical.as_str()).is_some())
}

pub(super) fn stray_workspace_watcher(canonical: &str) -> Option<u32> {
    let workspace = Path::new(canonical);
    let state = feanorfs_agent_core::ensure_workspace_state(workspace).ok()?;
    let marker = state.join("watch.pid");
    let metadata = fs::metadata(&marker).ok()?;
    let age = metadata.modified().ok()?.elapsed().ok()?;
    if age > STRAY_WATCHER_MAX_AGE {
        return None;
    }
    let content = fs::read_to_string(marker).ok()?;
    let mut marker_lines = content.lines();
    let pid = marker_lines.next()?.trim().parse::<u32>().ok()?;
    let _refreshed_at = marker_lines.next()?.trim().parse::<u64>().ok()?;
    let recorded_start = marker_lines.next()?.trim().parse::<u64>().ok()?;
    if !feanorfs_agent_core::lock::pid_alive(pid)
        || process_start_epoch(pid).is_none_or(|actual| actual.abs_diff(recorded_start) > 3)
    {
        return None;
    }
    let command = process_command_line(pid)?;
    let program = std::env::current_exe().ok()?;
    // argv text alone can be spoofed through argv[0]. Verify the kernel's
    // executable identity independently before considering the exact argv.
    if process_executable(pid)? != program {
        return None;
    }
    watcher_command_matches(&command, &program, canonical).then_some(pid)
}

/// Returns only an exact runner worker recorded by a supervisor instance that
/// is no longer alive. A live supervisor remains the sole authority for its
/// children.
pub(super) fn stray_runner_worker(status: &SupervisorStatus, canonical: &str) -> Option<u32> {
    if status_supervisor_alive(status) {
        return None;
    }
    let child = status.runners.get(canonical)?;
    #[cfg(target_os = "windows")]
    if child.job_owned {
        // A Job-owned child is killed by the crashed supervisor's handle
        // close. Do not fall back to PID scans if an unexpected live process
        // remains; wait_for_runner_stopped reports it as unresolved instead.
        return None;
    }
    let pid = child.pid?;
    if !recorded_process_is_alive(Some(pid), child.since) {
        return None;
    }
    if !child
        .process_start_id
        .as_deref()
        .is_some_and(|identity| process_tree::process_start_matches(pid, identity))
    {
        return None;
    }
    #[cfg(not(target_os = "windows"))]
    let program = std::env::current_exe().ok()?;
    #[cfg(target_os = "windows")]
    {
        child
            .executable_identity
            .as_deref()
            .is_some_and(|identity| process_tree::executable_identity_matches(pid, identity))
            .then_some(pid)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let expected_identity = child.executable_identity.as_deref()?;
        let expected_command = managed_command_line(&program, "runner-run", canonical);
        process_command_ownership_matches_with_identity(
            pid,
            &program,
            Some(expected_identity),
            &expected_command,
        )
        .then_some(pid)
    }
}

pub(super) fn finish_runner_workspace_exit(workspace: &Path) -> anyhow::Result<()> {
    let store = feanorfs_agent_core::RunnerStore::open_configured(workspace)
        .context("open configured runner while finishing worker exit")?;
    let status = store
        .status()
        .context("read runner status while finishing worker exit")?;
    if status.active_message_id.is_some()
        && status.phase != feanorfs_agent_core::RunnerPhase::NeedsAttention
    {
        let mode = if status.enabled {
            feanorfs_agent_core::RunnerExecutionMode::Supervised
        } else {
            feanorfs_agent_core::RunnerExecutionMode::Foreground
        };
        let session = store
            .execution_session(workspace, mode)
            .context("checkpoint runner interruption")?;
        // Release the exact execution lease before inspecting or cleaning any
        // persisted residual process group. A failed cleanup remains
        // retryable on the next reconciliation pass.
        drop(session);
    }
    cleanup_residual_runner_group(workspace)
        .context("clean up residual runner process group after worker exit")
}

fn finish_runner_worker_exit(spec: &ChildSpec) -> anyhow::Result<()> {
    if let ChildKind::Runner(workspace) = &spec.kind {
        finish_runner_workspace_exit(workspace.as_path())?;
    }
    Ok(())
}

fn exact_command_matches(command: &str, program: &Path, args: &[&str]) -> bool {
    let mut expected = program.display().to_string();
    for arg in args {
        expected.push(' ');
        expected.push_str(arg);
    }
    command == expected
}

/// Exact managed-watcher command-line check: the executable and every argv
/// value must equal the recorded `service run <canonical>` invocation. Prefix
/// matches are unsafe because `/net` also prefixes `/network`.
pub(super) fn watcher_command_matches(command: &str, program: &Path, canonical: &str) -> bool {
    exact_command_matches(command, program, &["service", "run", canonical])
}

/// Exact managed-worker command-line check for orphan reaping. The caller
/// supplies the subcommand and operand recorded for that specific child; a
/// reused PID running another workspace, helper executable, or extra argv is
/// never accepted.
#[cfg(test)]
pub(super) fn managed_orphan_command_matches(
    command: &str,
    program: &Path,
    subcommand: &str,
    operand: &str,
) -> bool {
    exact_command_matches(command, program, &["service", subcommand, operand])
}

pub(super) fn tray_orphan_command_matches(command: &str, tray_program: &Path) -> bool {
    command == managed_tray_command_line(tray_program)
}

pub(super) fn runner_spawn_is_still_admitted(spec: &ChildSpec) -> bool {
    let ChildKind::Runner(workspace) = &spec.kind else {
        return true;
    };
    match feanorfs_agent_core::runner_status(workspace.as_path()) {
        Ok(Some(status)) => {
            status.enabled && status.phase != feanorfs_agent_core::RunnerPhase::NeedsAttention
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!("runner state unavailable before supervised spawn: {error:#}");
            false
        }
    }
}
