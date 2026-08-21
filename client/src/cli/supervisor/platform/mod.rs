//! Platform-specific job and process adapters.
//!
//! Unix uses `ps`/`proc` probes and dedicated process groups; Windows uses
//! kernel executable identities and Job Objects via
//! [`crate::cli::process_tree`]. Every stray/legacy PID signal is gated on
//! exact native identity checks so a recycled PID is never signaled.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use crate::cli::process_tree;

use super::*;

#[cfg(unix)]
pub(super) fn process_command_line(pid: u32) -> Option<String> {
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
pub(super) fn process_executable(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
pub(super) fn process_executable(pid: u32) -> Option<PathBuf> {
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
pub(super) fn process_executable(pid: u32) -> Option<PathBuf> {
    process_tree::executable_path(pid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(super) fn process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

/// Non-Unix platforms get no identity probe; conservatively never report a
/// stray (the sync lock still serializes watchers, so this is safe).
#[cfg(all(not(unix), not(target_os = "windows")))]
pub(super) fn process_command_line(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
pub(super) fn process_command_line(_pid: u32) -> Option<String> {
    // Windows legacy cleanup deliberately relies on the exact creation token
    // plus kernel executable image. Command-line retrieval has no ownership
    // value here and is therefore not used as a speculative PID signal.
    None
}

#[cfg(all(not(unix), not(target_os = "windows")))]
pub(super) fn process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
pub(super) fn parse_process_elapsed(value: &str) -> Option<u64> {
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
pub(super) fn process_start_epoch(pid: u32) -> Option<u64> {
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
pub(super) fn process_start_epoch(_pid: u32) -> Option<u64> {
    None
}

pub(super) fn runner_process_start_matches(
    metadata: &feanorfs_agent_core::RunnerProcessMetadata,
) -> bool {
    process_tree::process_start_matches(metadata.pid, &metadata.process_start_id)
}

#[cfg(unix)]
pub(super) fn capture_owned_identity(pid: u32) -> Option<process_tree::ProcessIdentity> {
    #[cfg(test)]
    if TEST_IDENTITY_UNAVAILABLE.load(AtomicOrdering::Acquire) {
        return None;
    }
    process_tree::ProcessIdentity::capture(pid)
}

#[cfg(unix)]
#[cfg(test)]
pub(super) fn runner_process_group_exists(pid: u32) -> bool {
    process_tree::ProcessGroup::for_child(pid).exists()
}

#[cfg(unix)]
pub(super) fn terminate_verified_runner_group(
    metadata: &feanorfs_agent_core::RunnerProcessMetadata,
) -> bool {
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
pub(super) fn terminate_verified_runner_group(
    metadata: &feanorfs_agent_core::RunnerProcessMetadata,
) -> bool {
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
pub(super) fn terminate_verified_runner_group(
    _metadata: &feanorfs_agent_core::RunnerProcessMetadata,
) -> bool {
    false
}

pub(super) fn cleanup_residual_runner_group(workspace: &Path) -> anyhow::Result<()> {
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
