//! The reconcile loop, instance lock, orphan reaping, and shutdown.
//!
//! `run_supervisor` holds the exclusive instance lock, reaps children
//! recorded by a previous supervisor, then reconciles the desired set every
//! 500 ms. Shutdown terminates every child concurrently and retains any
//! unresolved reaper/residual cleanup in the durable status projection.

use anyhow::{ensure, Context as _};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cli::process_tree;
use crate::cli::process_tree::{ReapTicket, CHILD_REAPER};
use feanorfs_client::workspace_path::CanonicalWorkspacePath;

use super::*;

pub(super) async fn reconcile(
    children: &mut BTreeMap<String, ManagedChild>,
    desired: &BTreeMap<String, ChildSpec>,
    restart_hub: bool,
) -> anyhow::Result<bool> {
    let mut changed = false;
    // Reap exited children and apply the restart policy.
    for managed in children.values_mut() {
        if managed.state == ChildState::Stopping {
            match poll_stopping_child(managed) {
                Ok(completed) => changed |= completed,
                Err(error) => {
                    tracing::warn!(
                        "supervisor child stop cleanup remains pending for {}: {error:#}",
                        managed.spec.program.display()
                    );
                    changed = true;
                }
            }
            if managed.state == ChildState::Stopping {
                continue;
            }
        }
        let Some(child) = managed.child.as_mut() else {
            continue;
        };
        let Some(status) = child.try_wait()? else {
            continue;
        };
        let exit = status.code();
        managed.last_exit = exit;
        managed.child = None;
        if let Some(tree) = managed.process_tree.as_ref() {
            // A worker can exit directly while descendants remain. The
            // native process-tree boundary owns the complete tree; force it
            // before allowing backoff or a respawn.
            let _ = tree.force_termination();
        }
        changed = true;
        let lived_long = managed
            .spawned_at
            .is_some_and(|at| at.elapsed() >= RESET_AFTER);
        if lived_long {
            managed.restarts = 0;
        }
        if managed.spec.restart_on_zero_exit || exit != Some(0) {
            managed.restarts = managed.restarts.saturating_add(1);
            managed.backoff_until =
                Some(Instant::now() + CHILD_RESTART_BACKOFF.delay(managed.restarts));
        } else {
            managed.backoff_until = None;
        }
        if let Err(error) = mark_finished_child_exit(managed) {
            tracing::warn!(
                "supervisor runner exit cleanup remains pending for {}: {error:#}",
                managed.spec.program.display()
            );
            managed.state = ChildState::Stopping;
        }
    }

    // A hub configuration change (relay route or listen port) restarts the hub
    // worker immediately, without crash backoff.
    if restart_hub {
        if let Some(hub) = children.get_mut(HUB_CHILD_KEY) {
            if let Err(error) = terminate_child(hub).await {
                tracing::warn!("hub restart termination remains pending: {error:#}");
            }
            if hub.state != ChildState::Stopping {
                hub.restarts = 0;
                hub.backoff_until = None;
            }
            changed = true;
        }
    }

    // Spawn children that are desired but missing.
    for (key, spec) in desired {
        // `desired` was built before exited children were reaped. Recheck the
        // local authority here so a crash checkpoint or concurrent stop cannot
        // spawn one runner from stale intent.
        if !runner_spawn_is_still_admitted(spec) {
            continue;
        }
        let managed = children
            .entry(key.clone())
            .or_insert_with(|| ManagedChild::new(spec.clone()));
        if managed.child.is_some() {
            continue;
        }
        if !should_respawn(managed) {
            continue;
        }
        if let ChildKind::Workspace(workspace) = &managed.spec.kind {
            if let Some(stray) = stray_workspace_watcher(workspace.as_str()) {
                tracing::warn!(
                    "stopping stray watcher {stray} before supervising {}",
                    workspace.as_str()
                );
                if let Ok(program) = std::env::current_exe() {
                    let command = managed_command_line(&program, "run", workspace.as_str());
                    terminate_stray_pid(stray, Duration::from_secs(1), &program, &command);
                }
            }
        }
        let expected_executable_identity =
            process_tree::executable_identity_for_path(&spec.program);
        match spawn_child(spec).await {
            Ok(spawned) => {
                let child_pid = spawned.child.id();
                // A ManagedChild reconstructed from an orphan handoff carries
                // the previous worker's executable identity. Once that
                // orphan is gone, bind the replacement to the image we are
                // actually about to release through the startup gate. Keeping
                // the stale identity would make the next supervisor unable to
                // prove ownership of this newly spawned worker.
                managed.spec = spec.clone();
                managed.expected_executable_identity = expected_executable_identity;
                managed.child = Some(spawned.child);
                managed.process_tree = Some(spawned.process_tree);
                managed.startup_gate = Some(spawned.startup_gate);
                managed.owned_pid = child_pid;
                managed.owned_process_start_id = child_pid.and_then(exact_child_process_start_id);
                managed.owned_since = now_epoch();
                managed.spawned_at = Some(Instant::now());
                managed.backoff_until = None;
                managed.state = ChildState::Running;
                changed = true;
            }
            Err(error) => {
                managed.restarts = managed.restarts.saturating_add(1);
                managed.backoff_until =
                    Some(Instant::now() + CHILD_RESTART_BACKOFF.delay(managed.restarts));
                managed.state = ChildState::Backoff;
                changed = true;
                eprintln!("FeanorFS worker failed to start: {error:#}");
            }
        }
    }

    // Terminate children that are no longer desired.
    let keys: Vec<String> = children.keys().cloned().collect();
    for key in keys {
        if desired.contains_key(&key) {
            continue;
        }
        let Some(managed) = children.get_mut(&key) else {
            continue;
        };
        if let Err(error) = terminate_child(managed).await {
            tracing::warn!("supervisor child termination remains pending for {key}: {error:#}");
        }
        if managed.state != ChildState::Stopping
            && managed.child.is_none()
            && managed.pending_reap.is_none()
            && managed.pending_orphan.is_none()
            && managed.process_tree.is_none()
            && managed.startup_gate.is_none()
        {
            children.remove(&key);
        }
        // Keep a stopping entry in the map until the exact reaper completion
        // and runner cleanup have fed the next reconciliation pass.
        changed = true;
    }
    Ok(changed)
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Atomically claims the single-supervisor instance lock. The lock file is
/// never removed, so ownership transfers safely on crash; `fs2` releases it
/// automatically when the owning process exits.
pub(super) fn acquire_supervisor_lock_at(path: &Path) -> anyhow::Result<Option<SupervisorGuard>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .context("open supervisor instance lock")?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let mut file = file;
            let setup = (|| -> anyhow::Result<(PathBuf, SupervisorLockOwner)> {
                // Clear the legacy record first so a reader cannot mistake a
                // previous live PID for the new lock owner during handoff.
                file.set_len(0).context("clear supervisor lock owner")?;
                let pid = std::process::id();
                let process_start_id = process_tree::process_start_identifier(pid, "supervisor");
                ensure!(
                    process_tree::process_start_matches(pid, &process_start_id),
                    "capture exact supervisor process identity"
                );
                let owner = SupervisorLockOwner {
                    pid,
                    process_start_id,
                };
                let owner_path = supervisor_lock_owner_path_at(path);
                match fs::remove_file(&owner_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("remove stale supervisor lock owner"),
                }
                write_supervisor_lock_owner_at(path, &owner)?;
                writeln!(file, "{pid}").context("write legacy supervisor lock owner")?;
                file.flush().context("flush legacy supervisor lock owner")?;
                Ok((owner_path, owner))
            })();
            match setup {
                Ok((owner_path, owner)) => Ok(Some(SupervisorGuard {
                    file,
                    owner_path,
                    owner,
                })),
                Err(error) => {
                    let _ = fs::remove_file(supervisor_lock_owner_path_at(path));
                    let _ = fs2::FileExt::unlock(&file);
                    Err(error)
                }
            }
        }
        Err(_) => Ok(None),
    }
}

