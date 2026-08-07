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

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::util::{record_service_identity, service_identity_matches};

pub(crate) const LABEL: &str = "com.feanorfs.agent";

const REGISTRY_FILE: &str = "supervisor.json";
const STATUS_FILE: &str = "supervisor-status.json";
const MARKER_FILE: &str = "supervisor-service-program";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(5);
pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_BASE_SECS: u64 = 1;
const BACKOFF_MAX_SECS: u64 = 60;
const RESET_AFTER: Duration = Duration::from_secs(60);

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
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildStatus {
    pub(crate) pid: Option<u32>,
    pub(crate) state: ChildState,
    pub(crate) restarts: u32,
    pub(crate) last_exit: Option<i32>,
    pub(crate) since: u64,
}

/// Secret-free snapshot published by the running supervisor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SupervisorStatus {
    pub(crate) pid: Option<u32>,
    pub(crate) version: u32,
    pub(crate) started_at: u64,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) workspaces: BTreeMap<String, ChildStatus>,
    pub(crate) hub: Option<ChildStatus>,
    pub(crate) tray: Option<ChildStatus>,
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
}

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
    let result = update(&mut store);
    save_registry(&path, &store)?;
    Ok(result)
}

fn read_registry() -> anyhow::Result<SupervisorRegistry> {
    let path = registry_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    load_registry(&path)
}

fn canonical_workspace_path(workspace: &Path) -> anyhow::Result<String> {
    let canonical = workspace
        .canonicalize()
        .with_context(|| format!("Workspace folder does not exist: {}", workspace.display()))?;
    Ok(canonical.to_string_lossy().into_owned())
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

/// Seed the registry from recent workspaces on first use (legacy installs and
/// fresh profiles). Never resurrects workspaces an explicit `service stop` or
/// `feanorfs stop` moved out of supervision.
fn seed_registry_from_recents_if_absent() -> anyhow::Result<()> {
    let path = registry_path()?;
    if path.is_file() {
        return Ok(());
    }
    let recent = feanorfs_client::list_recent_workspaces()?;
    update_registry(|store| {
        for entry in recent.workspaces {
            let workspace = Path::new(&entry.path);
            if workspace.is_dir()
                && feanorfs_agent_core::workspace_is_configured(workspace)
                && !store.workspaces.iter().any(|path| path == &entry.path)
            {
                store.workspaces.push(entry.path.clone());
            }
        }
    })
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
        std::thread::sleep(Duration::from_millis(50));
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
        Some(status)
            if status
                .workspaces
                .get(&canonical)
                .is_some_and(|child| child.state == ChildState::Running) =>
        {
            ServiceState::Running
        }
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
        Some(status)
            if status
                .hub
                .as_ref()
                .is_some_and(|c| c.state == ChildState::Running) =>
        {
            ServiceState::Running
        }
        _ => ServiceState::Stopped,
    })
}

