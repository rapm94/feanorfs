//! Durable state persistence and the typed atomic/private write policies.
//!
//! FeanorFS has exactly four explicit persistence policy values.
//! Every atomic or private write in the product is one of these; the policy is
//! named at the call site (or by the owning helper) instead of hiding inside
//! one magical helper:
//!
//! | Policy | Rename | Parent dir sync | Private mode | Create-new | Used by |
//! |---|---|---|---|---|---|
//! | **Atomic visibility** | yes | no | depends | no | object/manifest caches, upload registry, worktree sync output, tray status caches |
//! | **Crash-durable replacement** | yes | yes (unix) | depends | no | head/ref files, journals, engine state (`DurableJson`), resolution jobs |
//! | **Private create-new** | no (hard link) | yes (unix) | 0o600 | yes | recovery kit / bundle publication (`--no-replace`) |
//! | **Private durable replacement** | yes | yes (unix) | 0o600 | no | recovery kits (`--replace`), hub secrets, config, workspace identity/location |
//!
//! The two replacement policies are a single typed value, [`AtomicWritePolicy`],
//! consumed by [`atomic_overwrite_with_policy`] and the async engine writes in
//! [`crate::fs_util`]. The private variants are implemented by the private-file
//! helpers in each owning layer (`agent-core/src/local/private_file.rs`,
//! `agent-core/src/workspace_layout.rs::write_private`, `client/src/recovery.rs`,
//! `server/src/private_file.rs`); the server must not depend on the engine, so
//! it keeps its own equivalent private helpers (documented in
//! `server/src/private_file.rs`).
//!
//! ## Pre-commit versus post-commit error semantics
//!
//! A failure **before** the rename means the destination still holds its old
//! bytes and the caller may safely retry or roll back. A failure **after** the
//! rename (data sync, rename, or parent-directory sync) is reported as
//! [`commit_durability_is_uncertain`]: the new bytes are committed and must
//! NOT be rolled back. Callers classify with `commit_durability_is_uncertain`
//! so committed state is never discarded.
//!
//! ## Parent-directory sync and Windows
//!
//! POSIX does not guarantee that `rename` survives power loss unless the parent
//! directory is fsynced, so crash-durable policies sync the parent directory
//! after the rename. On Windows the equivalent directory-handle flush is not
//! supported by `std` (`File::open` on a directory fails), so the parent sync
//! is Unix-only and the Windows guarantee is weaker: NTFS journals directory
//! metadata, so renames are typically durable in practice, but FeanorFS does
//! not rely on it — Windows callers get atomic visibility semantics from every
//! policy.
//!
//! ## Inventory of persistence call sites
//!
//! All sites classified by secrecy / replace semantics / mode / parent sync /
//! max bytes / fault model, as of the D1 audit:
//!
//! - `agent-core/src/durable.rs::atomic_overwrite*` — public state; replace;
//!   default mode; parent-synced (crash-durable); unbounded (callers bound
//!   before serialization); pre-commit vs post-commit fault-injected.
//! - `agent-core/src/fs_util.rs::atomic_write_visible` — public; replace;
//!   default mode; **no** parent sync (atomic visibility); used for object
//!   cache, manifest cache, upload registry, and worktree sync output.
//! - `agent-core/src/fs_util.rs::atomic_write_durable` — public; replace;
//!   default mode; parent-synced (crash-durable); refs, journals, migration
//!   and resolution artifacts.
//! - `agent-core/src/local/private_file.rs::write_private_json` — **private**
//!   (0o600); replace; parent-synced on unix (private durable replacement);
//!   non-unix falls back to `durable::atomic_overwrite`; local + global config
//!   and credentials.
//! - `agent-core/src/workspace_layout.rs::write_private` — **private** (0o600);
//!   replace; parent-synced on unix (private durable replacement); workspace
//!   identity, `location`, migrated `local_state.json`, imported ignore policy,
//!   `maintenance.stamp`.
//! - `server/src/private_file.rs::atomic_private_write` — **private** (0o600);
//!   replace; parent-synced on unix; hub CA/cert/key, auth token, recovery
//!   markers and bundles (`--replace`).
//! - `server/src/private_file.rs::atomic_private_create_new` — **private**
//!   (0o600); create-new via random temp + hard link; parent-synced on unix;
//!   recovery bundle publication without replace.
//! - `client/src/recovery.rs::atomic_private_write` / `::atomic_private_create_new`
//!   — private workspace recovery kits (same two private policies as server).
//! - `client/src/recent.rs::save_store` — **private** (0o600); replace;
//!   **no** parent sync (private atomic visibility); ≤256 KiB read bound;
//!   lock-protected; loss only clears the tray switcher list.
//! - `client/src/cli/update.rs::save_update_state_at` — **private** (0o600);
//!   replace; **no** parent sync (private atomic visibility); ≤16 KiB bound;
//!   throttle cache, loss only forces an earlier re-check.
//! - `client/src/cli/hub_service.rs::save_listen_port` / `::save_hub_relay` —
//!   **private** (0o600); replace; parent-synced on unix (private durable
//!   replacement); hub listen port and relay configuration.
//! - `client/src/cli/supervisor/registry.rs::save_registry` — **private**
//!   (0o600); replace; parent-synced on unix (private durable replacement);
//!   lock protected; the supervisor registry (stop tombstones, runner
//!   records) so a lost rename cannot revive a stopped runner.
//! - `client/src/tray.rs::write_atomic` — **private** (0o600); replace; **no**
//!   parent sync (private atomic visibility, correct for a pure cache); agent
//!   cache with TTL, loss is benign and never coordination-sensitive.
//! - `agent-core/src/large_file.rs::atomic_write_destination` — public;
//!   replace; **no** parent sync (atomic visibility); large-file materialized
//!   content inside the worktree; durability via the materialization commit.
//! - `client/src/cli/util.rs::record_service_identity` — public; replace;
//!   **no** parent sync (atomic visibility); non-secret service identity
//!   marker.
//! - `agent-core/src/hub_state/store.rs` — hub state via
//!   `durable::atomic_overwrite` (crash-durable replacement).
//! - `agent-core/src/agent/{continuous,runner,runtime}.rs`, `state/durable.rs`
//!   — agent status, runner state, and `local_state.json` via
//!   `durable::atomic_overwrite*` (crash-durable replacement).
//!
//! Fault tests live in `state/tests/atomic.rs` and `fs_util.rs` tests.

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
    error
        .chain()
        .any(|cause| cause.downcast_ref::<CommitDurabilityUncertain>().is_some())
}

