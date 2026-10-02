use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STALE_SYNC_SECS: u64 = 600;

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

/// Ownership is the kernel lock, never the diagnostic PID or wall-clock age.
/// Errors other than absence fail closed. This probe never unlinks a file:
/// even an unlocked inode may already be open in another contender.
pub fn is_stale(path: &Path, _max_age_secs: u64) -> bool {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    };
    fs2::FileExt::try_lock_exclusive(&file).is_ok()
}

/// Check whether the sync lock is actively held (not stale) by another process.
///
/// The argument is an already-resolved private workspace state directory.
/// Unlike [`is_sync_lock_active`], this helper does not resolve, migrate, or
/// maintain a workspace path.
pub fn is_sync_lock_active_at_state(state: &Path) -> bool {
    let path = state.join("sync.lock");
    path.exists()
        && !held_sync_locks().contains(&lock_identity(&path))
        && !is_stale(&path, STALE_SYNC_SECS)
}

/// Sync locks this process holds. Windows locks are mandatory, so a probe
/// cannot read the holder's diagnostic PID through a second handle; whether
/// this process is the holder is recorded here instead.
static SYNC_LOCKS_HELD: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn held_sync_locks() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    SYNC_LOCKS_HELD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    file.set_len(0)?;
    writeln!(file, "{pid}\n{ts}")?;
    Ok(())
}

/// Nonblocking counterpart of durable::create_lock_acquire_exclusive.
/// Keep the inode permanently: unlink/rename can split ownership between an
/// already-open contender and a newly-created file. Stale diagnostics are
/// overwritten only after ownership is won; closing the file releases it.
pub(crate) fn try_acquire_lock_file(path: &Path, label: &str) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            write_pid_ts(&mut file)?;
            Ok(file)
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Err(lock_contention(format!(
                "another {label} is running; wait for it to finish ({})",
                path.display()
            )))
        }
        Err(error) => Err(error.into()),
    }
}

/// Cross-process and process-local sync lock in global workspace state.
///
/// Acquisitions are deliberately non-reentrant: same-PID concurrent futures
/// must serialize just like separate processes. Callers that already hold a
/// guard use an explicitly guarded internal operation instead of reacquiring.
/// The kernel releases ownership on guard drop or process exit, without unlink.
pub struct SyncLock {
    _file: File,
    path: PathBuf,
}

impl SyncLock {
    pub fn acquire(base: &Path) -> Result<Self> {
        let path = lock_path(base, "sync.lock")?;
        Ok(Self::hold(try_acquire_lock_file(&path, "sync")?, &path))
    }

    fn hold(file: File, path: &Path) -> Self {
        let path = lock_identity(path);
        held_sync_locks().push(path.clone());
        Self { _file: file, path }
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let mut held = held_sync_locks();
        if let Some(index) = held.iter().position(|path| *path == self.path) {
            held.swap_remove(index);
        }
    }
}

/// Land lock serializes concurrent `agent land` operations.
pub struct LandLock {
    _file: File,
}

impl LandLock {
    pub fn acquire(base: &Path) -> Result<Self> {
        let path = lock_path(base, "land.lock")?;
        Ok(Self {
            _file: try_acquire_lock_file(&path, "agent land")?,
        })
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
    _file: File,
}

impl DispatcherLock {
    /// Nonblocking, non-reentrant acquisition; age never revokes ownership.
    pub fn acquire(base: &Path) -> Result<Self> {
        let path = lock_path(base, "dispatcher.lock")?;
        Ok(Self {
            _file: try_acquire_lock_file(&path, "integrator dispatcher")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn current_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn live_kernel_lock_never_expires_or_depends_on_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.lock");
        let mut held = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&held).unwrap();
        // The publication window, corrupt metadata and arbitrarily old dates
        // must all preserve ownership. A separate open simulates a contender.
        for bytes in [
            String::new(),
            "garbage".to_string(),
            format!("{}\n0\n", std::process::id()),
        ] {
            held.set_len(0).unwrap();
            std::io::Seek::rewind(&mut held).unwrap();
            held.write_all(bytes.as_bytes()).unwrap();
            assert!(!is_stale(&path, 0));
            assert!(is_lock_contention(
                &try_acquire_lock_file(&path, "test").unwrap_err()
            ));
        }
        drop(held);
        assert!(is_stale(&path, u64::MAX));
        let next = try_acquire_lock_file(&path, "test").unwrap();
        assert!(!is_stale(&path, 0));
        drop(next);
        assert!(path.is_file());
    }

    #[test]
    fn unheld_diagnostics_are_reusable_even_for_live_pid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stale.lock");
        for bytes in [
            "not-a-lock".to_string(),
            format!("{}\n0\n", std::process::id()),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(is_stale(&path, 600));
            let _held = try_acquire_lock_file(&path, "test").unwrap();
            assert!(!is_stale(&path, 600));
        }
    }

    #[test]
    fn sync_and_land_are_non_reentrant_and_release_without_unlink() {
        let base = tempfile::tempdir().unwrap();
        let sync = SyncLock::acquire(base.path()).unwrap();
        assert!(is_lock_contention(
            &SyncLock::acquire(base.path()).err().unwrap()
        ));
        let land = LandLock::acquire(base.path()).unwrap();
        assert!(is_lock_contention(
            &LandLock::acquire(base.path()).err().unwrap()
        ));
        drop(sync);
        drop(land);
        assert!(lock_path(base.path(), "sync.lock").unwrap().exists());
        let _sync = SyncLock::acquire(base.path()).unwrap();
        let _land = LandLock::acquire(base.path()).unwrap();
    }

    #[test]
    fn pre_resolved_sync_lock_probe_uses_state_directory_directly() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("sync.lock");
        fs::write(&path, format!("{}\n0\n", i32::MAX)).unwrap();
        assert!(!is_sync_lock_active_at_state(state.path()));
        // A holder outside this process's sync locks is active whatever PID
        // its diagnostics name (Windows cannot even read them).
        let foreign = try_acquire_lock_file(&path, "test").unwrap();
        assert!(is_sync_lock_active_at_state(state.path()));
        drop(foreign);
        let own = SyncLock::hold(try_acquire_lock_file(&path, "test").unwrap(), &path);
        assert!(!is_sync_lock_active_at_state(state.path()));
        drop(own);
        assert!(!is_sync_lock_active_at_state(state.path()));
    }
}
