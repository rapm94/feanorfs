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

use anyhow::{ensure, Context as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::process_tree;
use super::util::{record_service_identity, service_identity_matches};

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
const BACKOFF_BASE_SECS: u64 = 1;
const BACKOFF_MAX_SECS: u64 = 60;
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
struct RunnerReconcileAck {
    /// Canonical runner workspace whose stop token this record acknowledges.
    #[serde(default)]
    workspace: String,
    pid: u32,
    #[serde(default)]
    process_start_id: Option<String>,
    started_at: u64,
    registry_digest: String,
    /// Durable registry mutation generation observed by the supervisor. This
    /// is separate from the acknowledgement sequence and prevents an ABA
    /// registry update from satisfying an older stop waiter merely because
    /// the list content returned to the same digest.
    #[serde(default)]
    registry_generation: u64,
    /// Exact durable stop token for `workspace`. This is the per-stop ABA
    /// boundary; unrelated runner tombstones never participate in matching.
    #[serde(default)]
    stop_token: String,
    generation: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RunnerReconcileAckStore {
    /// One durable acknowledgement per canonical runner. Keeping records
    /// independent prevents a stuck runner B from delaying an already
    /// reconciled runner A.
    #[serde(default)]
    acks: BTreeMap<String, RunnerReconcileAck>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SupervisorRegistry {
    #[serde(default)]
    workspaces: Vec<String>,
    #[serde(default)]
    stopped: Vec<String>,
    #[serde(default)]
    runners: Vec<String>,
    /// Durable per-runner stop tombstones. A stop token survives unrelated
    /// registry mutations, is cleared by re-add, and is replaced by every
    /// subsequent removal of the same runner.
    #[serde(default)]
    runner_stop_tokens: BTreeMap<String, RunnerStopTombstone>,
    /// Monotonic mutation generation. Legacy files omit this field and are
    /// intentionally ineligible for stop acknowledgement until a mutation
    /// rewrites them with a non-zero generation.
    #[serde(default)]
    mutation_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RunnerStopTombstone {
    token: String,
    generation: u64,
}

const MAX_RUNNER_STOP_TOMBSTONES: usize = 256;

/// The visible runner controller removes a registry entry and then waits in
/// the same process. Remember the exact durable mutation it requested so a
/// later ABA registry mutation cannot satisfy that waiter. Missing state is
/// fail-closed rather than inferred from the current list contents.
static PENDING_RUNNER_STOP_TOKENS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

fn registry_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(REGISTRY_FILE))
}

fn create_store_dir(path: &Path) -> anyhow::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_store_lock(path: &Path) -> anyhow::Result<File> {
    let lock_path = path.with_extension("lock");
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock = options
        .open(&lock_path)
        .with_context(|| format!("open supervisor registry lock {}", lock_path.display()))?;
    fs2::FileExt::lock_exclusive(&lock)
        .with_context(|| format!("lock supervisor registry {}", lock_path.display()))?;
    Ok(lock)
}

fn load_registry(path: &Path) -> anyhow::Result<SupervisorRegistry> {
    if !path.is_file() {
        return Ok(SupervisorRegistry::default());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("read supervisor registry {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("parse supervisor registry {}", path.display()))
}

fn save_registry(path: &Path, store: &SupervisorRegistry) -> anyhow::Result<()> {
    let content = serde_json::to_string_pretty(store)?;
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(content.as_bytes())?;
    file.commit()?;
    Ok(())
}

fn update_registry<T>(update: impl FnOnce(&mut SupervisorRegistry) -> T) -> anyhow::Result<T> {
    let path = registry_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    let mut store = load_registry(&path)?;
    store.mutation_generation = store
        .mutation_generation
        .checked_add(1)
        .context("supervisor registry mutation generation exhausted")?;
    let result = update(&mut store);
    save_registry(&path, &store)?;
    Ok(result)
}

/// Checked registry mutation used by operations that must not persist a
/// partially-applied change.  The ordinary helper predates fallible runner
/// tombstone allocation and intentionally preserves its legacy "save after
/// closure" behavior; stop-token capacity is a transactional boundary.
fn update_registry_checked<T>(
    update: impl FnOnce(&mut SupervisorRegistry) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = registry_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    let mut store = load_registry(&path)?;
    store.mutation_generation = store
        .mutation_generation
        .checked_add(1)
        .context("supervisor registry mutation generation exhausted")?;
    let result = update(&mut store)?;
    save_registry(&path, &store)?;
    Ok(result)
}

fn read_registry() -> anyhow::Result<SupervisorRegistry> {
    let path = registry_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    load_registry(&path)
}

/// Read the registry without creating either the registry directory or its
/// lock file when the registry has never been created.  Once the registry is
/// present, retain the same locked read boundary as `read_registry` so status
/// projections cannot race a mutator.
fn read_registry_if_present() -> anyhow::Result<SupervisorRegistry> {
    read_registry_if_present_at(&registry_path()?)
}

fn read_registry_if_present_at(path: &Path) -> anyhow::Result<SupervisorRegistry> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "supervisor registry is not a regular file"
            );
            let _lock = open_store_lock(path)?;
            load_registry(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SupervisorRegistry::default())
        }
        Err(error) => Err(error).context("inspect supervisor registry"),
    }
}

fn canonical_workspace_path(workspace: &Path) -> anyhow::Result<String> {
    let canonical = workspace
        .canonicalize()
        .with_context(|| format!("Workspace folder does not exist: {}", workspace.display()))?;
    workspace_registry_key(&canonical)
}

fn workspace_registry_key(canonical: &Path) -> anyhow::Result<String> {
    canonical
        .to_str()
        .map(str::to_owned)
        .context("canonical workspace path must be valid UTF-8")
}

/// Validate that `workspace` is a configured FeanorFS mirror and return its
/// canonical path string.
fn ensure_workspace_configured(workspace: &Path) -> anyhow::Result<String> {
    let canonical = canonical_workspace_path(workspace)?;
    feanorfs_client::load_config(Path::new(&canonical)).with_context(|| {
        format!(
            "{} is not a FeanorFS workspace; run `feanorfs start` there first",
            Path::new(&canonical).display()
        )
    })?;
    Ok(canonical)
}

pub(crate) fn add_workspace(workspace: &Path) -> anyhow::Result<()> {
    let canonical = ensure_workspace_configured(workspace)?;
    update_registry(|store| {
        if !store.workspaces.iter().any(|path| path == &canonical) {
            store.workspaces.push(canonical.clone());
        }
        store.stopped.retain(|path| path != &canonical);
    })
}

pub(crate) fn stop_workspace_in_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = canonical_workspace_path(workspace)?;
    update_registry(|store| {
        if let Some(index) = store.workspaces.iter().position(|path| path == &canonical) {
            store.workspaces.remove(index);
            if !store.stopped.iter().any(|path| path == &canonical) {
                store.stopped.push(canonical.clone());
            }
        }
    })
}

pub(crate) fn start_workspace_in_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = ensure_workspace_configured(workspace)?;
    update_registry(|store| {
        if let Some(index) = store.stopped.iter().position(|path| path == &canonical) {
            store.stopped.remove(index);
        }
        if !store.workspaces.iter().any(|path| path == &canonical) {
            store.workspaces.push(canonical.clone());
        }
    })
}

pub(crate) fn remove_workspace_from_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = canonical_workspace_path(workspace)?;
    update_registry(|store| {
        store.workspaces.retain(|path| path != &canonical);
        store.stopped.retain(|path| path != &canonical);
    })
}

/// True when the workspace is remembered (supervised or explicitly stopped).
pub(crate) fn is_registered(workspace: &Path) -> anyhow::Result<bool> {
    let canonical = canonical_workspace_path(workspace)?;
    let registry = read_registry()?;
    Ok(registry.workspaces.iter().any(|path| path == &canonical)
        || registry.stopped.iter().any(|path| path == &canonical))
}

pub(crate) fn registered_workspaces() -> anyhow::Result<Vec<String>> {
    Ok(read_registry()?.workspaces)
}

/// Register the canonical workspace root whose configured runner may be
/// supervised. Enablement and needs-attention state are checked separately
/// when the supervisor builds its desired child set.
pub(crate) fn add_runner(workspace: &Path) -> anyhow::Result<()> {
    let canonical = ensure_workspace_configured(workspace)?;
    feanorfs_agent_core::RunnerStore::open_configured(Path::new(&canonical))
        .context("open the configured agent runner")?;
    seed_registry_from_recents_if_absent()?;
    update_registry(|store| {
        if !store.runners.iter().any(|path| path == &canonical) {
            store.runners.push(canonical.clone());
        }
        // Re-adding the same runner invalidates every prior stop token. A
        // later removal will create a fresh token, so an old waiter can never
        // acknowledge a new A→removed→A cycle.
        store.runner_stop_tokens.remove(&canonical);
    })
}

pub(crate) fn remove_runner_from_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = canonical_workspace_path(workspace)?;
    seed_registry_from_recents_if_absent()?;
    let token = update_registry_checked(|store| -> anyhow::Result<String> {
        // Concurrent stop callers for the same removed runner share the
        // existing durable tombstone.  Replacing it would strand the first
        // waiter and create an avoidable ABA boundary without any re-add.
        if !store.runners.iter().any(|path| path == &canonical) {
            if let Some(existing) = store.runner_stop_tokens.get(&canonical) {
                ensure!(
                    !existing.token.is_empty() && existing.generation != 0,
                    "existing runner stop tombstone is malformed"
                );
                return Ok(existing.token.clone());
            }
        }
        let ack_store = read_runner_reconcile_ack_store()?.unwrap_or_default();
        let already_present = store.runner_stop_tokens.contains_key(&canonical);
        if !already_present && store.runner_stop_tokens.len() >= MAX_RUNNER_STOP_TOMBSTONES {
            prune_runner_stop_tokens(store, &ack_store)?;
            ensure!(
                store.runner_stop_tokens.len() < MAX_RUNNER_STOP_TOMBSTONES,
                "runner stop tombstone capacity ({MAX_RUNNER_STOP_TOMBSTONES}) is full; wait for a durable stop acknowledgement before removing another runner"
            );
        }
        store.runners.retain(|path| path != &canonical);
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).context("generate runner stop token")?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(canonical.as_bytes());
        hasher.update(&store.mutation_generation.to_le_bytes());
        hasher.update(&nonce);
        let token = hasher.finalize().to_hex().to_string();
        store.runner_stop_tokens.insert(
            canonical.clone(),
            RunnerStopTombstone {
                token: token.clone(),
                generation: store.mutation_generation,
            },
        );
        Ok(token)
    })?;
    remember_runner_stop_token(canonical, token);
    Ok(())
}

fn remember_runner_stop_token(canonical: String, token: String) {
    let mut pending = PENDING_RUNNER_STOP_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.insert(canonical, token);
}

fn pending_runner_stop_token(canonical: &str) -> Option<String> {
    PENDING_RUNNER_STOP_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(canonical)
        .cloned()
}

fn clear_runner_stop_token(canonical: &str, expected: Option<&str>) {
    let mut pending = PENDING_RUNNER_STOP_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if expected.is_none_or(|token| {
        pending
            .get(canonical)
            .is_some_and(|current| current == token)
    }) {
        pending.remove(canonical);
    }
}

fn prune_runner_stop_tokens(
    registry: &mut SupervisorRegistry,
    ack_store: &RunnerReconcileAckStore,
) -> anyhow::Result<()> {
    while registry.runner_stop_tokens.len() >= MAX_RUNNER_STOP_TOMBSTONES {
        let oldest = registry
            .runner_stop_tokens
            .iter()
            .filter(|(workspace, tombstone)| {
                ack_store.acks.get(*workspace).is_some_and(|ack| {
                    ack.workspace == **workspace && ack.stop_token == tombstone.token
                })
            })
            .min_by_key(|(workspace, tombstone)| (tombstone.generation, *workspace))
            .map(|(workspace, _)| workspace.clone());
        let Some(oldest) = oldest else {
            anyhow::bail!(
                "runner stop tombstone capacity ({MAX_RUNNER_STOP_TOMBSTONES}) is full and no completed tombstone can be reclaimed"
            );
        };
        registry.runner_stop_tokens.remove(&oldest);
    }
    Ok(())
}

pub(crate) fn is_runner_registered(workspace: &Path) -> anyhow::Result<bool> {
    let canonical = canonical_workspace_path(workspace)?;
    Ok(read_registry_if_present()?
        .runners
        .iter()
        .any(|path| path == &canonical))
}

/// Returns whether a durable supervisor status artifact still establishes
/// runner authority for `workspace` before a visible controller mutates the
/// runner state. A status entry is retained even when its child is stopped: it
/// is evidence that a supervisor may have owned this runner. Reconcile acks do
/// not carry a workspace identity, so they are intentionally not sufficient
/// evidence by themselves; otherwise an unrelated stale ack would make a
/// truly fresh `runner stop` wait forever.
pub(crate) fn runner_stop_authority_exists(workspace: &Path) -> anyhow::Result<bool> {
    let canonical = canonical_workspace_path(workspace)?;
    Ok(read_status()?.is_some_and(|status| status.runners.contains_key(&canonical)))
}

/// Seed the registry from recent workspaces on first use (legacy installs and
/// fresh profiles). Never resurrects workspaces an explicit `service stop` or
/// `feanorfs stop` moved out of supervision.
fn seed_registry_from_recents_if_absent() -> anyhow::Result<()> {
    let path = registry_path()?;
    if path.is_file() {
        return Ok(());
    }
    let recent = feanorfs_client::list_recent_workspaces()?;
    let mut workspaces = Vec::new();
    for entry in recent.workspaces {
        let workspace = Path::new(&entry.path);
        if workspace.is_dir()
            && feanorfs_agent_core::workspace_is_configured(workspace)
            && !workspaces.contains(&entry.path)
        {
            workspaces.push(entry.path);
        }
    }
    seed_registry_file_if_absent(&path, workspaces)
}

fn seed_registry_file_if_absent(path: &Path, workspaces: Vec<String>) -> anyhow::Result<()> {
    create_store_dir(path)?;
    let _lock = open_store_lock(path)?;
    if path.is_file() {
        return Ok(());
    }
    save_registry(
        path,
        &SupervisorRegistry {
            workspaces,
            mutation_generation: 1,
            ..SupervisorRegistry::default()
        },
    )
}

// ---------------------------------------------------------------------------
// Supervisor job installation
// ---------------------------------------------------------------------------

fn marker_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(MARKER_FILE))
}

pub(crate) fn installed_program_matches() -> bool {
    let Ok(marker) = marker_path() else {
        return false;
    };
    let Ok(program) = std::env::current_exe() else {
        return false;
    };
    service_identity_matches(&marker, &[&program])
}