pub(super) struct SupervisorGuard {
    file: std::fs::File,
    owner_path: PathBuf,
    owner: SupervisorLockOwner,
}

impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        if fs::read_to_string(&self.owner_path)
            .ok()
            .and_then(|content| serde_json::from_str::<SupervisorLockOwner>(&content).ok())
            .as_ref()
            == Some(&self.owner)
        {
            let _ = fs::remove_file(&self.owner_path);
        }
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub(super) fn supervisor_lock_path() -> anyhow::Result<PathBuf> {
    // Deliberately distinct from the registry lock file (registry lock is
    // `supervisor.json` -> `supervisor.lock`): the instance lock is held for
    // the whole supervisor lifetime, and a second flock on the same file from
    // another descriptor would deadlock the registry reads.
    Ok(feanorfs_agent_core::global_state_root()?.join("supervisor.instance.lock"))
}

/// Terminates only legacy processes recorded by a previous supervisor
/// instance. The caller holds the supervisor lock, so any still-live recorded
/// PID is an orphan. New Windows Job-owned workers are intentionally skipped:
/// their kernel Job handle closed with the old supervisor. Legacy Windows
/// cleanup requires the exact creation token plus normalized kernel image;
/// Unix additionally retains the exact argv check.
///
/// `Tray` is only constructed on non-Windows hosts (see
/// `reap_orphaned_children`), so Windows builds never name that variant;
/// narrow the dead-code allowance to the platform that genuinely lacks a
/// consumer instead of suppressing the whole enum everywhere.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(super) enum OrphanIdentity {
    Worker {
        subcommand: &'static str,
        operand: String,
        process_start_id: Option<String>,
        job_owned: bool,
    },
    Tray {
        path: PathBuf,
        process_start_id: Option<String>,
    },
}

