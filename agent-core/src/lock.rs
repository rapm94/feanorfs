use anyhow::{bail, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STALE_SYNC_SECS: u64 = 600;
const STALE_LAND_SECS: u64 = 600;

fn lock_path(base: &Path, name: &str) -> PathBuf {
    base.join(".feanorfs").join(name)
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
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
    now.saturating_sub(ts) > max_age_secs
}

/// Check whether the sync lock is actively held (not stale) by another process.
pub fn is_sync_lock_active(base: &Path) -> bool {
    let path = lock_path(base, "sync.lock");
    if !path.exists() || is_stale(&path, STALE_SYNC_SECS) {
        return false;
    }
    read_lock_meta(&path).is_some_and(|(pid, _)| pid != std::process::id())
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

/// Process-wide sync lock (`.feanorfs/sync.lock`). Re-entrant for the owning pid.
pub struct SyncLock {
    path: Option<PathBuf>,
    _file: File,
}

impl SyncLock {
    pub fn acquire(base: &Path) -> Result<Self> {
        let dir = base.join(".feanorfs");
        std::fs::create_dir_all(&dir)?;
        let path = lock_path(base, "sync.lock");
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
                bail!(
                    "another sync is running on this folder; wait or remove {}",
                    path.display()
                )
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
        let dir = base.join(".feanorfs");
        std::fs::create_dir_all(&dir)?;
        let path = lock_path(base, "land.lock");

        break_stale(&path, STALE_LAND_SECS, "agent land");

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        match opts.open(&path) {
            Ok(mut file) => {
                write_pid_ts(&mut file)?;
                Ok(Self { path, _file: file })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!(
                    "another agent land is in progress; wait or remove {}",
                    path.display()
                )
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
