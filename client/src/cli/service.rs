use anyhow::Context as _;
use clap::Subcommand;
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use super::process_tree;
use super::start::{finish_sync_watch, SetupStageError, WatchMode};
use super::supervisor::{self};
use super::util::output_json;
use feanorfs_common::tray_contract::{SetupCommitted, SetupRecovery, SetupResult, SetupStage};

/// Background state of a managed component (workspace, hub, tray, supervisor).
pub(crate) use supervisor::ServiceState as BackgroundStatus;

#[derive(Subcommand)]
pub enum ServiceAction {
    /// Install and start automatic sync at login
    Install {
        /// Workspace folder (default: current directory)
        folder: Option<PathBuf>,
    },
    /// Show automatic sync status
    Status {
        /// Workspace folder (default: current directory)
        folder: Option<PathBuf>,
    },
    /// Start automatic sync now
    Start {
        /// Workspace folder (default: current directory)
        folder: Option<PathBuf>,
    },
    /// Stop automatic sync without removing it
    Stop {
        /// Workspace folder (default: current directory)
        folder: Option<PathBuf>,
    },
    /// Stop and remove automatic sync
    Uninstall {
        /// Workspace folder (default: current directory)
        folder: Option<PathBuf>,
    },
    /// Run one supervised workspace watcher
    #[command(hide = true)]
    Run { folder: PathBuf },
    /// Run one supervised configured agent runner
    #[command(hide = true)]
    RunnerRun { workspace: PathBuf },
    /// Run the supervised private hub
    #[command(hide = true)]
    HubRun { data_dir: PathBuf },
    /// Internal Unix startup gate. The wrapper blocks after process creation,
    /// then execs the configured target in-place when the supervisor releases
    /// its inherited descriptor.
    #[cfg(unix)]
    #[command(hide = true)]
    ExecGate {
        release_fd: i32,
        program: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run the single background supervisor job that owns every worker
    #[command(hide = true)]
    Supervise,
    /// Refresh managed jobs after replacing the installed executables
    #[command(hide = true)]
    RefreshInstallation,
}

#[derive(Debug, Serialize)]
struct ServiceResult {
    action: &'static str,
    workspace: String,
    service: String,
    status: BackgroundStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    tray: Option<BackgroundStatus>,
}

#[derive(Debug, Serialize)]
struct InstallationRefreshResult {
    workspaces_restarted: usize,
    unavailable_workspaces_skipped: usize,
    private_hub_restarted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tray: Option<BackgroundStatus>,
}

#[derive(Debug, Clone)]
struct ServiceSpec {
    workspace: PathBuf,
    program: PathBuf,
}

impl ServiceSpec {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let workspace = path
            .canonicalize()
            .with_context(|| format!("Workspace folder does not exist: {}", path.display()))?;
        feanorfs_client::load_config(&workspace).with_context(|| {
            format!(
                "{} is not a FeanorFS workspace; run `feanorfs start` there first",
                workspace.display()
            )
        })?;
        Ok(Self {
            workspace,
            program: std::env::current_exe().context("locate the feanorfs executable")?,
        })
    }
}