pub(super) struct PendingOrphanCleanup {
    pub(super) key: String,
    pub(super) spec: ChildSpec,
    pub(super) pid: Option<u32>,
    pub(super) expected_since: u64,
    pub(super) recorded_state: ChildState,
    pub(super) process_start_id: Option<String>,
    /// Executable identity persisted by the previous supervisor.  This is
    /// deliberately independent from the new supervisor's `current_exe`:
    /// an in-place upgrade may leave the old mapped image at a deleted path.
    pub(super) executable_identity: Option<String>,
    /// A Windows worker recorded as Job-owned was under the previous
    /// supervisor's kernel ownership boundary. A replacement supervisor must
    /// never turn that stale record into a speculative PID signal: the old
    /// Job handle closing is the only supported cleanup operation.
    pub(super) job_owned: bool,
    pub(super) expected_executable: PathBuf,
    pub(super) expected_command: String,
    pub(super) grace: Duration,
    pub(super) ticket: ReapTicket,
}

#[cfg(test)]
pub(super) fn pending_orphan_cleanup(
    program: &Path,
    pid: Option<u32>,
    expected_since: u64,
    identity: &OrphanIdentity,
    expected_command: String,
    grace: Duration,
) -> PendingOrphanCleanup {
    pending_orphan_cleanup_with_state(
        program,
        pid,
        expected_since,
        ChildState::Stopping,
        identity,
        expected_command,
        grace,
    )
}

pub(super) fn pending_orphan_cleanup_with_state(
    program: &Path,
    pid: Option<u32>,
    expected_since: u64,
    recorded_state: ChildState,
    identity: &OrphanIdentity,
    expected_command: String,
    grace: Duration,
) -> PendingOrphanCleanup {
    let (key, kind, args, process_start_id) = match identity {
        OrphanIdentity::Worker {
            subcommand,
            operand,
            process_start_id,
            ..
        } => {
            let (key, kind) = match *subcommand {
                "hub-run" => (HUB_CHILD_KEY.to_string(), ChildKind::Hub),
                "run" => (
                    workspace_child_key(operand),
                    ChildKind::Workspace(CanonicalWorkspacePath::from_exact_string(
                        operand.clone(),
                    )),
                ),
                "runner-run" => (
                    runner_child_key(operand),
                    ChildKind::Runner(CanonicalWorkspacePath::from_exact_string(operand.clone())),
                ),
                _ => (
                    format!("orphan:{subcommand}:{operand}"),
                    ChildKind::Workspace(CanonicalWorkspacePath::from_exact_string(
                        operand.clone(),
                    )),
                ),
            };
            (
                key,
                kind,
                vec![
                    OsString::from("service"),
                    OsString::from(*subcommand),
                    OsString::from(operand.as_str()),
                ],
                process_start_id.clone(),
            )
        }
        OrphanIdentity::Tray {
            path: _tray,
            process_start_id,
        } => (
            TRAY_CHILD_KEY.to_string(),
            ChildKind::Tray,
            managed_tray_args(),
            process_start_id.clone(),
        ),
    };
    PendingOrphanCleanup {
        key,
        spec: ChildSpec {
            kind,
            program: program.to_path_buf(),
            args,
            env: Vec::new(),
            restart_on_zero_exit: !matches!(identity, OrphanIdentity::Tray { .. }),
        },
        pid,
        expected_since,
        recorded_state,
        process_start_id,
        executable_identity: process_tree::executable_identity_for_path(program),
        job_owned: matches!(
            identity,
            OrphanIdentity::Worker {
                job_owned: true,
                ..
            }
        ),
        expected_executable: match identity {
            OrphanIdentity::Tray { path: tray, .. } => tray.clone(),
            OrphanIdentity::Worker { .. } => program.to_path_buf(),
        },
        expected_command,
        grace,
        ticket: ReapTicket::new(),
    }
}

