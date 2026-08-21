//! Crash-safe workspace-state identity index, full-lifetime per-slot state
//! leases, and authenticated tombstone retirement.
//!
//! Three independent guarantees live here:
//!
//! 1. **Identity index.** `~/.feanorfs/workspaces/.identity-index.json` maps a
//!    stored workspace identity and its last authenticated canonical location
//!    to the state slot. Identity lookup replaces the bounded O(N)
//!    moved-workspace scan; location lookup lets explicit retirement find a
//!    relocated slot after the folder is gone. Files inside the slot stay
//!    authoritative, stale entries fall back to a bounded ambiguity-checking
//!    scan, and the next successful `ensure` repairs the cache.
//!
//! 2. **Full-lifetime state leases.** Every process that resolves a workspace
//!    state slot holds a shared advisory lock (`fs2`, flock/LockFileEx) on a
//!    per-slot lease file for its entire lifetime; the kernel releases it on
//!    process death, so there is no pid bookkeeping and no ABA window.
//!    Mutations that move or delete slot bytes — path-hash migration,
//!    quarantine, retirement — must win an exclusive lock first. The dance
//!    drops the caller's own shared hold, tries the exclusive lock, and
//!    restores the shared hold on contention, so any other live process makes
//!    the mutation fail closed instead of racing it.
//!
//! 3. **Authenticated tombstone retirement.** State is only ever quarantined
//!    or deleted after an explicit tombstone request that records the slot's
//!    identity and canonical path. Tombstoned slots stay in place for a grace
//!    period, then move to `quarantine/` for a further retention window, and
//!    are deleted only after the folder is re-verified gone (or non-matching)
//!    under an exclusive lease. Nothing is ever deleted because of age, a
//!    missing location, registry absence, or a temporary-looking name.

use crate::workspace_layout::{
    canonical_workspace, current_workspace_identity, private_dir, read_workspace_identity,
    unique_child, workspace_state_id, write_private, WorkspaceIdentity, MAX_WORKSPACE_STATE_SLOTS,
};
use anyhow::{bail, Context as _, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const IDENTITY_INDEX_FILE: &str = ".identity-index.json";
pub(crate) const PROVENANCE_FILE: &str = "provenance.json";
pub(crate) const TOMBSTONE_FILE: &str = "tombstone.json";
pub const DEFAULT_RETIRE_GRACE_SECS: u64 = 7 * 24 * 60 * 60;
/// Default retention inside `quarantine/` before verified deletion.
const DEFAULT_QUARANTINE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const STATE_LEASE_SUFFIX: &str = ".lease";
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// Opportunistic retirement sweep gate: one global sweep at most this often
/// per process, so routine `ensure` calls stay O(1).
pub(crate) const RETIREMENT_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

fn valid_workspace_state_slot(slot: &str) -> bool {
    slot.len() == 64 && slot.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default()
}

fn quarantine_retention() -> Duration {
    std::env::var("FEANORFS_RETIRE_QUARANTINE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_QUARANTINE_RETENTION)
}

fn read_bounded(path: &Path, max_bytes: u64, label: &str) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => bail!("{label} is not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {label}")),
    };
    if metadata.len() > max_bytes {
        bail!("{label} exceeds bounded size");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .with_context(|| format!("open {label}"))?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label}"))?;
    if bytes.len() as u64 > max_bytes {
        bail!("{label} exceeds bounded size");
    }
    Ok(Some(bytes))
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<Option<T>> {
    let Some(bytes) = read_bounded(path, MAX_RECORD_BYTES, label)? else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<T>(&bytes)
        .with_context(|| format!("parse {label} at {}", path.display()))?;
    Ok(Some(record))
}

fn write_bounded_json<T: Serialize>(path: &Path, record: &T, label: &str) -> Result<()> {
    let bytes = serde_json::to_vec(record).with_context(|| format!("encode {label}"))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        bail!("{label} exceeds bounded size");
    }
    write_private(path, &bytes)
}

// Full-lifetime per-slot state leases

/// Shared lease files, held for the lifetime of this process. Keyed by the
/// global root and slot name so tests and `FEANORFS_HOME` overrides keep
/// independent leases. The open file descriptors are intentionally never
/// closed: the kernel releases the advisory lock when the process exits, and
/// no pid-based reclamation is ever needed.
static SHARED_STATE_LEASES: std::sync::LazyLock<Mutex<BTreeMap<(PathBuf, String), File>>> =
    std::sync::LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn shared_leases() -> &'static Mutex<BTreeMap<(PathBuf, String), File>> {
    &SHARED_STATE_LEASES
}

fn lease_file(workspaces: &Path, slot: &str) -> PathBuf {
    workspaces.join(format!(".{slot}{STATE_LEASE_SUFFIX}"))
}

