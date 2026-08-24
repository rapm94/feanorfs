//! Pause / watch / sync indicators for the tray companion.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const PAUSED_FILE: &str = "paused";
const WATCH_PID_FILE: &str = "watch.pid";
static WATCH_PROCESS_STARTED_AT: OnceLock<u64> = OnceLock::new();

fn feanorfs_dir(base: &Path) -> std::io::Result<std::path::PathBuf> {
    feanorfs_agent_core::ensure_workspace_state(base)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

pub(crate) fn is_paused_at_state(state: &Path) -> bool {
    state.join(PAUSED_FILE).is_file()
}

pub fn is_paused(base: &Path) -> bool {
    feanorfs_dir(base).is_ok_and(|state| is_paused_at_state(&state))
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

pub async fn pause_and_wait(base: &Path, wait: std::time::Duration) -> anyhow::Result<()> {
    set_paused(base, true)?;
    let guard = feanorfs_agent_core::lock::try_acquire_sync_lock(base, wait)
        .await
        .map_err(|error| {
            error.context("sync is paused, but an in-flight sync did not quiesce before timeout")
        })?;
    drop(guard);
    Ok(())
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

pub(crate) fn is_watching_at_state(state: &Path) -> bool {
    let path = state.join(WATCH_PID_FILE);
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

pub fn write_watch_pid(base: &Path) {
    let Ok(dir) = feanorfs_dir(base) else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let pid = std::process::id();
    let now = now_secs();
    let started_at = *WATCH_PROCESS_STARTED_AT.get_or_init(|| now);
    let content = format!("{pid}\n{now}\n{started_at}\n");
    let _ = fs::write(dir.join(WATCH_PID_FILE), content);
}

pub fn clear_watch_pid(base: &Path) {
    if let Ok(dir) = feanorfs_dir(base) {
        let _ = fs::remove_file(dir.join(WATCH_PID_FILE));
    }
}

#[cfg(test)]
mod pause_tests {
    use super::*;

    #[tokio::test]
    async fn pause_waits_for_the_in_flight_sync_lock() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("ws");
        std::fs::create_dir_all(&base).unwrap();
        let held = feanorfs_agent_core::lock::SyncLock::acquire(&base).unwrap();
        let task_base = base.clone();
        let task = tokio::spawn(async move {
            pause_and_wait(&task_base, std::time::Duration::from_secs(1)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!task.is_finished());
        drop(held);
        task.await.unwrap().unwrap();
        assert!(is_paused(&base));
    }
}

pub fn is_watching(base: &Path) -> bool {
    let Ok(state) = feanorfs_dir(base) else {
        return false;
    };
    is_watching_at_state(&state)
}

pub(crate) fn is_syncing_at_state(state: &Path) -> bool {
    feanorfs_agent_core::lock::is_sync_lock_active_at_state(state)
}

pub fn is_syncing(base: &Path) -> bool {
    feanorfs_dir(base).is_ok_and(|state| is_syncing_at_state(&state))
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
