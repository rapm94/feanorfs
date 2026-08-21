//! Private persistence helpers for engine state.
//!
//! `write_private_json` is the engine's **private durable replacement**
//! policy: mode 0o600 temp file, data sync, atomic rename, then a
//! parent-directory sync on Unix so the rename survives power loss (the
//! `local_state.json` and `config.json` state files must not silently revert).
//! On non-Unix platforms it falls back to [`crate::durable::atomic_overwrite`],
//! which degrades to atomic visibility on Windows (directory sync is
//! unavailable there — see the `crate::durable` module docs).
//!
//! `create_private_dir` enforces 0o700 on Unix for the private state
//! directory so secrets written into it are not world-readable.

use anyhow::Result;
use std::fs;
use std::path::Path;

pub(super) fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn write_private_json(path: &Path, content: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        let mut file = options.open(path)?;
        file.write_all(content.as_bytes())?;
        file.commit()?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    crate::durable::atomic_overwrite(path, content.as_bytes())?;
    Ok(())
}