fn open_lease_file(workspaces: &Path, slot: &str) -> Result<File> {
    private_dir(workspaces)?;
    let path = lease_file(workspaces, slot);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_CLOEXEC);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("open workspace-state lease for slot {slot}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Hold a shared workspace-state lease for `slot` for the rest of this
/// process's lifetime. Idempotent per (root, slot). Shared leases never
/// conflict with each other; an exclusive holder (a migration, quarantine, or
/// retirement) makes this block until it releases.
pub(crate) fn ensure_shared_state_lease(root: &Path, slot: &str) -> Result<()> {
    let mut leases = shared_leases()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (root.to_path_buf(), slot.to_string());
    let entry = leases.entry(key);
    if let std::collections::btree_map::Entry::Vacant(vacant) = entry {
        let file = open_lease_file(&root.join("workspaces"), slot)?;
        FileExt::lock_shared(&file)
            .with_context(|| format!("hold shared workspace-state lease for slot {slot}"))?;
        vacant.insert(file);
    }
    Ok(())
}

/// A successful exclusive workspace-state lease. The caller may move or delete
/// the slot's bytes while this guard lives. Acquiring drops this process's own
/// shared hold first (advisory locks cannot be upgraded), so any other live
/// holder in another process makes acquisition fail closed; this process's
/// shared hold is restored after both successful and failed attempts.
#[derive(Debug)]
pub(crate) struct ExclusiveStateLease {
    file: Option<File>,
    root: PathBuf,
    slot: String,
    restore_shared: bool,
}

impl ExclusiveStateLease {
    pub(crate) fn try_acquire(root: &Path, slot: &str) -> Result<Self> {
        let key = (root.to_path_buf(), slot.to_string());
        let restore_shared = shared_leases()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key)
            .is_some();
        let file = match open_lease_file(&root.join("workspaces"), slot) {
            Ok(file) => file,
            Err(error) => {
                if restore_shared {
                    ensure_shared_state_lease(root, slot).context(
                        "restore shared workspace-state lease after failed exclusive open",
                    )?;
                }
                return Err(error);
            }
        };
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self {
                file: Some(file),
                root: root.to_path_buf(),
                slot: slot.to_string(),
                restore_shared,
            }),
            Err(error) => {
                drop(file);
                if restore_shared {
                    ensure_shared_state_lease(root, slot).with_context(|| {
                        format!(
                            "restore shared workspace-state lease for slot {slot} after contention"
                        )
                    })?;
                }
                Err(error).with_context(|| {
                    format!("workspace-state slot {slot} is held by other live processes")
                })
            }
        }
    }
}

impl Drop for ExclusiveStateLease {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        drop(file);
        if self.restore_shared {
            if let Err(restore) = ensure_shared_state_lease(&self.root, &self.slot) {
                tracing::warn!(
                    "could not restore shared workspace-state lease for slot {}: {restore:#}",
                    self.slot
                );
            }
        }
    }
}

// Crash-safe identity index

#[derive(Serialize, Deserialize, Default)]
struct IdentityIndexV1 {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, String>,
    #[serde(default)]
    locations: BTreeMap<String, String>,
}

fn index_path(root: &Path) -> PathBuf {
    root.join("workspaces").join(IDENTITY_INDEX_FILE)
}

fn load_identity_index(root: &Path) -> Result<IdentityIndexV1> {
    let path = index_path(root);
    let Some(bytes) = read_bounded(&path, MAX_INDEX_BYTES, "workspace identity index")? else {
        return Ok(IdentityIndexV1::default());
    };
    let index = serde_json::from_slice::<IdentityIndexV1>(&bytes)
        .context("parse workspace identity index")?;
    if index.version != 1 {
        return Ok(IdentityIndexV1::default());
    }
    if index.entries.len() > MAX_WORKSPACE_STATE_SLOTS
        || index.locations.len() > MAX_WORKSPACE_STATE_SLOTS
    {
        return Ok(IdentityIndexV1::default());
    }
    Ok(index)
}

fn save_identity_index(root: &Path, index: &IdentityIndexV1) -> Result<()> {
    let bytes = serde_json::to_vec(index).context("encode workspace identity index")?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Ok(());
    }
    let path = index_path(root);
    write_private(&path, &bytes)?;

    // Atomic replacement updates the parent directory after writing the new
    // file, so its natural mtime can be microscopically older than the
    // directory and look stale immediately. Normalize the index mtime to the
    // post-rename directory mtime without another directory mutation. A crash
    // or concurrent slot mutation leaves the index stale and forces a scan.
    let workspaces = root.join("workspaces");
    let directory_mtime = fs::metadata(&workspaces)
        .context("inspect workspace registry after index update")?
        .modified()
        .context("read workspace registry modification time")?;
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .context("open workspace identity index after update")?;
    file.set_modified(directory_mtime)
        .context("normalize workspace identity index freshness")?;
    if fs::metadata(&workspaces)
        .context("reinspect workspace registry after index update")?
        .modified()
        .context("reread workspace registry modification time")?
        != directory_mtime
    {
        file.set_modified(UNIX_EPOCH)
            .context("mark raced workspace identity index stale")?;
    }
    Ok(())
}

/// The index is authoritative only while no slot-level directory mutation
/// (create, remove, or rename) happened after it was written. Any mtime
/// mismatch with the `workspaces` directory forces the bounded scan,
/// preserving the fail-closed duplicate-identity detection: a second slot
/// claiming the same identity is exactly the kind of mutation this guard
/// re-verifies.
pub(crate) fn identity_index_fresh(root: &Path, workspaces: &Path) -> Result<bool> {
    let index_modified = match fs::metadata(index_path(root)) {
        Ok(metadata) => metadata.modified().ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspect workspace identity index"),
    };
    let workspaces_modified = match fs::metadata(workspaces) {
        Ok(metadata) => metadata.modified().ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspect global workspace registry"),
    };
    Ok(index_modified
        .zip(workspaces_modified)
        .is_some_and(|(index, workspaces)| index == workspaces))
}

/// Look up the slot recorded for `identity`. Both the stable and the exact
/// legacy representation are consulted; if they disagree the identity is
/// ambiguous and the caller must fail closed. Callers re-verify the candidate
/// against the slot's authoritative identity file.
pub(crate) fn index_lookup(root: &Path, identity: &WorkspaceIdentity) -> Result<Option<PathBuf>> {
    let index = load_identity_index(root)?;
    let stable = index.entries.get(&identity.stable);
    let legacy = identity
        .compatible_legacy
        .as_ref()
        .and_then(|legacy| index.entries.get(legacy));
    let slot = match (stable, legacy) {
        (Some(stable), Some(legacy)) if stable != legacy => {
            bail!("workspace identity is ambiguous across multiple state directories")
        }
        (Some(slot), _) | (None, Some(slot)) => slot,
        (None, None) => return Ok(None),
    };
    if !valid_workspace_state_slot(slot) {
        return Ok(None);
    }
    Ok(Some(root.join("workspaces").join(slot)))
}