pub(crate) fn supervisor_job_state() -> anyhow::Result<ServiceState> {
    if manual_supervisor_running_for_test()? {
        return Ok(ServiceState::Running);
    }
    #[cfg(not(target_os = "windows"))]
    {
        use service_manager::{ServiceLevel, ServiceManager, ServiceStatus, ServiceStatusCtx};
        let mut manager = <dyn ServiceManager>::native().context("detect service manager")?;
        manager
            .set_level(ServiceLevel::User)
            .context("select per-user service management")?;
        let label: service_manager::ServiceLabel =
            LABEL.parse().context("build supervisor service label")?;
        let status = manager
            .status(ServiceStatusCtx { label })
            .context("read supervisor service status")?;
        Ok(match status {
            ServiceStatus::NotInstalled => ServiceState::NotInstalled,
            ServiceStatus::Running => ServiceState::Running,
            ServiceStatus::Stopped(_) => ServiceState::Stopped,
        })
    }
    #[cfg(target_os = "windows")]
    {
        Ok(
            match super::util::windows_task_running("\\FeanorFS\\", "Agent", "FeanorFS\\Agent")? {
                None => ServiceState::NotInstalled,
                Some(true) => ServiceState::Running,
                Some(false) => ServiceState::Stopped,
            },
        )
    }
}

#[cfg(not(target_os = "windows"))]
fn stop_supervisor_job() -> anyhow::Result<()> {
    use service_manager::{ServiceStatus, ServiceStatusCtx, ServiceStopCtx};
    let manager = manager()?;
    let label = native_label()?;
    if manager
        .status(ServiceStatusCtx {
            label: label.clone(),
        })
        .context("read supervisor service status")?
        != ServiceStatus::Running
    {
        return Ok(());
    }
    manager
        .stop(ServiceStopCtx { label })
        .context("stop the previous FeanorFS supervisor during upgrade")
}

#[cfg(not(target_os = "windows"))]
fn install_supervisor_job(program: &Path) -> anyhow::Result<()> {
    use service_manager::{RestartPolicy, ServiceInstallCtx, ServiceStartCtx};
    let manager = manager()?;
    let label = native_label()?;
    let mut environment: Vec<(String, String)> = super::service::find_tray_program(program)
        .into_iter()
        .map(|tray| {
            (
                "FEANORFS_TRAY_BIN".to_string(),
                tray.to_string_lossy().into_owned(),
            )
        })
        .collect();
    // Propagate the documented state-root override so sandboxed installs and
    // tests supervise the same profile they configured.
    if let Some(root) = std::env::var_os("FEANORFS_HOME") {
        environment.push((
            "FEANORFS_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        ));
    }
    manager
        .install(ServiceInstallCtx {
            label: label.clone(),
            program: program.to_path_buf(),
            args: vec![OsString::from("service"), OsString::from("supervise")],
            contents: None,
            username: None,
            working_directory: None,
            environment: Some(environment),
            autostart: true,
            restart_policy: RestartPolicy::OnFailure {
                delay_secs: None,
                max_retries: None,
                reset_after_secs: None,
            },
        })
        .context("install the FeanorFS supervisor job")?;
    manager
        .start(ServiceStartCtx { label })
        .context("start the FeanorFS supervisor job")
}

#[cfg(not(target_os = "windows"))]
fn manager() -> anyhow::Result<Box<dyn service_manager::ServiceManager>> {
    use service_manager::{ServiceLevel, ServiceManager};
    let mut manager = <dyn ServiceManager>::native().context("detect service manager")?;
    manager
        .set_level(ServiceLevel::User)
        .context("select per-user service management")?;
    Ok(manager)
}

#[cfg(not(target_os = "windows"))]
fn native_label() -> anyhow::Result<service_manager::ServiceLabel> {
    LABEL.parse().context("build supervisor service label")
}

#[cfg(target_os = "windows")]
fn stop_supervisor_job() -> anyhow::Result<()> {
    let _ = schtasks(&["/End", "/TN", "FeanorFS\\Agent"]);
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_supervisor_job(program: &Path) -> anyhow::Result<()> {
    let program = program.display().to_string();
    if program.contains('"') {
        anyhow::bail!("Windows paths containing double quotes cannot be installed as tasks");
    }
    super::util::windows_register_task(
        "\\FeanorFS\\",
        "Agent",
        &program,
        "service supervise",
        false,
    )
    .context("install the FeanorFS supervisor task")?;
    let output = schtasks(&["/Run", "/TN", "FeanorFS\\Agent"])?;
    if !output.status.success() {
        anyhow::bail!(
            "start the FeanorFS supervisor task: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn schtasks(args: &[&str]) -> anyhow::Result<std::process::Output> {
    std::process::Command::new("schtasks.exe")
        .args(args)
        .output()
        .context("run Windows Task Scheduler")
}

/// Install (or reinstall after an executable upgrade) the single supervisor
/// job and wait until it reports running. Returns true when the job had to be
/// reinstalled, which means every worker restarts.
pub(crate) fn ensure_supervisor_running() -> anyhow::Result<bool> {
    if manual_supervisor_running_for_test()? {
        return Ok(false);
    }
    seed_registry_from_recents_if_absent()?;
    let program = std::env::current_exe().context("locate the feanorfs executable")?;
    let state = supervisor_job_state()?;
    let marker = marker_path()?;
    let install_required =
        state == ServiceState::NotInstalled || !service_identity_matches(&marker, &[&program]);
    if install_required {
        if state == ServiceState::Running {
            stop_supervisor_job()?;
        }
        install_supervisor_job(&program)?;
        record_service_identity(&marker, &[&program])
            .context("record the FeanorFS supervisor executable")?;
    } else if state == ServiceState::Stopped {
        install_supervisor_job(&program)?;
    }
    wait_for_job_running()?;
    // Only after the supervisor job is proven running: adopt and remove the
    // legacy per-component jobs. A supervisor startup failure must never leave
    // the machine without background services, and while the legacy jobs are
    // still up the supervisor's stray-watcher detection (plus the hub port
    // bind) handles the brief overlap.
    migrate_legacy_jobs()?;
    Ok(install_required)
}

/// Test-only escape hatch for real-process CLI coverage. Release binaries
/// ignore the environment flag completely; debug builds accept it only from
/// the isolated file-credential test profile and only while the exact current
/// binary has already published a live supervisor status snapshot.
fn manual_supervisor_running_for_test() -> anyhow::Result<bool> {
    #[cfg(debug_assertions)]
    {
        use std::ffi::OsStr;

        if std::env::var_os(TEST_MANUAL_SUPERVISOR_ENV).as_deref() != Some(OsStr::new("1")) {
            return Ok(false);
        }
        ensure!(
            std::env::var_os("FEANORFS_CREDENTIAL_STORE").as_deref() == Some(OsStr::new("file"))
                && std::env::var_os("FEANORFS_HOME").is_some(),
            "the manual supervisor test bypass requires an isolated file-credential test profile"
        );
        let status = read_status()?
            .context("the manual supervisor test bypass requires a live supervisor status")?;
        ensure!(
            status.version == STATUS_VERSION && status_supervisor_alive(&status),
            "the manual supervisor test bypass requires a live matching supervisor status"
        );
        #[cfg(unix)]
        {
            let pid = status
                .pid
                .context("the manual supervisor test status has no process id")?;
            let expected = std::fs::canonicalize(
                std::env::current_exe().context("locate the current feanorfs executable")?,
            )?;
            let actual = process_executable(pid)
                .and_then(|path| std::fs::canonicalize(path).ok())
                .context("read the manual supervisor executable")?;
            ensure!(
                actual == expected,
                "the manual supervisor test status belongs to another executable"
            );
            Ok(true)
        }
        #[cfg(not(unix))]
        {
            Ok(true)
        }
    }
    #[cfg(not(debug_assertions))]
    {
        Ok(false)
    }
}

/// True when the supervisor job is installed and running.
pub(crate) fn supervisor_job_running() -> anyhow::Result<bool> {
    Ok(supervisor_job_state()? == ServiceState::Running)
}

fn wait_for_job_running() -> anyhow::Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if supervisor_job_state()? == ServiceState::Running {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!(
        "the FeanorFS background job did not reach the running state within 5 seconds; check `feanorfs doctor` and retry"
    )
}

// ---------------------------------------------------------------------------
// Workspace service surface (used by `service install|status|start|stop|uninstall`)
// ---------------------------------------------------------------------------

pub(crate) fn install_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    add_workspace(Path::new(&canonical))?;
    ensure_supervisor_running()?;
    wait_for_workspace_child(&canonical, READY_TIMEOUT)?;
    Ok(ServiceState::Running)
}

pub(crate) fn start_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    if !is_registered(Path::new(&canonical))? {
        anyhow::bail!("Automatic sync is not installed; run `feanorfs service install`");
    }
    start_workspace_in_registry(Path::new(&canonical))?;
    ensure_supervisor_running()?;
    wait_for_workspace_child(&canonical, READY_TIMEOUT)?;
    Ok(ServiceState::Running)
}

pub(crate) fn stop_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    if !is_registered(Path::new(&canonical))? {
        return Ok(ServiceState::NotInstalled);
    }
    stop_workspace_in_registry(Path::new(&canonical))?;
    wait_for_workspace_stopped(Path::new(&canonical))?;
    Ok(ServiceState::Stopped)
}

pub(crate) fn uninstall_workspace(workspace: &Path) -> anyhow::Result<ServiceState> {
    let canonical = ensure_workspace_configured(workspace)?;
    if !is_registered(Path::new(&canonical))? {
        return Ok(ServiceState::NotInstalled);
    }
    remove_workspace_from_registry(Path::new(&canonical))?;
    wait_for_workspace_stopped(Path::new(&canonical))?;
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
        Some(status) if child_is_running(&status, &canonical) => ServiceState::Running,
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
        Some(status) if runner_child_is_running(&status, &canonical) => ServiceState::Running,
        _ => ServiceState::Stopped,
    })
}