fn pending_orphan_cleanup_with_executable_identity(
    program: &Path,
    pid: Option<u32>,
    expected_since: u64,
    recorded_state: ChildState,
    executable_identity: Option<String>,
    identity: &OrphanIdentity,
) -> PendingOrphanCleanup {
    let expected_command = match identity {
        OrphanIdentity::Worker {
            subcommand,
            operand,
            ..
        } => {
            #[cfg(target_os = "windows")]
            {
                let _ = (subcommand, operand);
                String::new()
            }
            #[cfg(not(target_os = "windows"))]
            {
                managed_command_line(program, subcommand, operand)
            }
        }
        OrphanIdentity::Tray { path: tray, .. } => {
            #[cfg(target_os = "windows")]
            {
                let _ = tray;
                String::new()
            }
            #[cfg(not(target_os = "windows"))]
            {
                managed_tray_command_line(tray)
            }
        }
    };
    let grace = if matches!(
        identity,
        OrphanIdentity::Worker {
            subcommand: "runner-run",
            ..
        }
    ) {
        STOP_GRACE
    } else {
        Duration::from_secs(1)
    };
    let mut pending = pending_orphan_cleanup_with_state(
        program,
        pid,
        expected_since,
        recorded_state,
        identity,
        expected_command,
        grace,
    );
    pending.executable_identity = executable_identity;
    pending
}

fn orphan_process_matches(pending: &PendingOrphanCleanup) -> bool {
    let Some(pid) = pending.pid else {
        return true;
    };
    // Never let a stale status record target the supervisor process itself,
    // even if a reused PID happens to satisfy a weak legacy identity probe.
    if pid == std::process::id() || !feanorfs_agent_core::lock::pid_alive(pid) {
        return false;
    }
    // The status snapshot is the handoff record for a prior supervisor. A
    // PID and a wall-clock age are insufficient after PID reuse; require the
    // exact native creation identity on every platform that can expose one.
    // Legacy records without it remain unresolved and are never signaled.
    if !pending
        .process_start_id
        .as_deref()
        .is_some_and(|identity| process_tree::process_start_matches(pid, identity))
    {
        return false;
    }
    if !pending
        .executable_identity
        .as_deref()
        .is_some_and(|identity| process_tree::executable_identity_matches(pid, identity))
    {
        return false;
    }
    #[cfg(not(target_os = "windows"))]
    if pending.expected_since == 0
        || process_start_epoch(pid).is_none_or(|actual| actual.abs_diff(pending.expected_since) > 3)
    {
        return false;
    }
    if matches!(pending.spec.kind, ChildKind::Tray) {
        tray_orphan_command_matches(&pending.expected_command, &pending.expected_executable)
    } else {
        process_command_ownership_matches(
            pid,
            &pending.expected_executable,
            &pending.expected_command,
        )
    }
}

