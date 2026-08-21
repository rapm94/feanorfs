//! Single supervised background job.
//!
//! FeanorFS registers exactly one per-user OS background job
//! (`com.feanorfs.agent` on macOS launchd / Linux systemd user services,
//! `FeanorFS\Agent` on Windows Task Scheduler) so consumer background-item
//! lists stay at one row. That job runs `feanorfs service supervise`, which
//! spawns and supervises every worker: the automatic private hub
//! (`service hub-run <data-dir>`), each registered workspace watcher
//! (`service run <folder>`), and the desktop tray when one is installed.
//!
//! The set of supervised workspaces lives in a locked atomic registry file
//! (`~/.feanorfs/supervisor.json`) written by `start`/`stop` and the service
//! commands. The supervisor reconciles every 500 ms against that registry,
//! against the hub data directory, and against the hub's `relay.json` and
//! `listen-port` files, starting missing children, terminating children that
//! are no longer desired, and restarting crashed children with bounded
//! exponential backoff. A secret-free status snapshot is published to
//! `~/.feanorfs/supervisor-status.json` for `service status`, `doctor`, and
//! the tray.
//!
//! Ownership of the split:
//! - `registry`: locked registry schema, canonical workspace keys, runner
//!   stop tombstones, and registry mutations.
//! - `installation`: launchd/systemd/Task Scheduler job installation and
//!   readiness.
//! - `status`: constant-cost snapshots, liveness projections, and runner
//!   reconcile acknowledgements.
//! - `child`: desired child specs and the restart state machine.
//! - `loop`: the reconcile loop, instance lock, orphan reaping, and
//!   shutdown.
//! - `migration`: legacy per-component job discovery and removal.
//! - `platform`: platform-specific job/process adapters.
//!
//! The single-supervisor ownership model is preserved: exactly one process
//! holds `supervisor.instance.lock` and owns every child; the canonical
//! process-tree reaper lives in [`crate::cli::process_tree`].
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use feanorfs_client::backoff::{BackoffGrowth, BackoffReset, ExponentialBackoff};

pub(crate) const LABEL: &str = "com.feanorfs.agent";

const REGISTRY_FILE: &str = "supervisor.json";
const STATUS_FILE: &str = "supervisor-status.json";
const RUNNER_ACK_FILE: &str = "supervisor-runner-ack.json";
const MARKER_FILE: &str = "supervisor-service-program";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(5);
pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(5);
// Runner startup also performs native process-tree adoption and exact identity checks.
pub(crate) const RUNNER_READY_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_REAP_GRACE: Duration = Duration::from_secs(1);
/// Child restart backoff: base 1 s doubling from the second failure, 60 s
/// cap. Sequence (restarts 0..): 1, 1, 2, 4, 8, 16, 32, 60, 60, ... The
/// supervisor always increments `restarts` before asking, so the
/// zero-restart base delay only matters for direct unit use.
const CHILD_RESTART_BACKOFF: ExponentialBackoff =
    ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60))
        .with_growth(BackoffGrowth::DoublesFromSecondFailure)
        .with_reset(BackoffReset::Base);
const RESET_AFTER: Duration = Duration::from_secs(60);
const STATUS_VERSION: u32 = 2;
#[cfg(debug_assertions)]
const TEST_MANUAL_SUPERVISOR_ENV: &str = "FEANORFS_TEST_MANUAL_SUPERVISOR";
#[cfg(test)]
static TEST_TERMINATION_GRACE_MILLIS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_FORCE_REAP_TIMEOUT: AtomicBool = AtomicBool::new(false);
#[cfg(all(test, unix))]
static TEST_IDENTITY_UNAVAILABLE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_ACK_PUBLISH_FAILURE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_SHUTDOWN_PANIC_ONCE: AtomicBool = AtomicBool::new(false);
const HUB_CHILD_KEY: &str = "component:hub";
const TRAY_CHILD_KEY: &str = "component:tray";
const WORKSPACE_CHILD_PREFIX: &str = "workspace:";
const RUNNER_CHILD_PREFIX: &str = "runner:";

fn now_epoch() -> u64 {
    epoch_at(Instant::now())
}

fn epoch_at(instant: Instant) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.saturating_sub(instant.elapsed()).as_secs())
        .unwrap_or(0)
}

