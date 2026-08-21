//! Per-user tray instance ownership.
//!
//! The supervisor is the normal tray owner, but the same executable can also
//! be launched from Finder, a desktop entry, or a terminal. Hold one advisory
//! lock for the complete UI lifetime so every supported launch path converges
//! on a single tray process.

use fs2::FileExt as _;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

const INSTANCE_LOCK: &str = "tray-instance.lock";
const USER_QUIT_MARKER: &str = "tray-user-quit";

pub(crate) struct InstanceGuard {
    _file: File,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}

pub(crate) enum Claim {
    Primary(InstanceGuard),
    AlreadyRunning,
}

fn state_root() -> Result<PathBuf, String> {
    // Tray ownership is per desktop user, not per FeanorFS data profile.
    // FEANORFS_HOME may isolate automation state, but it must never create a
    // second tray namespace for the same signed-in user.
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(".feanorfs"))
        .ok_or_else(|| "HOME or USERPROFILE is not set".to_string())
}

fn open_private_file(path: &Path, description: &str) -> Result<File, String> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(format!(
                "tray {description} is not a regular file: {}",
                path.display()
            ));
        }
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("open tray {description} at {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("protect tray {description}: {error}"))?;
    }
    Ok(file)
}

fn claim_at(path: &Path) -> Result<Claim, String> {
    let file = open_private_file(path, "instance lock")?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Claim::Primary(InstanceGuard { _file: file })),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(Claim::AlreadyRunning)
        }
        Err(error) => Err(format!("claim tray instance lock: {error}")),
    }
}

fn protected_state_root() -> Result<PathBuf, String> {
    let root = state_root()?;
    fs::create_dir_all(&root)
        .map_err(|error| format!("create FeanorFS state directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("protect FeanorFS state directory: {error}"))?;
    }
    Ok(root)
}

pub(crate) fn claim() -> Result<Claim, String> {
    claim_at(&protected_state_root()?.join(INSTANCE_LOCK))
}

fn record_user_quit_at(path: &Path) -> Result<(), String> {
    let file = open_private_file(path, "user-quit marker")?;
    file.set_len(0)
        .map_err(|error| format!("reset tray user-quit marker: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("persist tray user-quit marker: {error}"))
}

pub(crate) fn record_user_quit() -> Result<(), String> {
    record_user_quit_at(&protected_state_root()?.join(USER_QUIT_MARKER))
}

fn take_user_quit_at(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("inspect tray user-quit marker: {error}")),
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "tray user-quit marker is not a regular file: {}",
            path.display()
        ));
    }
    fs::remove_file(path)
        .map(|()| true)
        .map_err(|error| format!("consume tray user-quit marker: {error}"))
}

pub(crate) fn take_user_quit() -> Result<bool, String> {
    take_user_quit_at(&protected_state_root()?.join(USER_QUIT_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_claim_is_live_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INSTANCE_LOCK);
        let first = match claim_at(&path).unwrap() {
            Claim::Primary(guard) => guard,
            Claim::AlreadyRunning => panic!("first tray must own the lock"),
        };
        assert!(matches!(claim_at(&path).unwrap(), Claim::AlreadyRunning));
        drop(first);
        assert!(matches!(claim_at(&path).unwrap(), Claim::Primary(_)));
    }

    #[test]
    fn user_quit_marker_is_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(USER_QUIT_MARKER);
        record_user_quit_at(&path).unwrap();
        assert!(take_user_quit_at(&path).unwrap());
        assert!(!take_user_quit_at(&path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_lock_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"").unwrap();
        let path = dir.path().join(INSTANCE_LOCK);
        symlink(&target, &path).unwrap();
        assert!(claim_at(&path).is_err());
    }
}
