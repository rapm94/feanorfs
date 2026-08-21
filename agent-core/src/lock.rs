use anyhow::{bail, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STALE_SYNC_SECS: u64 = 600;
const STALE_LAND_SECS: u64 = 600;

/// Typed marker for an otherwise healthy operation that lost a non-blocking
/// workspace lock race. Callers may preserve arbitrary context around this
/// error and still classify the condition without inspecting rendered text.
#[derive(Debug)]
pub struct LockContentionError {
    message: String,
}

impl std::fmt::Display for LockContentionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LockContentionError {}

fn lock_contention(message: String) -> anyhow::Error {
    LockContentionError { message }.into()
}

/// Returns true when any cause in an anyhow chain is typed lock contention.
pub fn is_lock_contention(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<LockContentionError>().is_some())
}

fn lock_path(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(crate::workspace_layout::ensure_workspace_state(base)?.join(name))
}

pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let queried = GetExitCodeProcess(handle, &mut exit_code) != 0;
        let _ = CloseHandle(handle);
        queried && exit_code == STILL_ACTIVE as u32
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn read_lock_meta(path: &Path) -> Option<(u32, u64)> {
    let mut file = File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    let mut lines = buf.lines();
    let pid: u32 = lines.next()?.parse().ok()?;
    let ts: u64 = lines.next()?.parse().ok()?;
    Some((pid, ts))
}

/// Locks owned by a live process are never stale within this window. The age
/// bound at call sites only guards against PID reuse after a crash; breaking a
/// live process's lock would let a second sync run concurrently with a
/// long-running chunked upload (which legitimately exceeds 10 minutes).
const LIVE_PID_STALE_GRACE_SECS: u64 = 24 * 60 * 60;

