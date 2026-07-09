//! Spawn `feanorfs` subprocesses — the tray never duplicates sync logic.

use feanorfs_common::tray_contract::{RecentWorkspacesResult, TrayStatusResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn feanorfs_bin() -> String {
    std::env::var("FEANORFS_BIN").unwrap_or_else(|_| "feanorfs".into())
}

pub fn workspace_has_config(path: &Path) -> bool {
    path.join(".feanorfs").join("config.json").is_file()
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn run_in(workspace: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(feanorfs_bin())
        .args(args)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

fn run_checked(workspace: &Path, args: &[&str]) -> Result<(), String> {
    let out = run_in(workspace, args).map_err(|e| format!("failed to run feanorfs: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            format!("feanorfs exited with {}", out.status)
        } else {
            stderr
        };
        return Err(truncate_error(&msg));
    }
    Ok(())
}

fn truncate_error(msg: &str) -> String {
    const MAX_CHARS: usize = 300;
    if msg.chars().count() <= MAX_CHARS {
        return msg.to_string();
    }
    let limited: String = msg.chars().take(MAX_CHARS).collect();
    let first_line: String = limited.lines().next().unwrap_or(&limited).to_string();
    if first_line.chars().count() < limited.chars().count() {
        format!("{first_line}…")
    } else {
        format!("{limited}…")
    }
}

pub fn tray_status(workspace: &Path) -> Option<TrayStatusResult> {
    let out = run_in(workspace, &["--json", "tray", "status"]).ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

pub fn tray_pause(workspace: &Path, pause: bool) -> Result<(), String> {
    let sub = if pause { "pause" } else { "resume" };
    run_checked(workspace, &["tray", sub])
}

pub fn tray_recent() -> Option<RecentWorkspacesResult> {
    let out = run_in(&home_dir(), &["--json", "tray", "recent"]).ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

pub fn tray_activate(path: &Path) -> Result<(), String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| "workspace path is not valid UTF-8".to_string())?;
    run_checked(&home_dir(), &["tray", "activate", "--", path_str])
}

pub fn conflicts_keep(workspace: &Path, path: &str, choice: &str) -> Result<(), String> {
    let flag = match choice {
        "local" => "--local",
        "cloud" => "--cloud",
        "both" => "--both",
        _ => return Err(format!("unknown keep choice: {choice}")),
    };
    run_checked(workspace, &["conflicts", "keep", flag, "--", path])
}

pub fn agent_land(workspace: &Path, name: &str) -> Result<(), String> {
    run_checked(workspace, &["agent", "land", "--", name])
}

pub fn sync_once(workspace: &Path) -> Result<(), String> {
    run_checked(workspace, &["sync", "--no-watch"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_error_respects_char_boundary() {
        let msg = "é".repeat(400);
        let out = truncate_error(&msg);
        assert!(out.chars().count() <= 301);
    }
}