/// Private-hub state as seen through the supervisor. `NotInstalled` when no
/// hub data directory exists yet.
pub(crate) fn hub_status() -> anyhow::Result<ServiceState> {
    if !super::hub_service::hub_data_present() {
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
fn recorded_process_is_alive(pid: Option<u32>, started_at: u64) -> bool {
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

fn status_supervisor_alive(status: &SupervisorStatus) -> bool {
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
fn child_is_running(status: &SupervisorStatus, key: &str) -> bool {
    status_supervisor_alive(status)
        && status.workspaces.get(key).is_some_and(|child| {
            child.state == ChildState::Running
                && recorded_process_is_alive(child.pid, child.since)
                && child_identity_is_current(child)
        })
}

fn runner_child_is_running(status: &SupervisorStatus, key: &str) -> bool {
    status_supervisor_alive(status)
        && status.runners.get(key).is_some_and(|child| {
            child.state == ChildState::Running
                && recorded_process_is_alive(child.pid, child.since)
                && child_identity_is_current(child)
        })
}

#[cfg(test)]
fn runner_recorded_by_dead_supervisor(status: &SupervisorStatus, key: &str) -> bool {
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
    let canonical_str = canonical
        .to_str()
        .context("canonical runner workspace path must be valid UTF-8")?
        .to_owned();
    // Capture the token for this stop operation once. A concurrent second
    // removal of the same runner must not replace the first waiter's expected
    // token and let an ABA acknowledgement satisfy the older operation.
    let expected_stop_token = pending_runner_stop_token(&canonical_str);
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
                &canonical_str,
                expected_stop_token.as_deref(),
                Some(owner_pid),
                None,
            )? {
                clear_runner_stop_token(&canonical_str, expected_stop_token.as_deref());
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
                &canonical_str,
                expected_stop_token.as_deref(),
                snapshot.as_ref().and_then(|status| status.pid),
                snapshot.as_ref().map(|status| status.started_at),
            )? {
                clear_runner_stop_token(&canonical_str, expected_stop_token.as_deref());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if let Some(pid) = snapshot
            .as_ref()
            .and_then(|status| stray_runner_worker(status, &canonical_str))
        {
            tracing::warn!("stopping orphaned runner worker {pid} for {canonical_str}");
            if let Ok(program) = std::env::current_exe() {
                let command = managed_command_line(&program, "runner-run", &canonical_str);
                terminate_stray_pid(pid, STOP_GRACE, &program, &command);
            }
            if feanorfs_agent_core::lock::pid_alive(pid) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        let status_child = snapshot
            .as_ref()
            .and_then(|status| status.runners.get(&canonical_str));
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
            &canonical_str,
            expected_stop_token.as_deref(),
        )?;
        let has_recorded_worker_pid = status_child.is_some_and(|child| child.pid.is_some());
        if !dead_ack && !has_recorded_worker_pid {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        match finish_runner_workspace_exit(canonical) {
            Ok(()) => {
                clear_runner_stop_token(&canonical_str, expected_stop_token.as_deref());
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

// ---------------------------------------------------------------------------
// Supervisor status snapshot
// ---------------------------------------------------------------------------

fn status_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(STATUS_FILE))
}

fn runner_ack_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(RUNNER_ACK_FILE))
}

fn registry_digest(registry: &SupervisorRegistry) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(registry).context("serialize supervisor registry generation")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Digest only the durable stop record for one runner.  In particular, this
/// must not include the complete tombstone map: stopping runner B (or an
/// unrelated workspace mutation) cannot invalidate an already reconciled A.
fn runner_ack_digest(registry: &SupervisorRegistry, canonical: &str) -> anyhow::Result<String> {
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
fn runner_reconcile_projection_digest(
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

fn publish_runner_reconcile_ack(
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

fn read_runner_reconcile_ack_store() -> anyhow::Result<Option<RunnerReconcileAckStore>> {
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
fn read_runner_reconcile_ack() -> anyhow::Result<Option<RunnerReconcileAck>> {
    Ok(read_runner_reconcile_ack_store()?.and_then(|store| store.acks.into_values().next()))
}

/// Returns true when another process currently holds the supervisor instance
/// lock. This is the authoritative liveness check when status publication is
/// missing or malformed: a live lock owner may still own an unreported child,
/// so stop waits rather than treating the absent snapshot as success.
fn supervisor_instance_lock_held() -> anyhow::Result<bool> {
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
struct SupervisorLockOwner {
    pid: u32,
    process_start_id: String,
}

fn supervisor_lock_owner_path_at(lock_path: &Path) -> PathBuf {
    lock_path.with_extension("owner")
}

#[cfg(test)]
fn read_supervisor_lock_owner_at(lock_path: &Path) -> Option<SupervisorLockOwner> {
    let content = fs::read_to_string(supervisor_lock_owner_path_at(lock_path)).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_supervisor_lock_owner_at(
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

fn supervisor_lock_owner_pid() -> anyhow::Result<Option<u32>> {
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
fn runner_reconciliation_complete(
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
    canonical: &str,
) -> bool {
    if registry.runners.iter().any(|path| path == canonical) {
        return false;
    }
    !children.values().any(|managed| {
        matches!(&managed.spec.kind, ChildKind::Runner(workspace) if workspace == canonical)
    })
}

fn runner_stop_acknowledged(
    canonical: &str,
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
        && ack.workspace == canonical
        && tombstone.token == expected_stop_token
        && ack.stop_token == expected_stop_token
        && ack.registry_digest == runner_ack_digest(&registry, canonical)?
        && ack_matches_current_supervisor(&ack, expected_pid, expected_started_at))
}

fn runner_stop_acknowledged_by_dead_supervisor(
    canonical: &str,
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
        || ack.workspace != canonical
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

fn publish_status(
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

fn has_pending_startup_gates(children: &BTreeMap<String, ManagedChild>) -> bool {
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

fn release_startup_gates(children: &mut BTreeMap<String, ManagedChild>) -> anyhow::Result<()> {
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

fn build_status(children: &BTreeMap<String, ManagedChild>, started_at: u64) -> SupervisorStatus {
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
                status.workspaces.insert(workspace.clone(), child);
            }
            ChildKind::Runner(workspace) => {
                status.runners.insert(workspace.clone(), child);
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

// ---------------------------------------------------------------------------
// The supervisor loop
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ChildSpec {
    kind: ChildKind,
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    restart_on_zero_exit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildKind {
    Hub,
    Tray,
    Workspace(String),
    Runner(String),
}

fn workspace_child_key(workspace: &str) -> String {
    format!("{WORKSPACE_CHILD_PREFIX}{workspace}")
}

fn runner_child_key(workspace: &str) -> String {
    format!("{RUNNER_CHILD_PREFIX}{workspace}")
}

struct ManagedChild {
    spec: ChildSpec,
    /// Target image identity captured from `spec.program` before launch.  A
    /// Unix startup gate initially runs this binary's wrapper, so status must
    /// never infer the durable worker identity from the pre-exec PID.
    expected_executable_identity: Option<String>,
    child: Option<tokio::process::Child>,
    /// Set when termination had to defer the kernel wait to the persistent
    /// reaper. The ManagedChild remains in the supervisor map until this
    /// ticket completes, so reconciliation cannot acknowledge a runner stop
    /// while the exact child is still owned elsewhere.
    pending_reap: Option<ReapTicket>,
    /// A child recorded by a previous supervisor but not safely reaped at
    /// startup. PID-only orphan cleanup is deliberately represented in the
    /// same map as live children: until this ticket completes, the exact
    /// runner authority remains stopping and cannot be acknowledged or
    /// respawned.
    pending_orphan: Option<PendingOrphanCleanup>,
    /// Native process-tree ownership for children spawned by this
    /// supervisor. Unix uses a fresh process group; Windows uses a private
    /// Job Object. Retain it through root wait and residual verification.
    process_tree: Option<process_tree::ProcessTree>,
    /// Child has entered the trusted internal startup wrapper but has not yet
    /// been released to exec its configured worker. Keep the gate owned until
    /// the status/identity ledger is durably published.
    startup_gate: Option<process_tree::StartupGate>,
    /// Durable identity of the exact direct child, retained after its Tokio
    /// handle is handed to the background reaper. These fields are projected
    /// into `supervisor-status.json` so a replacement supervisor can retry
    /// the same process instead of treating `pid: null` as completed.
    owned_pid: Option<u32>,
    owned_process_start_id: Option<String>,
    owned_since: u64,
    state: ChildState,
    restarts: u32,
    last_exit: Option<i32>,
    backoff_until: Option<Instant>,
    spawned_at: Option<Instant>,
}

#[cfg(windows)]
fn assert_managed_child_send<T: Send>() {}

#[cfg(windows)]
const _: fn() = assert_managed_child_send::<ManagedChild>;

impl ManagedChild {
    fn new(spec: ChildSpec) -> Self {
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

/// Persistent handoff for a child whose Tokio wait path is temporarily
/// unavailable.  Dropping a live `tokio::process::Child` does not reap it;
/// this coordinator keeps ownership until a later `try_wait` succeeds.
#[derive(Clone)]
struct ReapTicket {
    completed: Arc<AtomicBool>,
}

impl ReapTicket {
    fn new() -> Self {
        Self {
            completed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_complete(&self) -> bool {
        self.completed.load(AtomicOrdering::Acquire)
    }

    fn complete(&self) {
        self.completed.store(true, AtomicOrdering::Release);
    }
}

async fn await_reap_ticket(ticket: &ReapTicket) {
    while !ticket.is_complete() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct ReaperEntry {
    child: tokio::process::Child,
    ticket: ReapTicket,
}

struct SupervisorChildReaper {
    pending: Mutex<VecDeque<ReaperEntry>>,
    wake: Condvar,
    initialization: Mutex<()>,
    ready: AtomicBool,
    #[cfg(test)]
    fail_next_enqueue: AtomicBool,
    #[cfg(test)]
    fail_worker_start: AtomicBool,
}

static SUPERVISOR_CHILD_REAPER: SupervisorChildReaper = SupervisorChildReaper::new();

impl SupervisorChildReaper {
    const fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            initialization: Mutex::new(()),
            ready: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_enqueue: AtomicBool::new(false),
            #[cfg(test)]
            fail_worker_start: AtomicBool::new(false),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, VecDeque<ReaperEntry>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ensure_ready(&'static self) -> std::io::Result<()> {
        #[cfg(test)]
        if self.fail_worker_start.load(AtomicOrdering::Acquire) {
            return Err(std::io::Error::other(
                "supervisor reaper worker start failure injected by test",
            ));
        }
        if !self.ready.load(AtomicOrdering::Acquire) {
            let _initialization = self
                .initialization
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !self.ready.load(AtomicOrdering::Acquire) {
                let thread = std::thread::Builder::new()
                    .name("feanorfs-supervisor-reaper".to_string())
                    .spawn(move || {
                        let _ =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run()));
                        self.ready.store(false, AtomicOrdering::Release);
                    })?;
                drop(thread);
                self.ready.store(true, AtomicOrdering::Release);
            }
        }
        Ok(())
    }

    /// Transfer a child to the persistent reaper, or retain it in this async
    /// caller until its owned process is synchronously reaped.  The function
    /// is intentionally infallible: once called, no live child remains in a
    /// caller-owned `ManagedChild` that may be dropped on an error path.
    async fn enqueue(&'static self, child_slot: &mut Option<tokio::process::Child>) -> ReapTicket {
        let ticket = ReapTicket::new();
        #[cfg(test)]
        let primary_failed = self.fail_next_enqueue.swap(false, AtomicOrdering::AcqRel);
        #[cfg(not(test))]
        let primary_failed = false;

        if !primary_failed && self.ensure_ready().is_ok() {
            let child = child_slot
                .take()
                .expect("reaper enqueue owns a live child handle");
            let mut pending = self.pending();
            pending.push_back(ReaperEntry {
                child,
                ticket: ticket.clone(),
            });
            drop(pending);
            self.wake.notify_one();
            return ticket;
        }

        // An unavailable coordinator cannot be allowed to turn the Child
        // back into a fallible return value.  Retain ownership locally and
        // wait on the original Tokio handle until the kernel reports it
        // reaped.  `Child::wait` never signals by a guessed PID, and retries
        // preserve the handle even if an unusual wait error is transient.
        if primary_failed {
            tracing::warn!(
                "supervisor child reaper enqueue failure; synchronously retaining child"
            );
        } else {
            tracing::warn!("supervisor child reaper unavailable; synchronously retaining child");
        }
        loop {
            match child_slot
                .as_mut()
                .expect("synchronous reaper fallback retains child handle")
                .wait()
                .await
            {
                Ok(_) => {
                    // The kernel wait completed while this future still
                    // retained the handle in the caller's slot. Drop only
                    // that now-reaped handle; cancellation before this point
                    // leaves it available for the caller's ownership guard.
                    child_slot.take();
                    ticket.complete();
                    break;
                }
                Err(error) => {
                    tracing::warn!(
                        "supervisor child synchronous reap failed; retaining child: {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        ticket
    }

    #[cfg(test)]
    fn fail_next_enqueue_for_test(&self) {
        self.fail_next_enqueue.store(true, AtomicOrdering::Release);
    }

    #[cfg(test)]
    fn fail_worker_start_for_test(&self, fail: bool) {
        self.fail_worker_start.store(fail, AtomicOrdering::Release);
    }

    fn wait_for_child(&'static self) -> ReaperEntry {
        let mut pending = self.pending();
        loop {
            if let Some(entry) = pending.pop_front() {
                return entry;
            }
            pending = self
                .wake
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn requeue(&'static self, entry: ReaperEntry) {
        let mut pending = self.pending();
        pending.push_back(entry);
        drop(pending);
        self.wake.notify_one();
    }

    fn run(&'static self) -> ! {
        loop {
            let mut child = SupervisorReaperChild::new(self, self.wait_for_child());
            loop {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    child.child_mut().try_wait()
                }));
                match result {
                    Ok(Ok(Some(_))) => {
                        child.reaped();
                        break;
                    }
                    Ok(Ok(None)) => std::thread::sleep(Duration::from_millis(100)),
                    Ok(Err(error)) => {
                        tracing::warn!(
                            "supervisor child reaper wait failed; retaining child: {error}"
                        );
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => {
                        tracing::warn!("supervisor child reaper recovered from child wait panic");
                        break;
                    }
                }
            }
        }
    }
}

struct SupervisorReaperChild {
    reaper: &'static SupervisorChildReaper,
    child: Option<tokio::process::Child>,
    ticket: ReapTicket,
}

impl SupervisorReaperChild {
    fn new(reaper: &'static SupervisorChildReaper, entry: ReaperEntry) -> Self {
        Self {
            reaper,
            child: Some(entry.child),
            ticket: entry.ticket,
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child
            .as_mut()
            .expect("supervisor reaper child present")
    }

    fn reaped(&mut self) {
        self.child = None;
        self.ticket.complete();
    }
}

impl Drop for SupervisorReaperChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            self.reaper.requeue(ReaperEntry {
                child,
                ticket: self.ticket.clone(),
            });
        }
    }
}

/// True when a desired-but-missing child should be spawned on this poll.
///
/// A child that exited cleanly and opted out of restarts (e.g. the user quit
/// the tray, or the hub shut down on purpose) must stay stopped: without this
/// the desired-set diff respawns it within one poll, and a clean-exit child
/// would spin without ever engaging backoff.
fn should_respawn(managed: &ManagedChild) -> bool {
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

fn backoff_duration(restarts: u32) -> Duration {
    let secs = BACKOFF_BASE_SECS.saturating_mul(1u64 << restarts.saturating_sub(1).min(6));
    Duration::from_secs(secs.min(BACKOFF_MAX_SECS))
}

fn desired_specs(
    registry: &SupervisorRegistry,
    tray_program: &Option<PathBuf>,
) -> anyhow::Result<BTreeMap<String, ChildSpec>> {
    let mut desired = BTreeMap::new();
    let program = std::env::current_exe().context("locate the feanorfs executable")?;

    if super::hub_service::hub_data_present() {
        let data_dir = super::hub_service::default_data_dir()?;
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
                args: Vec::new(),
                env: vec![(
                    OsString::from("FEANORFS_BIN"),
                    program.clone().into_os_string(),
                )],
                restart_on_zero_exit: false,
            },
        );
    }
    for path in &registry.workspaces {
        let workspace = Path::new(path);
        if !workspace.is_dir() || !feanorfs_agent_core::workspace_is_configured(workspace) {
            continue;
        }
        desired.insert(
            workspace_child_key(path),
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
        let workspace = Path::new(path);
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
            runner_child_key(path),
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

fn hub_config_mtime(relative: &str) -> Option<u64> {
    super::hub_service::default_data_dir()
        .ok()
        .map(|dir| dir.join(relative))
        .and_then(|path| fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
}

struct SpawnedChild {
    child: tokio::process::Child,
    process_tree: process_tree::ProcessTree,
    startup_gate: process_tree::StartupGate,
}

fn exact_child_process_start_id(pid: u32) -> Option<String> {
    let identity = process_tree::process_start_identifier(pid, "supervisor-child");
    process_tree::process_start_matches(pid, &identity).then_some(identity)
}

async fn spawn_child(spec: &ChildSpec) -> anyhow::Result<SpawnedChild> {
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
                    let ticket = SUPERVISOR_CHILD_REAPER.enqueue(&mut child).await;
                    await_reap_ticket(&ticket).await;
                }
                Err(_) => {
                    tracing::warn!(
                        "supervisor child adoption failed before bounded reap; retaining child"
                    );
                    let mut child = Some(child);
                    let ticket = SUPERVISOR_CHILD_REAPER.enqueue(&mut child).await;
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
fn force_termination_allowed(identity_current: bool, command_current: bool) -> bool {
    identity_current && command_current
}

fn mark_finished_child_exit(managed: &mut ManagedChild) -> anyhow::Result<()> {
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
fn poll_stopping_child(managed: &mut ManagedChild) -> anyhow::Result<bool> {
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

async fn terminate_child(managed: &mut ManagedChild) -> anyhow::Result<()> {
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
                        let ticket = SUPERVISOR_CHILD_REAPER.enqueue(&mut guard.child).await;
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
                        let ticket = SUPERVISOR_CHILD_REAPER.enqueue(&mut guard.child).await;
                        guard.managed.pending_reap = Some(ticket);
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn process_command_ownership_matches(
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

fn managed_command_line(program: &Path, subcommand: &str, operand: &str) -> String {
    format!("{} service {subcommand} {operand}", program.display())
}

/// Kill a stray worker left behind by a previous supervisor or a manual
/// process.  The native start token and exact executable/argv are revalidated
/// immediately before escalation; a recycled PID is never force-killed.
fn terminate_stray_pid(
    pid: u32,
    grace: Duration,
    expected_executable: &Path,
    expected_command: &str,
) -> bool {
    terminate_stray_pid_with_identity(pid, grace, expected_executable, None, expected_command)
}

fn terminate_stray_pid_with_identity(
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
    Ok(stray_workspace_watcher(&canonical).is_some())
}

fn stray_workspace_watcher(canonical: &str) -> Option<u32> {
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
fn stray_runner_worker(status: &SupervisorStatus, canonical: &str) -> Option<u32> {
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

#[cfg(unix)]
fn process_command_line(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    let mut bytes = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `bytes` is writable for the supplied capacity and proc_pidpath
    // writes at most that capacity without retaining the pointer.
    let length = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
        )
    };
    if length <= 0 {
        return None;
    }
    bytes.truncate(length as usize);
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(target_os = "windows")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    process_tree::executable_path(pid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

/// Non-Unix platforms get no identity probe; conservatively never report a
/// stray (the sync lock still serializes watchers, so this is safe).
#[cfg(all(not(unix), not(target_os = "windows")))]
fn process_command_line(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn process_command_line(_pid: u32) -> Option<String> {
    // Windows legacy cleanup deliberately relies on the exact creation token
    // plus kernel executable image. Command-line retrieval has no ownership
    // value here and is therefore not used as a speculative PID signal.
    None
}

#[cfg(all(not(unix), not(target_os = "windows")))]
fn process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn parse_process_elapsed(value: &str) -> Option<u64> {
    let value = value.trim();
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<u64>().ok()?, clock)
    } else {
        (0, value)
    };
    let parts = clock
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] => hours
            .checked_mul(3600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    days.checked_mul(86_400)?.checked_add(seconds)
}

#[cfg(unix)]
fn process_start_epoch(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "etime="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let elapsed = parse_process_elapsed(String::from_utf8(output.stdout).ok()?.trim())?;
    Some(now_epoch().saturating_sub(elapsed))
}

#[cfg(not(unix))]
fn process_start_epoch(_pid: u32) -> Option<u64> {
    None
}

fn runner_process_start_matches(metadata: &feanorfs_agent_core::RunnerProcessMetadata) -> bool {
    process_tree::process_start_matches(metadata.pid, &metadata.process_start_id)
}

#[cfg(unix)]
fn capture_owned_identity(pid: u32) -> Option<process_tree::ProcessIdentity> {
    #[cfg(test)]
    if TEST_IDENTITY_UNAVAILABLE.load(AtomicOrdering::Acquire) {
        return None;
    }
    process_tree::ProcessIdentity::capture(pid)
}

#[cfg(unix)]
#[cfg(test)]
fn runner_process_group_exists(pid: u32) -> bool {
    process_tree::ProcessGroup::for_child(pid).exists()
}

#[cfg(unix)]
fn terminate_verified_runner_group(metadata: &feanorfs_agent_core::RunnerProcessMetadata) -> bool {
    let group = process_tree::ProcessGroup::for_child_with_identity(
        metadata.pid,
        &metadata.process_start_id,
    );
    if !group.exists() {
        return true;
    }
    // The configured harness is placed in a new process group whose id is its
    // pid. Verify both the persisted process start and the live group leader
    // before signaling the group.
    if !runner_process_start_matches(metadata) {
        return false;
    }
    if !group.request_termination() {
        return false;
    }
    let deadline = Instant::now() + STOP_GRACE;
    while group.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if group.exists() {
        if runner_process_start_matches(metadata) {
            let _ = group.force_termination();
            let deadline = Instant::now() + CHILD_REAP_GRACE;
            while group.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        } else {
            tracing::warn!(
                "leaving configured runner group alive because its exact identity changed before KILL"
            );
            return false;
        }
    }
    !group.exists()
}

#[cfg(target_os = "windows")]
fn terminate_verified_runner_group(metadata: &feanorfs_agent_core::RunnerProcessMetadata) -> bool {
    if !runner_process_start_matches(metadata) {
        return false;
    }
    let Some(program) = std::env::current_exe().ok() else {
        return false;
    };
    if !process_tree::executable_matches(metadata.pid, &program) {
        return false;
    }
    let Some(identity) = process_tree::ProcessIdentity::capture(metadata.pid) else {
        return false;
    };
    if !identity.request_termination() {
        return false;
    }
    let deadline = Instant::now() + STOP_GRACE;
    while feanorfs_agent_core::lock::pid_alive(metadata.pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if feanorfs_agent_core::lock::pid_alive(metadata.pid) {
        if identity.is_current() && process_tree::executable_matches(metadata.pid, &program) {
            let _ = identity.force_termination();
        } else {
            return false;
        }
    }
    let deadline = Instant::now() + CHILD_REAP_GRACE;
    while feanorfs_agent_core::lock::pid_alive(metadata.pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !feanorfs_agent_core::lock::pid_alive(metadata.pid)
}

#[cfg(all(not(unix), not(target_os = "windows")))]
fn terminate_verified_runner_group(_metadata: &feanorfs_agent_core::RunnerProcessMetadata) -> bool {
    false
}

fn cleanup_residual_runner_group(workspace: &Path) -> anyhow::Result<()> {
    let Some(metadata) = feanorfs_agent_core::runner_process_metadata(workspace)? else {
        return Ok(());
    };
    if !terminate_verified_runner_group(&metadata) {
        anyhow::bail!(
            "configured runner process group could not be cleaned up because its exact persisted start identity could not be verified"
        );
    }
    Ok(())
}

fn finish_runner_workspace_exit(workspace: &Path) -> anyhow::Result<()> {
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
        finish_runner_workspace_exit(Path::new(workspace))?;
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
fn watcher_command_matches(command: &str, program: &Path, canonical: &str) -> bool {
    exact_command_matches(command, program, &["service", "run", canonical])
}

/// Exact managed-worker command-line check for orphan reaping. The caller
/// supplies the subcommand and operand recorded for that specific child; a
/// reused PID running another workspace, helper executable, or extra argv is
/// never accepted.
#[cfg(test)]
fn managed_orphan_command_matches(
    command: &str,
    program: &Path,
    subcommand: &str,
    operand: &str,
) -> bool {
    exact_command_matches(command, program, &["service", subcommand, operand])
}

fn tray_orphan_command_matches(command: &str, tray_program: &Path) -> bool {
    exact_command_matches(command, tray_program, &[])
}

fn runner_spawn_is_still_admitted(spec: &ChildSpec) -> bool {
    let ChildKind::Runner(workspace) = &spec.kind else {
        return true;
    };
    match feanorfs_agent_core::runner_status(Path::new(workspace)) {
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

async fn reconcile(
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
            managed.backoff_until = Some(Instant::now() + backoff_duration(managed.restarts));
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
            if let Some(stray) = stray_workspace_watcher(workspace) {
                tracing::warn!("stopping stray watcher {stray} before supervising {workspace}");
                if let Ok(program) = std::env::current_exe() {
                    let command = managed_command_line(&program, "run", workspace);
                    terminate_stray_pid(stray, Duration::from_secs(1), &program, &command);
                }
            }
        }
        match spawn_child(spec).await {
            Ok(spawned) => {
                let child_pid = spawned.child.id();
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
                managed.backoff_until = Some(Instant::now() + backoff_duration(managed.restarts));
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

fn now_epoch() -> u64 {
    epoch_at(Instant::now())
}

fn epoch_at(instant: Instant) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.saturating_sub(instant.elapsed()).as_secs())
        .unwrap_or(0)
}

/// Atomically claims the single-supervisor instance lock. The lock file is
/// never removed, so ownership transfers safely on crash; `fs2` releases it
/// automatically when the owning process exits.
fn acquire_supervisor_lock_at(path: &Path) -> anyhow::Result<Option<SupervisorGuard>> {
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

struct SupervisorGuard {
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

fn supervisor_lock_path() -> anyhow::Result<PathBuf> {
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
#[allow(dead_code)]
enum OrphanIdentity {
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

struct PendingOrphanCleanup {
    key: String,
    spec: ChildSpec,
    pid: Option<u32>,
    expected_since: u64,
    recorded_state: ChildState,
    process_start_id: Option<String>,
    /// Executable identity persisted by the previous supervisor.  This is
    /// deliberately independent from the new supervisor's `current_exe`:
    /// an in-place upgrade may leave the old mapped image at a deleted path.
    executable_identity: Option<String>,
    /// A Windows worker recorded as Job-owned was under the previous
    /// supervisor's kernel ownership boundary. A replacement supervisor must
    /// never turn that stale record into a speculative PID signal: the old
    /// Job handle closing is the only supported cleanup operation.
    job_owned: bool,
    expected_executable: PathBuf,
    expected_command: String,
    grace: Duration,
    ticket: ReapTicket,
}

#[cfg(test)]
fn pending_orphan_cleanup(
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

fn pending_orphan_cleanup_with_state(
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
                    ChildKind::Workspace(operand.clone()),
                ),
                "runner-run" => (
                    runner_child_key(operand),
                    ChildKind::Runner(operand.clone()),
                ),
                _ => (
                    format!("orphan:{subcommand}:{operand}"),
                    ChildKind::Workspace(operand.clone()),
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
            Vec::new(),
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
                tray.display().to_string()
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

fn retry_one_pending_orphan_cleanup(cleanup: &mut PendingOrphanCleanup) {
    if cleanup.ticket.is_complete() {
        return;
    }
    // A previous Windows supervisor's Job handle is not serializable and must
    // never be replaced with speculative PID signalling. Reaching this path
    // means the replacement supervisor already owns the singleton instance
    // lock, so the previous process has exited and closed its non-inheritable
    // KILL_ON_JOB_CLOSE handle. The kernel boundary, rather than a PID scan,
    // is therefore the proof that every process in that Job was terminated.
    #[cfg(target_os = "windows")]
    if cleanup.job_owned {
        cleanup.ticket.complete();
        return;
    }
    // A `job_owned` record is a Windows-only ownership proof. If such a status
    // is moved to another platform, keep it unresolved instead of assigning
    // Windows Job semantics where they cannot be verified.
    #[cfg(not(target_os = "windows"))]
    if cleanup.job_owned {
        return;
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

#[allow(dead_code)]
fn retry_pending_orphan_cleanups(pending: &mut BTreeMap<String, PendingOrphanCleanup>) {
    for cleanup in pending.values_mut() {
        retry_one_pending_orphan_cleanup(cleanup);
    }
}

fn orphan_cleanup_is_runner(cleanup: &PendingOrphanCleanup) -> bool {
    matches!(cleanup.spec.kind, ChildKind::Runner(_))
}

#[cfg(test)]
#[allow(dead_code)]
fn make_orphan_cleanup(
    program: &Path,
    pid: Option<u32>,
    expected_since: u64,
    identity: &OrphanIdentity,
) -> PendingOrphanCleanup {
    make_orphan_cleanup_with_state(program, pid, expected_since, ChildState::Stopping, identity)
}

#[allow(dead_code)]
fn make_orphan_cleanup_with_state(
    program: &Path,
    pid: Option<u32>,
    expected_since: u64,
    recorded_state: ChildState,
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
                tray.display().to_string()
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
    pending_orphan_cleanup_with_state(
        program,
        pid,
        expected_since,
        recorded_state,
        identity,
        expected_command,
        grace,
    )
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
    if let (Some(child), Ok(data_dir)) = (status.hub, super::hub_service::default_data_dir()) {
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
    if let (Some(child), Some(tray)) = (status.tray, super::service::find_tray_program(&program)) {
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
    SUPERVISOR_CHILD_REAPER
        .ensure_ready()
        .context("start supervisor child reaper")?;
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
    let started_at = now_epoch();
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
                .and_then(super::service::find_tray_program)
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

async fn terminate_all_children(
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

// ---------------------------------------------------------------------------
// Legacy migration
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct LegacyJob {
    label: String,
    unit_file: Option<PathBuf>,
    marker: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
fn legacy_jobs() -> anyhow::Result<Vec<LegacyJob>> {
    let mut jobs = Vec::new();
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let agents = PathBuf::from(home).join("Library/LaunchAgents");
    let entries = match fs::read_dir(&agents) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(jobs),
        Err(error) => return Err(error).context("list legacy launch agents"),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let sync = name.starts_with("com.feanorfs.sync-");
        if !(sync || name == "com.feanorfs.hub.plist" || name == "com.feanorfs.tray.plist") {
            continue;
        }
        let label = name.strip_suffix(".plist").unwrap_or(&name).to_string();
        let mut job = LegacyJob {
            label,
            unit_file: Some(entry.path()),
            marker: None,
        };
        let root = feanorfs_agent_core::global_state_root().ok();
        if sync {
            // Legacy per-workspace jobs run `feanorfs service run <workspace>`,
            // so the workspace is ProgramArguments index 3 (0 = binary).
            if let Some(workspace) = plist_program_argument(&entry.path(), 3) {
                if let Ok(state) =
                    feanorfs_agent_core::ensure_workspace_state(Path::new(&workspace))
                {
                    job.marker = Some(state.join("service-program"));
                }
            }
        } else if name == "com.feanorfs.hub.plist" {
            job.marker = root.map(|root| root.join("hub-data/service-program"));
        } else {
            job.marker = root.map(|root| root.join("tray-service-program"));
        }
        jobs.push(job);
    }
    Ok(jobs)
}

#[cfg(target_os = "macos")]
fn plist_program_argument(plist: &Path, index: usize) -> Option<String> {
    let output = std::process::Command::new("/usr/bin/plutil")
        .args([
            "-extract",
            &format!("ProgramArguments.{index}"),
            "raw",
            "-o",
            "-",
        ])
        .arg(plist)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn legacy_jobs() -> anyhow::Result<Vec<LegacyJob>> {
    let mut jobs = Vec::new();
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let units = PathBuf::from(home).join(".config/systemd/user");
    let entries = match fs::read_dir(&units) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(jobs),
        Err(error) => return Err(error).context("list legacy systemd user units"),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let sync = name.starts_with("com.feanorfs.sync-");
        if !(sync || name == "com.feanorfs.hub.service" || name == "com.feanorfs.tray.service") {
            continue;
        }
        let label = name.strip_suffix(".service").unwrap_or(&name).to_string();
        let mut job = LegacyJob {
            label,
            unit_file: Some(entry.path()),
            marker: None,
        };
        let root = feanorfs_agent_core::global_state_root().ok();
        if sync {
            if let Some(workspace) = unit_execstart_workspace(&entry.path()) {
                if let Ok(state) =
                    feanorfs_agent_core::ensure_workspace_state(Path::new(&workspace))
                {
                    job.marker = Some(state.join("service-program"));
                }
            }
        } else if name == "com.feanorfs.hub.service" {
            job.marker = root.map(|root| root.join("hub-data/service-program"));
        } else {
            job.marker = root.map(|root| root.join("tray-service-program"));
        }
        jobs.push(job);
    }
    Ok(jobs)
}

#[cfg(target_os = "linux")]
fn unit_execstart_workspace(unit: &Path) -> Option<String> {
    let content = fs::read_to_string(unit).ok()?;
    let line = content
        .lines()
        .find(|line| line.starts_with("ExecStart="))?;
    let mut parts = line["ExecStart=".len()..].split_whitespace();
    let mut workspace = None;
    while let Some(part) = parts.next() {
        if part == "run" {
            workspace = parts
                .next()
                .map(|value| value.trim_matches('"').to_string());
        }
    }
    workspace
}

#[cfg(target_os = "windows")]
fn legacy_jobs() -> anyhow::Result<Vec<LegacyJob>> {
    let mut jobs = Vec::new();
    let output = schtasks(&["/Query", "/FO", "CSV", "/TN", "FeanorFS\\"])?;
    if !output.status.success() {
        return Ok(jobs);
    }
    let root = feanorfs_agent_core::global_state_root().ok();
    for line in String::from_utf8_lossy(&output.stdout).lines().skip(1) {
        let task = line.split(',').next().unwrap_or_default().trim_matches('"');
        let Some(name) = task.strip_prefix("FeanorFS\\") else {
            continue;
        };
        if name == "Agent" || name == "Tray" {
            continue;
        }
        let mut job = LegacyJob {
            label: name.to_string(),
            unit_file: None,
            marker: None,
        };
        if name.starts_with("com.feanorfs.sync-") {
            if let Some(workspace) = windows_task_workspace(name) {
                if let Ok(state) =
                    feanorfs_agent_core::ensure_workspace_state(Path::new(&workspace))
                {
                    job.marker = Some(state.join("service-program"));
                }
            }
        } else if name == "com.feanorfs.hub" {
            job.marker = root
                .as_ref()
                .map(|root| root.join("hub-data/service-program"));
        } else {
            job.marker = root.as_ref().map(|root| root.join("tray-service-program"));
        }
        jobs.push(job);
    }
    Ok(jobs)
}

#[cfg(target_os = "windows")]
fn windows_task_workspace(name: &str) -> Option<String> {
    let output = schtasks(&[
        "/Query",
        "/V",
        "/FO",
        "LIST",
        "/TN",
        &format!("FeanorFS\\{name}"),
    ])
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with("Task To Run:"))?;
    let value = line["Task To Run:".len()..].trim();
    let mut parts = value.split('"');
    let mut workspace = None;
    while let Some(part) = parts.next() {
        if part.trim_end() == "service run" {
            workspace = parts.next().map(|path| path.to_string());
        }
    }
    workspace
}

/// Uninstall every legacy per-component background job (per-workspace sync
/// jobs, the old hub job, and the old tray job) so the single supervisor job
/// is the only background item. Returns how many jobs were removed.
pub(crate) fn migrate_legacy_jobs() -> anyhow::Result<usize> {
    let jobs = legacy_jobs()?;
    for job in &jobs {
        let _ = uninstall_label(&job.label);
        if let Some(unit_file) = &job.unit_file {
            let _ = fs::remove_file(unit_file);
        }
        if let Some(marker) = &job.marker {
            let _ = fs::remove_file(marker);
        }
    }
    // Sweep every remaining per-workspace identity marker: they are artifacts
    // of the old per-workspace jobs only, and a marker that cannot be mapped
    // back to a legacy plist (older installs, interrupted migrations) would
    // otherwise keep `doctor` reporting a stale executable forever. The new
    // model records exactly one identity: `supervisor-service-program`.
    if let Ok(root) = feanorfs_agent_core::global_state_root() {
        if let Ok(entries) = fs::read_dir(root.join("workspaces")) {
            for entry in entries.flatten() {
                let marker = entry.path().join("service-program");
                let _ = fs::remove_file(marker);
            }
        }
        let _ = fs::remove_file(root.join("hub-data/service-program"));
        let _ = fs::remove_file(root.join("tray-service-program"));
    }
    Ok(jobs.len())
}

#[cfg(not(target_os = "windows"))]
fn uninstall_label(label: &str) -> anyhow::Result<()> {
    use service_manager::{ServiceStatus, ServiceStatusCtx, ServiceUninstallCtx};
    let manager = manager()?;
    let label: service_manager::ServiceLabel = label.parse()?;
    if manager
        .status(ServiceStatusCtx {
            label: label.clone(),
        })
        .context("read legacy service status")?
        != ServiceStatus::NotInstalled
    {
        manager
            .uninstall(ServiceUninstallCtx { label })
            .context("uninstall legacy service")?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_label(label: &str) -> anyhow::Result<()> {
    let output = schtasks(&["/Delete", "/TN", &format!("FeanorFS\\{label}"), "/F"])?;
    if !output.status.success() {
        anyhow::bail!(
            "uninstall legacy task: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static ACK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static RUNNER_FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[cfg(unix)]
    static REAPER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(unix)]
    struct ReaperTestReset;

    #[cfg(unix)]
    impl Drop for ReaperTestReset {
        fn drop(&mut self) {
            TEST_FORCE_REAP_TIMEOUT.store(false, AtomicOrdering::Release);
            TEST_TERMINATION_GRACE_MILLIS.store(0, AtomicOrdering::Release);
            SUPERVISOR_CHILD_REAPER
                .fail_next_enqueue
                .store(false, AtomicOrdering::Release);
            SUPERVISOR_CHILD_REAPER.fail_worker_start_for_test(false);
            TEST_SHUTDOWN_PANIC_ONCE.store(false, AtomicOrdering::Release);
        }
    }

    fn id(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn configured_runner_fixture() -> (
        crate::cli::RunnerTestWorkspace,
        PathBuf,
        feanorfs_agent_core::RunnerStore,
    ) {
        let dir = crate::cli::RunnerTestWorkspace::new();
        let workspace = dir.path().canonicalize().unwrap();
        let fixture_sequence = RUNNER_FIXTURE_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        feanorfs_client::save_config(
            &workspace,
            &feanorfs_client::Config {
                server_url: "http://127.0.0.1:1".to_string(),
                workspace_id: format!("supervisor-runner-test-{fixture_sequence}"),
                encryption_password: Some("e".repeat(64)),
                server_password: None,
                tls_ca_pem: None,
                format_version: 3,
                hub_local: false,
                relay: None,
            },
        )
        .unwrap();
        let worktree = feanorfs_agent_core::agent_dir(&workspace, "worker").unwrap();
        let root = worktree.parent().unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("state/base-snapshot"), id('a')).unwrap();
        let program = std::env::current_exe().unwrap().canonicalize().unwrap();
        let store = feanorfs_agent_core::RunnerStore::configure(
            &workspace,
            "worker",
            &program,
            vec!["--fixed".to_string()],
            60,
            &id('a'),
        )
        .unwrap();
        (dir, workspace, store)
    }

    #[cfg(target_os = "windows")]
    fn release_test_suspended_child(children: &mut BTreeMap<String, ManagedChild>, key: &str) {
        let managed = children
            .get_mut(key)
            .expect("test reconcile spawned the managed child");
        let tree = managed
            .process_tree
            .as_ref()
            .expect("test child has a private Windows Job Object");
        let child = managed
            .child
            .as_ref()
            .expect("test child retains its process handle");
        tree.release_child(child)
            .expect("resume adopted suspended test child");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_workspace_is_rejected_before_registry_mutation() {
        use std::os::unix::ffi::OsStringExt as _;

        let _guard = ACK_TEST_LOCK.lock().unwrap();
        let registry_file = registry_path().unwrap();
        let before = fs::read(&registry_file).ok();
        let first = PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0x80]));
        let second = PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0x81]));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());

        for workspace in [&first, &second] {
            let error = workspace_registry_key(workspace).unwrap_err();
            assert!(error
                .to_string()
                .contains("canonical workspace path must be valid UTF-8"));
        }
        assert_eq!(fs::read(&registry_file).ok(), before);
    }

    #[test]
    fn runner_stop_ack_rejects_missing_stale_and_failed_publication() {
        let _guard = ACK_TEST_LOCK.lock().unwrap();
        let registry_file = registry_path().unwrap();
        let ack_file = runner_ack_path().unwrap();
        let status_file = status_path().unwrap();
        let lock_file = supervisor_lock_path().unwrap();
        let owner_file = supervisor_lock_owner_path_at(&lock_file);
        let original_registry = fs::read(&registry_file).ok();
        let original_ack = fs::read(&ack_file).ok();
        let original_status = fs::read(&status_file).ok();
        let original_lock = fs::read(&lock_file).ok();
        let original_owner = fs::read(&owner_file).ok();
        let restore = |path: &Path, original: &Option<Vec<u8>>| match original {
            Some(content) => fs::write(path, content).unwrap(),
            None => {
                let _ = fs::remove_file(path);
            }
        };

        let supervisor_guard = acquire_supervisor_lock_at(&supervisor_lock_path().unwrap())
            .unwrap()
            .expect("ack test owns supervisor lock");
        let current_owner = fs::read(&owner_file).expect("read current supervisor owner");
        let token_a = "token-a".to_string();
        let registry = SupervisorRegistry {
            mutation_generation: 1,
            runner_stop_tokens: BTreeMap::from([(
                "/ack-test".to_string(),
                RunnerStopTombstone {
                    token: token_a.clone(),
                    generation: 1,
                },
            )]),
            ..SupervisorRegistry::default()
        };
        create_store_dir(&registry_file).unwrap();
        save_registry(&registry_file, &registry).unwrap();
        let _ = fs::remove_file(&ack_file);
        fs::write(&status_file, b"not-json").unwrap();

        assert!(supervisor_instance_lock_held().unwrap());
        fs::write(&owner_file, b"not-json").unwrap();
        assert_eq!(supervisor_lock_owner_pid().unwrap(), None);
        fs::write(&owner_file, &current_owner).unwrap();
        assert_eq!(
            supervisor_lock_owner_pid().unwrap(),
            Some(std::process::id())
        );
        assert!(
            !runner_stop_acknowledged("/ack-test", None, Some(std::process::id()), None,).unwrap()
        );

        TEST_ACK_PUBLISH_FAILURE.store(true, AtomicOrdering::Release);
        let children = BTreeMap::new();
        assert!(publish_runner_reconcile_ack(&children, &registry, now_epoch(), 1).is_err());
        TEST_ACK_PUBLISH_FAILURE.store(false, AtomicOrdering::Release);
        assert!(!runner_stop_acknowledged(
            "/ack-test",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());

        publish_runner_reconcile_ack(&children, &registry, now_epoch(), 2).unwrap();
        assert!(runner_stop_acknowledged(
            "/ack-test",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());
        // Unrelated registry mutations do not invalidate runner A's durable
        // stop token. The acknowledgement remains tied to this runner's
        // tombstone rather than to a global generation equality.
        let mut unrelated_registry = registry.clone();
        unrelated_registry.mutation_generation = 2;
        unrelated_registry.workspaces.push("/other".to_string());
        save_registry(&registry_file, &unrelated_registry).unwrap();
        assert!(runner_stop_acknowledged(
            "/ack-test",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());

        // Re-adding runner A clears its tombstone, so the old token can no
        // longer acknowledge a later stop operation.
        let mut readded_registry = unrelated_registry.clone();
        readded_registry.runners.push("/ack-test".to_string());
        readded_registry.runner_stop_tokens.remove("/ack-test");
        readded_registry.mutation_generation = 3;
        save_registry(&registry_file, &readded_registry).unwrap();
        assert!(!runner_stop_acknowledged(
            "/ack-test",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());

        // A second removal receives a fresh token. The first token is rejected
        // even when the runner list has returned to the same content (ABA).
        let token_b = "token-b".to_string();
        let second_removal = SupervisorRegistry {
            mutation_generation: 4,
            runner_stop_tokens: BTreeMap::from([(
                "/ack-test".to_string(),
                RunnerStopTombstone {
                    token: token_b.clone(),
                    generation: 4,
                },
            )]),
            workspaces: readded_registry.workspaces.clone(),
            ..SupervisorRegistry::default()
        };
        save_registry(&registry_file, &second_removal).unwrap();
        publish_runner_reconcile_ack(&children, &second_removal, now_epoch(), 3).unwrap();
        assert!(!runner_stop_acknowledged(
            "/ack-test",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());
        assert!(runner_stop_acknowledged(
            "/ack-test",
            Some(&token_b),
            Some(std::process::id()),
            None,
        )
        .unwrap());
        let mut stale = read_runner_reconcile_ack().unwrap().unwrap();
        stale.process_start_id = Some(format!("spawn:{}:stale", stale.pid));
        fs::write(&ack_file, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(!runner_stop_acknowledged(
            "/ack-test",
            Some(&token_b),
            Some(std::process::id()),
            None,
        )
        .unwrap());

        drop(supervisor_guard);
        restore(&registry_file, &original_registry);
        restore(&ack_file, &original_ack);
        restore(&status_file, &original_status);
        restore(&lock_file, &original_lock);
        restore(&owner_file, &original_owner);
    }

    #[test]
    fn runner_ack_store_is_independent_per_removed_runner() {
        let _guard = ACK_TEST_LOCK.lock().unwrap();
        let registry_file = registry_path().unwrap();
        let ack_file = runner_ack_path().unwrap();
        let original_registry = fs::read(&registry_file).ok();
        let original_ack = fs::read(&ack_file).ok();
        let restore = |path: &Path, original: &Option<Vec<u8>>| match original {
            Some(content) => fs::write(path, content).unwrap(),
            None => {
                let _ = fs::remove_file(path);
            }
        };

        let token_a = "store-token-a".to_string();
        let token_b = "store-token-b".to_string();
        let registry = SupervisorRegistry {
            mutation_generation: 10,
            runner_stop_tokens: BTreeMap::from([
                (
                    "/store-a".to_string(),
                    RunnerStopTombstone {
                        token: token_a.clone(),
                        generation: 8,
                    },
                ),
                (
                    "/store-b".to_string(),
                    RunnerStopTombstone {
                        token: token_b.clone(),
                        generation: 9,
                    },
                ),
            ]),
            ..SupervisorRegistry::default()
        };
        create_store_dir(&registry_file).unwrap();
        save_registry(&registry_file, &registry).unwrap();
        let child_b = ChildSpec {
            kind: ChildKind::Runner("/store-b".to_string()),
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let children = BTreeMap::from([(runner_child_key("/store-b"), ManagedChild::new(child_b))]);
        let _ = fs::remove_file(&ack_file);
        publish_runner_reconcile_ack(&children, &registry, now_epoch(), 1).unwrap();
        let store = read_runner_reconcile_ack_store().unwrap().unwrap();
        assert!(store.acks.contains_key("/store-a"));
        assert!(!store.acks.contains_key("/store-b"));
        assert!(runner_stop_acknowledged(
            "/store-a",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());
        assert!(!runner_stop_acknowledged(
            "/store-b",
            Some(&token_b),
            Some(std::process::id()),
            None,
        )
        .unwrap());

        // B can complete later without changing A's record or token.
        publish_runner_reconcile_ack(&BTreeMap::new(), &registry, now_epoch(), 2).unwrap();
        assert!(runner_stop_acknowledged(
            "/store-a",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());
        assert!(runner_stop_acknowledged(
            "/store-b",
            Some(&token_b),
            Some(std::process::id()),
            None,
        )
        .unwrap());

        let mut unrelated = registry.clone();
        unrelated.mutation_generation = 11;
        unrelated.workspaces.push("/unrelated".to_string());
        save_registry(&registry_file, &unrelated).unwrap();
        assert!(runner_stop_acknowledged(
            "/store-a",
            Some(&token_a),
            Some(std::process::id()),
            None,
        )
        .unwrap());

        restore(&registry_file, &original_registry);
        restore(&ack_file, &original_ack);
    }

    #[test]
    fn registry_roundtrips_workspaces_stopped_and_runners() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");
        let mut store = SupervisorRegistry::default();
        store.workspaces.push("/a".into());
        store.stopped.push("/b".into());
        store.runners.push("/c".into());
        save_registry(&path, &store).unwrap();
        let loaded = load_registry(&path).unwrap();
        assert_eq!(loaded.workspaces, vec!["/a"]);
        assert_eq!(loaded.stopped, vec!["/b"]);
        assert_eq!(loaded.runners, vec!["/c"]);
    }

    #[test]
    fn runner_ack_allows_removed_runner_a_while_runner_b_remains_active() {
        let registry = SupervisorRegistry {
            runners: vec!["/runner-b".to_string()],
            mutation_generation: 7,
            ..SupervisorRegistry::default()
        };
        let runner_b = ChildSpec {
            kind: ChildKind::Runner("/runner-b".to_string()),
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let runner_a = ChildSpec {
            kind: ChildKind::Runner("/runner-a".to_string()),
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let mut children = BTreeMap::new();
        children.insert(runner_child_key("/runner-b"), ManagedChild::new(runner_b));
        assert!(runner_reconciliation_complete(&children, &registry));

        children.insert(runner_child_key("/runner-a"), ManagedChild::new(runner_a));
        assert!(
            !runner_reconciliation_complete(&children, &registry),
            "a stale removed runner entry must still gate the acknowledgement"
        );
        children.remove(&runner_child_key("/runner-a"));
        assert!(runner_reconciliation_complete(&children, &registry));
    }

    #[test]
    fn null_pid_orphan_cleanup_is_fail_closed_for_non_stopped_states() {
        let program = std::env::current_exe().unwrap();
        let identity = OrphanIdentity::Worker {
            subcommand: "runner-run",
            operand: "/null-pid-runner".to_string(),
            process_start_id: None,
            job_owned: false,
        };
        for state in [
            ChildState::Running,
            ChildState::Backoff,
            ChildState::Stopping,
        ] {
            let mut cleanup = pending_orphan_cleanup_with_state(
                &program,
                None,
                0,
                state,
                &identity,
                managed_command_line(&program, "runner-run", "/null-pid-runner"),
                STOP_GRACE,
            );
            retry_one_pending_orphan_cleanup(&mut cleanup);
            assert!(
                !cleanup.ticket.is_complete(),
                "pid:null with {state:?} must remain unresolved"
            );
        }
        let mut stopped = pending_orphan_cleanup_with_state(
            &program,
            None,
            0,
            ChildState::Stopped,
            &identity,
            managed_command_line(&program, "runner-run", "/null-pid-runner"),
            STOP_GRACE,
        );
        retry_one_pending_orphan_cleanup(&mut stopped);
        assert!(stopped.ticket.is_complete());

        let job_owned_identity = OrphanIdentity::Worker {
            subcommand: "runner-run",
            operand: "/job-owned-null".to_string(),
            process_start_id: None,
            job_owned: true,
        };
        let mut job_owned = pending_orphan_cleanup_with_state(
            &program,
            None,
            0,
            ChildState::Stopped,
            &job_owned_identity,
            managed_command_line(&program, "runner-run", "/job-owned-null"),
            STOP_GRACE,
        );
        retry_one_pending_orphan_cleanup(&mut job_owned);
        #[cfg(target_os = "windows")]
        assert!(
            job_owned.ticket.is_complete(),
            "the replacement supervisor's instance lock proves the old kill-on-close Job handle closed"
        );
        #[cfg(not(target_os = "windows"))]
        assert!(
            !job_owned.ticket.is_complete(),
            "Windows Job ownership cannot be inferred on another platform"
        );
    }

    #[test]
    fn runner_stop_tombstone_capacity_rejects_unacknowledged_entries() {
        let mut registry = SupervisorRegistry {
            mutation_generation: 1,
            ..SupervisorRegistry::default()
        };
        for index in 0..MAX_RUNNER_STOP_TOMBSTONES {
            let workspace = format!("/capacity-{index}");
            registry.runner_stop_tokens.insert(
                workspace,
                RunnerStopTombstone {
                    token: format!("token-{index}"),
                    generation: index as u64 + 1,
                },
            );
        }
        let before = registry.runner_stop_tokens.clone();
        let result = prune_runner_stop_tokens(&mut registry, &RunnerReconcileAckStore::default());
        assert!(result.is_err());
        assert_eq!(registry.runner_stop_tokens, before);
    }

    #[test]
    fn runner_stop_tombstone_capacity_reclaims_only_matching_ack() {
        let mut registry = SupervisorRegistry {
            mutation_generation: 1,
            ..SupervisorRegistry::default()
        };
        for index in 0..MAX_RUNNER_STOP_TOMBSTONES {
            let workspace = format!("/capacity-{index}");
            registry.runner_stop_tokens.insert(
                workspace,
                RunnerStopTombstone {
                    token: format!("token-{index}"),
                    generation: index as u64 + 1,
                },
            );
        }
        let reclaim = "/capacity-0".to_string();
        let mut ack_store = RunnerReconcileAckStore::default();
        ack_store.acks.insert(
            reclaim.clone(),
            RunnerReconcileAck {
                workspace: reclaim.clone(),
                pid: std::process::id(),
                process_start_id: None,
                started_at: 1,
                registry_digest: String::new(),
                registry_generation: 1,
                stop_token: "token-0".to_string(),
                generation: 1,
            },
        );
        prune_runner_stop_tokens(&mut registry, &ack_store).unwrap();
        assert_eq!(
            registry.runner_stop_tokens.len(),
            MAX_RUNNER_STOP_TOMBSTONES - 1
        );
        assert!(!registry.runner_stop_tokens.contains_key(&reclaim));
        assert!(registry.runner_stop_tokens.contains_key("/capacity-1"));
    }

    #[test]
    fn absent_registry_seeding_preserves_existing_and_stopped_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");
        seed_registry_file_if_absent(&path, vec!["/recent".to_string()]).unwrap();
        assert_eq!(load_registry(&path).unwrap().workspaces, vec!["/recent"]);

        seed_registry_file_if_absent(&path, vec!["/must-not-replace".to_string()]).unwrap();
        assert_eq!(load_registry(&path).unwrap().workspaces, vec!["/recent"]);

        let stopped_path = dir.path().join("stopped-supervisor.json");
        let mut stopped = SupervisorRegistry::default();
        stopped.stopped.push("/stopped".to_string());
        save_registry(&stopped_path, &stopped).unwrap();
        seed_registry_file_if_absent(&stopped_path, vec!["/stopped".to_string()]).unwrap();
        let loaded = load_registry(&stopped_path).unwrap();
        assert!(loaded.workspaces.is_empty());
        assert_eq!(loaded.stopped, vec!["/stopped"]);
    }

    #[test]
    fn legacy_registry_without_runners_remains_compatible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");
        std::fs::write(&path, r#"{"workspaces":["/a"],"stopped":["/b"]}"#).unwrap();
        let loaded = load_registry(&path).unwrap();
        assert_eq!(loaded.workspaces, vec!["/a"]);
        assert_eq!(loaded.stopped, vec!["/b"]);
        assert!(loaded.runners.is_empty());
    }

    #[test]
    fn missing_registry_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = load_registry(&dir.path().join("missing.json")).unwrap();
        assert!(store.workspaces.is_empty());
        assert!(store.stopped.is_empty());
        assert!(store.runners.is_empty());
    }

    #[test]
    fn read_registry_if_present_does_not_create_absent_store_or_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");
        let lock = dir.path().join("supervisor.lock");

        assert!(read_registry_if_present_at(&path)
            .unwrap()
            .runners
            .is_empty());
        assert!(!path.exists());
        assert!(!lock.exists());

        save_registry(&path, &SupervisorRegistry::default()).unwrap();
        assert!(read_registry_if_present_at(&path)
            .unwrap()
            .runners
            .is_empty());
        assert!(lock.exists());
    }

    #[test]
    fn stray_watcher_matching_requires_exact_binary_invocation() {
        let program = Path::new("/usr/local/bin/feanorfs");
        let canonical = "/Users/raulpuigbo/p/net";
        // The real watcher command line matches.
        assert!(watcher_command_matches(
            "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net",
            program,
            canonical
        ));
        // Innocent processes whose command lines merely MENTION the product
        // name or the workspace path must never match: killing them would be
        // a real bug.
        for innocent in [
            "vim /Users/raulpuigbo/p/net/feanorfs-notes.txt",
            "code /Users/raulpuigbo/p/net/feanorfs/README.md",
            "rg feanorfs /Users/raulpuigbo/p/net",
            "/usr/bin/feanorfs-backup-tool --dir /Users/raulpuigbo/p/net",
            "/usr/local/bin/feanorfs sync /Users/raulpuigbo/p/net",
            "/usr/local/bin/feanorfs service status /Users/raulpuigbo/p/net",
            "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/network",
            "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net --extra",
            "/usr/local/bin/feanorfs-helper service run /Users/raulpuigbo/p/net",
        ] {
            assert!(
                !watcher_command_matches(innocent, program, canonical),
                "innocent command line matched: {innocent}"
            );
        }
        // A different workspace must not match either.
        assert!(!watcher_command_matches(
            "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/logs",
            program,
            canonical
        ));
    }

    #[test]
    fn orphan_reaping_matching_requires_managed_subcommand_or_tray() {
        let program = Path::new("/usr/local/bin/feanorfs");
        assert!(managed_orphan_command_matches(
            "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net",
            program,
            "run",
            "/Users/raulpuigbo/p/net",
        ));
        assert!(managed_orphan_command_matches(
            "/usr/local/bin/feanorfs service hub-run /Users/raulpuigbo/.feanorfs/hub-data",
            program,
            "hub-run",
            "/Users/raulpuigbo/.feanorfs/hub-data",
        ));
        assert!(managed_orphan_command_matches(
            "/usr/local/bin/feanorfs service runner-run /Users/raulpuigbo/p/net",
            program,
            "runner-run",
            "/Users/raulpuigbo/p/net",
        ));
        for innocent in [
            "/usr/local/bin/feanorfs start --foreground /Users/raulpuigbo/p/net",
            "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/network",
            "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net --extra",
            "/usr/local/bin/feanorfs-helper service run /Users/raulpuigbo/p/net",
            "vim /Users/raulpuigbo/p/net/service run notes.txt",
            "python3 /tmp/feanorfs-tray-test.py",
            "/usr/bin/feanorfs-helper --service run",
        ] {
            assert!(
                !managed_orphan_command_matches(
                    innocent,
                    program,
                    "run",
                    "/Users/raulpuigbo/p/net",
                ),
                "innocent command line matched for reaping: {innocent}"
            );
        }
        let tray = Path::new("/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray");
        assert!(tray_orphan_command_matches(
            "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray",
            tray,
        ));
        assert!(!tray_orphan_command_matches("/tmp/feanorfs-tray", tray,));
        assert!(!tray_orphan_command_matches(
            "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray --first-run",
            tray,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn process_elapsed_parser_is_bounded_and_exact() {
        assert_eq!(parse_process_elapsed("00:07"), Some(7));
        assert_eq!(parse_process_elapsed("01:02:03"), Some(3_723));
        assert_eq!(parse_process_elapsed("2-01:02:03"), Some(176_523));
        assert_eq!(parse_process_elapsed("bogus"), None);
        assert_eq!(parse_process_elapsed("1:2:3:4"), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn process_identity_reads_kernel_executable_path() {
        let actual = process_executable(std::process::id()).expect("read current executable");
        let expected = std::env::current_exe().expect("resolve current executable");
        assert_eq!(
            std::fs::canonicalize(actual).unwrap(),
            std::fs::canonicalize(expected).unwrap()
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn verified_residual_runner_group_is_terminated() {
        use std::os::unix::process::CommandExt as _;

        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let process_start_id = process_tree::process_start_identifier(pid, "residual-test");
        let metadata = feanorfs_agent_core::RunnerProcessMetadata {
            pid,
            process_start_id,
        };
        assert!(runner_process_start_matches(&metadata));

        let cleanup = std::thread::spawn(move || terminate_verified_runner_group(&metadata));
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(cleanup.join().unwrap());
        assert!(!runner_process_group_exists(pid));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn mismatched_runner_identity_never_signals_group() {
        use std::os::unix::process::CommandExt as _;

        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let valid = process_tree::process_start_identifier(pid, "mismatch-test");
        let mismatch = if valid.ends_with(":0") {
            format!("{valid}1")
        } else {
            format!("{valid}0")
        };
        let metadata = feanorfs_agent_core::RunnerProcessMetadata {
            pid,
            process_start_id: mismatch,
        };
        assert!(!runner_process_start_matches(&metadata));
        assert!(!terminate_verified_runner_group(&metadata));
        assert!(feanorfs_agent_core::lock::pid_alive(pid));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn force_escalation_requires_identity_and_command_ownership() {
        assert!(force_termination_allowed(true, true));
        assert!(!force_termination_allowed(false, true));
        assert!(!force_termination_allowed(true, false));
        assert!(!force_termination_allowed(false, false));
    }

    #[tokio::test]
    async fn supervisor_reaper_handoff_reaps_owned_child() {
        #[cfg(unix)]
        let _guard = REAPER_TEST_LOCK.lock().await;
        let mut child = spawn_long_running_test_child();
        let pid = child.id().expect("reaper child pid");
        let _ = child.start_kill();
        let mut child = Some(child);
        SUPERVISOR_CHILD_REAPER.enqueue(&mut child).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while feanorfs_agent_core::lock::pid_alive(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("supervisor reaper reaped child");
    }

    fn spawn_long_running_test_child() -> tokio::process::Child {
        #[cfg(unix)]
        let mut command = {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args(["-c", "sleep 30"]);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = tokio::process::Command::new("cmd.exe");
            command.args(["/C", "ping 127.0.0.1 -n 31 >NUL"]);
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("spawn cross-platform long-running child")
    }

    #[cfg(unix)]
    async fn spawn_term_ignoring_child() -> tokio::process::Child {
        use tokio::io::AsyncReadExt as _;

        // Ignored SIGTERM forces terminate_child through its bounded force
        // path. The readiness byte proves the trap is installed before the
        // caller can signal the child, and the builtin loop leaves no helper
        // descendant behind.
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; printf 1; while :; do :; done"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn termination test child");
        let mut ready = [0_u8; 1];
        tokio::time::timeout(
            Duration::from_secs(2),
            child
                .stdout
                .as_mut()
                .expect("termination test child has readiness pipe")
                .read_exact(&mut ready),
        )
        .await
        .expect("termination test child became ready")
        .expect("read termination test child readiness");
        assert_eq!(ready, [b'1']);
        child.stdout.take();
        child
    }

    #[cfg(unix)]
    async fn assert_pid_reaped(pid: u32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while feanorfs_agent_core::lock::pid_alive(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("termination test child was reaped");
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn supervisor_job_descendant_helper() {
        let descendant_path = std::env::var_os("FEANORFS_SUPERVISOR_DESCENDANT")
            .map(PathBuf::from)
            .expect("descendant pid path");
        let executable = std::env::current_exe().expect("test executable");
        let descendant = std::process::Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "cli::supervisor::tests::supervisor_job_descendant_sleep_helper",
                "--nocapture",
            ])
            .spawn()
            .expect("spawn descendant");
        std::fs::write(descendant_path, descendant.id().to_string())
            .expect("record descendant pid");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn supervisor_job_descendant_sleep_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn supervisor_termination_closes_job_owned_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let descendant_path = temp.path().join("descendant.pid");
        let program = std::env::current_exe().unwrap();
        let spec = ChildSpec {
            kind: ChildKind::Hub,
            program: program.clone(),
            args: vec![
                "--ignored".into(),
                "--exact".into(),
                "cli::supervisor::tests::supervisor_job_descendant_helper".into(),
                "--nocapture".into(),
            ],
            env: vec![(
                "FEANORFS_SUPERVISOR_DESCENDANT".into(),
                descendant_path.as_os_str().to_owned(),
            )],
            restart_on_zero_exit: true,
        };
        let key = "component:hub".to_string();
        let mut desired = BTreeMap::new();
        desired.insert(key.clone(), spec);
        let mut children = BTreeMap::new();
        reconcile(&mut children, &desired, false).await.unwrap();
        #[cfg(target_os = "windows")]
        release_test_suspended_child(&mut children, &key);
        // The nested test harness starts another copy of this binary.  On a
        // loaded Windows runner that startup can exceed the supervisor's
        // normal five-second service readiness bound even though the Job
        // adoption itself is correct.  Keep a bounded test-only window wide
        // enough to observe the descendant without weakening the product
        // timeout or the ownership assertions below.
        let descendant = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(value) = fs::read_to_string(&descendant_path) {
                    if let Ok(pid) = value.parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("supervisor descendant became ready");
        let _ = reconcile(&mut children, &BTreeMap::new(), false).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while feanorfs_agent_core::lock::pid_alive(descendant)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!feanorfs_agent_core::lock::pid_alive(descendant));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_removal_keeps_child_owned_when_primary_enqueue_fails() {
        let _guard = REAPER_TEST_LOCK.lock().await;
        let _reset = ReaperTestReset;
        TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
        TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);
        SUPERVISOR_CHILD_REAPER.fail_worker_start_for_test(false);
        SUPERVISOR_CHILD_REAPER.fail_next_enqueue_for_test();

        let child = spawn_term_ignoring_child().await;
        let pid = child.id().expect("termination test child pid");
        assert!(feanorfs_agent_core::lock::pid_alive(pid));
        let spec = ChildSpec {
            kind: ChildKind::Workspace("/reconcile-removal".to_string()),
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let mut managed = ManagedChild::new(spec);
        managed.child = Some(child);
        let mut children = BTreeMap::from([("workspace:test".to_string(), managed)]);

        let result = reconcile(&mut children, &BTreeMap::new(), false).await;
        assert!(
            result.is_ok(),
            "bounded termination is retained for reconciliation"
        );
        assert_eq!(children.len(), 1, "reconcile retains the stopping entry");
        assert_eq!(
            children.values().next().unwrap().state,
            ChildState::Stopping,
            "a deferred reaper handoff remains explicitly stopping"
        );
        assert_pid_reaped(pid).await;

        reconcile(&mut children, &BTreeMap::new(), false)
            .await
            .expect("reaper completion reconciles on the next pass");
        assert!(
            children.is_empty(),
            "completed child is removed after reaping"
        );

        TEST_FORCE_REAP_TIMEOUT.store(false, AtomicOrdering::Release);
        TEST_TERMINATION_GRACE_MILLIS.store(0, AtomicOrdering::Release);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_runner_stop_withholds_reconcile_until_reaper_completion() {
        let _guard = REAPER_TEST_LOCK.lock().await;
        let _reset = ReaperTestReset;
        TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
        TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);

        let (_dir, workspace, _store) = configured_runner_fixture();
        let child = spawn_term_ignoring_child().await;
        let pid = child.id().expect("runner termination test child pid");
        assert!(feanorfs_agent_core::lock::pid_alive(pid));
        let spec = ChildSpec {
            kind: ChildKind::Runner(workspace.to_string_lossy().into_owned()),
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let key = runner_child_key(&workspace.to_string_lossy());
        let mut children = BTreeMap::from([(key, {
            let mut managed = ManagedChild::new(spec);
            managed.child = Some(child);
            managed
        })]);

        reconcile(&mut children, &BTreeMap::new(), false)
            .await
            .expect("deferred runner termination is retained, not dropped");
        assert_eq!(children.len(), 1);
        assert_eq!(
            children.values().next().unwrap().state,
            ChildState::Stopping
        );
        let reap_ticket = children
            .values()
            .next()
            .and_then(|managed| managed.pending_reap.clone())
            .expect("deferred runner termination retains its reaper ticket");
        assert!(
            !runner_reconciliation_complete(&children, &SupervisorRegistry::default()),
            "durable runner stop acknowledgement must remain gated"
        );
        assert!(publish_runner_reconcile_ack(
            &children,
            &SupervisorRegistry::default(),
            now_epoch(),
            1,
        )
        .is_err());

        await_reap_ticket(&reap_ticket).await;
        assert_pid_reaped(pid).await;
        reconcile(&mut children, &BTreeMap::new(), false)
            .await
            .expect("reaper completion and runner cleanup reconcile");
        assert!(children.is_empty());
        assert!(runner_reconciliation_complete(
            &children,
            &SupervisorRegistry::default()
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_keeps_child_owned_when_reaper_worker_start_fails() {
        let _guard = REAPER_TEST_LOCK.lock().await;
        let _reset = ReaperTestReset;
        TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
        TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);
        SUPERVISOR_CHILD_REAPER.fail_worker_start_for_test(true);

        let child = spawn_term_ignoring_child().await;
        let pid = child.id().expect("shutdown test child pid");
        let spec = ChildSpec {
            kind: ChildKind::Hub,
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let mut managed = ManagedChild::new(spec);
        managed.child = Some(child);
        let children = BTreeMap::from([("component:hub".to_string(), managed)]);

        terminate_all_children(children).await;
        assert_pid_reaped(pid).await;

        SUPERVISOR_CHILD_REAPER.fail_worker_start_for_test(false);
        TEST_FORCE_REAP_TIMEOUT.store(false, AtomicOrdering::Release);
        TEST_TERMINATION_GRACE_MILLIS.store(0, AtomicOrdering::Release);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_recovers_child_when_termination_task_panics() {
        let _guard = REAPER_TEST_LOCK.lock().await;
        let _reset = ReaperTestReset;
        TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
        TEST_SHUTDOWN_PANIC_ONCE.store(true, AtomicOrdering::Release);

        let child = spawn_term_ignoring_child().await;
        let pid = child.id().expect("panic recovery child pid");
        let spec = ChildSpec {
            kind: ChildKind::Hub,
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let remaining = terminate_all_children(BTreeMap::from([(
            "component:hub".to_string(),
            ManagedChild {
                child: Some(child),
                ..ManagedChild::new(spec)
            },
        )]))
        .await;
        assert!(remaining.is_empty(), "panic recovery must finish cleanup");
        assert_pid_reaped(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_awaits_normal_background_reaper_handoff() {
        let _guard = REAPER_TEST_LOCK.lock().await;
        let _reset = ReaperTestReset;
        TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
        // Force the bounded wait path to hand the still-owned Tokio Child to
        // the normal background reaper. The shutdown helper must retain the
        // ManagedChild and await this ticket before returning an empty map.
        TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);
        SUPERVISOR_CHILD_REAPER.fail_worker_start_for_test(false);

        let child = spawn_term_ignoring_child().await;
        let pid = child.id().expect("normal handoff child pid");
        let spec = ChildSpec {
            kind: ChildKind::Hub,
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let mut managed = ManagedChild::new(spec);
        managed.child = Some(child);
        let remaining =
            terminate_all_children(BTreeMap::from([("component:hub".to_string(), managed)])).await;
        assert!(remaining.is_empty(), "normal reaper handoff was awaited");
        assert_pid_reaped(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresolved_startup_runner_authority_retries_and_then_clears() {
        let (_dir, workspace, _store) = configured_runner_fixture();
        let canonical = workspace.to_string_lossy().into_owned();
        let program = std::env::current_exe().unwrap();
        let identity = OrphanIdentity::Worker {
            subcommand: "runner-run",
            operand: canonical.clone(),
            process_start_id: None,
            job_owned: false,
        };
        // The current supervisor PID is deliberately unkillable by the
        // orphan path. This models an unresolved live/mismatched authority
        // and proves that it remains in the child map rather than being
        // dropped at the first reconcile.
        let pending = pending_orphan_cleanup(
            &program,
            Some(std::process::id()),
            0,
            &identity,
            managed_command_line(&program, "runner-run", &canonical),
            Duration::from_millis(1),
        );
        let key = pending.key.clone();
        let expected_executable_identity = pending.executable_identity.clone();
        let mut children = BTreeMap::from([(
            key,
            ManagedChild {
                spec: pending.spec.clone(),
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
        )]);
        reconcile(&mut children, &BTreeMap::new(), false)
            .await
            .unwrap();
        assert_eq!(children.len(), 1, "unresolved runner authority is retained");
        assert!(!runner_reconciliation_complete(
            &children,
            &SupervisorRegistry::default()
        ));

        // Recovery is deterministic: once the recorded PID is gone, the
        // retry completes the orphan ticket, checkpoints runner state, and
        // only then removes the stale runner entry.
        children
            .values_mut()
            .next()
            .unwrap()
            .pending_orphan
            .as_mut()
            .unwrap()
            .pid = Some(999_999);
        reconcile(&mut children, &BTreeMap::new(), false)
            .await
            .unwrap();
        assert!(children.is_empty(), "recovered runner authority is cleared");
    }

    #[test]
    fn pending_orphan_status_preserves_process_identity_and_start_time() {
        let program = std::env::current_exe().unwrap();
        let identity = OrphanIdentity::Worker {
            subcommand: "runner-run",
            operand: "/pending-runner".to_string(),
            process_start_id: Some("linux:123:456".to_string()),
            job_owned: false,
        };
        let pending = pending_orphan_cleanup(
            &program,
            Some(123),
            987,
            &identity,
            managed_command_line(&program, "runner-run", "/pending-runner"),
            STOP_GRACE,
        );
        let key = pending.key.clone();
        let expected_executable_identity = pending.executable_identity.clone();
        let mut children = BTreeMap::new();
        children.insert(
            key,
            ManagedChild {
                spec: pending.spec.clone(),
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
        );
        let status = build_status(&children, 1);
        let child = status.runners.get("/pending-runner").unwrap();
        assert_eq!(child.pid, Some(123));
        assert_eq!(child.process_start_id.as_deref(), Some("linux:123:456"));
        assert_eq!(child.since, 987);
        assert_eq!(child.state, ChildState::Stopping);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_reaper_status_roundtrip_preserves_exact_handoff_identity() {
        let _guard = REAPER_TEST_LOCK.lock().await;
        let mut child = spawn_term_ignoring_child().await;
        let pid = child.id().expect("pending reaper child pid");
        let process_start_id = exact_child_process_start_id(pid).expect("native child identity");
        let since = now_epoch();
        let canonical = "/pending-reaper".to_string();
        let spec = ChildSpec {
            kind: ChildKind::Runner(canonical.clone()),
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let mut managed = ManagedChild::new(spec.clone());
        // Simulate the post-deadline state: the Tokio Child has been handed to
        // the persistent reaper, while the supervisor retains only the exact
        // identity needed for a durable restart handoff.
        managed.state = ChildState::Stopping;
        managed.pending_reap = Some(ReapTicket::new());
        managed.owned_pid = Some(pid);
        managed.owned_process_start_id = Some(process_start_id.clone());
        managed.owned_since = since;
        let children = BTreeMap::from([(runner_child_key(&canonical), managed)]);

        let status = build_status(&children, 1);
        let encoded = serde_json::to_vec(&status).expect("serialize pending reaper status");
        let decoded: SupervisorStatus =
            serde_json::from_slice(&encoded).expect("deserialize pending reaper status");
        let recorded = decoded
            .runners
            .get(&canonical)
            .expect("runner status entry");
        assert_eq!(recorded.pid, Some(pid));
        assert_eq!(
            recorded.process_start_id.as_deref(),
            Some(process_start_id.as_str())
        );
        assert_eq!(recorded.since, since);
        assert_eq!(recorded.state, ChildState::Stopping);

        // A replacement supervisor can carry these serialized fields into its
        // pending-orphan map. The stopping marker and exact identity prevent a
        // desired-set pass from respawning the runner before cleanup completes.
        let identity = OrphanIdentity::Worker {
            subcommand: "runner-run",
            operand: canonical.clone(),
            process_start_id: recorded.process_start_id.clone(),
            job_owned: false,
        };
        let pending = pending_orphan_cleanup(
            &std::env::current_exe().unwrap(),
            recorded.pid,
            recorded.since,
            &identity,
            managed_command_line(&std::env::current_exe().unwrap(), "runner-run", &canonical),
            STOP_GRACE,
        );
        assert_eq!(pending.pid, Some(pid));
        assert_eq!(pending.expected_since, since);
        assert_eq!(
            pending.process_start_id.as_deref(),
            Some(process_start_id.as_str())
        );
        let expected_executable_identity = pending.executable_identity.clone();
        let replacement = ManagedChild {
            spec: pending.spec.clone(),
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
        };
        assert!(!should_respawn(&replacement));

        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_child_falls_back_to_direct_kill_when_identity_probe_is_unavailable() {
        let _guard = REAPER_TEST_LOCK.lock().await;
        TEST_IDENTITY_UNAVAILABLE.store(true, AtomicOrdering::Release);
        let child = spawn_term_ignoring_child().await;
        let pid = child.id().expect("identity fallback child pid");
        let spec = ChildSpec {
            kind: ChildKind::Hub,
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let mut managed = ManagedChild::new(spec);
        managed.child = Some(child);
        let result = terminate_child(&mut managed).await;
        TEST_IDENTITY_UNAVAILABLE.store(false, AtomicOrdering::Release);
        assert!(
            result.is_ok(),
            "direct Child kill should remain recoverable"
        );
        assert_pid_reaped(pid).await;
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unverified_runner_process_start_is_never_signaled() {
        let metadata = feanorfs_agent_core::RunnerProcessMetadata {
            pid: std::process::id(),
            process_start_id: format!("spawn:{}:session", std::process::id()),
        };
        assert!(!runner_process_start_matches(&metadata));
        #[cfg(not(unix))]
        assert!(!terminate_verified_runner_group(&metadata));
    }

    #[tokio::test]
    async fn runner_exit_cleanup_waits_for_the_exact_session_then_checkpoints() {
        let (_dir, workspace, store) = configured_runner_fixture();
        let session = store
            .execution_session(
                &workspace,
                feanorfs_agent_core::RunnerExecutionMode::Foreground,
            )
            .unwrap();
        let request_id = id('b');
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('c'),
                cursor_reset: false,
                messages: vec![feanorfs_common::AgentMessage {
                    message_id: request_id.clone(),
                    from: "requester".to_string(),
                    to: "worker".to_string(),
                    kind: feanorfs_common::AgentMessageKind::Request,
                    body: "run".to_string(),
                    about_snapshot: id('a'),
                    reply_to: None,
                    created_at_ms: 1,
                }],
            })
            .unwrap();
        session.begin_next(&id('c')).unwrap();
        session
            .mark_spawned(
                &request_id,
                std::process::id(),
                "unsupported-test-process-identity",
            )
            .unwrap();

        let _ = finish_runner_workspace_exit(&workspace);
        assert_eq!(
            store.status().unwrap().phase,
            feanorfs_agent_core::RunnerPhase::Running
        );

        drop(session);
        let _ = finish_runner_workspace_exit(&workspace);
        let status = store.status().unwrap();
        assert_eq!(
            status.phase,
            feanorfs_agent_core::RunnerPhase::NeedsAttention
        );
        assert_eq!(
            status.attention,
            Some(feanorfs_agent_core::RunnerAttention::AmbiguousExecution)
        );
    }

    #[test]
    fn status_liveness_rejects_stale_or_dead_children() {
        let current_start = process_start_epoch(std::process::id()).unwrap_or(1);
        let current_identity =
            process_tree::process_start_identifier(std::process::id(), "status-test");
        let mut status = SupervisorStatus {
            pid: Some(std::process::id()),
            process_start_id: Some(current_identity.clone()),
            started_at: current_start,
            ..SupervisorStatus::default()
        };
        // Alive supervisor + alive child pid (this test process) -> running.
        status.workspaces.insert(
            "/ws".into(),
            ChildStatus {
                pid: Some(std::process::id()),
                process_start_id: Some(current_identity.clone()),
                job_owned: false,
                executable_identity: None,
                state: ChildState::Running,
                restarts: 0,
                last_exit: None,
                since: current_start,
            },
        );
        assert!(child_is_running(&status, "/ws"));
        status.runners.insert(
            "/ws".into(),
            ChildStatus {
                pid: Some(std::process::id()),
                process_start_id: Some(current_identity),
                job_owned: false,
                executable_identity: None,
                state: ChildState::Running,
                restarts: 0,
                last_exit: None,
                since: current_start,
            },
        );
        assert!(runner_child_is_running(&status, "/ws"));
        #[cfg(unix)]
        {
            status.workspaces.get_mut("/ws").unwrap().since = current_start.saturating_sub(30);
        }
        #[cfg(target_os = "windows")]
        {
            // Windows intentionally does not infer process liveness from the
            // wall-clock `since` field.  Exercise the same stale-child
            // boundary with a mismatched kernel creation token instead.
            status.workspaces.get_mut("/ws").unwrap().process_start_id =
                Some("windows:1".to_string());
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            status.workspaces.get_mut("/ws").unwrap().state = ChildState::Stopped;
        }
        assert!(!child_is_running(&status, "/ws"));
        status.workspaces.get_mut("/ws").unwrap().since = current_start;
        // Dead child pid -> not running.
        status.workspaces.get_mut("/ws").unwrap().pid = Some(999_999);
        assert!(!child_is_running(&status, "/ws"));
        // Dead supervisor pid -> nothing is running even with a live child.
        status.workspaces.get_mut("/ws").unwrap().pid = Some(std::process::id());
        status.pid = Some(999_999);
        assert!(!child_is_running(&status, "/ws"));
        assert!(!runner_child_is_running(&status, "/ws"));
        assert!(runner_recorded_by_dead_supervisor(&status, "/ws"));
        // Missing file entry -> not running.
        assert!(!child_is_running(&status, "/missing"));
        assert!(!runner_recorded_by_dead_supervisor(&status, "/missing"));
    }

    #[test]
    fn workers_restart_on_clean_exit_but_tray_respects_quit() {
        // Hub, workspace, and runner workers are "always running" services: a clean
        // exit (exit code 0) must still be restarted.
        let mut hub = ManagedChild::new(ChildSpec {
            kind: ChildKind::Hub,
            program: PathBuf::from("/usr/local/bin/feanorfs"),
            args: vec![],
            env: vec![],
            restart_on_zero_exit: true,
        });
        hub.last_exit = Some(0);
        assert!(should_respawn(&hub));
        let mut runner = ManagedChild::new(ChildSpec {
            kind: ChildKind::Runner("/workspace".to_string()),
            program: PathBuf::from("/usr/local/bin/feanorfs"),
            args: vec![],
            env: vec![],
            restart_on_zero_exit: true,
        });
        runner.last_exit = Some(0);
        assert!(should_respawn(&runner));
        // The tray exits 0 when the user quits it; it must stay stopped.
        let mut tray = ManagedChild::new(ChildSpec {
            kind: ChildKind::Tray,
            program: PathBuf::from("/usr/local/bin/feanorfs-tray"),
            args: vec![],
            env: vec![],
            restart_on_zero_exit: false,
        });
        tray.last_exit = Some(0);
        assert!(!should_respawn(&tray));
        // A crashed tray (nonzero exit) is restarted.
        tray.last_exit = Some(1);
        assert!(should_respawn(&tray));
    }

    #[tokio::test]
    async fn clean_runner_exit_enters_bounded_restart_backoff() {
        let (_dir, workspace, store) = configured_runner_fixture();
        store.set_enabled(true).unwrap();
        let workspace = workspace.to_string_lossy().into_owned();
        let spec = ChildSpec {
            kind: ChildKind::Runner(workspace.clone()),
            program: which::which("true").unwrap(),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let key = runner_child_key(&workspace);
        let mut desired = BTreeMap::new();
        desired.insert(key.clone(), spec);
        let mut children = BTreeMap::new();
        assert!(reconcile(&mut children, &desired, false).await.unwrap());
        // Isolate the first exit-to-backoff transition. On a loaded Windows
        // worker, cleanup can consume the one-second backoff and otherwise
        // let this test observe a newly spawned (and suspended) replacement.
        store.set_enabled(false).unwrap();
        #[cfg(target_os = "windows")]
        release_test_suspended_child(&mut children, &key);
        // Process creation is materially slower on Windows when the complete
        // client test binary is starting children in parallel.  A fixed 20 ms
        // sleep races the first `true` process and makes this test assert on
        // the still-running state.  Poll the exact managed child until its
        // exit is observed instead; the backoff assertion remains unchanged.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let changed = reconcile(&mut children, &desired, false).await.unwrap();
            if children.get(&key).is_some_and(|managed| {
                managed.last_exit == Some(0) && managed.state == ChildState::Backoff
            }) {
                assert!(changed);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "clean runner test child did not exit within the bounded test window"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let managed = &children[&key];
        assert_eq!(managed.last_exit, Some(0));
        assert_eq!(managed.state, ChildState::Backoff);
        assert_eq!(managed.restarts, 1);
        assert!(managed.backoff_until.is_some());
    }

    #[tokio::test]
    async fn stale_desired_runner_is_not_spawned_after_disable() {
        let (_dir, workspace, store) = configured_runner_fixture();
        store.set_enabled(true).unwrap();
        let workspace = workspace.to_string_lossy().into_owned();
        let key = runner_child_key(&workspace);
        let mut desired = BTreeMap::new();
        desired.insert(
            key,
            ChildSpec {
                kind: ChildKind::Runner(workspace),
                program: which::which("true").unwrap(),
                args: Vec::new(),
                env: Vec::new(),
                restart_on_zero_exit: true,
            },
        );
        store.set_enabled(false).unwrap();

        let mut children = BTreeMap::new();
        assert!(!reconcile(&mut children, &desired, false).await.unwrap());
        assert!(children.is_empty());
    }

    #[test]
    fn supervisor_lock_is_exclusive_and_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.lock");
        let owner_path = supervisor_lock_owner_path_at(&path);
        let first = acquire_supervisor_lock_at(&path)
            .unwrap()
            .expect("first supervisor owns lock");
        assert_eq!(
            read_supervisor_lock_owner_at(&path).map(|owner| owner.pid),
            Some(std::process::id())
        );
        // A second supervisor in the same process cannot re-acquire: flock is
        // per open-file-description, so this exercises the cross-process path.
        let second = acquire_supervisor_lock_at(&path).unwrap();
        assert!(second.is_none());
        drop(first);
        assert!(!owner_path.exists());
        fs::write(&owner_path, b"stale-owner").unwrap();
        let third = acquire_supervisor_lock_at(&path)
            .unwrap()
            .expect("third supervisor reuses released lock");
        assert_eq!(
            read_supervisor_lock_owner_at(&path).map(|owner| owner.pid),
            Some(std::process::id())
        );
        drop(third);
        assert!(!owner_path.exists());
    }

    #[test]
    fn stray_watcher_requires_fresh_marker() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let state = feanorfs_agent_core::ensure_workspace_state(&workspace).unwrap();
        std::fs::write(state.join("watch.pid"), format!("{}\n", std::process::id())).unwrap();
        let marker = state.join("watch.pid");
        // Simulate an ancient marker (belongs to a dead watcher).
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let file = std::fs::File::options().write(true).open(&marker).unwrap();
        file.set_modified(old).unwrap();
        assert!(
            stray_workspace_watcher(&workspace.to_string_lossy()).is_none(),
            "stale watch markers must not produce killable pids"
        );
        // A fresh marker for an unrelated process (no feanorfs in argv) is
        // also not a stray: pid reuse must never kill an innocent process.
        // Use a live non-feanorfs process: the cargo test harness itself.
        std::fs::write(
            &marker,
            format!(
                "{}\n{}\n{}\n",
                std::process::id(),
                now_epoch(),
                process_start_epoch(std::process::id()).unwrap_or(0)
            ),
        )
        .unwrap();
        assert!(
            stray_workspace_watcher(&workspace.to_string_lossy()).is_none(),
            "live pids without a FeanorFS command line must not be killed"
        );
    }

    #[test]
    fn clean_exit_without_restart_policy_stays_stopped() {
        let spec = ChildSpec {
            kind: ChildKind::Tray,
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: false,
        };
        let mut managed = ManagedChild::new(spec.clone());
        // Fresh child: spawn it.
        assert!(should_respawn(&managed));
        // Simulate a clean exit: never respawn (user quit the tray, hub
        // shutdown on purpose), and no backoff is engaged (no tight loop).
        managed.last_exit = Some(0);
        assert!(!should_respawn(&managed));
        managed.last_exit = Some(1);
        assert!(should_respawn(&managed), "crash exits must restart");
        // With an explicit restart-on-zero policy, clean exits restart.
        let mut managed = ManagedChild::new(ChildSpec {
            restart_on_zero_exit: true,
            ..spec
        });
        managed.last_exit = Some(0);
        assert!(should_respawn(&managed));
    }

    #[test]
    fn backoff_holds_respawns_until_deadline() {
        let spec = ChildSpec {
            kind: ChildKind::Tray,
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: false,
        };
        let mut managed = ManagedChild::new(spec);
        managed.backoff_until = Some(Instant::now() + Duration::from_secs(60));
        assert!(!should_respawn(&managed));
        managed.backoff_until = None;
        assert!(should_respawn(&managed));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_duration(1), Duration::from_secs(1));
        assert_eq!(backoff_duration(2), Duration::from_secs(2));
        assert_eq!(backoff_duration(3), Duration::from_secs(4));
        assert_eq!(backoff_duration(20), Duration::from_secs(BACKOFF_MAX_SECS));
    }

    #[test]
    fn status_snapshot_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut status = SupervisorStatus {
            pid: Some(42),
            version: STATUS_VERSION,
            started_at: 1,
            updated_at: 2,
            ..SupervisorStatus::default()
        };
        status.hub = Some(ChildStatus {
            pid: Some(7),
            process_start_id: None,
            job_owned: false,
            executable_identity: None,
            state: ChildState::Running,
            restarts: 0,
            last_exit: None,
            since: 1,
        });
        status.workspaces.insert(
            "/ws".into(),
            ChildStatus {
                pid: Some(8),
                process_start_id: None,
                job_owned: false,
                executable_identity: None,
                state: ChildState::Backoff,
                restarts: 3,
                last_exit: Some(1),
                since: 2,
            },
        );
        status.runners.insert(
            "/ws".into(),
            ChildStatus {
                pid: Some(9),
                process_start_id: None,
                job_owned: false,
                executable_identity: None,
                state: ChildState::Stopped,
                restarts: 1,
                last_exit: Some(0),
                since: 3,
            },
        );
        let content = serde_json::to_vec_pretty(&status).unwrap();
        std::fs::write(dir.path().join("status.json"), content).unwrap();
        let path = dir.path().join("status.json");
        let raw = std::fs::read(&path).unwrap();
        let parsed: SupervisorStatus = serde_json::from_slice(&raw).unwrap();
        assert_eq!(parsed.pid, Some(42));
        assert_eq!(parsed.hub.as_ref().unwrap().state, ChildState::Running);
        assert_eq!(parsed.workspaces["/ws"].state, ChildState::Backoff);
        assert_eq!(parsed.runners["/ws"].state, ChildState::Stopped);

        let legacy: SupervisorStatus = serde_json::from_str(
            r#"{"pid":null,"version":1,"started_at":0,"updated_at":0,"workspaces":{},"hub":null,"tray":null}"#,
        )
        .unwrap();
        assert!(legacy.runners.is_empty());
    }

    #[test]
    fn status_projects_runner_and_watcher_separately() {
        let watcher_spec = ChildSpec {
            kind: ChildKind::Workspace("/same".to_string()),
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let runner_spec = ChildSpec {
            kind: ChildKind::Runner("/same".to_string()),
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        };
        let mut children = BTreeMap::new();
        children.insert(
            workspace_child_key("/same"),
            ManagedChild::new(watcher_spec),
        );
        children.insert(runner_child_key("/same"), ManagedChild::new(runner_spec));
        let status = build_status(&children, 1);
        assert_eq!(status.version, STATUS_VERSION);
        assert!(status.workspaces.contains_key("/same"));
        assert!(status.runners.contains_key("/same"));
        assert_eq!(status.workspaces.len(), 1);
        assert_eq!(status.runners.len(), 1);
    }

    #[tokio::test]
    async fn desired_runner_spec_is_exact_redacted_and_state_gated() {
        let (_dir, workspace, store) = configured_runner_fixture();
        store.set_enabled(true).unwrap();
        let canonical = workspace.to_string_lossy().into_owned();
        let mut registry = SupervisorRegistry::default();
        registry.workspaces.push(canonical.clone());
        registry.runners.push(canonical.clone());

        let desired = desired_specs(&registry, &None).unwrap();
        let watcher = &desired[&workspace_child_key(&canonical)];
        assert_eq!(watcher.kind, ChildKind::Workspace(canonical.clone()));
        assert!(watcher.restart_on_zero_exit);
        let runner = &desired[&runner_child_key(&canonical)];
        assert_eq!(runner.kind, ChildKind::Runner(canonical.clone()));
        assert_eq!(
            runner.args,
            vec![
                OsString::from("service"),
                OsString::from("runner-run"),
                workspace.as_os_str().to_owned(),
            ]
        );
        assert!(runner.env.is_empty());
        assert!(runner.restart_on_zero_exit);
        assert_eq!(runner.program, std::env::current_exe().unwrap());
        assert_ne!(
            workspace_child_key(&canonical),
            runner_child_key(&canonical)
        );

        store.set_enabled(false).unwrap();
        assert!(!desired_specs(&registry, &None)
            .unwrap()
            .contains_key(&runner_child_key(&canonical)));

        store.set_enabled(true).unwrap();
        let session = store
            .execution_session(
                &workspace,
                feanorfs_agent_core::RunnerExecutionMode::Supervised,
            )
            .unwrap();
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('b'),
                cursor_reset: false,
                messages: vec![feanorfs_common::AgentMessage {
                    message_id: id('1'),
                    from: "human".to_string(),
                    to: "worker".to_string(),
                    kind: feanorfs_common::AgentMessageKind::Request,
                    body: "private request body".to_string(),
                    about_snapshot: id('a'),
                    reply_to: None,
                    created_at_ms: 1,
                }],
            })
            .unwrap();
        session.begin_next(&id('b')).unwrap();
        drop(session);
        drop(
            store
                .execution_session(
                    &workspace,
                    feanorfs_agent_core::RunnerExecutionMode::Supervised,
                )
                .unwrap(),
        );
        assert_eq!(
            store.status().unwrap().phase,
            feanorfs_agent_core::RunnerPhase::NeedsAttention
        );
        assert!(!desired_specs(&registry, &None)
            .unwrap()
            .contains_key(&runner_child_key(&canonical)));
    }

    #[test]
    fn desired_specs_skip_unavailable_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = SupervisorRegistry::default();
        registry
            .workspaces
            .push(dir.path().to_string_lossy().into_owned());
        registry.workspaces.push("/definitely/missing".into());
        let desired = desired_specs(&registry, &None).unwrap();
        // The missing workspace is skipped; the real temp dir is included only
        // when it is a configured FeanorFS workspace, which it is not. The hub
        // and tray entries may legitimately appear on a dev machine.
        let workspace_keys: Vec<_> = desired
            .values()
            .filter(|spec| matches!(&spec.kind, ChildKind::Workspace(_) | ChildKind::Runner(_)))
            .map(|spec| &spec.kind)
            .collect();
        assert!(
            workspace_keys.is_empty(),
            "unexpected workspace keys: {workspace_keys:?}"
        );
    }

    #[test]
    fn legacy_label_detection_parses_macos_plists() {
        // macOS plist parsing needs /usr/bin/plutil; the pure label filtering
        // is exercised here through the registry instead.
        assert!(LABEL.starts_with("com.feanorfs."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_workspace_plist_extracts_program_argument_three() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("com.feanorfs.sync-test.plist");
        let mut file = std::fs::File::create(&plist).unwrap();
        file.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>ProgramArguments</key><array>
<string>/usr/local/bin/feanorfs</string>
<string>service</string>
<string>run</string>
<string>/Users/me/My Project</string>
</array>
</dict></plist>"#,
        )
        .unwrap();
        drop(file);
        // `feanorfs service run <workspace>`: the workspace is index 3.
        assert_eq!(
            plist_program_argument(&plist, 3).as_deref(),
            Some("/Users/me/My Project")
        );
        // The previous index-2 read returned the subcommand, not the path.
        assert_eq!(plist_program_argument(&plist, 2).as_deref(), Some("run"));
    }
}