/// Look up the state slot whose authoritative `location` was last recorded as
/// `canonical_path`. Retirement uses this only while the index is fresh and
/// re-verifies the location file before trusting the candidate.
fn index_lookup_location(root: &Path, canonical_path: &str) -> Result<Option<PathBuf>> {
    let index = load_identity_index(root)?;
    let Some(slot) = index.locations.get(canonical_path) else {
        return Ok(None);
    };
    if !valid_workspace_state_slot(slot) {
        return Ok(None);
    }
    Ok(Some(root.join("workspaces").join(slot)))
}

/// Persist `identity -> slot` after a verified resolution. The index is a
/// verified cache: a crash or lost update merely forces the next lookup to
/// fall back to the bounded scan, which repairs it on the next `ensure`.
pub(crate) fn upsert_index_entry(
    root: &Path,
    identity: &str,
    canonical_path: &str,
    slot: &str,
) -> Result<()> {
    if !valid_workspace_state_slot(slot) {
        bail!("workspace identity index slot is invalid");
    }
    let mut index = load_identity_index(root)?;
    index.version = 1;
    if !index.entries.contains_key(identity) && index.entries.len() >= MAX_WORKSPACE_STATE_SLOTS {
        return Ok(());
    }
    if !index.locations.contains_key(canonical_path)
        && index.locations.len() >= MAX_WORKSPACE_STATE_SLOTS
    {
        return Ok(());
    }

    index.entries.insert(identity.to_string(), slot.to_string());

    index
        .locations
        .retain(|path, indexed_slot| indexed_slot != slot || path == canonical_path);
    index
        .locations
        .insert(canonical_path.to_string(), slot.to_string());
    save_identity_index(root, &index)
}

/// Remove every identity and location cache entry for a tombstoned slot so
/// resolution and retirement can never rediscover it through a fast path.
fn remove_index_slot(root: &Path, identity: &str, slot: &str) -> Result<()> {
    let mut index = load_identity_index(root)?;
    let entries_before = index.entries.len();
    index.entries.retain(|indexed_identity, indexed_slot| {
        indexed_identity != identity && indexed_slot != slot
    });
    let locations_before = index.locations.len();
    index
        .locations
        .retain(|_, indexed_slot| indexed_slot != slot);
    if index.entries.len() != entries_before || index.locations.len() != locations_before {
        save_identity_index(root, &index)?;
    }
    Ok(())
}

// Authenticated provenance and tombstone records

/// Prospective provenance recorded when a slot is first established (or
/// adopted from path-only state). Every field except `adopted_from` is
/// identity- or location-bound, so a later tombstone can be revalidated
/// against the same binding.
#[derive(Serialize, Deserialize)]
pub(crate) struct SlotProvenance {
    pub(crate) version: u32,
    pub(crate) established_unix_ns: u64,
    pub(crate) identity: String,
    pub(crate) canonical_path: String,
    #[serde(default)]
    pub(crate) adopted_from: Option<String>,
}

/// An explicit retirement request. A slot is inert once this exists: it is
/// excluded from the identity index, skipped by resolution scans, quarantined
/// after the grace period, and deleted only after re-verifying that the
/// recorded folder no longer matches the recorded identity.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct SlotTombstone {
    pub(crate) version: u32,
    pub(crate) requested_unix_ns: u64,
    pub(crate) grace_seconds: u64,
    pub(crate) identity: String,
    pub(crate) canonical_path: String,
    #[serde(default)]
    pub(crate) quarantined_unix_ns: Option<u64>,
}

pub(crate) fn slot_tombstoned(slot: &Path) -> Result<bool> {
    Ok(fs::symlink_metadata(slot.join(TOMBSTONE_FILE))
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink()))
}

pub(crate) fn read_slot_tombstone(slot: &Path) -> Result<Option<SlotTombstone>> {
    read_bounded_json(&slot.join(TOMBSTONE_FILE), "workspace-state tombstone")
}

pub(crate) fn write_slot_provenance(slot: &Path, provenance: &SlotProvenance) -> Result<()> {
    write_bounded_json(
        &slot.join(PROVENANCE_FILE),
        provenance,
        "workspace-state provenance",
    )
}

fn write_slot_tombstone(slot: &Path, tombstone: &SlotTombstone) -> Result<()> {
    write_bounded_json(
        &slot.join(TOMBSTONE_FILE),
        tombstone,
        "workspace-state tombstone",
    )
}

pub(crate) fn clear_slot_tombstone(slot: &Path) -> Result<()> {
    match fs::remove_file(slot.join(TOMBSTONE_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("clear workspace-state tombstone"),
    }
}

fn read_location(slot: &Path) -> Result<Option<String>> {
    let Some(bytes) = read_bounded(
        &slot.join("location"),
        MAX_RECORD_BYTES,
        "workspace location",
    )?
    else {
        return Ok(None);
    };
    let location = String::from_utf8(bytes)
        .context("workspace location is not valid UTF-8")?
        .trim()
        .to_string();
    Ok(Some(location))
}

/// The recorded folder is live when its current identity matches the
/// recorded one. `None` means the folder is gone; an error or a mismatch
/// means the identity no longer applies. Deletion revalidation treats only
/// `Ok(false)` as deletable.
fn folder_matches_recorded_identity(path: &str, identity: &str) -> Result<bool> {
    Ok(current_workspace_identity(Path::new(path))?
        .is_some_and(|current| current.matches(Some(identity))))
}

/// Reconstruct the canonical path a folder had while it existed, for a folder
/// that is now gone. Walking to the nearest existing ancestor and appending
/// the missing components reproduces what `fs::canonicalize` returned while
/// the folder was live (including macOS `/var` → `/private/var` resolution),
/// so the path-hash slot name and the recorded `location` still match.
fn canonical_path_for_missing_workspace(workspace: &Path) -> Result<PathBuf> {
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace)
    };
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    while fs::symlink_metadata(ancestor).is_err() {
        let name = ancestor
            .file_name()
            .context("workspace path has no reconstructable parent")?;
        missing.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .context("workspace path has no reconstructable parent")?;
    }
    let mut canonical = fs::canonicalize(ancestor).context("canonicalize existing ancestor")?;
    for name in missing.into_iter().rev() {
        canonical.push(name);
    }
    Ok(canonical)
}