pub(super) fn retry_one_pending_orphan_cleanup(cleanup: &mut PendingOrphanCleanup) {
    if cleanup.ticket.is_complete() {
        return;
    }
    // A previous Windows supervisor's Job handle is not serializable and
    // cannot be reopened by this process.  Even a null/dead root PID gives no
    // proof that descendants are gone; retain the ownership record forever
    // (and therefore withhold the runner ACK) until the original kernel Job
    // boundary has removed it.
    if cleanup.job_owned {
        return;
    }
    if cleanup.pid.is_none() && matches!(cleanup.spec.kind, ChildKind::Tray) {
        // The tray is a non-authoritative UI child and never owns workspace,
        // runner, or hub state. A shutdown snapshot can legitimately publish
        // `stopping` after the exact tray process was already reaped but
        // before the in-memory entry advanced to `stopped`. With no PID or
        // process identity left to validate, retaining that record forever
        // only prevents the next supervisor from launching the tray again.
        cleanup.ticket.complete();
        return;
    }
    if cleanup.pid.is_none() {
        if let ChildKind::Workspace(workspace) = &cleanup.spec.kind {
            // Workspace watchers publish a fresh, process-start-bound marker
            // before doing file work. With the supervisor lifetime lock held,
            // no previous supervisor can create another watcher concurrently.
            // If no exact marked watcher remains, a pid:null stopping snapshot
            // is complete and must not block a later explicit `start` forever.
            if let Some(pid) = stray_workspace_watcher(workspace.as_str()) {
                if terminate_stray_pid_with_identity(
                    pid,
                    cleanup.grace,
                    &cleanup.expected_executable,
                    cleanup.executable_identity.as_deref(),
                    &cleanup.expected_command,
                ) {
                    cleanup.ticket.complete();
                }
            } else {
                cleanup.ticket.complete();
            }
            return;
        }
    }
    let Some(pid) = cleanup.pid else {
        // A null PID is complete only when the durable snapshot proves that
        // this entry was already stopped and had never acquired a process
        // identity. Stopping/Running/Backoff plus pid:null is an ambiguous
        // crash handoff; retain it fail-closed instead of treating omission
        // as proof that no worker survived.
        if cleanup.recorded_state == ChildState::Stopped && cleanup.expected_since == 0 {
            cleanup.ticket.complete();
        }
        return;
    };
    if pid == std::process::id() {
        // A stale record must never make the new supervisor signal itself.
        // Keep the authority pending until a later process instance can
        // disambiguate the record.
        return;
    }
    if !feanorfs_agent_core::lock::pid_alive(pid) {
        cleanup.ticket.complete();
        return;
    }
    if !orphan_process_matches(cleanup) {
        return;
    }
    if terminate_stray_pid_with_identity(
        pid,
        cleanup.grace,
        &cleanup.expected_executable,
        cleanup.executable_identity.as_deref(),
        &cleanup.expected_command,
    ) {
        cleanup.ticket.complete();
    }
}

fn orphan_cleanup_is_runner(cleanup: &PendingOrphanCleanup) -> bool {
    matches!(cleanup.spec.kind, ChildKind::Runner(_))
}