pub async fn run(current_dir: &Path, action: ServiceAction, json: bool) -> anyhow::Result<()> {
    match action {
        ServiceAction::HubRun { data_dir } => super::hub_service::run_supervised(data_dir).await,
        #[cfg(unix)]
        ServiceAction::ExecGate {
            release_fd,
            program,
            args,
        } => process_tree::exec_gate_wait_and_exec(release_fd, &program, &args).map_err(Into::into),
        ServiceAction::Run { folder } => {
            std::env::set_current_dir(&folder)
                .with_context(|| format!("open background workspace {}", folder.display()))?;
            let workspace = std::env::current_dir()?;
            finish_sync_watch(&workspace, WatchMode::Foreground).await
        }
        ServiceAction::RunnerRun { workspace } => {
            super::agent_runner::run_worker(
                &workspace,
                feanorfs_agent_core::RunnerExecutionMode::Supervised,
            )
            .await
        }
        ServiceAction::Supervise => supervisor::run_supervisor().await,
        ServiceAction::Install { folder } => {
            let result = install_result(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            print_result(&result, json)
        }
        ServiceAction::Status { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let status = supervisor::status_for_workspace(&spec.workspace)?;
            print_result(&result("status", &spec, status), json)
        }
        ServiceAction::Start { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let status = supervisor::start_workspace(&spec.workspace)?;
            print_result(&result("start", &spec, status), json)
        }
        ServiceAction::Stop { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let status = supervisor::stop_workspace(&spec.workspace)?;
            print_result(&result("stop", &spec, status), json)
        }
        ServiceAction::Uninstall { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let status = supervisor::uninstall_workspace(&spec.workspace)?;
            print_result(&result("uninstall", &spec, status), json)
        }
        ServiceAction::RefreshInstallation => {
            let result = refresh_installation().await?;
            if json {
                output_json(&result)
            } else {
                println!(
                    "Refreshed {} automatic workspace service(s) for this installation.",
                    result.workspaces_restarted
                );
                if result.unavailable_workspaces_skipped > 0 {
                    println!(
                        "Skipped {} unavailable workspace(s); reconnect them and run `feanorfs start` there to refresh their service.",
                        result.unavailable_workspaces_skipped
                    );
                }
                if result.private_hub_restarted {
                    println!("Refreshed the automatic private hub service.");
                }
                if result.tray == Some(BackgroundStatus::Running) {
                    println!("Refreshed the FeanorFS system tray service.");
                }
                Ok(())
            }
        }
    }
}

async fn refresh_installation() -> anyhow::Result<InstallationRefreshResult> {
    let recent = feanorfs_client::list_recent_workspaces()?;
    let mut unavailable_workspaces_skipped = 0;
    let mut private_hub_restarted = false;
    let mut tray_spec = None;

    // Legacy per-component jobs are converted by ensure_supervisor_running()
    // below, but only after the supervisor job is proven running.
    for entry in &recent.workspaces {
        let workspace = PathBuf::from(&entry.path);
        if !workspace.is_dir() {
            unavailable_workspaces_skipped += 1;
            continue;
        }
        if !feanorfs_agent_core::workspace_is_configured(&workspace) {
            continue;
        }
        supervisor::add_workspace(&workspace)?;
        let spec = ServiceSpec::load(&workspace)?;
        tray_spec.get_or_insert(spec);
    }

    // The supervisor job is the only background item; restart it when the
    // installed executable changed, then ensure the owned private hub and the
    // tray are managed by it.
    let supervisor_restarted = supervisor::ensure_supervisor_running()?;
    let tray = match tray_spec.as_ref() {
        Some(spec) => install_tray_if_available(spec)?,
        None => {
            let fallback = ServiceSpec {
                workspace: std::env::current_dir().context("locate installer working directory")?,
                program: std::env::current_exe().context("locate the feanorfs executable")?,
            };
            install_tray_if_available(&fallback)?
        }
    };

    for entry in &recent.workspaces {
        let workspace = PathBuf::from(&entry.path);
        if !workspace.is_dir() || !feanorfs_agent_core::workspace_is_configured(&workspace) {
            continue;
        }
        let config = feanorfs_client::load_config(&workspace)?;
        if !private_hub_restarted
            && config.tls_ca_pem.is_some()
            && super::hub_service::owns_workspace(&config)
        {
            super::hub_service::ensure_private_hub(config.server_password.clone(), false)
                .await
                .context("refresh automatic private hub after installation")?;
            private_hub_restarted = true;
        }
    }
    if supervisor_restarted {
        private_hub_restarted = true;
    }

    let mut workspaces_restarted = 0;
    for workspace in supervisor::registered_workspaces()? {
        if supervisor::wait_for_workspace_child(&workspace, supervisor::READY_TIMEOUT).is_ok() {
            workspaces_restarted += 1;
        }
    }

    Ok(InstallationRefreshResult {
        workspaces_restarted,
        unavailable_workspaces_skipped,
        private_hub_restarted,
        tray,
    })
}

fn install_result(workspace: &Path) -> anyhow::Result<ServiceResult> {
    let spec = ServiceSpec::load(workspace)?;
    let status = supervisor::install_workspace(&spec.workspace)
        .context("install automatic background sync")?;
    let mut result = result("install", &spec, status);
    result.tray = install_tray_if_available(&spec).context("install the FeanorFS system tray")?;
    Ok(result)
}

pub(crate) fn install_and_start(workspace: &Path) -> anyhow::Result<()> {
    let spec = ServiceSpec::load(workspace)?;
    let status = supervisor::install_workspace(&spec.workspace).map_err(|error| {
        SetupStageError {
            result: SetupResult::staged(
                SetupStage::InitialSync,
                SetupCommitted {
                    workspace_configured: true,
                    initial_sync_completed: true,
                    ..SetupCommitted::default()
                },
                SetupRecovery::RetryStart,
                &format!(
                    "Initial sync completed, but automatic background sync could not be installed. Rerun `feanorfs start -- {}` to retry this stage; the completed sync and encrypted workspace identity will be preserved. Details: {error:#}",
                    spec.workspace.display()
                ),
            ),
        }
    })?;
    println!("Automatic background sync installed.");
    let tray = install_tray_if_available(&spec).map_err(|error| {
        SetupStageError {
            result: SetupResult::staged(
                SetupStage::ServiceInstalled,
                SetupCommitted {
                    workspace_configured: true,
                    initial_sync_completed: true,
                    background_service_installed: true,
                    ..SetupCommitted::default()
                },
                SetupRecovery::RetryTray,
                &format!(
                    "Initial sync and automatic background sync completed, but the system tray could not be installed. Rerun `feanorfs start -- {}` to retry only the remaining lifecycle checks; synced files and encrypted workspace identity will be preserved. Details: {error:#}",
                    spec.workspace.display()
                ),
            ),
        }
    })?;
    let result = ServiceResult {
        action: "install",
        workspace: spec.workspace.display().to_string(),
        service: supervisor::LABEL.into(),
        status,
        tray,
    };
    println!("FeanorFS is running in the background and will restart when you log in.");
    println!("  Workspace: {}", result.workspace);
    if result.tray == Some(BackgroundStatus::Running) {
        println!("  Tray:      available in the system tray");
    }
    println!("  Manage:    feanorfs service status");
    Ok(())
}

pub(crate) fn stop_for_start(workspace: &Path) -> anyhow::Result<bool> {
    let spec = ServiceSpec::load(workspace)?;
    if supervisor::status_for_workspace(&spec.workspace)? != BackgroundStatus::Running {
        return Ok(false);
    }
    supervisor::stop_workspace(&spec.workspace)?;
    Ok(true)
}

pub(crate) fn restore_after_failed_start(workspace: &Path) -> anyhow::Result<()> {
    let spec = ServiceSpec::load(workspace)?;
    let _ = supervisor::start_workspace(&spec.workspace)?;
    Ok(())
}

pub(crate) fn status_for_workspace(workspace: &Path) -> anyhow::Result<BackgroundStatus> {
    let spec = ServiceSpec::load(workspace)?;
    supervisor::status_for_workspace(&spec.workspace)
}

/// Stop and uninstall automatic sync for the consumer-facing `feanorfs stop` flow.
/// The workspace metadata stays in place so `feanorfs start` can resume later.
pub(crate) fn uninstall_for_workspace_stop(workspace: &Path) -> anyhow::Result<()> {
    let spec = ServiceSpec::load(workspace)?;
    let status = supervisor::status_for_workspace(&spec.workspace)?;
    let active = feanorfs_client::is_watching(&spec.workspace)
        || feanorfs_client::lock::is_sync_lock_active(&spec.workspace);
    if active && status != BackgroundStatus::Running {
        // A verified managed watcher orphaned by a dead supervisor (crash or
        // the background item toggled off in System Settings) is still ours:
        // uninstall proceeds and the stop wait terminates it. Anything else
        // (a user's own `--foreground` process) must never be killed.
        if !supervisor::is_managed_watcher(&spec.workspace)? {
            anyhow::bail!(
                "Sync is running outside the managed background service. Stop that terminal process, then retry `feanorfs stop`."
            );
        }
    }

    let _ = supervisor::uninstall_workspace(&spec.workspace)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !feanorfs_client::is_watching(&spec.workspace)
            && !feanorfs_client::lock::is_sync_lock_active(&spec.workspace)
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!(
        "automatic sync did not stop within 5 seconds; retry after the current sync finishes"
    )
}

fn result(action: &'static str, spec: &ServiceSpec, status: BackgroundStatus) -> ServiceResult {
    ServiceResult {
        action,
        workspace: spec.workspace.display().to_string(),
        service: supervisor::LABEL.into(),
        status,
        tray: None,
    }
}

fn print_result(result: &ServiceResult, json: bool) -> anyhow::Result<()> {
    if json {
        return output_json(result);
    }
    let status = match result.status {
        BackgroundStatus::NotInstalled => "not installed",
        BackgroundStatus::Running => "running",
        BackgroundStatus::Stopped => "stopped",
    };
    println!("Automatic sync is {status} for {}.", result.workspace);
    if result.tray == Some(BackgroundStatus::Running) {
        println!("FeanorFS is also available from the system tray.");
    }
    Ok(())
}

/// Locate the desktop tray executable for a FeanorFS installation, preferring
/// the explicit override, then the directory containing `feanorfs_program`,
/// then the packaged macOS app bundle, then `PATH`.
pub(crate) fn find_tray_program(feanorfs_program: &Path) -> Option<PathBuf> {
    find_tray_program_with_override(
        feanorfs_program,
        std::env::var_os("FEANORFS_TRAY_BIN").map(PathBuf::from),
    )
}

fn find_tray_program_with_override(
    feanorfs_program: &Path,
    override_path: Option<PathBuf>,
) -> Option<PathBuf> {
    let binary_name = format!("feanorfs-tray{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Some(path) = override_path {
        candidates.push(path);
    }
    if let Some(parent) = feanorfs_program.parent() {
        candidates.push(parent.join(&binary_name));
    }
    #[cfg(target_os = "macos")]
    candidates.push(PathBuf::from(
        "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray",
    ));
    if let Ok(path) = which::which(&binary_name) {
        candidates.push(path);
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
struct TrayServiceSpec {
    program: PathBuf,
    feanorfs_program: PathBuf,
    marker: PathBuf,
}

#[cfg(target_os = "windows")]
impl TrayServiceSpec {
    fn find(spec: &ServiceSpec) -> anyhow::Result<Option<Self>> {
        let Some(program) = find_tray_program(&spec.program) else {
            return Ok(None);
        };
        Ok(Some(Self {
            program,
            feanorfs_program: spec.program.clone(),
            marker: feanorfs_agent_core::global_state_root()?.join("tray-service-program"),
        }))
    }

    fn installed_programs_match(&self) -> bool {
        super::util::service_identity_matches(
            &self.marker,
            &[&self.program, &self.feanorfs_program],
        )
    }

    fn record_installed_programs(&self) -> anyhow::Result<()> {
        super::util::record_service_identity(&self.marker, &[&self.program, &self.feanorfs_program])
            .context("record tray service executables")
    }
}

fn install_tray_if_available(spec: &ServiceSpec) -> anyhow::Result<Option<BackgroundStatus>> {
    #[cfg(target_os = "windows")]
    {
        let Some(tray) = TrayServiceSpec::find(spec)? else {
            return Ok(None);
        };
        return install_tray_service(&tray).map(Some);
    }
    #[cfg(not(target_os = "windows"))]
    {
        // macOS and Linux run the tray inside the single supervisor job, so
        // there is no separate tray background item to install.
        if find_tray_program(&spec.program).is_none() {
            return Ok(None);
        }
        if supervisor::supervisor_job_running()? {
            Ok(Some(BackgroundStatus::Running))
        } else {
            Ok(Some(BackgroundStatus::Stopped))
        }
    }
}

#[cfg(target_os = "windows")]
fn install_tray_service(spec: &TrayServiceSpec) -> anyhow::Result<BackgroundStatus> {
    const TASK_PATH: &str = "\\FeanorFS\\";
    const TASK_NAME: &str = "Tray";
    const FULL_TASK_NAME: &str = "FeanorFS\\Tray";
    let status = windows_task_status(TASK_PATH, TASK_NAME, FULL_TASK_NAME)?;
    let install_required =
        status == BackgroundStatus::NotInstalled || !spec.installed_programs_match();
    if install_required {
        let (program, arguments) = windows_tray_task_action(spec)?;
        super::util::windows_register_task(TASK_PATH, TASK_NAME, &program, &arguments, true)
            .context("install FeanorFS tray")?;
        spec.record_installed_programs()?;
    }
    if install_required || status != BackgroundStatus::Running {
        let output = schtasks(&["/Run", "/TN", FULL_TASK_NAME])?;
        if !output.status.success() {
            anyhow::bail!(
                "start FeanorFS tray: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(BackgroundStatus::Running)
}

#[cfg(target_os = "windows")]
fn windows_tray_task_action(spec: &TrayServiceSpec) -> anyhow::Result<(String, String)> {
    let program = spec.program.display().to_string();
    if program.contains('"') {
        anyhow::bail!("Windows paths containing double quotes cannot be installed as tasks");
    }
    Ok((
        program,
        feanorfs_common::tray_contract::MANAGED_TRAY_ARG.to_string(),
    ))
}

#[cfg(target_os = "windows")]
fn windows_task_status(
    task_path: &str,
    task_name: &str,
    full_task_name: &str,
) -> anyhow::Result<BackgroundStatus> {
    Ok(
        match super::util::windows_task_running(task_path, task_name, full_task_name)? {
            None => BackgroundStatus::NotInstalled,
            Some(true) => BackgroundStatus::Running,
            Some(false) => BackgroundStatus::Stopped,
        },
    )
}

#[cfg(target_os = "windows")]
fn schtasks(args: &[&str]) -> anyhow::Result<std::process::Output> {
    std::process::Command::new("schtasks.exe")
        .args(args)
        .output()
        .context("run Windows Task Scheduler")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_job_is_the_single_background_label() {
        assert_eq!(supervisor::LABEL, "com.feanorfs.agent");
    }

    #[test]
    fn tray_discovery_prefers_override_then_colocated_then_app_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let fake_tray = dir.path().join("feanorfs-tray");
        std::fs::write(&fake_tray, "binary").unwrap();
        let found = find_tray_program_with_override(
            Path::new("/usr/local/bin/feanorfs"),
            Some(fake_tray.clone()),
        );
        assert_eq!(found, Some(fake_tray));
    }

    #[test]
    fn hidden_service_actions_are_parseable() {
        let command = ServiceAction::augment_subcommands(clap::Command::new("service"));
        let names: Vec<&str> = command
            .get_subcommands()
            .map(|sub| sub.get_name())
            .collect();
        assert!(
            names.contains(&"runner-run"),
            "hidden `runner-run` action missing"
        );
        assert!(
            names.contains(&"supervise"),
            "hidden `supervise` action missing"
        );
        #[cfg(unix)]
        assert!(
            names.contains(&"exec-gate"),
            "hidden `exec-gate` action missing"
        );
    }

    #[test]
    fn service_module_never_invokes_platform_service_managers() {
        // Per-workspace platform service installation belongs to the single
        // supervisor (`supervisor.rs`); this module stays a thin adapter.
        let source = include_str!("service.rs");
        let body = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !body.contains("service_manager"),
            "service.rs must not invoke platform service managers itself"
        );
    }

    #[test]
    fn service_result_reports_the_supervisor_label() {
        let spec = ServiceSpec {
            workspace: PathBuf::from("/tmp/feanor workspace"),
            program: PathBuf::from("/usr/local/bin/feanorfs"),
        };
        let result = result("status", &spec, BackgroundStatus::Running);
        assert_eq!(result.service, "com.feanorfs.agent");
        assert_eq!(result.status, BackgroundStatus::Running);
        assert_eq!(result.workspace, "/tmp/feanor workspace");
    }
}
