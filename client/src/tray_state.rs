//! Pause / watch / sync indicators for the tray companion.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const PAUSED_FILE: &str = "paused";
const WATCH_PID_FILE: &str = "watch.pid";

fn feanorfs_dir(base: &Path) -> std::io::Result<std::path::PathBuf> {
    feanorfs_agent_core::ensure_workspace_state(base)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

pub fn is_paused(base: &Path) -> bool {
    feanorfs_dir(base).is_ok_and(|dir| dir.join(PAUSED_FILE).is_file())
}

pub fn set_paused(base: &Path, paused: bool) -> std::io::Result<()> {
    let dir = feanorfs_dir(base)?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(PAUSED_FILE);
    if paused {
        fs::write(path, "1")
    } else if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

fn pid_alive(pid: u32) -> bool {
    feanorfs_agent_core::lock::pid_alive(pid)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_watch_pid(base: &Path) {
    let Ok(dir) = feanorfs_dir(base) else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let pid = std::process::id();
    let content = format!("{pid}\n{}\n", now_secs());
    let _ = fs::write(dir.join(WATCH_PID_FILE), content);
}

pub fn clear_watch_pid(base: &Path) {
    if let Ok(dir) = feanorfs_dir(base) {
        let _ = fs::remove_file(dir.join(WATCH_PID_FILE));
    }
}

pub fn is_watching(base: &Path) -> bool {
    let Ok(dir) = feanorfs_dir(base) else {
        return false;
    };
    let path = dir.join(WATCH_PID_FILE);
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let mut lines = content.lines();
    let Some(pid_line) = lines.next() else {
        return false;
    };
    let Ok(pid) = pid_line.trim().parse::<u32>() else {
        return false;
    };
    if !pid_alive(pid) {
        return false;
    }
    // Stale pid file from a crash long ago — if timestamp is missing, accept pid_alive only.
    let Some(ts_line) = lines.next() else {
        return true;
    };
    let Ok(written_at) = ts_line.trim().parse::<u64>() else {
        return true;
    };
    let age = now_secs().saturating_sub(written_at);
    // If the file hasn't been refreshed in 24h but pid is alive, it may be a recycled pid.
    age < 86_400
}

pub fn is_syncing(base: &Path) -> bool {
    feanorfs_agent_core::lock::is_sync_lock_active(base)
}

#[cfg(test)]
mod tests {
    use super::{clear_watch_pid, is_watching, write_watch_pid};

    #[test]
    fn current_process_watch_marker_is_live() {
        let workspace = tempfile::tempdir().unwrap();
        write_watch_pid(workspace.path());
        assert!(is_watching(workspace.path()));
        clear_watch_pid(workspace.path());
        assert!(!is_watching(workspace.path()));
    }
}