/// Build the typed "committed but not durable" error used after a rename whose
/// parent-directory sync failed (or was fault-injected). Callers must treat
/// this as committed data and never roll it back.
pub(crate) fn durability_uncertain(parent: &Path, detail: impl std::fmt::Display) -> anyhow::Error {
    CommitDurabilityUncertain {
        message: format!(
            "committed-but-durability-uncertain: new state written but directory sync failed for {}: {detail}",
            parent.display()
        ),
    }
    .into()
}

/// The two replacement policies for atomic writes.
///
/// The private variants (private create-new / private durable replacement) are
/// implemented by the private-file helpers in each owning layer; this type
/// types the visibility-versus-durability choice that call sites must make
/// explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicWritePolicy {
    /// Rename-only publication: readers observe the old or the new bytes,
    /// never a mix, and a crash may revert to the old directory entry. No
    /// parent-directory sync; the caller must not assume the new name
    /// survives power loss. Appropriate for caches and recomputable state.
    AtomicVisibility,
    /// Atomic visibility plus a parent-directory sync after the rename: once
    /// this returns, the new content survives power loss on POSIX. On Windows
    /// the parent sync is unavailable and the guarantee degrades to atomic
    /// visibility (see module docs). Appropriate for refs, journals, and
    /// other state that must not silently revert.
    CrashDurableReplacement,
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
/// given [`AtomicWritePolicy`] and the fault-injection semantics described in
/// the module docs.
///
/// The callback runs while the destination still contains its old bytes. Any
/// callback error drops the temporary file without committing it. This keeps
/// callers from having to allocate the complete replacement before entering
/// the atomic-write path.
pub(crate) fn atomic_overwrite_with_policy<F>(
    path: &Path,
    policy: AtomicWritePolicy,
    write: F,
) -> Result<()>
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

    if policy == AtomicWritePolicy::CrashDurableReplacement {
        if let Some(parent) = path.parent() {
            if let Ok(dir) = File::open(parent) {
                if let Err(e) = dir.sync_all() {
                    return Err(durability_uncertain(parent, e));
                }
            }
        }
    }

    Ok(())
}

/// Crash-durable replacement: stream a replacement, then sync the parent
/// directory so the rename survives power loss (POSIX).
pub(crate) fn atomic_overwrite_with<F>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(&mut AtomicWriteFile) -> Result<()>,
{
    atomic_overwrite_with_policy(path, AtomicWritePolicy::CrashDurableReplacement, write)
}

/// Crash-durable replacement of `path` with `data` (see module docs).
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
