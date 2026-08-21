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

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
struct AtomicFaults {
    fail_before_commit: bool,
    fail_after_commit: bool,
}

#[cfg(test)]
thread_local! {
    static TEST_ATOMIC_FAULTS: std::cell::RefCell<AtomicFaults> = const { std::cell::RefCell::new(AtomicFaults { fail_before_commit: false, fail_after_commit: false }) };
}

#[cfg(test)]
fn set_atomic_faults(faults: AtomicFaults) {
    TEST_ATOMIC_FAULTS.with(|f| *f.borrow_mut() = faults);
}

/// Atomic **visibility** write: `base_path/rel` is replaced via a sibling
/// temporary file and `rename` (same-filesystem rename is atomic). Readers see
/// the old or the new bytes, never a mix. The parent directory is **not**
/// synced: a crash may revert to the old entry. Use this for caches and
/// recomputable state (object/manifest caches, upload registry, worktree sync
/// output). See `crate::durable` for the full policy taxonomy.
pub async fn atomic_write_visible(base_path: &Path, rel: &str, content: &[u8]) -> Result<()> {
    atomic_write_inner(
        base_path,
        rel,
        content,
        None,
        None,
        crate::durable::AtomicWritePolicy::AtomicVisibility,
    )
    .await
}

/// Crash-durable replacement: like [`atomic_write_visible`], plus a
/// parent-directory sync after the rename so the new entry survives power loss
/// on POSIX. On Windows the parent sync is unavailable and the guarantee
/// degrades to atomic visibility (see `crate::durable` module docs). Use
/// this for refs, journals, and other state that must not silently revert.
///
/// A parent-sync failure reports the write as
/// `committed-but-durability-uncertain` (the rename already happened) so
/// callers do not roll back committed data.
pub async fn atomic_write_durable(base_path: &Path, rel: &str, content: &[u8]) -> Result<()> {
    atomic_write_inner(
        base_path,
        rel,
        content,
        None,
        None,
        crate::durable::AtomicWritePolicy::CrashDurableReplacement,
    )
    .await
}

