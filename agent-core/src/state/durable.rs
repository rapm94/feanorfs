use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::{LocalStateV1, MAX_LOCAL_STATE_BYTES};
use crate::durable::{
    atomic_overwrite_with, create_lock_acquire_exclusive, open_lock_exclusive, open_lock_shared,
};

/// Cross-process locked, crash-safe JSON state persistence.
///
/// Each mutable operation follows: acquire exclusive lock → reload → mutate → commit.
/// Read operations use a shared lock.
#[derive(Debug)]
pub struct DurableState {
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl DurableState {
    pub fn new(fs_dir: &Path) -> Result<Self> {
        fs::create_dir_all(fs_dir).context("create feanorfs directory")?;
        let state_path = fs_dir.join("local_state.json");
        let lock_path = fs_dir.join("local_state.lock");
        let lock_file = create_lock_acquire_exclusive(&lock_path)?;

        if state_path.exists() {
            let content = read_local_state_text(&state_path)?;
            LocalStateV1::from_json(&content)?;
        } else {
            let state = LocalStateV1::default();
            atomic_overwrite_with(&state_path, |file| state.write_json(file).map(|_| ()))?;
        }

        drop(lock_file);
        Ok(Self {
            state_path,
            lock_path,
        })
    }

    /// Perform a read-only operation under a shared lock.
    pub fn with_read<F, T>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&LocalStateV1) -> Result<T>,
    {
        let lock = open_lock_shared(&self.lock_path)?;
        let state = read_state_file(&self.state_path)?;
        let result = operation(&state);
        drop(lock);
        result
    }

    /// Perform a mutating operation under an exclusive lock and commit it atomically.
    pub fn with_write<F, T>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&mut LocalStateV1) -> Result<T>,
    {
        self.with_write_inner(MAX_LOCAL_STATE_BYTES, operation)
    }

    fn with_write_inner<F, T>(&self, max_bytes: usize, operation: F) -> Result<T>
    where
        F: FnOnce(&mut LocalStateV1) -> Result<T>,
    {
        let _lock = open_lock_exclusive(&self.lock_path)?;
        let mut state = read_state_file(&self.state_path)?;
        let result = operation(&mut state)?;
        atomic_overwrite_with(&self.state_path, |file| {
            state.write_json_with_limit(file, max_bytes).map(|_| ())
        })?;
        Ok(result)
    }

    #[cfg(test)]
    pub(crate) fn with_write_limit_for_test<F, T>(
        &self,
        max_bytes: usize,
        operation: F,
    ) -> Result<T>
    where
        F: FnOnce(&mut LocalStateV1) -> Result<T>,
    {
        self.with_write_inner(max_bytes, operation)
    }

    #[cfg(test)]
    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }

    #[cfg(test)]
    pub(crate) fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

fn read_state_file(path: &Path) -> Result<LocalStateV1> {
    let content = read_local_state_text(path)?;
    LocalStateV1::from_json(&content)
}

pub(crate) fn read_local_state_text(path: &Path) -> Result<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{} is missing — local state is corrupt", path.display())
        }
        Err(error) => return Err(error).context("inspect local state"),
    };
    if metadata.len() > MAX_LOCAL_STATE_BYTES as u64 {
        bail!("local_state.json exceeds {MAX_LOCAL_STATE_BYTES} byte limit");
    }
    let file = fs::File::open(path).context("open local state")?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_LOCAL_STATE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .context("read local state")?;
    if bytes.len() > MAX_LOCAL_STATE_BYTES {
        bail!("local_state.json exceeds {MAX_LOCAL_STATE_BYTES} byte limit");
    }
    String::from_utf8(bytes).context("local_state.json is not UTF-8")
}

/// Reject any live `local_cache.db`; migration must run first.
pub fn check_no_legacy_db(fs_dir: &Path) -> Result<()> {
    let db_path = fs_dir.join("local_cache.db");
    if db_path.exists() {
        bail!(
            "Legacy SQLite database found at {}. \
             Run 'feanorfs migrate' from the workspace root to convert it.",
            db_path.display()
        );
    }
    Ok(())
}
