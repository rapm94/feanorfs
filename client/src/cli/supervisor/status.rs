//! Constant-cost status snapshots and runner reconcile acknowledgements.
//!
//! A secret-free snapshot (`supervisor-status.json`) is authoritative only
//! while the reporting supervisor process is alive; every child projection
//! revalidates the exact native process identity. Runner stop waits bind to
//! durable per-runner reconcile acknowledgements
//! (`supervisor-runner-ack.json`) rather than to the wall-clock snapshot.

use anyhow::{ensure, Context as _};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use crate::cli::process_tree;
use feanorfs_client::workspace_path::CanonicalWorkspacePath;

use super::*;

/// State of a managed background component (the supervisor job, a workspace,
/// the private hub, or the tray).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServiceState {
    NotInstalled,
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChildState {
    Running,
    Backoff,
    /// The supervisor no longer admits this child, but its exact process
    /// handle or runner cleanup is still owned by a pending stop operation.
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChildStatus {
    pub(crate) pid: Option<u32>,
    /// Exact kernel creation identity used by Windows legacy cleanup. Unix
    /// status readers retain the field for schema parity but use their native
    /// start probes directly.
    #[serde(default)]
    pub(crate) process_start_id: Option<String>,
    /// True only for children adopted into this supervisor's Windows Job
    /// Object. A stale status with this bit set must not trigger speculative
    /// PID cleanup after a supervisor crash; closing the job is the ownership
    /// boundary.
    #[serde(default)]
    pub(crate) job_owned: bool,
    /// Stable executable identity captured from the owned process image.
    /// Unix stores device/inode so a mapped image remains identifiable after
    /// an in-place upgrade unlinks its old pathname.
    #[serde(default)]
    pub(crate) executable_identity: Option<String>,
    pub(crate) state: ChildState,
    pub(crate) restarts: u32,
    pub(crate) last_exit: Option<i32>,
    pub(crate) since: u64,
}

/// Secret-free snapshot published by the running supervisor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SupervisorStatus {
    pub(crate) pid: Option<u32>,
    #[serde(default)]
    pub(crate) process_start_id: Option<String>,
    pub(crate) version: u32,
    pub(crate) started_at: u64,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) workspaces: BTreeMap<String, ChildStatus>,
    #[serde(default)]
    pub(crate) runners: BTreeMap<String, ChildStatus>,
    pub(crate) hub: Option<ChildStatus>,
    pub(crate) tray: Option<ChildStatus>,
}

/// Durable acknowledgement of the registry generation most recently
/// reconciled by the live supervisor. A status snapshot alone is insufficient:
/// an initial/failed status publish can leave a current supervisor owning a
/// worker while readers observe a missing or stale status file. The ack is
/// secret-free and binds the generation to the supervisor's exact native
/// process identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RunnerReconcileAck {
    /// Canonical runner workspace whose stop token this record acknowledges.
    pub(super) workspace: CanonicalWorkspacePath,
    pub(super) pid: u32,
    #[serde(default)]
    pub(super) process_start_id: Option<String>,
    pub(super) started_at: u64,
    pub(super) registry_digest: String,
    /// Durable registry mutation generation observed by the supervisor. This
    /// is separate from the acknowledgement sequence and prevents an ABA
    /// registry update from satisfying an older stop waiter merely because
    /// the list content returned to the same digest.
    #[serde(default)]
    pub(super) registry_generation: u64,
    /// Exact durable stop token for `workspace`. This is the per-stop ABA
    /// boundary; unrelated runner tombstones never participate in matching.
    #[serde(default)]
    pub(super) stop_token: String,
    pub(super) generation: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct RunnerReconcileAckStore {
    /// One durable acknowledgement per canonical runner. Keeping records
    /// independent prevents a stuck runner B from delaying an already
    /// reconciled runner A.
    #[serde(default)]
    pub(super) acks: BTreeMap<CanonicalWorkspacePath, RunnerReconcileAck>,
}

// Registry

