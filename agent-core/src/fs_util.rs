use anyhow::Result;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;
use tokio::io::AsyncWriteExt;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempFileGuard(Option<PathBuf>);

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Read filesystem mtime as milliseconds since Unix epoch.
pub async fn file_mtime_ms(path: &Path) -> Result<i64> {
    let meta = fs::metadata(path).await?;
    Ok(meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0))
}

/// Write `content` to `base_path/rel` atomically via a temp file under
/// `.feanorfs/tmp/` and `rename` (same-filesystem rename is atomic).
pub async fn atomic_write(base_path: &Path, rel: &str, content: &[u8]) -> Result<()> {
    atomic_write_inner(base_path, rel, content, None, None).await
}

async fn atomic_write_inner(
    base_path: &Path,
    rel: &str,
    content: &[u8],
    fail_after_bytes: Option<usize>,
    forced_temp_stem: Option<&str>,
) -> Result<()> {
    let dest = base_path.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }

    let tmp_dir = base_path.join(".feanorfs/tmp");
    fs::create_dir_all(&tmp_dir).await?;

    let temp_stem = match forced_temp_stem {
        Some(stem) => stem.to_owned(),
        None => {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            format!("{}-{sequence}", std::process::id())
        }
    };
    let (tmp_path, mut temp) = create_temp_file(&tmp_dir, &temp_stem).await?;
    let mut temp_guard = TempFileGuard::new(tmp_path.clone());

    if let Some(limit) = fail_after_bytes {
        temp.write_all(&content[..limit.min(content.len())]).await?;
        return Err(io::Error::from_raw_os_error(libc::ENOSPC).into());
    }

    temp.write_all(content).await?;
    temp.flush().await?;
    temp.sync_all().await?;
    drop(temp);
    fs::rename(&tmp_path, &dest).await?;
    temp_guard.disarm();
    Ok(())
}

async fn create_temp_file(tmp_dir: &Path, stem: &str) -> Result<(PathBuf, fs::File)> {
    let mut attempt = 0_u64;
    loop {
        let path = tmp_dir.join(format!("{stem}-{attempt}"));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                attempt = attempt.wrapping_add(1);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Set or clear read-only bit on a file (placeholder safety DX-9).
pub async fn set_readonly(path: &Path, readonly: bool) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let meta = fs::metadata(path).await?;
    let mut perms = meta.permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms).await?;
    Ok(())
}

/// Applies portable executable intent while preserving non-execute permission bits.
pub async fn apply_executable_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path).await?;
        let mut permissions = metadata.permissions();
        let current = permissions.mode();
        let updated = if mode == feanorfs_common::EXECUTABLE_MODE {
            current | 0o111
        } else {
            current & !0o111
        };
        permissions.set_mode(updated);
        fs::set_permissions(path, permissions).await?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::atomic_write_inner;
    use std::fs;

    fn assert_temp_dir_empty(workspace: &std::path::Path) {
        assert_eq!(
            fs::read_dir(workspace.join(".feanorfs/tmp"))
                .expect("read temp directory")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn storage_full_after_partial_write_preserves_destination_and_cleans_temp() {
        let workspace = tempfile::tempdir().expect("create workspace");
        fs::write(workspace.path().join("file.txt"), b"original").expect("seed destination");

        let error = atomic_write_inner(workspace.path(), "file.txt", b"replacement", Some(4), None)
            .await
            .expect_err("simulated storage exhaustion must fail");

        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(libc::ENOSPC)
        );
        assert_eq!(
            fs::read(workspace.path().join("file.txt")).expect("read original destination"),
            b"original"
        );
        assert_temp_dir_empty(workspace.path());
    }

    #[tokio::test]
    async fn storage_full_does_not_create_destination() {
        let workspace = tempfile::tempdir().expect("create workspace");

        atomic_write_inner(
            workspace.path(),
            "missing.txt",
            b"replacement",
            Some(4),
            None,
        )
        .await
        .expect_err("simulated storage exhaustion must fail");

        assert!(!workspace.path().join("missing.txt").exists());
        assert_temp_dir_empty(workspace.path());
    }

    #[tokio::test]
    async fn temp_name_collision_never_removes_another_writers_file() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let tmp_dir = workspace.path().join(".feanorfs/tmp");
        fs::create_dir_all(&tmp_dir).expect("create temp directory");
        let colliding = tmp_dir.join("forced-collision-0");
        fs::write(&colliding, b"other writer").expect("seed colliding temp file");

        atomic_write_inner(
            workspace.path(),
            "file.txt",
            b"replacement",
            None,
            Some("forced-collision"),
        )
        .await
        .expect("retry with a unique temp name");

        assert_eq!(
            fs::read(&colliding).expect("read other writer temp file"),
            b"other writer"
        );
        assert_eq!(
            fs::read(workspace.path().join("file.txt")).expect("read destination"),
            b"replacement"
        );
        assert_eq!(
            fs::read_dir(&tmp_dir)
                .expect("read temp directory")
                .map(|entry| entry.expect("read temp entry").path())
                .collect::<Vec<_>>(),
            vec![colliding]
        );
    }
}