mod child;
mod installation;
mod r#loop;
mod migration;
mod platform;
mod registry;
mod status;

pub(crate) use child::is_managed_watcher;
pub(crate) use installation::{
    ensure_supervisor_running, installed_program_matches, supervisor_job_running,
    supervisor_job_state,
};
pub(crate) use migration::migrate_legacy_jobs;
pub(crate) use r#loop::run_supervisor;
pub(crate) use registry::{
    add_runner, add_workspace, is_registered, is_runner_registered, registered_workspaces,
    remove_runner_from_registry, remove_workspace_from_registry, runner_stop_authority_exists,
    start_workspace_in_registry, stop_workspace_in_registry,
};
#[cfg(test)]
pub(crate) use status::ChildStatus;
pub(crate) use status::{
    hub_status, install_workspace, read_status, start_workspace, status_for_runner,
    status_for_workspace, stop_workspace, uninstall_workspace, wait_for_runner_child,
    wait_for_runner_stopped, wait_for_workspace_child, ChildState, ServiceState, SupervisorStatus,
};

#[cfg(test)]
use child::{
    await_reap_ticket, force_termination_allowed, managed_orphan_command_matches,
    watcher_command_matches,
};
use child::{
    desired_specs, exact_child_process_start_id, finish_runner_workspace_exit, hub_config_mtime,
    managed_command_line, managed_tray_args, managed_tray_command_line, mark_finished_child_exit,
    poll_stopping_child, process_command_ownership_matches, runner_child_key,
    runner_spawn_is_still_admitted, should_respawn, spawn_child, stray_runner_worker,
    stray_workspace_watcher, terminate_child, terminate_stray_pid,
    terminate_stray_pid_with_identity, tray_orphan_command_matches, workspace_child_key, ChildKind,
    ChildSpec, ManagedChild,
};
#[cfg(not(target_os = "windows"))]
use installation::manager;
#[cfg(target_os = "windows")]
use installation::schtasks;
#[cfg(all(test, target_os = "macos"))]
use migration::plist_program_argument;
#[cfg(unix)]
use platform::capture_owned_identity;
#[cfg(all(test, unix))]
use platform::parse_process_elapsed;
#[cfg(all(test, unix))]
use platform::runner_process_group_exists;
use platform::{
    cleanup_residual_runner_group, process_command_line, process_executable, process_start_epoch,
};
#[cfg(test)]
use platform::{runner_process_start_matches, terminate_verified_runner_group};
#[cfg(test)]
use r#loop::{
    acquire_supervisor_lock_at, current_supervisor_started_at, pending_orphan_cleanup,
    pending_orphan_cleanup_with_state, reconcile, terminate_all_children, OrphanIdentity,
};
use r#loop::{retry_one_pending_orphan_cleanup, supervisor_lock_path, PendingOrphanCleanup};
#[cfg(all(test, unix))]
use registry::open_registry_for_read;
use registry::{
    canonical_workspace_path, clear_runner_stop_token, create_store_dir,
    ensure_workspace_configured, pending_runner_stop_token, read_registry,
    seed_registry_from_recents_if_absent, SupervisorRegistry,
};
#[cfg(test)]
use registry::{
    load_registry, prune_runner_stop_tokens, read_registry_if_present_at, registry_path,
    save_registry, seed_registry_file_if_absent, workspace_registry_key, RunnerStopTombstone,
    MAX_REGISTRY_BYTES, MAX_RUNNER_STOP_TOMBSTONES, MAX_SUPERVISOR_WORKSPACES,
};
#[cfg(test)]
use status::{
    build_status, child_is_running, read_runner_reconcile_ack, read_supervisor_lock_owner_at,
    runner_ack_path, runner_child_is_running, runner_reconciliation_complete,
    runner_recorded_by_dead_supervisor, runner_stop_acknowledged, status_path,
    supervisor_instance_lock_held, supervisor_lock_owner_pid, RunnerReconcileAck,
};
use status::{
    has_pending_startup_gates, publish_runner_reconcile_ack, publish_status,
    read_runner_reconcile_ack_store, recorded_process_is_alive, release_startup_gates,
    runner_reconcile_projection_digest, status_supervisor_alive, supervisor_lock_owner_path_at,
    write_supervisor_lock_owner_at, RunnerReconcileAckStore, SupervisorLockOwner,
};

#[cfg(test)]
mod tests;