pub(crate) fn install_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    add_workspace(canonical.as_path())?;
    ensure_supervisor_running()?;
    wait_for_workspace_child(canonical.as_str(), READY_TIMEOUT)?;
    Ok(ServiceState::Running)
}

pub(crate) fn start_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    if !is_registered(canonical.as_path())? {
        anyhow::bail!("Automatic sync is not installed; run `feanorfs service install`");
    }
    start_workspace_in_registry(canonical.as_path())?;
    ensure_supervisor_running()?;
    wait_for_workspace_child(canonical.as_str(), READY_TIMEOUT)?;
    Ok(ServiceState::Running)
}

pub(crate) fn stop_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    if !is_registered(canonical.as_path())? {
        return Ok(ServiceState::NotInstalled);
    }
    stop_workspace_in_registry(canonical.as_path())?;
    wait_for_workspace_stopped(canonical.as_path())?;
    Ok(ServiceState::Stopped)
}

pub(crate) fn uninstall_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    if !is_registered(canonical.as_path())? {
        return Ok(ServiceState::NotInstalled);
    }
    remove_workspace_from_registry(canonical.as_path())?;
    wait_for_workspace_stopped(canonical.as_path())?;
    Ok(ServiceState::NotInstalled)
}

pub(crate) fn status_for_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    let registry = read_registry()?;
    let supervised = registry.workspaces.iter().any(|path| path == &canonical);
    let remembered = registry.stopped.iter().any(|path| path == &canonical);
    if !supervised && !remembered {
        return Ok(ServiceState::NotInstalled);
    }
    if !supervised || supervisor_job_state()? != ServiceState::Running {
        return Ok(ServiceState::Stopped);
    }
    Ok(match read_status()? {
        Some(status) if child_is_running(&status, canonical.as_str()) => ServiceState::Running,
        _ => ServiceState::Stopped,
    })
}

pub(crate) fn status_for_runner(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    if !read_registry()?
        .runners
        .iter()
        .any(|path| path == &canonical)
    {
        return Ok(ServiceState::NotInstalled);
    }
    if supervisor_job_state()? != ServiceState::Running {
        return Ok(ServiceState::Stopped);
    }
    Ok(match read_status()? {
        Some(status) if runner_child_is_running(&status, canonical.as_str()) => {
            ServiceState::Running
        }
        _ => ServiceState::Stopped,
    })
}

/// Private-hub state as seen through the supervisor. `NotInstalled` when no
/// hub data directory exists yet.
pub(crate) fn hub_status() -> anyhow::Result<ServiceState> {
    if !crate::cli::hub_service::hub_data_present() {
        return Ok(ServiceState::NotInstalled);
    }
    if supervisor_job_state()? != ServiceState::Running {
        return Ok(ServiceState::Stopped);
    }
    Ok(match read_status()? {
        Some(status) if component_is_running(&status, &status.hub) => ServiceState::Running,
        _ => ServiceState::Stopped,
    })
}

/// A status snapshot is only authoritative while its supervisor process is
/// still alive. A stale file (previous instance that crashed or was replaced,
/// or a startup publish that failed) must never report Running to readers.
pub(super) fn recorded_process_is_alive(pid: Option<u32>, started_at: u64) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    if !feanorfs_agent_core::lock::pid_alive(pid) {
        return false;
    }
    #[cfg(unix)]
    {
        started_at != 0
            && process_start_epoch(pid).is_some_and(|actual| actual.abs_diff(started_at) <= 3)
    }
    #[cfg(not(unix))]
    {
        // Platforms without a trusted creation-time probe retain the existing
        // status-only liveness behavior. They never use this predicate to
        // authorize signaling.
        let _ = started_at;
        true
    }
}

pub(super) fn status_supervisor_alive(status: &SupervisorStatus) -> bool {
    if !recorded_process_is_alive(status.pid, status.started_at) {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        let Some(pid) = status.pid else {
            return false;
        };
        return status
            .process_start_id
            .as_deref()
            .is_some_and(|identity| process_tree::process_start_matches(pid, identity));
    }
    #[cfg(not(target_os = "windows"))]
    true
}