fn state_directory_exists(state: &Path) -> Result<bool> {
    match fs::symlink_metadata(state) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => bail!(
            "workspace state is not a regular directory: {}",
            state.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect workspace state"),
    }
}

fn state_location_matches(state: &Path, canonical_path: &str) -> Result<bool> {
    Ok(state_directory_exists(state)? && read_location(state)?.as_deref() == Some(canonical_path))
}

fn lease_state_path(root: &Path, state: &Path) -> Result<String> {
    let slot = state
        .file_name()
        .and_then(|name| name.to_str())
        .context("workspace-state slot is not valid UTF-8")?;
    if !valid_workspace_state_slot(slot) {
        bail!("workspace-state slot name is invalid");
    }
    ensure_shared_state_lease(root, slot)?;
    Ok(slot.to_string())
}

fn lease_matching_retirement_state(
    root: &Path,
    state: &Path,
    canonical_path: &str,
) -> Result<(String, PathBuf)> {
    let slot = lease_state_path(root, state)?;
    if !state_location_matches(state, canonical_path)? {
        bail!("workspace state changed while its lifetime lease was being acquired");
    }
    Ok((slot, state.to_path_buf()))
}

/// Resolve retirement by the authoritative recorded location. The path-hash
/// slot remains the common case, while a fresh location index finds state that
/// intentionally stayed in its original slot after a workspace move. A stale
/// or missing index falls back to a bounded scan and rejects ambiguity.
fn resolve_retirement_state(
    root: &Path,
    canonical: &Path,
    canonical_path: &str,
) -> Result<(String, PathBuf)> {
    let workspaces = root.join("workspaces");
    let preferred_slot = workspace_state_id(canonical)?;
    let preferred = workspaces.join(&preferred_slot);
    let preferred_exists = state_directory_exists(&preferred)?;
    if preferred_exists && state_location_matches(&preferred, canonical_path)? {
        return lease_matching_retirement_state(root, &preferred, canonical_path);
    }

    if identity_index_fresh(root, &workspaces)? {
        if let Some(candidate) = index_lookup_location(root, canonical_path)? {
            if state_location_matches(&candidate, canonical_path)? {
                return lease_matching_retirement_state(root, &candidate, canonical_path);
            }
        }
    }

    let entries = match fs::read_dir(&workspaces) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("no workspace state is registered for {canonical_path}")
        }
        Err(error) => return Err(error).context("search workspace state for retirement"),
    };
    let mut matched = None;
    let mut scanned = 0usize;
    for entry in entries {
        let entry = entry.context("read workspace-state registry entry")?;
        if !entry
            .file_type()
            .context("inspect workspace-state registry entry")?
            .is_dir()
        {
            continue;
        }
        scanned = scanned.saturating_add(1);
        if scanned > MAX_WORKSPACE_STATE_SLOTS {
            bail!("global workspace registry exceeds bounded slot count");
        }
        let candidate = entry.path();
        if !state_location_matches(&candidate, canonical_path)? {
            continue;
        }
        if matched.replace(candidate).is_some() {
            bail!("workspace location is ambiguous across multiple state directories");
        }
    }
    if let Some(candidate) = matched {
        return lease_matching_retirement_state(root, &candidate, canonical_path);
    }

    if preferred_exists {
        let slot = lease_state_path(root, &preferred)?;
        if !state_directory_exists(&preferred)? {
            bail!("workspace state changed while its lifetime lease was being acquired");
        }
        return Ok((slot, preferred));
    }
    bail!("no workspace state is registered for {canonical_path}")
}

// Retirement: tombstone -> grace -> quarantine -> verified deletion

/// Typed result of one tombstone request (CLI `--json` surface).
#[derive(Debug, Clone, Serialize)]
pub struct TombstoneRecord {
    pub slot: String,
    pub tombstoned_unix_ns: u64,
    pub grace_seconds: u64,
    pub quarantined: bool,
}

/// Typed result of one retirement sweep (CLI `--json` surface).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RetirementSweep {
    pub quarantined: Vec<String>,
    pub deleted: Vec<String>,
    pub retained: Vec<String>,
}

/// Tombstone the global workspace state registered for `workspace`.
///
/// Fails closed unless: the slot exists and carries a recorded identity, the
/// folder is gone or no longer matches that identity, the slot's recorded
/// location equals the requested path, and no other live
/// process holds the slot. A zero grace quarantines immediately; otherwise
/// the slot stays inert until a sweep sees the grace elapse.
pub fn retire_workspace_state(workspace: &Path, grace: Duration) -> Result<TombstoneRecord> {
    retire_workspace_state_in(
        &crate::workspace_layout::global_state_root()?,
        workspace,
        grace,
    )
}

