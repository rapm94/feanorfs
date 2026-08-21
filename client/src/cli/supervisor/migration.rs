//! Legacy per-component background job discovery and removal.
//!
//! Older installs registered one OS job per workspace (`com.feanorfs.sync-*`),
//! plus separate hub and tray jobs. `migrate_legacy_jobs` uninstalls every
//! legacy job and sweeps their identity markers so the single supervisor job
//! is the only background item.

use anyhow::Context as _;
use std::fs;
use std::path::{Path, PathBuf};

use super::*;

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
pub(super) fn plist_program_argument(plist: &Path, index: usize) -> Option<String> {
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