/// A child counts as running only when the reporting supervisor and the exact
/// recorded child process instances are both alive.
pub(super) fn child_is_running(status: &SupervisorStatus, key: &str) -> bool {
    status_supervisor_alive(status)
        && status.workspaces.get(key).is_some_and(|child| {
            child.state == ChildState::Running
                && recorded_process_is_alive(child.pid, child.since)
                && child_identity_is_current(child)
        })
}

pub(super) fn runner_child_is_running(status: &SupervisorStatus, key: &str) -> bool {
    status_supervisor_alive(status)
        && status.runners.get(key).is_some_and(|child| {
            child.state == ChildState::Running
                && recorded_process_is_alive(child.pid, child.since)
                && child_identity_is_current(child)
        })
}

#[cfg(test)]
pub(super) fn runner_recorded_by_dead_supervisor(status: &SupervisorStatus, key: &str) -> bool {
    !status_supervisor_alive(status) && status.runners.contains_key(key)
}

fn component_is_running(status: &SupervisorStatus, component: &Option<ChildStatus>) -> bool {
    status_supervisor_alive(status)
        && component.as_ref().is_some_and(|child| {
            child.state == ChildState::Running
                && recorded_process_is_alive(child.pid, child.since)
                && child_identity_is_current(child)
        })
}

fn child_identity_is_current(child: &ChildStatus) -> bool {
    #[cfg(target_os = "windows")]
    {
        let Some(pid) = child.pid else {
            return false;
        };
        return child
            .process_start_id
            .as_deref()
            .is_some_and(|identity| process_tree::process_start_matches(pid, identity));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = child;
        true
    }
}

pub(crate) fn wait_for_workspace_child(canonical: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if read_status()?.is_some_and(|status| child_is_running(&status, canonical)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "automatic sync did not reach the running state within 5 seconds; check `feanorfs service status` and retry `feanorfs start`"
    )
}