pub(crate) fn retire_workspace_state_in(
    root: &Path,
    workspace: &Path,
    grace: Duration,
) -> Result<TombstoneRecord> {
    let canonical = if fs::symlink_metadata(workspace).is_ok() {
        canonical_workspace(workspace)?
    } else {
        canonical_path_for_missing_workspace(workspace)?
    };
    let canonical_str = canonical
        .to_str()
        .context("workspace path is not valid UTF-8 and cannot be retired portably")?;
    let (slot, state) = resolve_retirement_state(root, &canonical, canonical_str)?;
    let Some(identity) = read_workspace_identity(&state)? else {
        bail!(
            "workspace state at {} has no recorded identity; refusing to retire unauthenticated state",
            state.display()
        );
    };
    let Some(location) = read_location(&state)? else {
        bail!(
            "workspace state at {} has no recorded location; refusing to retire unauthenticated state",
            state.display()
        );
    };
    if location != canonical_str {
        bail!(
            "requested path {} does not match the recorded workspace location {}",
            canonical_str,
            location
        );
    }
    if folder_matches_recorded_identity(canonical_str, &identity)? {
        bail!(
            "workspace folder {} is still live; refusing to retire its state",
            canonical_str
        );
    }
    let _exclusive = ExclusiveStateLease::try_acquire(root, &slot)?;
    // Everything is revalidated after winning the exclusive lease, so a
    // concurrent ensure or retirement cannot be raced.
    if !state_directory_exists(&state)? {
        bail!("workspace state disappeared while retiring");
    }
    let Some(still) = read_workspace_identity(&state)? else {
        bail!("workspace state lost its recorded identity while retiring");
    };
    if still != identity {
        bail!("workspace state identity changed while retiring; refusing to retire");
    }
    if read_location(&state)?.as_deref() != Some(canonical_str) {
        bail!("workspace state location changed while retiring; refusing to retire");
    }
    if folder_matches_recorded_identity(canonical_str, &identity)? {
        bail!("workspace folder is still live; refusing to retire its state");
    }
    let requested = unix_nanos_now();
    let tombstone = SlotTombstone {
        version: 1,
        requested_unix_ns: requested,
        grace_seconds: grace.as_secs(),
        identity,
        canonical_path: canonical_str.to_string(),
        quarantined_unix_ns: None,
    };
    write_slot_tombstone(&state, &tombstone)?;
    remove_index_slot(root, &tombstone.identity, &slot)?;
    let mut record = TombstoneRecord {
        slot: slot.clone(),
        tombstoned_unix_ns: requested,
        grace_seconds: tombstone.grace_seconds,
        quarantined: false,
    };
    if grace.is_zero() {
        quarantine_slot(root, &slot, &state, &tombstone)?;
        record.quarantined = true;
    }
    Ok(record)
}

/// Move one tombstoned slot into `quarantine/`. The caller must hold the
/// slot's exclusive lease and must have verified the grace elapsed and the
/// folder no longer matches. The tombstone records the quarantine time before
/// the rename so the later deletion stage has both bindings.
fn quarantine_slot(root: &Path, slot: &str, state: &Path, tombstone: &SlotTombstone) -> Result<()> {
    let quarantine = root.join("quarantine");
    private_dir(&quarantine)?;
    let mut quarantined = SlotTombstone {
        quarantined_unix_ns: Some(unix_nanos_now()),
        ..(*tombstone).clone()
    };
    write_slot_tombstone(state, &quarantined)?;
    quarantined.quarantined_unix_ns = None;
    let destination = unique_child(&quarantine, &format!("workspace-{slot}"));
    fs::rename(state, &destination).with_context(|| {
        format!(
            "quarantine retired workspace state at {}",
            destination.display()
        )
    })?;
    tracing::info!(
        "Quarantined retired workspace state {} at {}",
        slot,
        destination.display()
    );
    Ok(())
}

/// Process expired tombstones: quarantine slots whose grace elapsed, then
/// delete quarantined state whose retention elapsed and whose recorded folder
/// is still verified gone. Every step is exclusive-leased and identity
/// revalidated; anything unproven or live is retained.
pub fn sweep_retired_state() -> Result<RetirementSweep> {
    sweep_retired_state_in(&crate::workspace_layout::global_state_root()?)
}

pub(crate) fn sweep_retired_state_in(root: &Path) -> Result<RetirementSweep> {
    let mut sweep = RetirementSweep::default();
    let now = unix_nanos_now();
    sweep_tombstoned_slots(root, now, &mut sweep)?;
    sweep_quarantine(root, now, &mut sweep)?;
    Ok(sweep)
}

fn sweep_tombstoned_slots(root: &Path, now: u64, sweep: &mut RetirementSweep) -> Result<()> {
    let workspaces = root.join("workspaces");
    let entries = match fs::read_dir(&workspaces) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("scan global workspace registry for retirement"),
    };
    let mut scanned = 0usize;
    for entry in entries {
        let entry = entry.context("read global workspace registry entry")?;
        if !entry
            .file_type()
            .context("inspect global workspace registry entry")?
            .is_dir()
        {
            continue;
        }
        scanned = scanned.saturating_add(1);
        if scanned > MAX_WORKSPACE_STATE_SLOTS {
            bail!("global workspace registry exceeds bounded slot count");
        }
        let state = entry.path();
        let Some(tombstone) = read_slot_tombstone(&state)? else {
            continue;
        };
        let grace_elapsed = now
            >= tombstone
                .requested_unix_ns
                .saturating_add(tombstone.grace_seconds.saturating_mul(1_000_000_000));
        if !grace_elapsed {
            continue;
        }
        let Some(slot) = state.file_name().and_then(|name| name.to_str()) else {
            sweep.retained.push(state.display().to_string());
            continue;
        };
        ensure_shared_state_lease(root, slot)?;
        let Ok(_exclusive) = ExclusiveStateLease::try_acquire(root, slot) else {
            sweep.retained.push(slot.to_string());
            continue;
        };
        // Revalidate under the exclusive lease; never move live or changed state.
        let Some(current) = read_slot_tombstone(&state)? else {
            continue;
        };
        if current.identity != tombstone.identity {
            sweep.retained.push(slot.to_string());
            continue;
        }
        match folder_matches_recorded_identity(&current.canonical_path, &current.identity) {
            Ok(true) => {
                // The recorded folder is live again: fail closed and keep the
                // bytes until a human decides.
                tracing::warn!(
                    "Retired workspace state for {} is live again; retained in place",
                    current.canonical_path
                );
                sweep.retained.push(slot.to_string());
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    "Could not revalidate retired workspace state at {}: {error:#}; retained",
                    state.display()
                );
                sweep.retained.push(slot.to_string());
                continue;
            }
        }
        quarantine_slot(root, slot, &state, &current)?;
        sweep.quarantined.push(slot.to_string());
    }
    Ok(())
}

