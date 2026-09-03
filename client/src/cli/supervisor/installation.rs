//! Launchd/systemd/Task Scheduler installation of the single supervisor job.
//!
//! Exactly one per-user OS background job runs `feanorfs service supervise`.
//! `ensure_supervisor_running` installs (or reinstalls after an executable
//! upgrade) that job, records the installed executable identity marker, and
//! migrates legacy per-component jobs once the supervisor is proven running.

#[cfg(debug_assertions)]
use anyhow::ensure;
use anyhow::Context as _;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::util::{record_service_identity, service_identity_matches};

use super::*;

/// Native service managers may throttle a job that just failed. In
/// particular, launchd can keep a repaired job in its penalty box longer than
/// the five seconds allowed for ordinary supervisor-owned child startup.
/// Keep this wait bounded, but give the OS-level job its own recovery budget.
const SUPERVISOR_JOB_READY_TIMEOUT: Duration = Duration::from_secs(15);

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
            match crate::cli::util::windows_task_running(
                "\\FeanorFS\\",
                "Agent",
                "FeanorFS\\Agent",
            )? {
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
    let mut environment: Vec<(String, String)> = crate::cli::service::find_tray_program(program)
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
pub(super) fn manager() -> anyhow::Result<Box<dyn service_manager::ServiceManager>> {
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
    crate::cli::util::windows_register_task(
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
pub(super) fn schtasks(args: &[&str]) -> anyhow::Result<std::process::Output> {
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
    let deadline = Instant::now() + SUPERVISOR_JOB_READY_TIMEOUT;
    while Instant::now() < deadline {
        if supervisor_job_state()? == ServiceState::Running {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!(
        "the FeanorFS background job did not reach the running state within {} seconds; check `feanorfs doctor` and retry",
        SUPERVISOR_JOB_READY_TIMEOUT.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_job_recovery_budget_exceeds_child_startup_budget() {
        assert!(SUPERVISOR_JOB_READY_TIMEOUT > READY_TIMEOUT);
        assert!(SUPERVISOR_JOB_READY_TIMEOUT <= Duration::from_secs(30));
    }
}