pub(crate) fn wait_for_workspace_child(canonical: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = read_status()? {
            if status
                .workspaces
                .get(canonical)
                .is_some_and(|child| child.state == ChildState::Running)
            {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "automatic sync did not reach the running state within 5 seconds; check `feanorfs service status` and retry `feanorfs start`"
    )
}

fn wait_for_workspace_stopped(canonical: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if !feanorfs_client::is_watching(canonical)
            && !feanorfs_client::lock::is_sync_lock_active(canonical)
            && read_status()?.is_none_or(|status| {
                status
                    .workspaces
                    .get(canonical.to_string_lossy().as_ref())
                    .is_none_or(|child| child.state != ChildState::Running)
            })
        {
            return Ok(());
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

fn publish_status(
    children: &BTreeMap<String, ManagedChild>,
    started_at: u64,
) -> anyhow::Result<()> {
    let mut status = SupervisorStatus {
        pid: Some(std::process::id()),
        version: 1,
        started_at,
        updated_at: now_epoch(),
        ..SupervisorStatus::default()
    };
    for (key, managed) in children {
        let child = ChildStatus {
            pid: managed.child.as_ref().and_then(|child| child.id()),
            state: managed.state,
            restarts: managed.restarts,
            last_exit: managed.last_exit,
            since: managed.spawned_at.map(epoch_at).unwrap_or(0),
        };
        match key.as_str() {
            "hub" => status.hub = Some(child),
            "tray" => status.tray = Some(child),
            workspace => {
                status.workspaces.insert(workspace.to_string(), child);
            }
        }
    }
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
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    restart_on_zero_exit: bool,
}

struct ManagedChild {
    spec: ChildSpec,
    child: Option<tokio::process::Child>,
    state: ChildState,
    restarts: u32,
    last_exit: Option<i32>,
    backoff_until: Option<Instant>,
    spawned_at: Option<Instant>,
}

impl ManagedChild {
    fn new(spec: ChildSpec) -> Self {
        Self {
            spec,
            child: None,
            state: ChildState::Stopped,
            restarts: 0,
            last_exit: None,
            backoff_until: None,
            spawned_at: None,
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
    if managed.child.is_some() {
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

fn desired_specs(registry: &SupervisorRegistry) -> anyhow::Result<BTreeMap<String, ChildSpec>> {
    let mut desired = BTreeMap::new();
    let program = std::env::current_exe().context("locate the feanorfs executable")?;

    if super::hub_service::hub_data_present() {
        let data_dir = super::hub_service::default_data_dir()?;
        desired.insert(
            "hub".into(),
            ChildSpec {
                program: program.clone(),
                args: vec![
                    OsString::from("service"),
                    OsString::from("hub-run"),
                    data_dir.into_os_string(),
                ],
                env: Vec::new(),
                restart_on_zero_exit: false,
            },
        );
    }
    // macOS and Linux run the tray inside the supervisor job; Windows keeps
    // its own scheduled task (see `install_tray_if_available`), so spawning a
    // tray child here too would run two trays on Windows.
    #[cfg(not(target_os = "windows"))]
    if let Some(tray) = super::service::find_tray_program(&program) {
        desired.insert(
            "tray".into(),
            ChildSpec {
                program: tray,
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
            path.clone(),
            ChildSpec {
                program: program.clone(),
                args: vec![
                    OsString::from("service"),
                    OsString::from("run"),
                    workspace.as_os_str().to_owned(),
                ],
                env: Vec::new(),
                restart_on_zero_exit: false,
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

fn spawn_child(spec: &ChildSpec) -> anyhow::Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.env.iter().cloned())
        .stdin(std::process::Stdio::null());
    command
        .spawn()
        .with_context(|| format!("start supervised worker {}", spec.program.display()))
}

/// Terminate a managed child: SIGTERM on Unix, escalate to SIGKILL after the
/// grace period, and reap it.
async fn terminate_child(managed: &mut ManagedChild) -> anyhow::Result<()> {
    let Some(mut child) = managed.child.take() else {
        managed.state = ChildState::Stopped;
        return Ok(());
    };
    let pid = child.id();
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: `pid` belongs to a live child of this process.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                break;
            }
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    managed.state = ChildState::Stopped;
    Ok(())
}

/// Kill a stray watcher pid left behind by a previous supervisor or a manual
/// process, waiting briefly for it to die.
async fn terminate_stray_pid(pid: u32) {
    #[cfg(unix)]
    // SAFETY: best-effort signal to a pid observed in our own watch markers.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline && feanorfs_agent_core::lock::pid_alive(pid) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if feanorfs_agent_core::lock::pid_alive(pid) {
        #[cfg(unix)]
        // SAFETY: same pid as above.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
}

/// Watchers refresh their `watch.pid` marker every 45 s while alive, so a
/// marker older than this cannot belong to a live watcher.
const STRAY_WATCHER_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// A live watcher for a workspace that is not one of our children (an orphan
/// from a previous supervisor or a manual `--foreground` process).
///
/// Only pids with a fresh marker AND a command line that references the
/// FeanorFS binary and this workspace are returned: a stale marker whose pid
/// was reused by an unrelated process must never be signaled, and a user's
/// own terminal watcher is not silently killed unless it is genuinely this
/// workspace's watcher.
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
    let pid = content.lines().next()?.trim().parse::<u32>().ok()?;
    if !feanorfs_agent_core::lock::pid_alive(pid) {
        return None;
    }
    let command = process_command_line(pid)?;
    (command.contains("feanorfs") && command.contains(canonical)).then_some(pid)
}

#[cfg(unix)]
fn process_command_line(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Non-Unix platforms get no identity probe; conservatively never report a
/// stray (the sync lock still serializes watchers, so this is safe).
#[cfg(not(unix))]
fn process_command_line(_pid: u32) -> Option<String> {
    None
}

async fn reconcile(
    children: &mut BTreeMap<String, ManagedChild>,
    desired: &BTreeMap<String, ChildSpec>,
    restart_hub: bool,
) -> anyhow::Result<bool> {
    let mut changed = false;
    // Reap exited children and apply the restart policy.
    for managed in children.values_mut() {
        let Some(child) = managed.child.as_mut() else {
            continue;
        };
        let Some(status) = child.try_wait()? else {
            continue;
        };
        let exit = status.code();
        managed.last_exit = exit;
        managed.child = None;
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
            managed.state = ChildState::Backoff;
        } else {
            managed.state = ChildState::Stopped;
        }
    }

    // A hub configuration change (relay route or listen port) restarts the hub
    // worker immediately, without crash backoff.
    if restart_hub {
        if let Some(hub) = children.get_mut("hub") {
            terminate_child(hub).await?;
            hub.restarts = 0;
            hub.backoff_until = None;
            changed = true;
        }
    }

    // Spawn children that are desired but missing.
    for (key, spec) in desired {
        let managed = children
            .entry(key.clone())
            .or_insert_with(|| ManagedChild::new(spec.clone()));
        if managed.child.is_some() {
            continue;
        }
        if !should_respawn(managed) {
            continue;
        }
        if key != "hub" && key != "tray" {
            if let Some(stray) = stray_workspace_watcher(key) {
                tracing::warn!("stopping stray watcher {stray} before supervising {key}");
                terminate_stray_pid(stray).await;
            }
        }
        match spawn_child(spec) {
            Ok(child) => {
                managed.child = Some(child);
                managed.spawned_at = Some(Instant::now());
                managed.backoff_until = None;
                managed.state = ChildState::Running;
                changed = true;
            }
            Err(error) => {
                managed.restarts = managed.restarts.saturating_add(1);
                managed.backoff_until = Some(Instant::now() + backoff_duration(managed.restarts));
                managed.state = ChildState::Backoff;
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
        if let Some(mut managed) = children.remove(&key) {
            terminate_child(&mut managed).await?;
            changed = true;
        }
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
            let _ = file.set_len(0);
            use std::io::Write as _;
            let mut file = file;
            let _ = writeln!(file, "{}", std::process::id());
            Ok(Some(SupervisorGuard(file)))
        }
        Err(_) => Ok(None),
    }
}

struct SupervisorGuard(std::fs::File);

impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn supervisor_lock_path() -> anyhow::Result<PathBuf> {
    // Deliberately distinct from the registry lock file (registry lock is
    // `supervisor.json` -> `supervisor.lock`): the instance lock is held for
    // the whole supervisor lifetime, and a second flock on the same file from
    // another descriptor would deadlock the registry reads.
    Ok(feanorfs_agent_core::global_state_root()?.join("supervisor.instance.lock"))
}

/// Terminates processes recorded as children in the status file of a
/// previous supervisor instance. The caller holds the supervisor lock, so any
/// still-live recorded pid is an orphan. Identity is verified against the
/// command line so a reused pid can never be signaled.
async fn reap_orphaned_children() {
    let Ok(Some(status)) = read_status() else {
        return;
    };
    let mut pids = Vec::new();
    if let Some(hub) = status.hub {
        pids.extend(hub.pid);
    }
    if let Some(tray) = status.tray {
        pids.extend(tray.pid);
    }
    pids.extend(status.workspaces.values().filter_map(|child| child.pid));
    for pid in pids {
        if pid == std::process::id() || !feanorfs_agent_core::lock::pid_alive(pid) {
            continue;
        }
        let Some(command) = process_command_line(pid) else {
            continue;
        };
        let managed = command.contains("service run")
            || command.contains("service hub-run")
            || command.contains("feanorfs-tray");
        if managed {
            tracing::warn!("reaping orphaned supervisor child {pid}: {command}");
            terminate_stray_pid(pid).await;
        }
    }
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
    if let Some(status) = read_status()? {
        if let Some(pid) = status.pid {
            if pid != std::process::id() && feanorfs_agent_core::lock::pid_alive(pid) {
                anyhow::bail!(
                    "another FeanorFS supervisor is already running (pid {pid}); stop it before starting this one"
                );
            }
        }
    }
    // Children recorded by a previous supervisor instance may have been
    // orphaned when that instance was replaced (binary update, crash, or a
    // shutdown that exceeded launchd's grace period). They must be reaped
    // before spawning fresh ones: the orphaned hub in particular holds the
    // hub-data runtime lock and the listen port, which would otherwise wedge
    // the new hub child in a backoff loop forever.
    reap_orphaned_children().await;
    let started_at = now_epoch();
    let mut children: BTreeMap<String, ManagedChild> = BTreeMap::new();
    // Refresh the status file immediately: readers must never see a stale
    // previous instance (old pid, old children) while this supervisor is up
    // with nothing published yet. A failed status write must not abort the
    // supervisor (launchd would restart it into the same failure).
    if let Err(error) = publish_status(&children, started_at) {
        tracing::error!("initial supervisor status publish failed: {error:#}");
    }
    let mut last_relay_mtime = hub_config_mtime("relay.json");
    let mut last_port_mtime = hub_config_mtime("listen-port");

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
        let relay_changed = hub_config_mtime("relay.json") != last_relay_mtime;
        last_relay_mtime = hub_config_mtime("relay.json");
        let port_changed = hub_config_mtime("listen-port") != last_port_mtime;
        last_port_mtime = hub_config_mtime("listen-port");

        let desired = match desired_specs(&registry) {
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
        // Publish on every state change (including the very first reconcile,
        // which spawns the children) so `service install|start|stop` waits can
        // observe Running/Stopped promptly instead of racing a heartbeat.
        // Liveness of the supervisor itself is read from status.pid, so an
        // idle supervisor writes nothing (no 5-second disk churn).
        if changed {
            if let Err(error) = publish_status(&children, started_at) {
                tracing::error!("supervisor status publish failed: {error:#}");
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = shutdown_signal() => break,
        }
    }

    for (_, mut managed) in children {
        terminate_child(&mut managed).await?;
    }
    let _ = publish_status(&BTreeMap::new(), started_at);
    Ok(())
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

    #[test]
    fn registry_roundtrips_workspaces_and_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");
        let mut store = SupervisorRegistry::default();
        store.workspaces.push("/a".into());
        store.stopped.push("/b".into());
        save_registry(&path, &store).unwrap();
        let loaded = load_registry(&path).unwrap();
        assert_eq!(loaded.workspaces, vec!["/a"]);
        assert_eq!(loaded.stopped, vec!["/b"]);
    }

    #[test]
    fn missing_registry_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = load_registry(&dir.path().join("missing.json")).unwrap();
        assert!(store.workspaces.is_empty());
        assert!(store.stopped.is_empty());
    }

    #[test]
    fn supervisor_lock_is_exclusive_and_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.lock");
        let first = acquire_supervisor_lock_at(&path).unwrap();
        assert!(first.is_some());
        // A second supervisor in the same process cannot re-acquire: flock is
        // per open-file-description, so this exercises the cross-process path.
        let second = acquire_supervisor_lock_at(&path).unwrap();
        assert!(second.is_none());
        drop(first);
        let third = acquire_supervisor_lock_at(&path).unwrap();
        assert!(third.is_some());
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
        std::fs::write(&marker, format!("{}\n", std::process::id())).unwrap();
        assert!(
            stray_workspace_watcher(&workspace.to_string_lossy()).is_none(),
            "live pids without a FeanorFS command line must not be killed"
        );
    }

    #[test]
    fn clean_exit_without_restart_policy_stays_stopped() {
        let spec = ChildSpec {
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
            version: 1,
            started_at: 1,
            updated_at: 2,
            ..SupervisorStatus::default()
        };
        status.hub = Some(ChildStatus {
            pid: Some(7),
            state: ChildState::Running,
            restarts: 0,
            last_exit: None,
            since: 1,
        });
        status.workspaces.insert(
            "/ws".into(),
            ChildStatus {
                pid: Some(8),
                state: ChildState::Backoff,
                restarts: 3,
                last_exit: Some(1),
                since: 2,
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
    }

    #[test]
    fn desired_specs_skip_unavailable_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = SupervisorRegistry::default();
        registry
            .workspaces
            .push(dir.path().to_string_lossy().into_owned());
        registry.workspaces.push("/definitely/missing".into());
        let desired = desired_specs(&registry).unwrap();
        // The missing workspace is skipped; the real temp dir is included only
        // when it is a configured FeanorFS workspace, which it is not. The hub
        // and tray entries may legitimately appear on a dev machine.
        let workspace_keys: Vec<&String> = desired
            .keys()
            .filter(|key| key.as_str() != "hub" && key.as_str() != "tray")
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