/// Inspect the previous status snapshot and return every child whose cleanup
/// is not proven complete. In particular, runner entries are retained even
/// when their PID is already dead so `finish_runner_workspace_exit` runs under
/// the new supervisor before the workspace-specific stop ACK can publish.
async fn reap_orphaned_children() -> BTreeMap<String, PendingOrphanCleanup> {
    let Ok(Some(status)) = read_status() else {
        return BTreeMap::new();
    };
    let Ok(program) = std::env::current_exe() else {
        return BTreeMap::new();
    };
    let mut expected = Vec::<(Option<u32>, u64, ChildState, Option<String>, OrphanIdentity)>::new();
    if let (Some(child), Ok(data_dir)) = (status.hub, crate::cli::hub_service::default_data_dir()) {
        expected.push((
            child.pid,
            child.since,
            child.state,
            child.executable_identity,
            OrphanIdentity::Worker {
                subcommand: "hub-run",
                operand: data_dir.display().to_string(),
                process_start_id: child.process_start_id,
                job_owned: child.job_owned,
            },
        ));
    }
    for (workspace, child) in status.workspaces {
        expected.push((
            child.pid,
            child.since,
            child.state,
            child.executable_identity,
            OrphanIdentity::Worker {
                subcommand: "run",
                operand: workspace,
                process_start_id: child.process_start_id,
                job_owned: child.job_owned,
            },
        ));
    }
    for (workspace, child) in status.runners {
        expected.push((
            child.pid,
            child.since,
            child.state,
            child.executable_identity,
            OrphanIdentity::Worker {
                subcommand: "runner-run",
                operand: workspace,
                process_start_id: child.process_start_id,
                job_owned: child.job_owned,
            },
        ));
    }
    #[cfg(not(target_os = "windows"))]
    if let (Some(child), Some(tray)) = (
        status.tray,
        crate::cli::service::find_tray_program(&program),
    ) {
        expected.push((
            child.pid,
            child.since,
            child.state,
            child.executable_identity,
            OrphanIdentity::Tray {
                path: tray,
                process_start_id: child.process_start_id,
            },
        ));
    }

    let mut pending = BTreeMap::new();
    for (pid, expected_since, recorded_state, executable_identity, identity) in expected {
        let mut cleanup = pending_orphan_cleanup_with_executable_identity(
            &program,
            pid,
            expected_since,
            recorded_state,
            executable_identity,
            &identity,
        );
        retry_one_pending_orphan_cleanup(&mut cleanup);
        // A runner needs a final state checkpoint even when its direct worker
        // was already gone. Other completed components need no map entry.
        if !cleanup.ticket.is_complete() || orphan_cleanup_is_runner(&cleanup) {
            pending.insert(cleanup.key.clone(), cleanup);
        }
    }
    pending
}

/// Run the supervised background job until terminated. Spawns the private hub
/// worker, every registered workspace watcher, and the tray, and keeps them
/// running.
pub(super) fn current_supervisor_started_at() -> u64 {
    process_start_epoch(std::process::id()).unwrap_or_else(now_epoch)
}