/// Extract the 64-hex slot name from a `workspace-<slot>-<stamp>` quarantine
/// directory name. Anything else returns `None` and is never deleted.
fn quarantined_slot_name(directory: &Path) -> Option<String> {
    let name = directory.file_name()?.to_str()?;
    let rest = name.strip_prefix("workspace-")?;
    let slot = rest.get(..64)?;
    if !slot.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    rest.as_bytes().get(64).filter(|byte| **byte == b'-')?;
    Some(slot.to_string())
}

fn sweep_quarantine(root: &Path, now: u64, sweep: &mut RetirementSweep) -> Result<()> {
    let quarantine = root.join("quarantine");
    let entries = match fs::read_dir(&quarantine) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("scan workspace-state quarantine"),
    };
    let retention = quarantine_retention();
    let mut scanned = 0usize;
    for entry in entries {
        let entry = entry.context("read workspace-state quarantine entry")?;
        if !entry
            .file_type()
            .context("inspect workspace-state quarantine entry")?
            .is_dir()
        {
            continue;
        }
        scanned = scanned.saturating_add(1);
        if scanned > MAX_WORKSPACE_STATE_SLOTS {
            bail!("workspace-state quarantine exceeds bounded slot count");
        }
        let directory = entry.path();
        // Only explicit tombstones ever authorize deletion. Preservation
        // quarantine from legacy-migration conflicts has no tombstone and is
        // never touched here.
        let Some(tombstone) = read_slot_tombstone(&directory)? else {
            continue;
        };
        let Some(quarantined_ns) = tombstone.quarantined_unix_ns else {
            continue;
        };
        if now < quarantined_ns.saturating_add(retention.as_secs().saturating_mul(1_000_000_000)) {
            continue;
        }
        let Some(slot) = quarantined_slot_name(&directory) else {
            sweep.retained.push(directory.display().to_string());
            continue;
        };
        ensure_shared_state_lease(root, &slot)?;
        let Ok(_exclusive) = ExclusiveStateLease::try_acquire(root, &slot) else {
            sweep.retained.push(slot);
            continue;
        };
        match folder_matches_recorded_identity(&tombstone.canonical_path, &tombstone.identity) {
            Ok(false) => {
                fs::remove_dir_all(&directory).with_context(|| {
                    format!(
                        "delete quarantined workspace state at {}",
                        directory.display()
                    )
                })?;
                tracing::info!(
                    "Deleted quarantined workspace state for {}",
                    tombstone.canonical_path
                );
                sweep.deleted.push(slot);
            }
            Ok(true) => {
                tracing::warn!(
                    "Quarantined workspace state for {} is live again; retained",
                    tombstone.canonical_path
                );
                sweep.retained.push(slot);
            }
            Err(error) => {
                tracing::warn!(
                    "Could not revalidate quarantined workspace state at {}: {error:#}; retained",
                    directory.display()
                );
                sweep.retained.push(slot);
            }
        }
    }
    Ok(())
}