pub fn is_stale(path: &Path, max_age_secs: u64) -> bool {
    let Some((pid, ts)) = read_lock_meta(path) else {
        return true;
    };
    if !pid_alive(pid) {
        return true;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(ts) > max_age_secs.max(LIVE_PID_STALE_GRACE_SECS)
}

/// Check whether the sync lock is actively held (not stale) by another process.
///
/// The argument is an already-resolved private workspace state directory.
/// Unlike [`is_sync_lock_active`], this helper does not resolve, migrate, or
/// maintain a workspace path.
pub fn is_sync_lock_active_at_state(state: &Path) -> bool {
    let path = state.join("sync.lock");
    if !path.exists() || is_stale(&path, STALE_SYNC_SECS) {
        return false;
    }
    read_lock_meta(&path).is_some_and(|(pid, _)| pid != std::process::id())
}

pub fn is_sync_lock_active(base: &Path) -> bool {
    let Ok(state) = crate::workspace_layout::ensure_workspace_state(base) else {
        return false;
    };
    is_sync_lock_active_at_state(&state)
}

fn write_pid_ts(file: &mut File) -> Result<()> {
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writeln!(file, "{pid}\n{ts}")?;
    Ok(())
}

fn break_stale(path: &Path, max_age_secs: u64, label: &str) {
    if path.exists() && is_stale(path, max_age_secs) {
        tracing::warn!("Breaking stale {label} lock at {}", path.display());
        let _ = std::fs::remove_file(path);
    }
}

/// Cross-process and process-local sync lock in global workspace state.
///
/// Acquisitions are deliberately non-reentrant: same-PID concurrent futures
/// must serialize just like separate processes. Callers that already hold a
/// guard use an explicitly guarded internal operation instead of reacquiring.
pub struct SyncLock {
    path: Option<PathBuf>,
    _file: File,
}

impl SyncLock {
    pub fn acquire(base: &Path) -> Result<Self> {
        let dir = crate::workspace_layout::ensure_workspace_state(base)?;
        std::fs::create_dir_all(&dir)?;
        let path = lock_path(base, "sync.lock")?;
        break_stale(&path, STALE_SYNC_SECS, "sync");

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        match opts.open(&path) {
            Ok(mut file) => {
                write_pid_ts(&mut file)?;
                Ok(Self {
                    path: Some(path),
                    _file: file,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(lock_contention(format!(
                    "another sync is running on this folder; wait or remove {}",
                    path.display()
                )))
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        if let Some(ref path) = self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Land lock serializes concurrent `agent land` operations.
pub struct LandLock {
    path: PathBuf,
    _file: File,
}

impl LandLock {
    pub fn acquire(base: &Path) -> Result<Self> {
        let dir = crate::workspace_layout::ensure_workspace_state(base)?;
        std::fs::create_dir_all(&dir)?;
        let path = lock_path(base, "land.lock")?;

        break_stale(&path, STALE_LAND_SECS, "agent land");

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        match opts.open(&path) {
            Ok(mut file) => {
                write_pid_ts(&mut file)?;
                Ok(Self { path, _file: file })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(lock_contention(format!(
                    "another agent land is in progress; wait or remove {}",
                    path.display()
                )))
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for LandLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Brief wait for sync lock (watch loop).
pub async fn try_acquire_sync_lock(base: &Path, wait: Duration) -> Result<SyncLock> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match SyncLock::acquire(base) {
            Ok(g) => return Ok(g),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Orchestrator dispatcher lock serializes `agent integrator` operations and
/// makes a second dispatcher fail closed on the workspace orchestration lock.
pub struct DispatcherLock {
    path: Option<PathBuf>,
    _file: File,
}

impl DispatcherLock {
    /// Acquire the per-workspace dispatcher lock; fails when another
    /// dispatcher process holds it (stale locks are broken after 10 minutes).
    pub fn acquire(base: &Path) -> Result<Self> {
        const STALE_DISPATCHER_SECS: u64 = 600;
        let dir = crate::workspace_layout::ensure_workspace_state(base)?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("dispatcher.lock");
        let self_pid = std::process::id();

        if let Some((pid, _)) = read_lock_meta(&path) {
            if pid == self_pid {
                let file = File::open(&path)?;
                return Ok(Self {
                    path: None,
                    _file: file,
                });
            }
        }

        break_stale(&path, STALE_DISPATCHER_SECS, "dispatcher");

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        match opts.open(&path) {
            Ok(mut file) => {
                write_pid_ts(&mut file)?;
                Ok(Self {
                    path: Some(path),
                    _file: file,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!(
                    "another integrator dispatcher is active for this workspace;                      one dispatcher per batch is required (or remove {})",
                    path.display()
                )
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for DispatcherLock {
    fn drop(&mut self) {
        if let Some(ref path) = self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_stale, is_sync_lock_active_at_state, pid_alive};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn current_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn lock_of_live_process_is_not_stale_within_grace() {
        let directory = tempfile::tempdir().unwrap();
        let path: PathBuf = directory.path().join("live.lock");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // A lock older than the call-site cap (600s) but held by a live
        // process must not be treated as stale.
        fs::write(&path, format!("{}\n{}\n", std::process::id(), now - 3600)).unwrap();
        assert!(!is_stale(&path, 600));

        // A dead pid is stale immediately, regardless of age. i32::MAX maps
        // to a positive pid that cannot exist (pid_max is ~4 million), so
        // kill(pid, 0) returns ESRCH instead of special -1/group semantics.
        fs::write(&path, format!("{}\n{}\n", i32::MAX, now)).unwrap();
        assert!(is_stale(&path, 600));
    }

    #[test]
    fn unparsable_lock_is_stale() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("garbage.lock");
        fs::write(&path, b"not-a-lock").unwrap();
        assert!(is_stale(&path, 600));
    }

    #[test]
    fn pre_resolved_sync_lock_probe_uses_state_directory_directly() {
        let state = tempfile::tempdir().unwrap();
        fs::write(
            state.path().join("sync.lock"),
            format!("{}\n{}\n", i32::MAX, 0),
        )
        .unwrap();

        // The helper receives the private state directory itself. It must not
        // reinterpret it as a project path and run workspace migration.
        assert!(!is_sync_lock_active_at_state(state.path()));
    }
}
