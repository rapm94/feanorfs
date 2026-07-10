use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::LocalStateV1;
use crate::durable::{
    atomic_overwrite, create_lock_acquire_exclusive, open_lock_exclusive, open_lock_shared,
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
            let content = fs::read_to_string(&state_path).context("read existing state")?;
            LocalStateV1::from_json(&content)?;
        } else {
            let json = LocalStateV1::default().to_json()?;
            atomic_overwrite(&state_path, json.as_bytes())?;
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
        let _lock = open_lock_exclusive(&self.lock_path)?;
        let mut state = read_state_file(&self.state_path)?;
        let result = operation(&mut state)?;
        let json = state.to_json()?;
        atomic_overwrite(&self.state_path, json.as_bytes())?;
        Ok(result)
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
    let content = crate::durable::read_file_required(path)?;
    LocalStateV1::from_json(&content)
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