/// Best-effort global sweep gated by an hourly stamp. Routine `ensure` calls
/// run this without ever failing: a contended or unsupported lease merely
/// defers the sweep to the next window.
pub(crate) fn maybe_sweep_retired_state(root: &Path) {
    let stamp = root.join("retirement.stamp");
    if fs::metadata(&stamp)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < RETIREMENT_SWEEP_INTERVAL)
    {
        return;
    }
    match sweep_retired_state_in(root) {
        Ok(sweep) => {
            if !sweep.quarantined.is_empty() || !sweep.deleted.is_empty() {
                tracing::info!(
                    "Workspace-state retirement sweep: {} quarantined, {} deleted, {} retained",
                    sweep.quarantined.len(),
                    sweep.deleted.len(),
                    sweep.retained.len()
                );
            }
            if let Err(error) = write_private(&stamp, b"retirement sweep v1\n") {
                tracing::debug!("could not record retirement sweep stamp: {error:#}");
            }
        }
        Err(error) => tracing::warn!("workspace-state retirement sweep deferred: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare_slot(root: &Path, workspace: &Path) -> (PathBuf, String) {
        let slot = workspace_state_id(workspace).unwrap();
        let state = crate::workspace_layout::ensure_workspace_state_in(workspace, root).unwrap();
        std::fs::write(state.join("config.json"), b"capability").unwrap();
        (state, slot)
    }

    #[test]
    fn index_roundtrips_identity_and_location_entries() {
        let root = tempfile::tempdir().unwrap();
        let workspaces = root.path().join("workspaces");
        fs::create_dir_all(&workspaces).unwrap();
        let slot_a = "a".repeat(64);
        let slot_b = "b".repeat(64);

        upsert_index_entry(root.path(), "identity-a", "/workspace/a", &slot_a).unwrap();
        upsert_index_entry(root.path(), "identity-b", "/workspace/b", &slot_b).unwrap();
        let index = load_identity_index(root.path()).unwrap();
        assert_eq!(
            index.entries.get("identity-a").map(String::as_str),
            Some(slot_a.as_str())
        );
        assert_eq!(
            index.entries.get("identity-b").map(String::as_str),
            Some(slot_b.as_str())
        );
        assert_eq!(
            index_lookup_location(root.path(), "/workspace/a").unwrap(),
            Some(workspaces.join(&slot_a))
        );

        remove_index_slot(root.path(), "identity-a", &slot_a).unwrap();
        let index = load_identity_index(root.path()).unwrap();
        assert!(!index.entries.contains_key("identity-a"));
        assert!(index.entries.contains_key("identity-b"));
        assert!(!index.locations.contains_key("/workspace/a"));
        assert!(index.locations.contains_key("/workspace/b"));
    }

    #[test]
    fn retire_refuses_live_and_unauthenticated_state() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, _slot) = prepare_slot(root.path(), workspace.path());
        let identity = fs::read(state.join("identity")).unwrap();

        // Live folder: fail closed, bytes untouched.
        let error =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("still live"));
        assert_eq!(fs::read(state.join("config.json")).unwrap(), b"capability");

        // Unauthenticated (no identity): fail closed.
        fs::remove_file(state.join("identity")).unwrap();
        let error =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("unauthenticated"));

        // A recorded identity without its exact location is not enough to
        // authorize retirement.
        fs::write(state.join("identity"), identity).unwrap();
        fs::remove_file(state.join("location")).unwrap();
        let error =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("no recorded location"));
        assert_eq!(fs::read(state.join("config.json")).unwrap(), b"capability");
    }

    #[test]
    fn relocated_workspace_state_can_be_retired_by_its_current_path() {
        let root = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("before");
        let relocated = parent.path().join("after");
        fs::create_dir(&original).unwrap();
        let state =
            crate::workspace_layout::ensure_workspace_state_in(&original, root.path()).unwrap();
        let slot = state.file_name().unwrap().to_str().unwrap().to_string();
        fs::write(state.join("config.json"), b"capability").unwrap();

        fs::rename(&original, &relocated).unwrap();
        let resolved =
            crate::workspace_layout::ensure_workspace_state_in(&relocated, root.path()).unwrap();
        assert_eq!(resolved, state);
        assert_eq!(
            index_lookup_location(
                root.path(),
                fs::canonicalize(&relocated).unwrap().to_str().unwrap()
            )
            .unwrap(),
            Some(state.clone())
        );

        fs::remove_dir_all(&relocated).unwrap();
        let record = retire_workspace_state_in(root.path(), &relocated, Duration::ZERO).unwrap();
        assert_eq!(record.slot, slot);
        assert!(record.quarantined);
        assert!(!state.exists());
    }

    #[test]
    fn retire_grace_then_sweep_quarantines_and_deletes_verified_state() {
        std::env::set_var("FEANORFS_RETIRE_QUARANTINE_SECS", "0");
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, slot) = prepare_slot(root.path(), workspace.path());
        fs::remove_dir_all(workspace.path()).unwrap();

        // Grace not elapsed: sweep retains in place.
        let record =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::from_secs(3600))
                .unwrap();
        assert!(!record.quarantined);
        let sweep = sweep_retired_state_in(root.path()).unwrap();
        assert!(
            sweep.quarantined.is_empty() && sweep.deleted.is_empty() && sweep.retained.is_empty()
        );
        assert!(state.is_dir());
        assert!(slot_tombstoned(&state).unwrap());

        // Zero grace: retire quarantines immediately; next sweep deletes.
        let record =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::ZERO).unwrap();
        assert!(record.quarantined);
        assert!(!state.is_dir());
        let sweep = sweep_retired_state_in(root.path()).unwrap();
        assert_eq!(sweep.deleted, vec![slot]);
        let quarantine = root.path().join("quarantine");
        let remaining = fs::read_dir(&quarantine)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().is_dir())
            .count();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn sweep_never_deletes_unproven_or_live_quarantine_entries() {
        std::env::set_var("FEANORFS_RETIRE_QUARANTINE_SECS", "0");
        let root = tempfile::tempdir().unwrap();
        let quarantine = root.path().join("quarantine");
        fs::create_dir_all(&quarantine).unwrap();

        // Preservation quarantine (legacy-migration conflict) has no
        // tombstone and must survive forever.
        let preserved = quarantine.join("workspace-aaaa-global-1-1");
        fs::create_dir_all(&preserved).unwrap();
        fs::write(preserved.join("config.json"), b"preserved bytes").unwrap();

        let sweep = sweep_retired_state_in(root.path()).unwrap();
        assert!(sweep.quarantined.is_empty() && sweep.deleted.is_empty());
        assert_eq!(
            fs::read(preserved.join("config.json")).unwrap(),
            b"preserved bytes"
        );

        // A tombstoned quarantine entry whose folder is live again is retained.
        let workspace = tempfile::tempdir().unwrap();
        let (_state, slot) = prepare_slot(root.path(), workspace.path());
        fs::remove_dir_all(workspace.path()).unwrap();
        let record =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::ZERO).unwrap();
        assert!(record.quarantined);
        let quarantined = fs::read_dir(&quarantine)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir() && *path != preserved)
            .unwrap();
        assert!(quarantined.join(TOMBSTONE_FILE).is_file());

        // Recreate the folder before the delete pass: identity differs, so
        // deletion is authorized for the old identity (the new folder gets a
        // fresh slot), and the quarantine entry goes away.
        fs::create_dir_all(workspace.path()).unwrap();
        let sweep = sweep_retired_state_in(root.path()).unwrap();
        assert_eq!(sweep.deleted, vec![slot]);
        assert!(!quarantined.exists());
    }

    #[test]
    fn tombstoned_slots_are_invisible_to_resolution_and_revivable_by_live_identity() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, slot) = prepare_slot(root.path(), workspace.path());
        let identity = fs::read_to_string(state.join("identity")).unwrap();
        fs::remove_dir_all(workspace.path()).unwrap();
        retire_workspace_state_in(root.path(), workspace.path(), Duration::from_secs(3600))
            .unwrap();
        assert!(slot_tombstoned(&state).unwrap());

        // Index removed and tombstoned slots skipped: the resolution scan
        // cannot resurrect the retired slot for a fresh folder.
        let index = load_identity_index(root.path()).unwrap();
        assert!(!index.entries.contains_key(&identity));
        // Recreating the folder gives it a new identity: resolution must fail
        // closed instead of resurrecting the retired slot, and the tombstone
        // stays until the sweep re-verifies and removes the old state.
        fs::create_dir_all(workspace.path()).unwrap();
        let error =
            crate::workspace_layout::workspace_state_path_in(workspace.path(), root.path(), false)
                .unwrap_err();
        assert!(error.to_string().contains("different folder"));
        assert!(slot_tombstoned(&state).unwrap());
        let _ = slot;
    }

    #[test]
    fn lease_contention_fails_closed_and_recovers() {
        let root = tempfile::tempdir().unwrap();
        let workspaces = root.path().join("workspaces");
        let key = (root.path().to_path_buf(), "slot".to_string());
        ensure_shared_state_lease(root.path(), "slot").unwrap();
        // A foreign open file description simulates another live process's
        // full-lifetime shared hold on the same slot.
        let foreign = open_lease_file(&workspaces, "slot").unwrap();
        FileExt::lock_shared(&foreign).unwrap();
        let error = ExclusiveStateLease::try_acquire(root.path(), "slot").unwrap_err();
        assert!(error.to_string().contains("other live processes"));
        FileExt::unlock(&foreign).unwrap();
        drop(foreign);
        // With no other holder the same process wins the exclusive dance.
        let exclusive = ExclusiveStateLease::try_acquire(root.path(), "slot").unwrap();
        drop(exclusive);
        assert!(shared_leases()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&key));
        let contender = open_lease_file(&workspaces, "slot").unwrap();
        assert!(FileExt::try_lock_exclusive(&contender).is_err());
    }

    #[test]
    fn relocated_resolution_holds_the_returned_slot_lifetime_lease() {
        let root = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("before");
        let relocated = parent.path().join("after");
        fs::create_dir(&original).unwrap();
        let state =
            crate::workspace_layout::ensure_workspace_state_in(&original, root.path()).unwrap();
        let slot = state.file_name().unwrap().to_str().unwrap().to_string();
        let key = (root.path().to_path_buf(), slot.clone());
        assert_eq!(
            crate::workspace_layout::ensure_workspace_state_in(&original, root.path()).unwrap(),
            state
        );

        // Simulate a fresh process: the state and index exist, but this
        // process has not retained the original resolver's shared lease.
        shared_leases()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        fs::rename(&original, &relocated).unwrap();
        let workspaces = root.path().join("workspaces");
        let index_mtime = fs::metadata(index_path(root.path()))
            .unwrap()
            .modified()
            .unwrap();
        let workspaces_mtime = fs::metadata(&workspaces).unwrap().modified().unwrap();
        assert!(
            identity_index_fresh(root.path(), &workspaces).unwrap(),
            "freshly written index is stale: index={index_mtime:?}, workspaces={workspaces_mtime:?}"
        );

        let resolved =
            crate::workspace_layout::workspace_state_path_in(&relocated, root.path(), false)
                .unwrap();
        assert_eq!(resolved, state);
        assert!(shared_leases()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&key));
        let contender = open_lease_file(&root.path().join("workspaces"), &slot).unwrap();
        assert!(FileExt::try_lock_exclusive(&contender).is_err());
    }

    #[test]
    fn retirement_fails_closed_while_another_live_process_holds_the_slot() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let _state = prepare_slot(root.path(), workspace.path());
        let ready = root.path().join("lease-ready");

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_state_registry::tests::hold_state_lease_child",
                "--ignored",
            ])
            .env("FEANORFS_LEASE_CHILD_ROOT", root.path())
            .env("FEANORFS_LEASE_CHILD_WORKSPACE", workspace.path())
            .spawn()
            .expect("spawn lease-holding child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "child never acquired its lease"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        // The folder itself is gone, so only the child's live lease can block
        // retirement.
        fs::remove_dir_all(workspace.path()).unwrap();
        let error =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("other live processes"));

        child.kill().expect("stop lease-holding child");
        child.wait().expect("reap lease-holding child");
        let record =
            retire_workspace_state_in(root.path(), workspace.path(), Duration::ZERO).unwrap();
        assert!(record.quarantined);
    }

    #[test]
    fn quarantined_slot_name_parsing_is_exact() {
        let slot = "ab12".repeat(16);
        let directory_path = format!("/q/workspace-{slot}-123-456");
        let directory = Path::new(&directory_path);
        assert_eq!(
            quarantined_slot_name(directory).as_deref(),
            Some(slot.as_str())
        );
        let exact_path = format!("/q/workspace-{slot}");
        assert_eq!(quarantined_slot_name(Path::new(&exact_path)), None);
        assert_eq!(
            quarantined_slot_name(Path::new("/q/workspace-ab12-123-456")),
            None
        );
        assert_eq!(quarantined_slot_name(Path::new("/q/other-ab12ab12")), None);
    }

    #[test]
    #[ignore]
    fn hold_state_lease_child() {
        let root = PathBuf::from(std::env::var("FEANORFS_LEASE_CHILD_ROOT").unwrap());
        let workspace = PathBuf::from(std::env::var("FEANORFS_LEASE_CHILD_WORKSPACE").unwrap());
        crate::workspace_layout::workspace_state_path_in(&workspace, &root, false)
            .expect("resolve state in child");
        fs::write(root.join("lease-ready"), b"held").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