async fn atomic_write_inner(
    base_path: &Path,
    rel: &str,
    content: &[u8],
    fail_after_bytes: Option<usize>,
    forced_temp_stem: Option<&str>,
    policy: crate::durable::AtomicWritePolicy,
) -> Result<()> {
    let dest = base_path.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }

    let tmp_dir = dest.parent().unwrap_or(base_path).to_path_buf();
    fs::create_dir_all(&tmp_dir).await?;

    let temp_stem = match forced_temp_stem {
        Some(stem) => stem.to_owned(),
        None => {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            format!(".feanorfs-tmp-{}-{sequence}", std::process::id())
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

    #[cfg(test)]
    {
        let fail = TEST_ATOMIC_FAULTS.with(|f| f.borrow().fail_before_commit);
        if fail {
            return Err(anyhow::anyhow!("injected pre-commit fault for testing"));
        }
    }

    fs::rename(&tmp_path, &dest).await?;
    temp_guard.disarm();

    #[cfg(test)]
    {
        let fail = TEST_ATOMIC_FAULTS.with(|f| f.borrow().fail_after_commit);
        if fail {
            return Err(crate::durable::durability_uncertain(
                dest.parent().unwrap_or(Path::new(".")),
                "injected post-commit fault",
            ));
        }
    }

    if policy == crate::durable::AtomicWritePolicy::CrashDurableReplacement {
        sync_parent_directory(&dest).await?;
    }
    Ok(())
}

/// Fsync the parent directory of `path` so a completed rename survives power
/// loss. Unix-only: on Windows, directory handles cannot be opened with
/// `std`, so this is a documented no-op (see `crate::durable` module docs).
async fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let display = parent.to_path_buf();
        let sync_path = display.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::File::open(&sync_path)?.sync_all()
        })
        .await
        .map_err(|join| anyhow::anyhow!("parent directory sync task failed: {join}"))?
        .map_err(|error| crate::durable::durability_uncertain(&display, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
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
    use super::{
        atomic_write_durable, atomic_write_inner, atomic_write_visible, set_atomic_faults,
        AtomicFaults,
    };
    use crate::durable::commit_durability_is_uncertain;
    use std::fs;

    fn assert_temp_dir_empty(workspace: &std::path::Path) {
        let leftovers = fs::read_dir(workspace)
            .expect("read workspace")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".feanorfs-tmp-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[tokio::test]
    async fn storage_full_after_partial_write_preserves_destination_and_cleans_temp() {
        let workspace = tempfile::tempdir().expect("create workspace");
        fs::write(workspace.path().join("file.txt"), b"original").expect("seed destination");

        let error = atomic_write_inner(
            workspace.path(),
            "file.txt",
            b"replacement",
            Some(4),
            None,
            crate::durable::AtomicWritePolicy::AtomicVisibility,
        )
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
            crate::durable::AtomicWritePolicy::AtomicVisibility,
        )
        .await
        .expect_err("simulated storage exhaustion must fail");

        assert!(!workspace.path().join("missing.txt").exists());
        assert_temp_dir_empty(workspace.path());
    }

    #[tokio::test]
    async fn temp_name_collision_never_removes_another_writers_file() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let colliding = workspace.path().join("forced-collision-0");
        fs::write(&colliding, b"other writer").expect("seed colliding temp file");

        atomic_write_inner(
            workspace.path(),
            "file.txt",
            b"replacement",
            None,
            Some("forced-collision"),
            crate::durable::AtomicWritePolicy::AtomicVisibility,
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
        assert_temp_dir_empty(workspace.path());
    }

    /// Crash/fault coverage: temp creation, data sync,
    /// rename, parent sync, and cleanup, exercised on the host platform.
    #[tokio::test]
    async fn durable_write_success_commits_and_cleans_temp() {
        let workspace = tempfile::tempdir().expect("create workspace");
        set_atomic_faults(AtomicFaults::default());

        atomic_write_durable(workspace.path(), "ref", b"committed")
            .await
            .expect("durable write should succeed");

        assert_eq!(
            fs::read(workspace.path().join("ref")).expect("read destination"),
            b"committed"
        );
        assert_temp_dir_empty(workspace.path());
    }

    /// Pre-commit fault (injected before rename): old bytes preserved, temp
    /// file cleaned, error is not a durability-uncertain post-commit error.
    #[tokio::test]
    async fn pre_commit_fault_preserves_old_bytes_and_cleans_temp() {
        let workspace = tempfile::tempdir().expect("create workspace");
        fs::write(workspace.path().join("ref"), b"original").expect("seed destination");
        set_atomic_faults(AtomicFaults {
            fail_before_commit: true,
            fail_after_commit: false,
        });

        let error = atomic_write_durable(workspace.path(), "ref", b"replacement")
            .await
            .expect_err("pre-commit fault must fail");

        assert!(error.to_string().contains("injected pre-commit fault"));
        assert!(!commit_durability_is_uncertain(&error));
        assert_eq!(
            fs::read(workspace.path().join("ref")).expect("read destination"),
            b"original"
        );
        assert_temp_dir_empty(workspace.path());
        set_atomic_faults(AtomicFaults::default());
    }

    /// Post-rename fault (injected after rename, before parent sync): the new
    /// bytes are committed, the error is durability-uncertain, temp is gone.
    #[tokio::test]
    async fn post_commit_fault_reports_uncertain_with_new_bytes() {
        let workspace = tempfile::tempdir().expect("create workspace");
        fs::write(workspace.path().join("ref"), b"original").expect("seed destination");
        set_atomic_faults(AtomicFaults {
            fail_before_commit: false,
            fail_after_commit: true,
        });

        let error = atomic_write_durable(workspace.path(), "ref", b"replacement")
            .await
            .expect_err("post-commit fault must fail");

        assert!(commit_durability_is_uncertain(&error));
        assert_eq!(
            fs::read(workspace.path().join("ref")).expect("read destination"),
            b"replacement"
        );
        assert_temp_dir_empty(workspace.path());
        set_atomic_faults(AtomicFaults::default());
    }

    /// The atomic-visibility variant is the contract used by hot-path caches:
    /// rename-only, no parent sync, and a post-rename fault still reports the
    /// commit as durability-uncertain (never rolls back committed bytes).
    #[tokio::test]
    async fn visible_write_commits_without_parent_sync() {
        let workspace = tempfile::tempdir().expect("create workspace");
        set_atomic_faults(AtomicFaults::default());

        atomic_write_visible(workspace.path(), "cache", b"cached")
            .await
            .expect("visibility write should succeed");

        assert_eq!(
            fs::read(workspace.path().join("cache")).expect("read destination"),
            b"cached"
        );
        assert_temp_dir_empty(workspace.path());
    }

    /// Rename atomicity: a reader never observes a mix of old and new bytes —
    /// a pre-commit fault leaves the complete old content at the destination.
    #[tokio::test]
    async fn rename_is_all_or_nothing_for_readers() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let large = vec![0xAB; 128 * 1024];
        fs::write(workspace.path().join("ref"), &large).expect("seed destination");
        set_atomic_faults(AtomicFaults {
            fail_before_commit: true,
            fail_after_commit: false,
        });

        let _ = atomic_write_durable(workspace.path(), "ref", b"partial").await;

        let after = fs::read(workspace.path().join("ref")).expect("read destination");
        assert_eq!(after, large);
        set_atomic_faults(AtomicFaults::default());
    }
}
