//! Private persistence helpers for the hub server.
//!
//! The server must not depend on the engine, so these helpers are the server's
//! own implementations of the two private policies from the shared taxonomy
//! (documented in `agent-core/src/durable.rs`):
//!
//! - `atomic_private_write` — **private durable replacement**: mode 0o600
//!   temp file, atomic rename, parent-directory sync on Unix. On Windows the
//!   parent sync is unavailable and the guarantee degrades to atomic
//!   visibility.
//! - `atomic_private_create_new` — **private create-new**: random 0o600 temp
//!   file, data sync, then a hard link into the destination so publication
//!   fails instead of replacing an existing file; parent sync on Unix.
//! - `durable_remove_if_exists` — removal plus parent sync on Unix.
//!
//! `create_private_dir` enforces 0o700 on Unix.

use anyhow::{Context as _, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::Path;

pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn open_private_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).context("open private state lock")
}

pub(crate) fn atomic_private_write(path: &Path, content: &[u8]) -> Result<()> {
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;

    file.write_all(content)?;
    file.commit()?;
    #[cfg(unix)]
    {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

pub(crate) fn atomic_private_create_new(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).context("canonicalize destination directory")?;
    let name = path.file_name().context("destination must name a file")?;
    let destination = parent.join(name);

    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("generate temp name: {error}"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = parent.join(format!(".feanorfs-recovery-{suffix}.tmp"));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("create private recovery temporary file")?;
    let result = (|| -> Result<()> {
        file.write_all(content)?;
        file.sync_all()?;
        fs::hard_link(&temporary, &destination).with_context(|| {
            format!(
                "publish recovery bundle without replacing {}",
                destination.display()
            )
        })?;
        fs::remove_file(&temporary)?;
        #[cfg(unix)]
        File::open(&parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn durable_remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}