pub(crate) async fn run_supervisor() -> anyhow::Result<()> {
    let Some(_instance_guard) = acquire_supervisor_lock_at(&supervisor_lock_path()?)? else {
        anyhow::bail!(
            "another FeanorFS supervisor is already running; stop it before starting this one"
        );
    };
    // The reaper must be live before any worker is spawned.  Child shutdown
    // remains ownership-total if the worker later exits: enqueue retries the
    // coordinator and falls back to an in-task wait, but normal operation
    // never starts with an unowned drain path.
    CHILD_REAPER
        .ensure_ready()
        .context("start supervisor child reaper")?;
    // Status liveness compares this value with the kernel's process start.
    // Startup orphan cleanup may exceed that bounded tolerance, so recording
    // the wall clock after cleanup would make live children appear stopped.
    let started_at = current_supervisor_started_at();
    // The lifetime flock is authoritative. A stale status PID may already have
    // been reused by an unrelated process; treating PID liveness as a second
    // ownership signal would let that stale snapshot block startup forever.
    // Children recorded by a previous supervisor instance may have been
    // orphaned when that instance was replaced (binary update, crash, or a
    // shutdown that exceeded launchd's grace period). They must be reaped
    // before spawning fresh ones: the orphaned hub in particular holds the
    // hub-data runtime lock and the listen port, which would otherwise wedge
    // the new hub child in a backoff loop forever.
    let pending_orphans = reap_orphaned_children().await;
    let mut children: BTreeMap<String, ManagedChild> = pending_orphans
        .into_iter()
        .map(|(key, pending)| {
            let spec = pending.spec.clone();
            let expected_executable_identity = pending.executable_identity.clone();
            (
                key,
                ManagedChild {
                    spec,
                    expected_executable_identity,
                    child: None,
                    pending_reap: None,
                    pending_orphan: Some(pending),
                    process_tree: None,
                    startup_gate: None,
                    owned_pid: None,
                    owned_process_start_id: None,
                    owned_since: 0,
                    state: ChildState::Stopping,
                    restarts: 0,
                    last_exit: None,
                    backoff_until: None,
                    spawned_at: None,
                },
            )
        })
        .collect();
    // Refresh the status file immediately: readers must never see a stale
    // previous instance (old pid, old children) while this supervisor is up
    // with nothing published yet. A failed status write must not abort the
    // supervisor (launchd would restart it into the same failure).
    if let Err(error) = publish_status(&children, started_at) {
        tracing::error!("initial supervisor status publish failed: {error:#}");
    }
    let tray_program = {
        #[cfg(not(target_os = "windows"))]
        {
            let program = std::env::current_exe().ok();
            program
                .as_deref()
                .and_then(crate::cli::service::find_tray_program)
        }
        #[cfg(target_os = "windows")]
        {
            None
        }
    };
    let mut last_relay_mtime = hub_config_mtime("relay.json");
    let mut last_port_mtime = hub_config_mtime("listen-port");
    let mut reconcile_generation = 0_u64;
    let mut last_registry_digest = None::<String>;

    loop {
        // A transient registry problem (lock contention, corrupt file) must
        // never kill the whole supervisor: keep the current children and
        // retry next poll. Exiting here would orphan every child process.
        let registry = match read_registry() {
            Ok(registry) => registry,
            Err(error) => {
                tracing::error!(
                    "supervisor registry unreadable; keeping current children: {error:#}"
                );
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        let relay_mtime = hub_config_mtime("relay.json");
        let port_mtime = hub_config_mtime("listen-port");
        let relay_changed = relay_mtime != last_relay_mtime;
        let port_changed = port_mtime != last_port_mtime;
        last_relay_mtime = relay_mtime;
        last_port_mtime = port_mtime;

        let desired = match desired_specs(&registry, &tray_program) {
            Ok(desired) => desired,
            Err(error) => {
                tracing::error!(
                    "building desired supervisor specs failed; keeping current children: {error:#}"
                );
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        // Workspace registry changes are handled by the desired-set diff below;
        // only hub configuration changes (relay route, listen port) restart the
        // hub worker itself.
        let changed = match reconcile(&mut children, &desired, relay_changed || port_changed).await
        {
            Ok(changed) => changed,
            Err(error) => {
                tracing::error!("supervisor reconcile failed; retrying: {error:#}");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        let digest = match runner_reconcile_projection_digest(&children, &registry) {
            Ok(digest) => digest,
            Err(error) => {
                tracing::error!("building runner reconcile projection failed: {error:#}");
                String::new()
            }
        };
        if !digest.is_empty() && last_registry_digest.as_deref() != Some(digest.as_str()) {
            reconcile_generation = reconcile_generation.saturating_add(1);
            match publish_runner_reconcile_ack(
                &children,
                &registry,
                started_at,
                reconcile_generation,
            ) {
                Ok(()) => last_registry_digest = Some(digest),
                Err(error) => tracing::error!(
                    "runner reconcile acknowledgement publish failed; stop waits remain fail-closed: {error:#}"
                ),
            }
        }
        // Publish on every state change (including the very first reconcile,
        // which spawns the children) so `service install|start|stop` waits can
        // observe Running/Stopped promptly instead of racing a heartbeat.
        // Liveness of the supervisor itself is read from status.pid, so an
        // idle supervisor writes nothing (no 5-second disk churn).
        if changed || has_pending_startup_gates(&children) {
            match publish_status(&children, started_at) {
                Ok(()) => {
                    if let Err(error) = release_startup_gates(&mut children) {
                        tracing::error!(
                            "supervisor startup release remains pending after status publication: {error:#}"
                        );
                    }
                }
                Err(error) => {
                    // Keep every startup gate closed. A child may exist, but
                    // no configured worker code can run until its exact PID,
                    // native ownership, and status ledger are durable.
                    tracing::error!("supervisor status publish failed: {error:#}");
                }
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = shutdown_signal() => break,
        }
    }

    // Terminate every child concurrently: launchd only grants a bounded
    // shutdown grace before SIGKILL, and sequential 5 s-per-child waits would
    // orphan the tail of the list.
    let remaining = terminate_all_children(children).await;
    if remaining.is_empty() {
        let _ = publish_status(&BTreeMap::new(), started_at);
        Ok(())
    } else {
        // Never publish an empty snapshot while a reaper ticket, native
        // process tree, or runner residual checkpoint is unresolved. The
        // non-empty stopping projection is the durable authority that makes
        // the next supervisor retry startup cleanup instead of respawning or
        // acknowledging a removed runner.
        let _ = publish_status(&remaining, started_at);
        anyhow::bail!(
            "supervisor shutdown retained {} child cleanup operation(s); retry will remain fail-closed",
            remaining.len()
        )
    }
}

/// Shut down every owned child concurrently. Each task owns its
/// `ManagedChild` until `terminate_child` has either reaped the process or
/// transferred it to the persistent reaper/synchronous wait path.
struct ShutdownChildGuard {
    key: String,
    managed: Option<ManagedChild>,
    recovery: Arc<Mutex<BTreeMap<String, ManagedChild>>>,
}

impl Drop for ShutdownChildGuard {
    fn drop(&mut self) {
        let Some(managed) = self.managed.take() else {
            return;
        };
        self.recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.key.clone(), managed);
    }
}

pub(super) async fn terminate_all_children(
    children: BTreeMap<String, ManagedChild>,
) -> BTreeMap<String, ManagedChild> {
    let recovery = Arc::new(Mutex::new(children));
    let keys = recovery
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut terminations = Vec::new();
    for key in keys {
        let managed = recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        let Some(managed) = managed else {
            continue;
        };
        let guard = ShutdownChildGuard {
            key: key.clone(),
            managed: Some(managed),
            recovery: Arc::clone(&recovery),
        };
        terminations.push(tokio::spawn(async move {
            let mut guard = guard;
            #[cfg(test)]
            if TEST_SHUTDOWN_PANIC_ONCE.swap(false, AtomicOrdering::AcqRel) {
                panic!("injected supervisor shutdown task panic");
            }
            let result = terminate_child(
                guard
                    .managed
                    .as_mut()
                    .expect("shutdown ownership guard contains child"),
            )
            .await;
            let managed = guard
                .managed
                .take()
                .expect("shutdown ownership guard retains child result");
            (key, managed, result)
        }));
    }
    let mut pending = BTreeMap::new();
    for termination in terminations {
        match termination.await {
            Ok((key, managed, Ok(()))) => {
                if managed.state == ChildState::Stopping
                    || managed.pending_reap.is_some()
                    || managed.pending_orphan.is_some()
                    || managed.process_tree.is_some()
                    || managed.startup_gate.is_some()
                {
                    pending.insert(key, managed);
                }
            }
            Ok((key, managed, Err(error))) => {
                tracing::warn!("supervisor child {key} shutdown remains pending: {error:#}");
                pending.insert(key, managed);
            }
            Err(error) => {
                tracing::error!("supervisor child shutdown task failed: {error}");
            }
        }
    }

    // A cancelled/panicking task drops its guard, which atomically returns
    // the exact ManagedChild to this recovery map. Reconcile those entries
    // synchronously before considering shutdown complete; no JoinError can
    // silently turn live ownership into an empty status.
    let recovered = std::mem::take(
        &mut *recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_iter()
    .collect::<Vec<_>>();
    for (key, mut managed) in recovered {
        let result = terminate_child(&mut managed).await;
        if let Err(error) = result {
            tracing::warn!("recovered supervisor child {key} remains pending: {error:#}");
        }
        if managed.state == ChildState::Stopping
            || managed.pending_reap.is_some()
            || managed.pending_orphan.is_some()
            || managed.process_tree.is_some()
            || managed.startup_gate.is_some()
        {
            pending.insert(key, managed);
        }
    }

    // A successful background handoff is not complete when enqueue returns:
    // the persistent reaper still owns the exact Tokio Child. Retain every
    // ManagedChild and await the ticket plus runner cleanup before allowing
    // the supervisor to publish an empty snapshot or exit.
    let deadline = Instant::now() + STOP_GRACE + CHILD_REAP_GRACE;
    while !pending.is_empty() && Instant::now() < deadline {
        let keys = pending.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            let Some(managed) = pending.get_mut(&key) else {
                continue;
            };
            if managed.state != ChildState::Stopping {
                continue;
            }
            match poll_stopping_child(managed) {
                Ok(true) => {}
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        "supervisor child {key} shutdown cleanup remains pending: {error:#}"
                    );
                }
            }
        }
        pending.retain(|_, managed| {
            managed.state == ChildState::Stopping
                || managed.pending_reap.is_some()
                || managed.pending_orphan.is_some()
                || managed.process_tree.is_some()
                || managed.startup_gate.is_some()
        });
        if !pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pending
}
