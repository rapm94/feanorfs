use anyhow::Context as _;
use clap::Subcommand;
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::start::{finish_sync_watch, WatchMode};
use super::util::{output_json, record_service_identity, service_identity_matches};

const LABEL_PREFIX: &str = "com.feanorfs.sync";

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
    /// Run the supervised private hub
    #[command(hide = true)]
    HubRun { data_dir: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackgroundStatus {
    NotInstalled,
    Running,
    Stopped,
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

#[derive(Debug, Clone)]
struct ServiceSpec {
    workspace: PathBuf,
    program: PathBuf,
    label: String,
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
        let digest = blake3::hash(workspace.to_string_lossy().as_bytes());
        let encoded_digest = digest.to_hex();
        let suffix = &encoded_digest.as_str()[..16];
        Ok(Self {
            workspace,
            program: std::env::current_exe().context("locate the feanorfs executable")?,
            label: format!("{LABEL_PREFIX}-{suffix}"),
        })
    }

    fn worker_args(&self) -> Vec<OsString> {
        vec![
            OsString::from("service"),
            OsString::from("run"),
            self.workspace.as_os_str().to_owned(),
        ]
    }

    fn marker_path(&self) -> PathBuf {
        self.workspace.join(".feanorfs/service-program")
    }

    fn installed_program_matches(&self) -> bool {
        service_identity_matches(&self.marker_path(), &[&self.program])
    }

    fn record_installed_program(&self) -> anyhow::Result<()> {
        record_service_identity(&self.marker_path(), &[&self.program])
            .context("record automatic sync executable")
    }
}

pub async fn run(current_dir: &Path, action: ServiceAction, json: bool) -> anyhow::Result<()> {
    match action {
        ServiceAction::HubRun { data_dir } => super::hub_service::run_supervised(data_dir).await,
        ServiceAction::Run { folder } => {
            std::env::set_current_dir(&folder)
                .with_context(|| format!("open background workspace {}", folder.display()))?;
            let workspace = std::env::current_dir()?;
            finish_sync_watch(&workspace, WatchMode::Foreground).await
        }
        ServiceAction::Install { folder } => {
            let result = install_result(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            print_result(&result, json)
        }
        ServiceAction::Status { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let result = result("status", &spec, platform_status(&spec)?);
            print_result(&result, json)
        }
        ServiceAction::Start { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let status = platform_start(&spec)?;
            let result = result("start", &spec, status);
            print_result(&result, json)
        }
        ServiceAction::Stop { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let status = platform_stop(&spec)?;
            let result = result("stop", &spec, status);
            print_result(&result, json)
        }
        ServiceAction::Uninstall { folder } => {
            let spec = ServiceSpec::load(&folder.unwrap_or_else(|| current_dir.to_path_buf()))?;
            let status = platform_uninstall(&spec)?;
            let result = result("uninstall", &spec, status);
            print_result(&result, json)
        }
    }
}

fn install_result(workspace: &Path) -> anyhow::Result<ServiceResult> {
    let spec = ServiceSpec::load(workspace)?;
    let status = platform_install_and_start(&spec)?;
    let mut result = result("install", &spec, status);
    match install_tray_if_available(&spec) {
        Ok(tray) => result.tray = tray,
        Err(error) => {
            eprintln!("Warning: background sync started, but the tray could not start: {error}")
        }
    }
    Ok(result)
}

pub(crate) fn install_and_start(workspace: &Path) -> anyhow::Result<()> {
    let result = install_result(workspace)?;
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
    if platform_status(&spec)? != BackgroundStatus::Running {
        return Ok(false);
    }
    platform_stop(&spec)?;
    Ok(true)
}

pub(crate) fn restore_after_failed_start(workspace: &Path) -> anyhow::Result<()> {
    let _ = install_result(workspace)?;
    Ok(())
}

pub(crate) fn status_for_workspace(workspace: &Path) -> anyhow::Result<BackgroundStatus> {
    let spec = ServiceSpec::load(workspace)?;
    platform_status(&spec)
}

/// Stop and uninstall automatic sync for the consumer-facing `feanorfs stop` flow.
/// The workspace metadata stays in place so `feanorfs start` can resume later.
pub(crate) fn uninstall_for_workspace_stop(workspace: &Path) -> anyhow::Result<()> {
    let spec = ServiceSpec::load(workspace)?;
    let status = platform_status(&spec)?;
    let active = feanorfs_client::is_watching(&spec.workspace)
        || feanorfs_client::lock::is_sync_lock_active(&spec.workspace);
    if active && status != BackgroundStatus::Running {
        anyhow::bail!(
            "Sync is running outside the managed background service. Stop that terminal process, then retry `feanorfs stop`."
        );
    }

    let _ = platform_uninstall(&spec)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !feanorfs_client::is_watching(&spec.workspace)
            && !feanorfs_client::lock::is_sync_lock_active(&spec.workspace)
        {
            match std::fs::remove_file(spec.marker_path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("remove automatic sync identity marker"),
            }
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
        service: spec.label.clone(),
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

#[derive(Debug, Clone)]
struct TrayServiceSpec {
    program: PathBuf,
    feanorfs_program: PathBuf,
    marker: PathBuf,
}

impl TrayServiceSpec {
    fn find(spec: &ServiceSpec) -> anyhow::Result<Option<Self>> {
        let binary_name = format!("feanorfs-tray{}", std::env::consts::EXE_SUFFIX);
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("FEANORFS_TRAY_BIN") {
            candidates.push(PathBuf::from(path));
        }
        if let Some(parent) = spec.program.parent() {
            candidates.push(parent.join(&binary_name));
        }
        #[cfg(target_os = "macos")]
        candidates.push(PathBuf::from(
            "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray",
        ));
        if let Ok(path) = which::which(&binary_name) {
            candidates.push(path);
        }
        let Some(program) = candidates.into_iter().find(|path| path.is_file()) else {
            return Ok(None);
        };
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("HOME or USERPROFILE is not set")?;
        Ok(Some(Self {
            program,
            feanorfs_program: spec.program.clone(),
            marker: PathBuf::from(home).join(".feanorfs/tray-service-program"),
        }))
    }

    fn installed_programs_match(&self) -> bool {
        service_identity_matches(&self.marker, &[&self.program, &self.feanorfs_program])
    }

    fn record_installed_programs(&self) -> anyhow::Result<()> {
        record_service_identity(&self.marker, &[&self.program, &self.feanorfs_program])
            .context("record tray service executables")
    }
}

fn install_tray_if_available(spec: &ServiceSpec) -> anyhow::Result<Option<BackgroundStatus>> {
    let Some(tray) = TrayServiceSpec::find(spec)? else {
        return Ok(None);
    };
    install_tray_service(&tray).map(Some)
}

#[cfg(not(target_os = "windows"))]
fn install_tray_service(spec: &TrayServiceSpec) -> anyhow::Result<BackgroundStatus> {
    use service_manager::{
        RestartPolicy, ServiceInstallCtx, ServiceStartCtx, ServiceStatus, ServiceStatusCtx,
    };

    let manager = manager()?;
    let label: service_manager::ServiceLabel = "com.feanorfs.tray"
        .parse()
        .context("build tray service label")?;
    let status = manager
        .status(ServiceStatusCtx {
            label: label.clone(),
        })
        .context("read tray service status")?;
    let install_required = status == ServiceStatus::NotInstalled
        || !tray_service_configuration_matches(&spec.program, &spec.feanorfs_program)
        || !spec.installed_programs_match();
    if install_required {
        if status == ServiceStatus::Running {
            manager
                .stop(service_manager::ServiceStopCtx {
                    label: label.clone(),
                })
                .context("stop the previous FeanorFS tray during upgrade")?;
        }
        manager
            .install(ServiceInstallCtx {
                label: label.clone(),
                program: spec.program.clone(),
                args: Vec::new(),
                contents: None,
                username: None,
                working_directory: None,
                environment: Some(vec![(
                    "FEANORFS_BIN".into(),
                    spec.feanorfs_program.display().to_string(),
                )]),
                autostart: true,
                restart_policy: RestartPolicy::OnFailure {
                    delay_secs: None,
                    max_retries: None,
                    reset_after_secs: None,
                },
            })
            .context("install FeanorFS tray at login")?;
        spec.record_installed_programs()?;
    }
    if install_required || status != ServiceStatus::Running {
        manager
            .start(ServiceStartCtx { label })
            .context("start FeanorFS tray")?;
    }
    Ok(BackgroundStatus::Running)
}

#[cfg(target_os = "macos")]
fn tray_service_configuration_matches(tray_program: &Path, feanorfs_program: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let plist = PathBuf::from(home).join("Library/LaunchAgents/com.feanorfs.tray.plist");
    let installed_tray = launchd_plist_string(&plist, "ProgramArguments.0");
    let installed_feanorfs = launchd_plist_string(&plist, "EnvironmentVariables.FEANORFS_BIN");
    match (installed_tray, installed_feanorfs) {
        (Some(tray), Some(feanorfs)) => {
            paths_equivalent(Path::new(&tray), tray_program)
                && paths_equivalent(Path::new(&feanorfs), feanorfs_program)
        }
        _ => false,
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn tray_service_configuration_matches(_tray_program: &Path, _feanorfs_program: &Path) -> bool {
    true
}

#[cfg(any(target_os = "windows", test))]
fn windows_tray_task_action(spec: &TrayServiceSpec) -> anyhow::Result<(String, String)> {
    let program = spec.program.display().to_string();
    if program.contains('"') {
        anyhow::bail!("Windows paths containing double quotes cannot be installed as tasks");
    }
    Ok((program, String::new()))
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

#[cfg(target_os = "macos")]
fn launchd_plist_string(plist: &Path, key: &str) -> Option<String> {
    let output = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(plist)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "macos")]
fn paths_equivalent(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(not(target_os = "windows"))]
fn manager() -> anyhow::Result<Box<dyn service_manager::ServiceManager>> {
    use service_manager::{ServiceLevel, ServiceManager};
    let mut manager =
        <dyn ServiceManager>::native().context("detect operating-system service manager")?;
    manager
        .set_level(ServiceLevel::User)
        .context("select per-user service management")?;
    Ok(manager)
}

#[cfg(not(target_os = "windows"))]
fn native_label(spec: &ServiceSpec) -> anyhow::Result<service_manager::ServiceLabel> {
    spec.label.parse().context("build background service label")
}

#[cfg(target_os = "macos")]
fn launchd_plist_path(spec: &ServiceSpec) -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", spec.label)))
}

#[cfg(target_os = "macos")]
fn launchctl_plist(command: &str, spec: &ServiceSpec) -> anyhow::Result<()> {
    let plist = launchd_plist_path(spec)?;
    let output = std::process::Command::new("launchctl")
        .arg(command)
        .arg(&plist)
        .output()
        .with_context(|| format!("launchctl {command} {}", plist.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl {command}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn platform_status(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    use service_manager::{ServiceStatus, ServiceStatusCtx};
    let status = manager()?
        .status(ServiceStatusCtx {
            label: native_label(spec)?,
        })
        .context("read background service status")?;
    Ok(match status {
        #[cfg(target_os = "macos")]
        ServiceStatus::NotInstalled if launchd_plist_path(spec)?.is_file() => {
            BackgroundStatus::Stopped
        }
        ServiceStatus::NotInstalled => BackgroundStatus::NotInstalled,
        ServiceStatus::Running => BackgroundStatus::Running,
        ServiceStatus::Stopped(_) => BackgroundStatus::Stopped,
    })
}

#[cfg(target_os = "macos")]
fn start_managed_service(spec: &ServiceSpec) -> anyhow::Result<()> {
    use service_manager::{ServiceStartCtx, ServiceStatus, ServiceStatusCtx};
    let manager = manager()?;
    let label = native_label(spec)?;
    let status = manager
        .status(ServiceStatusCtx {
            label: label.clone(),
        })
        .context("read automatic sync status")?;
    if status == ServiceStatus::NotInstalled && launchd_plist_path(spec)?.is_file() {
        return launchctl_plist("load", spec).context("load automatic sync");
    }
    manager
        .start(ServiceStartCtx { label })
        .context("start automatic sync")
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn start_managed_service(spec: &ServiceSpec) -> anyhow::Result<()> {
    use service_manager::ServiceStartCtx;
    manager()?
        .start(ServiceStartCtx {
            label: native_label(spec)?,
        })
        .context("start automatic sync")
}

#[cfg(not(target_os = "windows"))]
fn platform_install_and_start(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    use service_manager::{RestartPolicy, ServiceInstallCtx, ServiceStartCtx};
    let manager = manager()?;
    let label = native_label(spec)?;
    let status = platform_status(spec)?;
    let install_required =
        status == BackgroundStatus::NotInstalled || !spec.installed_program_matches();
    if install_required {
        manager
            .install(ServiceInstallCtx {
                label: label.clone(),
                program: spec.program.clone(),
                args: spec.worker_args(),
                contents: None,
                username: None,
                working_directory: Some(spec.workspace.clone()),
                environment: None,
                autostart: true,
                restart_policy: RestartPolicy::OnFailure {
                    delay_secs: None,
                    max_retries: None,
                    reset_after_secs: None,
                },
            })
            .context("install automatic sync")?;
        spec.record_installed_program()?;
        manager
            .start(ServiceStartCtx { label })
            .context("start automatic sync")?;
    } else if status == BackgroundStatus::Stopped {
        start_managed_service(spec)?;
    }
    Ok(BackgroundStatus::Running)
}

#[cfg(not(target_os = "windows"))]
fn platform_start(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    if platform_status(spec)? == BackgroundStatus::NotInstalled {
        anyhow::bail!("Automatic sync is not installed; run `feanorfs service install`");
    }
    start_managed_service(spec)?;
    Ok(BackgroundStatus::Running)
}

#[cfg(target_os = "macos")]
fn platform_stop(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    match platform_status(spec)? {
        BackgroundStatus::NotInstalled => return Ok(BackgroundStatus::NotInstalled),
        BackgroundStatus::Stopped => return Ok(BackgroundStatus::Stopped),
        BackgroundStatus::Running => {}
    }
    launchctl_plist("unload", spec).context("stop automatic sync")?;
    wait_for_managed_service_stop(spec)
}

#[cfg(not(target_os = "windows"))]
fn wait_for_managed_service_stop(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if platform_status(spec)? != BackgroundStatus::Running
            && !feanorfs_client::lock::is_sync_lock_active(&spec.workspace)
        {
            return Ok(BackgroundStatus::Stopped);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!(
        "automatic sync did not stop within 5 seconds; retry after the current sync finishes"
    );
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn platform_stop(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    use service_manager::ServiceStopCtx;
    match platform_status(spec)? {
        BackgroundStatus::NotInstalled => return Ok(BackgroundStatus::NotInstalled),
        BackgroundStatus::Stopped => return Ok(BackgroundStatus::Stopped),
        BackgroundStatus::Running => {}
    }
    manager()?
        .stop(ServiceStopCtx {
            label: native_label(spec)?,
        })
        .context("stop automatic sync")?;
    wait_for_managed_service_stop(spec)
}

#[cfg(not(target_os = "windows"))]
fn platform_uninstall(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    use service_manager::ServiceUninstallCtx;
    if platform_status(spec)? == BackgroundStatus::NotInstalled {
        return Ok(BackgroundStatus::NotInstalled);
    }
    let _ = platform_stop(spec)?;
    manager()?
        .uninstall(ServiceUninstallCtx {
            label: native_label(spec)?,
        })
        .context("uninstall automatic sync")?;
    Ok(BackgroundStatus::NotInstalled)
}

#[cfg(target_os = "windows")]
fn task_name(spec: &ServiceSpec) -> String {
    format!("FeanorFS\\{}", spec.label)
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

#[cfg(any(target_os = "windows", test))]
fn windows_task_action(spec: &ServiceSpec) -> anyhow::Result<(String, String)> {
    let program = spec.program.display().to_string();
    let workspace = spec.workspace.display().to_string();
    if program.contains('"') || workspace.contains('"') {
        anyhow::bail!("Windows paths containing double quotes cannot be installed as tasks");
    }
    Ok((program, format!("service run \"{workspace}\"")))
}

#[cfg(target_os = "windows")]
fn schtasks(args: &[&str]) -> anyhow::Result<std::process::Output> {
    let output = std::process::Command::new("schtasks.exe")
        .args(args)
        .output()
        .context("run Windows Task Scheduler")?;
    Ok(output)
}

#[cfg(target_os = "windows")]
fn platform_status(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    windows_task_status("\\FeanorFS\\", &spec.label, &task_name(spec))
}

#[cfg(target_os = "windows")]
fn platform_install_and_start(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    if platform_status(spec)? == BackgroundStatus::NotInstalled || !spec.installed_program_matches()
    {
        let (program, arguments) = windows_task_action(spec)?;
        super::util::windows_register_task(
            "\\FeanorFS\\",
            &spec.label,
            &program,
            &arguments,
            false,
        )
        .context("install automatic sync")?;
        spec.record_installed_program()?;
    }
    platform_start(spec)
}

#[cfg(target_os = "windows")]
fn platform_start(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    if platform_status(spec)? == BackgroundStatus::NotInstalled {
        anyhow::bail!("Automatic sync is not installed; run `feanorfs service install`");
    }
    let output = schtasks(&["/Run", "/TN", &task_name(spec)])?;
    if !output.status.success() {
        anyhow::bail!(
            "start automatic sync: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(BackgroundStatus::Running)
}

#[cfg(target_os = "windows")]
fn platform_stop(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    if platform_status(spec)? == BackgroundStatus::NotInstalled {
        return Ok(BackgroundStatus::NotInstalled);
    }
    let _ = schtasks(&["/End", "/TN", &task_name(spec)]);
    Ok(BackgroundStatus::Stopped)
}

#[cfg(target_os = "windows")]
fn platform_uninstall(spec: &ServiceSpec) -> anyhow::Result<BackgroundStatus> {
    if platform_status(spec)? == BackgroundStatus::NotInstalled {
        return Ok(BackgroundStatus::NotInstalled);
    }
    let _ = platform_stop(spec)?;
    let output = schtasks(&["/Delete", "/TN", &task_name(spec), "/F"])?;
    if !output.status.success() {
        anyhow::bail!(
            "uninstall automatic sync: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(BackgroundStatus::NotInstalled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_contains_only_workspace_location() {
        let spec = ServiceSpec {
            workspace: PathBuf::from("/tmp/feanor workspace"),
            program: PathBuf::from("/usr/local/bin/feanorfs"),
            label: "com.feanorfs.sync-test".into(),
        };
        assert_eq!(
            spec.worker_args(),
            vec!["service", "run", "/tmp/feanor workspace"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        let (program, arguments) = windows_task_action(&spec).unwrap();
        assert_eq!(program, "/usr/local/bin/feanorfs");
        assert_eq!(arguments, "service run \"/tmp/feanor workspace\"");

        let tray = TrayServiceSpec {
            program: PathBuf::from("C:\\Program Files\\FeanorFS\\feanorfs-tray.exe"),
            feanorfs_program: PathBuf::from("C:\\Program Files\\FeanorFS\\feanorfs.exe"),
            marker: PathBuf::from("C:\\Users\\test\\.feanorfs\\tray-service-program"),
        };
        let (tray_program, tray_arguments) = windows_tray_task_action(&tray).unwrap();
        assert_eq!(
            tray_program,
            "C:\\Program Files\\FeanorFS\\feanorfs-tray.exe"
        );
        assert!(tray_arguments.is_empty());
        let tray_action = format!("{tray_program} {tray_arguments}");
        assert!(!tray_action.contains("token"));
        assert!(!tray_action.contains("key"));
        assert!(!tray_action.contains("invite"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_tray_fields_are_read_without_shell_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("tray.plist");
        std::fs::write(
            &plist,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>ProgramArguments</key><array><string>/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray</string></array>
<key>EnvironmentVariables</key><dict><key>FEANORFS_BIN</key><string>/usr/local/bin/feanorfs</string></dict>
</dict></plist>"#,
        )
        .unwrap();
        assert_eq!(
            launchd_plist_string(&plist, "ProgramArguments.0").as_deref(),
            Some("/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray")
        );
        assert_eq!(
            launchd_plist_string(&plist, "EnvironmentVariables.FEANORFS_BIN").as_deref(),
            Some("/usr/local/bin/feanorfs")
        );
        assert!(launchd_plist_string(&plist, "Missing").is_none());
    }
}
