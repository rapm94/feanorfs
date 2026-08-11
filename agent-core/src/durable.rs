use anyhow::{bail, Context, Result};
use atomic_write_file::AtomicWriteFile;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct CommitDurabilityUncertain {
    message: String,
}

impl std::fmt::Display for CommitDurabilityUncertain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CommitDurabilityUncertain {}

pub(crate) fn commit_durability_is_uncertain(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CommitDurabilityUncertain>().is_some()
}

fn durability_uncertain(parent: &Path, detail: impl std::fmt::Display) -> anyhow::Error {
    CommitDurabilityUncertain {
        message: format!(
            "committed-but-durability-uncertain: new state written but directory sync failed for {}: {detail}",
            parent.display()
        ),
    }
    .into()
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AtomicFaults {
    pub fail_before_commit: bool,
    pub fail_after_commit: bool,
}

#[cfg(test)]
thread_local! {
    static TEST_ATOMIC_FAULTS: std::cell::RefCell<AtomicFaults> = const { std::cell::RefCell::new(AtomicFaults { fail_before_commit: false, fail_after_commit: false }) };
}

#[cfg(test)]
pub fn set_atomic_faults(faults: AtomicFaults) {
    TEST_ATOMIC_FAULTS.with(|f| *f.borrow_mut() = faults);
}

pub fn open_lock_shared(lock_path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .open(lock_path)
        .context("open lock file for shared lock")?;
    fs2::FileExt::lock_shared(&file).context("acquire shared lock")?;
    Ok(file)
}

pub fn open_lock_exclusive(lock_path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .context("open lock file for exclusive lock")?;
    fs2::FileExt::lock_exclusive(&file).context("acquire exclusive lock")?;
    Ok(file)
}

/// Stream a replacement into an [`AtomicWriteFile`] and commit it with the
/// same durability and fault-injection semantics as [`atomic_overwrite`].
///
/// The callback runs while the destination still contains its old bytes. Any
/// callback error drops the temporary file without committing it. This keeps
/// callers from having to allocate the complete replacement before entering
/// the atomic-write path.
pub(crate) fn atomic_overwrite_with<F>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(&mut AtomicWriteFile) -> Result<()>,
{
    let mut awf = AtomicWriteFile::open(path).context("create atomic write file")?;
    write(&mut awf)?;

    #[cfg(test)]
    {
        let fail = TEST_ATOMIC_FAULTS.with(|f| f.borrow().fail_before_commit);
        if fail {
            return Err(anyhow::anyhow!("injected pre-commit fault for testing"));
        }
    }

    awf.commit().context("commit atomic write")?;

    #[cfg(debug_assertions)]
    if let Some(parent) = path.parent() {
        let marker = parent.join("test-atomic-post-commit-fault");
        if path
            .file_name()
            .is_some_and(|name| name == "local_state.json")
            && marker.exists()
        {
            let _ = fs::remove_file(marker);
            return Err(durability_uncertain(parent, "injected debug fault"));
        }
    }

    #[cfg(test)]
    {
        let fail = TEST_ATOMIC_FAULTS.with(|f| f.borrow().fail_after_commit);
        if fail {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            return Err(durability_uncertain(parent, "injected fault"));
        }
    }

    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            if let Err(e) = dir.sync_all() {
                return Err(durability_uncertain(parent, e));
            }
        }
    }

    Ok(())
}

pub fn atomic_overwrite(path: &Path, data: &[u8]) -> Result<()> {
    atomic_overwrite_with(path, |awf| {
        awf.write_all(data).context("write atomic temp file")
    })
}

pub fn create_lock_acquire_exclusive(lock_path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .context("open/create state lock file")?;
    fs2::FileExt::lock_exclusive(&file).context("acquire exclusive state lock")?;
    Ok(file)
}

pub fn read_file_required(path: &Path) -> Result<String> {
    if !path.exists() {
        bail!(
            "{} is missing — it may have been deleted. \
             Remove and re-create the workspace, or restore from backup.",
            path.display()
        );
    }
    fs::read_to_string(path).context("read state file")
}

#[derive(Debug)]
pub struct DurableJson<T: serde::de::DeserializeOwned + serde::Serialize> {
    pub state_path: PathBuf,
    pub lock_path: PathBuf,
    _marker: std::marker::PhantomData<T>,
}

impl<T: serde::de::DeserializeOwned + serde::Serialize> DurableJson<T> {
    pub fn open(fs_dir: &Path, file_name: &str, default: T) -> Result<Self> {
        fs::create_dir_all(fs_dir).context("create directory")?;

        let state_path = fs_dir.join(file_name);
        let lock_path = fs_dir.join(format!("{file_name}.lock"));

        let _lock = create_lock_acquire_exclusive(&lock_path)?;

        if state_path.exists() {
            let content = read_file_required(&state_path)?;
            let _state: T = serde_json::from_str(&content).context("parse state JSON")?;
        } else {
            let json = serde_json::to_string_pretty(&default).context("serialize default state")?;
            atomic_overwrite(&state_path, json.as_bytes())?;
        }

        Ok(Self {
            state_path,
            lock_path,
            _marker: std::marker::PhantomData,
        })
    }

    pub fn with_read<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&T) -> Result<R>,
    {
        let _lock = open_lock_shared(&self.lock_path)?;
        let content = read_file_required(&self.state_path)?;
        let state: T = serde_json::from_str(&content).context("deserialize state")?;
        f(&state)
    }

    pub fn with_write<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut T) -> Result<R>,
    {
        let _lock = open_lock_exclusive(&self.lock_path)?;
        let content = read_file_required(&self.state_path)?;
        let mut state: T = serde_json::from_str(&content).context("deserialize state")?;
        let result = f(&mut state)?;
        let json = serde_json::to_string_pretty(&state).context("serialize state")?;
        atomic_overwrite(&self.state_path, json.as_bytes())?;
        Ok(result)
    }
}