pub(crate) fn wait_for_runner_child(canonical: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if read_status()?.is_some_and(|status| runner_child_is_running(&status, canonical)) {
            std::thread::sleep(Duration::from_millis(100));
            if read_status()?.is_some_and(|status| runner_child_is_running(&status, canonical)) {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "the agent runner did not reach the supervised running state within {} seconds; run `feanorfs agent runner status` and retry",
        timeout.as_secs()
    )
}

pub(crate) fn wait_for_runner_stopped(canonical: &Path) -> anyhow::Result<()> {
    // The wait key is exact contract identity: a non-UTF-8 canonical path
    // cannot be acknowledged and is rejected up front rather than lossy-
    // mangled into a different runner.
    let canonical = CanonicalWorkspacePath::from_exact_string(
        canonical
            .to_str()
            .context("canonical runner workspace path must be valid UTF-8")?
            .to_owned(),
    );
    let canonical_str = canonical.as_str();
    // Capture the token for this stop operation once. A concurrent second
    // removal of the same runner must not replace the first waiter's expected
    // token and let an ABA acknowledgement satisfy the older operation.
    let expected_stop_token = pending_runner_stop_token(canonical_str);
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        // A live supervisor may own a child that is not present in a missing,
        // malformed, or stale status snapshot. Require its durable registry
        // generation acknowledgement before reporting success.
        if supervisor_instance_lock_held()? {
            // A held lock with an unreadable, dead, or otherwise unprovable
            // owner is ambiguous. Do not accept an acknowledgement from an
            // arbitrary live PID; wait until the owner can be validated or
            // the lock is released.
            let Some(owner_pid) = supervisor_lock_owner_pid()? else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            if runner_stop_acknowledged(
                &canonical,
                expected_stop_token.as_deref(),
                Some(owner_pid),
                None,
            )? {
                clear_runner_stop_token(canonical_str, expected_stop_token.as_deref());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let snapshot = read_status()?;
        // If a status names a supervisor that still passes exact liveness,
        // retain the live-owner rule even if the lock file was replaced or
        // temporarily unreadable. Only its registry acknowledgement can end
        // the wait.
        if snapshot.as_ref().is_some_and(status_supervisor_alive) {
            if runner_stop_acknowledged(
                &canonical,
                expected_stop_token.as_deref(),
                snapshot.as_ref().and_then(|status| status.pid),
                snapshot.as_ref().map(|status| status.started_at),
            )? {
                clear_runner_stop_token(canonical_str, expected_stop_token.as_deref());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if let Some(pid) = snapshot
            .as_ref()
            .and_then(|status| stray_runner_worker(status, canonical_str))
        {
            tracing::warn!("stopping orphaned runner worker {pid} for {canonical_str}");
            if let Ok(program) = std::env::current_exe() {
                let command = managed_command_line(&program, "runner-run", canonical_str);
                terminate_stray_pid(pid, STOP_GRACE, &program, &command);
            }
            if feanorfs_agent_core::lock::pid_alive(pid) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        let status_child = snapshot
            .as_ref()
            .and_then(|status| status.runners.get(canonical_str));
        if status_child
            .is_some_and(|child| child.pid.is_some_and(feanorfs_agent_core::lock::pid_alive))
        {
            // The old supervisor is gone, but its recorded worker is still
            // live and could not be proven as a safe legacy orphan. Do not
            // report success merely because status reconciliation stopped.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // A missing/malformed snapshot contains no worker identity to resolve.
        // Only a matching dead-supervisor acknowledgement can prove that its
        // reconciliation completed before the process disappeared.
        let dead_ack = runner_stop_acknowledged_by_dead_supervisor(
            &canonical,
            expected_stop_token.as_deref(),
        )?;
        let has_recorded_worker_pid = status_child.is_some_and(|child| child.pid.is_some());
        if !dead_ack && !has_recorded_worker_pid {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        match finish_runner_workspace_exit(canonical.as_path()) {
            Ok(()) => {
                clear_runner_stop_token(canonical_str, expected_stop_token.as_deref());
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(
                    "runner stop acknowledgement remains pending until exit cleanup succeeds: {error:#}"
                );
            }
        }
    }
    anyhow::bail!(
        "the agent runner did not stop within 5 seconds; retry after its current cancellation finishes"
    )
}

fn wait_for_workspace_stopped(canonical: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + STOP_GRACE;
    let canonical_str = canonical
        .to_str()
        .context("canonical workspace path must be valid UTF-8")?
        .to_owned();
    while Instant::now() < deadline {
        let status_running =
            read_status()?.is_some_and(|status| child_is_running(&status, &canonical_str));
        if !feanorfs_client::is_watching(canonical)
            && !feanorfs_client::lock::is_sync_lock_active(canonical)
            && !status_running
        {
            return Ok(());
        }
        // A dead supervisor cannot terminate its children on registry change.
        // If the still-running watcher is genuinely ours (verified command
        // line), stop it directly so `feanorfs stop` keeps working after a
        // supervisor crash or a background-item toggle in System Settings.
        if supervisor_job_state()? != ServiceState::Running {
            if let Some(pid) = stray_workspace_watcher(&canonical_str) {
                tracing::warn!("stopping orphaned watcher {pid} for {canonical_str}");
                if let Ok(program) = std::env::current_exe() {
                    let command = managed_command_line(&program, "run", &canonical_str);
                    terminate_stray_pid(pid, Duration::from_secs(1), &program, &command);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "automatic sync did not stop within 5 seconds; retry after the current sync finishes"
    )
}

// Supervisor status snapshot

pub(super) fn status_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(STATUS_FILE))
}

pub(super) fn runner_ack_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(RUNNER_ACK_FILE))
}

fn registry_digest(registry: &SupervisorRegistry) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(registry).context("serialize supervisor registry generation")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Digest only the durable stop record for one runner.  In particular, this
/// must not include the complete tombstone map: stopping runner B (or an
/// unrelated workspace mutation) cannot invalidate an already reconciled A.
fn runner_ack_digest(
    registry: &SupervisorRegistry,
    canonical: &CanonicalWorkspacePath,
) -> anyhow::Result<String> {
    let tombstone = registry.runner_stop_tokens.get(canonical);
    let removed = !registry.runners.iter().any(|path| path == canonical);
    let bytes = serde_json::to_vec(&(canonical, removed, tombstone))
        .context("serialize runner stop acknowledgement scope")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Digest the registry plus the per-runner completion projection used to
/// decide whether an acknowledgement publish is needed.  Registry content
/// can remain unchanged while a pending child finishes; including completion
/// state ensures that runner B receives its ACK later without forcing a disk
/// write on every idle poll.
pub(super) fn runner_reconcile_projection_digest(
    children: &BTreeMap<String, ManagedChild>,
    registry: &SupervisorRegistry,
) -> anyhow::Result<String> {
    let completion = registry
        .runner_stop_tokens
        .keys()
        .map(|workspace| {
            (
                workspace.clone(),
                runner_reconciliation_complete_for(children, registry, workspace),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let bytes = serde_json::to_vec(&(registry_digest(registry)?, completion))
        .context("serialize runner reconcile projection")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn write_runner_reconcile_ack_store(store: &RunnerReconcileAckStore) -> anyhow::Result<()> {
    let path = runner_ack_path()?;
    create_store_dir(&path)?;
    let content = serde_json::to_vec_pretty(store)?;
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(&path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(&path)?;
    file.write_all(&content)?;
    file.commit()?;
    Ok(())
}

pub(super) fn publish_runner_reconcile_ack(
    children: &BTreeMap<String, ManagedChild>,
    registry: &SupervisorRegistry,
    started_at: u64,
    generation: u64,
) -> anyhow::Result<()> {
    ensure!(
        registry.mutation_generation != 0,
        "legacy or uninitialized supervisor registry cannot publish a runner acknowledgement"
    );
    #[cfg(test)]
    if TEST_ACK_PUBLISH_FAILURE.load(AtomicOrdering::Acquire) {
        anyhow::bail!("injected runner reconcile acknowledgement publish failure");
    }
    let ack_identity = Some(process_tree::process_start_identifier(
        std::process::id(),
        "supervisor-ack",
    ))
    .filter(|identity| process_tree::process_start_matches(std::process::id(), identity));
    let mut store = read_runner_reconcile_ack_store()?.unwrap_or_default();
    // Publish one independent record per currently removed runner whose own
    // child has disappeared from the in-memory ownership map.  A stopped B
    // must never hold up A, and a still-live desired runner is irrelevant to
    // both records.
    for (workspace, tombstone) in &registry.runner_stop_tokens {
        if registry.runners.iter().any(|path| path == workspace)
            || tombstone.token.is_empty()
            || tombstone.generation == 0
            || !runner_reconciliation_complete_for(children, registry, workspace)
        {
            continue;
        }
        store.acks.insert(
            workspace.clone(),
            RunnerReconcileAck {
                workspace: workspace.clone(),
                pid: std::process::id(),
                process_start_id: ack_identity.clone(),
                started_at,
                registry_digest: runner_ack_digest(registry, workspace)?,
                registry_generation: registry.mutation_generation,
                stop_token: tombstone.token.clone(),
                generation,
            },
        );
    }
    write_runner_reconcile_ack_store(&store)
}

pub(super) fn read_runner_reconcile_ack_store() -> anyhow::Result<Option<RunnerReconcileAckStore>> {
    let Ok(path) = runner_ack_path() else {
        return Ok(None);
    };
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).context("read runner reconcile acknowledgement")?;
    match serde_json::from_str(&content) {
        Ok(status) => Ok(Some(status)),
        Err(_) => Ok(None),
    }
}

/// Compatibility accessor used by older in-tree diagnostics/tests.  It never
/// makes a legacy single-record JSON file authoritative; the matcher below
/// reads the explicit store and requires the per-runner identity fields.
#[cfg(test)]
pub(super) fn read_runner_reconcile_ack() -> anyhow::Result<Option<RunnerReconcileAck>> {
    Ok(read_runner_reconcile_ack_store()?.and_then(|store| store.acks.into_values().next()))
}

/// Returns true when another process currently holds the supervisor instance
/// lock. This is the authoritative liveness check when status publication is
/// missing or malformed: a live lock owner may still own an unreported child,
/// so stop waits rather than treating the absent snapshot as success.
pub(super) fn supervisor_instance_lock_held() -> anyhow::Result<bool> {
    let path = supervisor_lock_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SupervisorLockOwner {
    pub(super) pid: u32,
    pub(super) process_start_id: String,
}

pub(super) fn supervisor_lock_owner_path_at(lock_path: &Path) -> PathBuf {
    lock_path.with_extension("owner")
}

#[cfg(test)]
pub(super) fn read_supervisor_lock_owner_at(lock_path: &Path) -> Option<SupervisorLockOwner> {
    let content = fs::read_to_string(supervisor_lock_owner_path_at(lock_path)).ok()?;
    serde_json::from_str(&content).ok()
}

pub(super) fn write_supervisor_lock_owner_at(
    lock_path: &Path,
    owner: &SupervisorLockOwner,
) -> anyhow::Result<()> {
    let path = supervisor_lock_owner_path_at(lock_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec(owner).context("serialize supervisor lock owner")?;
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(&path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(&path)?;
    file.write_all(&content)?;
    file.commit().context("publish supervisor lock owner")
}

pub(super) fn supervisor_lock_owner_pid() -> anyhow::Result<Option<u32>> {
    let path = supervisor_lock_path()?;
    match fs::read_to_string(supervisor_lock_owner_path_at(&path)) {
        Ok(content) => {
            let owner = serde_json::from_str::<SupervisorLockOwner>(&content).ok();
            return Ok(owner.and_then(|owner| {
                (owner.pid != 0
                    && feanorfs_agent_core::lock::pid_alive(owner.pid)
                    && process_tree::process_start_matches(owner.pid, &owner.process_start_id))
                .then_some(owner.pid)
            }));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Ok(None),
    }

    // Compatibility for supervisors installed before the owner sidecar was
    // introduced. Windows whole-file locks make this record unreadable while
    // held, so old Windows supervisors remain fail-closed until restarted.
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let pid = content
        .lines()
        .next()
        .and_then(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| *pid != 0);
    Ok(pid.filter(|pid| {
        feanorfs_agent_core::lock::pid_alive(*pid)
            && process_tree::ProcessIdentity::capture(*pid).is_some()
    }))
}

fn ack_matches_current_supervisor(
    ack: &RunnerReconcileAck,
    expected_pid: Option<u32>,
    expected_started_at: Option<u64>,
) -> bool {
    if ack.generation == 0
        || !feanorfs_agent_core::lock::pid_alive(ack.pid)
        || expected_pid.is_some_and(|pid| pid != ack.pid)
        || expected_started_at.is_some_and(|started_at| started_at != ack.started_at)
    {
        return false;
    }
    let Some(identity) = ack.process_start_id.as_deref() else {
        return false;
    };
    process_tree::process_start_matches(ack.pid, identity)
}

/// A runner stop is not reconciled while an entry for a *removed* runner
/// remains in the in-memory child map. Other desired runners may remain live;
/// a stop acknowledgement for runner A must not wait for runner B to stop.
/// In particular, a stale removed entry may still be owned by the persistent
/// reaper or need residual process-group cleanup, so it gates the ack.
#[cfg(test)]
pub(super) fn runner_reconciliation_complete(
    children: &BTreeMap<String, ManagedChild>,
    registry: &SupervisorRegistry,
) -> bool {
    !children.values().any(|managed| {
        matches!(&managed.spec.kind, ChildKind::Runner(workspace)
                if !registry.runners.iter().any(|path| path == workspace))
    })
}

fn runner_reconciliation_complete_for(
    children: &BTreeMap<String, ManagedChild>,
    registry: &SupervisorRegistry,
    canonical: &CanonicalWorkspacePath,
) -> bool {
    if registry.runners.iter().any(|path| path == canonical) {
        return false;
    }
    !children.values().any(|managed| {
        matches!(&managed.spec.kind, ChildKind::Runner(workspace) if workspace == canonical)
    })
}

pub(super) fn runner_stop_acknowledged(
    canonical: &CanonicalWorkspacePath,
    expected_stop_token: Option<&str>,
    expected_pid: Option<u32>,
    expected_started_at: Option<u64>,
) -> anyhow::Result<bool> {
    let registry = read_registry()?;
    if registry.runners.iter().any(|path| path == canonical) {
        return Ok(false);
    }
    let Some(ack) =
        read_runner_reconcile_ack_store()?.and_then(|store| store.acks.get(canonical).cloned())
    else {
        return Ok(false);
    };
    let Some(expected_stop_token) = expected_stop_token.filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    let Some(tombstone) = registry.runner_stop_tokens.get(canonical) else {
        return Ok(false);
    };
    Ok(registry.mutation_generation != 0
        && ack.registry_generation != 0
        && tombstone.generation != 0
        && ack.workspace == *canonical
        && tombstone.token == expected_stop_token
        && ack.stop_token == expected_stop_token
        && ack.registry_digest == runner_ack_digest(&registry, canonical)?
        && ack_matches_current_supervisor(&ack, expected_pid, expected_started_at))
}

fn runner_stop_acknowledged_by_dead_supervisor(
    canonical: &CanonicalWorkspacePath,
    expected_stop_token: Option<&str>,
) -> anyhow::Result<bool> {
    let registry = read_registry()?;
    if registry.runners.iter().any(|path| path == canonical) {
        return Ok(false);
    }
    let Some(ack) =
        read_runner_reconcile_ack_store()?.and_then(|store| store.acks.get(canonical).cloned())
    else {
        return Ok(false);
    };
    let Some(expected_stop_token) = expected_stop_token.filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    let Some(tombstone) = registry.runner_stop_tokens.get(canonical) else {
        return Ok(false);
    };
    if registry.mutation_generation == 0
        || ack.registry_generation == 0
        || tombstone.generation == 0
        || ack.workspace != *canonical
        || tombstone.token != expected_stop_token
        || ack.stop_token != expected_stop_token
        || ack.registry_digest != runner_ack_digest(&registry, canonical)?
        || ack.generation == 0
    {
        return Ok(false);
    }
    let Some(identity) = ack.process_start_id.as_deref() else {
        return Ok(false);
    };
    // A matching live PID means the supervisor may still own an unreported
    // worker. A dead PID or an exact native-identity mismatch proves this ack
    // belongs to a terminated supervisor instance; PID reuse is not accepted
    // as the same process.
    Ok(!feanorfs_agent_core::lock::pid_alive(ack.pid)
        || !process_tree::process_start_matches(ack.pid, identity))
}

pub(super) fn publish_status(
    children: &BTreeMap<String, ManagedChild>,
    started_at: u64,
) -> anyhow::Result<()> {
    let status = build_status(children, started_at);
    let path = status_path()?;
    create_store_dir(&path)?;
    let content = serde_json::to_vec_pretty(&status)?;
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(&path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(&path)?;
    file.write_all(&content)?;
    file.commit()?;
    Ok(())
}

pub(super) fn has_pending_startup_gates(children: &BTreeMap<String, ManagedChild>) -> bool {
    children
        .values()
        .any(|managed| managed.startup_gate.is_some())
}

fn release_startup_gate(managed: &mut ManagedChild) -> anyhow::Result<()> {
    let Some(mut gate) = managed.startup_gate.take() else {
        return Ok(());
    };
    #[cfg(unix)]
    let result = gate.release();
    #[cfg(windows)]
    let result = gate.release(managed.process_tree.as_ref(), managed.child.as_ref());
    #[cfg(not(any(unix, windows)))]
    let result = gate.release();
    if let Err(error) = result {
        managed.startup_gate = Some(gate);
        return Err(error.into());
    }
    Ok(())
}

pub(super) fn release_startup_gates(
    children: &mut BTreeMap<String, ManagedChild>,
) -> anyhow::Result<()> {
    let keys = children.keys().cloned().collect::<Vec<_>>();
    let mut first_error = None;
    for key in keys {
        let Some(managed) = children.get_mut(&key) else {
            continue;
        };
        if let Err(error) = release_startup_gate(managed) {
            tracing::warn!("supervised child {key} startup release remains pending: {error:#}");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(super) fn build_status(
    children: &BTreeMap<String, ManagedChild>,
    started_at: u64,
) -> SupervisorStatus {
    let mut status = SupervisorStatus {
        pid: Some(std::process::id()),
        process_start_id: Some(process_tree::process_start_identifier(
            std::process::id(),
            "supervisor",
        ))
        .filter(|identity| process_tree::process_start_matches(std::process::id(), identity)),
        version: STATUS_VERSION,
        started_at,
        updated_at: now_epoch(),
        ..SupervisorStatus::default()
    };
    for managed in children.values() {
        let child = ChildStatus {
            pid: managed
                .child
                .as_ref()
                .and_then(|child| child.id())
                .or_else(|| {
                    managed
                        .pending_orphan
                        .as_ref()
                        .and_then(|orphan| orphan.pid)
                })
                .or(managed.owned_pid),
            process_start_id: managed
                .child
                .as_ref()
                .and_then(|child| child.id())
                .map(|pid| process_tree::process_start_identifier(pid, "supervisor-child"))
                .filter(|identity| {
                    managed
                        .child
                        .as_ref()
                        .and_then(|child| child.id())
                        .is_some_and(|pid| process_tree::process_start_matches(pid, identity))
                })
                .or_else(|| {
                    managed
                        .pending_orphan
                        .as_ref()
                        .and_then(|orphan| orphan.process_start_id.clone())
                })
                .or_else(|| managed.owned_process_start_id.clone()),
            job_owned: {
                #[cfg(target_os = "windows")]
                {
                    managed.process_tree.is_some()
                        || managed
                            .pending_orphan
                            .as_ref()
                            .is_some_and(|orphan| orphan.job_owned)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    false
                }
            },
            executable_identity: managed
                .expected_executable_identity
                .clone()
                .or_else(|| {
                    managed
                        .pending_orphan
                        .as_ref()
                        .and_then(|orphan| orphan.executable_identity.clone())
                })
                .or_else(|| {
                    managed
                        .child
                        .as_ref()
                        .and_then(|child| child.id())
                        .and_then(process_tree::executable_identity_for_pid)
                })
                .or_else(|| {
                    managed
                        .owned_pid
                        .and_then(process_tree::executable_identity_for_pid)
                }),
            state: managed.state,
            restarts: managed.restarts,
            last_exit: managed.last_exit,
            // `owned_since` is captured with the exact PID/start identity and
            // must win once a child is handed to the reaper. An in-memory
            // `Instant` cannot survive a shutdown/restart boundary and would
            // otherwise rewrite the durable handoff to a merely approximate
            // wall-clock value.
            since: (managed.owned_since != 0)
                .then_some(managed.owned_since)
                .or_else(|| {
                    managed
                        .pending_orphan
                        .as_ref()
                        .map(|orphan| orphan.expected_since)
                })
                .or_else(|| managed.spawned_at.map(epoch_at))
                .unwrap_or(0),
        };
        match &managed.spec.kind {
            ChildKind::Hub => status.hub = Some(child),
            ChildKind::Tray => status.tray = Some(child),
            ChildKind::Workspace(workspace) => {
                status
                    .workspaces
                    .insert(workspace.as_str().to_owned(), child);
            }
            ChildKind::Runner(workspace) => {
                status.runners.insert(workspace.as_str().to_owned(), child);
            }
        }
    }
    status
}

pub(crate) fn read_status() -> anyhow::Result<Option<SupervisorStatus>> {
    let Ok(path) = status_path() else {
        return Ok(None);
    };
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path).context("read supervisor status")?;
    match serde_json::from_str(&content) {
        Ok(status) => Ok(Some(status)),
        Err(_) => Ok(None),
    }
}
