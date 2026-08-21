//! Locked registry schema, canonical workspace keys, and runner stop
//! tombstones.
//!
//! The registry (`~/.feanorfs/supervisor.json`) is read and written under an
//! exclusive sidecar lock (`supervisor.lock`). Every mutation increments a
//! durable `mutation_generation` so runner stop acknowledgements can bind to
//! the exact registry state that produced them (ABA-safe).

use anyhow::{ensure, Context as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use feanorfs_client::workspace_path::CanonicalWorkspacePath;

use super::*;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct SupervisorRegistry {
    #[serde(default)]
    pub(super) workspaces: Vec<CanonicalWorkspacePath>,
    #[serde(default)]
    pub(super) stopped: Vec<CanonicalWorkspacePath>,
    #[serde(default)]
    pub(super) runners: Vec<CanonicalWorkspacePath>,
    /// Durable per-runner stop tombstones. A stop token survives unrelated
    /// registry mutations, is cleared by re-add, and is replaced by every
    /// subsequent removal of the same runner.
    #[serde(default)]
    pub(super) runner_stop_tokens: BTreeMap<CanonicalWorkspacePath, RunnerStopTombstone>,
    /// Monotonic mutation generation. Legacy files omit this field and are
    /// intentionally ineligible for stop acknowledgement until a mutation
    /// rewrites them with a non-zero generation.
    #[serde(default)]
    pub(super) mutation_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RunnerStopTombstone {
    pub(super) token: String,
    pub(super) generation: u64,
}

/// The supervisor reloads this registry every 500 ms. Keep both its bytes
/// and child-fan-out bounded so a corrupt local file cannot turn routine
/// reconciliation into unbounded memory, parse, or process work.
pub(super) const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const MAX_SUPERVISOR_WORKSPACES: usize = 256;
pub(super) const MAX_RUNNER_STOP_TOMBSTONES: usize = 256;
const MAX_RUNNER_STOP_TOKEN_BYTES: usize = 128;

/// The visible runner controller removes a registry entry and then waits in
/// the same process. Remember the exact durable mutation it requested so a
/// later ABA registry mutation cannot satisfy that waiter. Missing state is
/// fail-closed rather than inferred from the current list contents.
static PENDING_RUNNER_STOP_TOKENS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

pub(super) fn registry_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(REGISTRY_FILE))
}

pub(super) fn create_store_dir(path: &Path) -> anyhow::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_store_lock(path: &Path) -> anyhow::Result<File> {
    let lock_path = path.with_extension("lock");
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock = options
        .open(&lock_path)
        .with_context(|| format!("open supervisor registry lock {}", lock_path.display()))?;
    fs2::FileExt::lock_exclusive(&lock)
        .with_context(|| format!("lock supervisor registry {}", lock_path.display()))?;
    Ok(lock)
}

pub(super) fn load_registry(path: &Path) -> anyhow::Result<SupervisorRegistry> {
    let Some(content) = read_registry_file(path)? else {
        return Ok(SupervisorRegistry::default());
    };
    let registry = serde_json::from_slice(&content)
        .with_context(|| format!("parse supervisor registry {}", path.display()))?;
    validate_registry(&registry)?;
    Ok(registry)
}

pub(super) fn save_registry(path: &Path, store: &SupervisorRegistry) -> anyhow::Result<()> {
    validate_registry(store)?;
    #[cfg(unix)]
    let file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let file = atomic_write_file::AtomicWriteFile::open(path)?;
    let mut writer = BoundedWriter::new(file, MAX_REGISTRY_BYTES);
    serde_json::to_writer_pretty(&mut writer, store).context("encode supervisor registry")?;
    writer.flush()?;
    let file = writer.into_inner();
    file.commit()?;
    // The registry carries coordination-sensitive runner stop tombstones:
    // a completed rename must survive power loss. Unix syncs the parent
    // directory; on Windows directory handles cannot be opened with `std`,
    // so this is a documented no-op (see feanorfs_agent_core::durable).
    sync_parent_directory(path);
    Ok(())
}

/// Read one registry through a hard limit even if the file changes after its
/// metadata check. A malformed or oversized local registry is configuration
/// corruption, not an empty registry: callers must stop rather than guess a
/// child set.
fn read_registry_file(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => anyhow::bail!(
            "supervisor registry is not a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect supervisor registry {}", path.display()))
        }
    };
    anyhow::ensure!(
        metadata.len() <= MAX_REGISTRY_BYTES,
        "supervisor registry exceeds {MAX_REGISTRY_BYTES} byte limit"
    );

    let capacity = usize::try_from(metadata.len()).context("supervisor registry is too large")?;
    let mut content = Vec::with_capacity(capacity);
    open_registry_for_read(path)?
        .take(MAX_REGISTRY_BYTES.saturating_add(1))
        .read_to_end(&mut content)
        .with_context(|| format!("read supervisor registry {}", path.display()))?;
    anyhow::ensure!(
        content.len() as u64 <= MAX_REGISTRY_BYTES,
        "supervisor registry exceeds {MAX_REGISTRY_BYTES} byte limit"
    );
    Ok(Some(content))
}

/// Open and validate the actual registry descriptor. This closes the gap
/// between the path preflight and the read: a raced FIFO must not block the
/// 500 ms supervisor loop, and a reparse point must never be followed.
#[cfg(unix)]
pub(super) fn open_registry_for_read(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options
        .open(path)
        .with_context(|| format!("open supervisor registry {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "supervisor registry is not a regular file: {}",
        path.display()
    );
    Ok(file)
}

#[cfg(windows)]
pub(super) fn open_registry_for_read(path: &Path) -> anyhow::Result<File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options
        .open(path)
        .with_context(|| format!("open supervisor registry {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "supervisor registry is not a non-reparse regular file: {}",
        path.display()
    );
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_registry_for_read(path: &Path) -> anyhow::Result<File> {
    let file =
        File::open(path).with_context(|| format!("open supervisor registry {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "supervisor registry is not a regular file: {}",
        path.display()
    );
    Ok(file)
}

/// Writer used for the atomic registry replacement. Streaming serialization
/// means an invalid in-memory registry cannot allocate an arbitrary JSON
/// buffer before the same durable byte cap rejects it.
struct BoundedWriter<W> {
    inner: W,
    bytes_remaining: u64,
}

impl<W> BoundedWriter<W> {
    fn new(inner: W, max_bytes: u64) -> Self {
        Self {
            inner,
            bytes_remaining: max_bytes,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: std::io::Write> std::io::Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if byte_count > self.bytes_remaining {
            return Err(std::io::Error::other(
                "supervisor registry exceeds configured byte limit",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.bytes_remaining = self
            .bytes_remaining
            .checked_sub(u64::try_from(written).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                std::io::Error::other("supervisor registry byte accounting underflow")
            })?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn validate_registry(store: &SupervisorRegistry) -> anyhow::Result<()> {
    validate_path_list("active workspace", &store.workspaces)?;
    validate_path_list("stopped workspace", &store.stopped)?;
    validate_path_list("runner workspace", &store.runners)?;
    anyhow::ensure!(
        store.runner_stop_tokens.len() <= MAX_RUNNER_STOP_TOMBSTONES,
        "supervisor registry has more than {MAX_RUNNER_STOP_TOMBSTONES} runner stop tombstones"
    );

    let active = store.workspaces.iter().collect::<BTreeSet<_>>();
    for path in &store.stopped {
        anyhow::ensure!(
            !active.contains(path),
            "supervisor registry lists a workspace as both active and stopped"
        );
    }

    let runners = store.runners.iter().collect::<BTreeSet<_>>();
    for (path, tombstone) in &store.runner_stop_tokens {
        anyhow::ensure!(
            !path.as_str().is_empty(),
            "supervisor registry has an empty runner stop tombstone path"
        );
        anyhow::ensure!(
            tombstone.token.len() <= MAX_RUNNER_STOP_TOKEN_BYTES && !tombstone.token.is_empty(),
            "supervisor registry has an invalid runner stop tombstone token"
        );
        // A tombstone is evidence created by one completed registry
        // mutation. Future or zero generations could make a stale runner
        // acknowledgement appear valid, so reject them before reconcile.
        anyhow::ensure!(
            tombstone.generation != 0 && tombstone.generation <= store.mutation_generation,
            "supervisor registry has an invalid runner stop tombstone generation"
        );
        anyhow::ensure!(
            !runners.contains(path),
            "supervisor registry lists a runner as both active and stopped"
        );
    }
    Ok(())
}

fn validate_path_list(label: &str, paths: &[CanonicalWorkspacePath]) -> anyhow::Result<()> {
    anyhow::ensure!(
        paths.len() <= MAX_SUPERVISOR_WORKSPACES,
        "supervisor registry has more than {MAX_SUPERVISOR_WORKSPACES} {label} entries"
    );
    let mut unique = BTreeSet::new();
    for path in paths {
        anyhow::ensure!(
            !path.as_str().is_empty(),
            "supervisor registry has an empty {label} path"
        );
        anyhow::ensure!(
            unique.insert(path),
            "supervisor registry has duplicate {label} entries"
        );
    }
    Ok(())
}

/// Fsync the parent directory of `path` so a completed rename survives
/// power loss (Unix-only; documented no-op elsewhere).
#[cfg(unix)]
fn sync_parent_directory(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    // Best-effort: the write itself is already committed; a failed parent
    // sync only widens the post-commit uncertainty window, which the
    // supervisor's startup reconciliation absorbs.
    let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) {}

fn update_registry<T>(update: impl FnOnce(&mut SupervisorRegistry) -> T) -> anyhow::Result<T> {
    let path = registry_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    let mut store = load_registry(&path)?;
    store.mutation_generation = store
        .mutation_generation
        .checked_add(1)
        .context("supervisor registry mutation generation exhausted")?;
    let result = update(&mut store);
    save_registry(&path, &store)?;
    Ok(result)
}

/// Checked registry mutation used by operations that must not persist a
/// partially-applied change.  The ordinary helper predates fallible runner
/// tombstone allocation and intentionally preserves its legacy "save after
/// closure" behavior; stop-token capacity is a transactional boundary.
fn update_registry_checked<T>(
    update: impl FnOnce(&mut SupervisorRegistry) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = registry_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    let mut store = load_registry(&path)?;
    store.mutation_generation = store
        .mutation_generation
        .checked_add(1)
        .context("supervisor registry mutation generation exhausted")?;
    let result = update(&mut store)?;
    save_registry(&path, &store)?;
    Ok(result)
}

pub(super) fn read_registry() -> anyhow::Result<SupervisorRegistry> {
    let path = registry_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    load_registry(&path)
}

/// Read the registry without creating either the registry directory or its
/// lock file when the registry has never been created.  Once the registry is
/// present, retain the same locked read boundary as `read_registry` so status
/// projections cannot race a mutator.
fn read_registry_if_present() -> anyhow::Result<SupervisorRegistry> {
    read_registry_if_present_at(&registry_path()?)
}

pub(super) fn read_registry_if_present_at(path: &Path) -> anyhow::Result<SupervisorRegistry> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "supervisor registry is not a regular file"
            );
            let _lock = open_store_lock(path)?;
            load_registry(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SupervisorRegistry::default())
        }
        Err(error) => Err(error).context("inspect supervisor registry"),
    }
}

pub(super) fn canonical_workspace_path(workspace: &Path) -> anyhow::Result<CanonicalWorkspacePath> {
    let canonical = workspace
        .canonicalize()
        .with_context(|| format!("Workspace folder does not exist: {}", workspace.display()))?;
    workspace_registry_key(&canonical)
}

pub(super) fn workspace_registry_key(canonical: &Path) -> anyhow::Result<CanonicalWorkspacePath> {
    canonical
        .to_str()
        .map(|path| CanonicalWorkspacePath::from_exact_string(path.to_owned()))
        .context("canonical workspace path must be valid UTF-8")
}

/// Validate that `workspace` is a configured FeanorFS mirror and return its
/// canonical path as the exact contract-boundary type.
pub(super) fn ensure_workspace_configured(
    workspace: &Path,
) -> anyhow::Result<CanonicalWorkspacePath> {
    let canonical = canonical_workspace_path(workspace)?;
    feanorfs_client::load_config(canonical.as_path()).with_context(|| {
        format!(
            "{} is not a FeanorFS workspace; run `feanorfs start` there first",
            canonical.as_path().display()
        )
    })?;
    Ok(canonical)
}

pub(crate) fn add_workspace(workspace: &Path) -> anyhow::Result<()> {
    let canonical = ensure_workspace_configured(workspace)?;
    update_registry(|store| {
        if !store.workspaces.iter().any(|path| path == &canonical) {
            store.workspaces.push(canonical.clone());
        }
        store.stopped.retain(|path| path != &canonical);
    })
}

pub(crate) fn stop_workspace_in_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = canonical_workspace_path(workspace)?;
    update_registry(|store| {
        if let Some(index) = store.workspaces.iter().position(|path| path == &canonical) {
            store.workspaces.remove(index);
            if !store.stopped.iter().any(|path| path == &canonical) {
                store.stopped.push(canonical.clone());
            }
        }
    })
}

pub(crate) fn start_workspace_in_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = ensure_workspace_configured(workspace)?;
    update_registry(|store| {
        if let Some(index) = store.stopped.iter().position(|path| path == &canonical) {
            store.stopped.remove(index);
        }
        if !store.workspaces.iter().any(|path| path == &canonical) {
            store.workspaces.push(canonical.clone());
        }
    })
}

pub(crate) fn remove_workspace_from_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = canonical_workspace_path(workspace)?;
    update_registry(|store| {
        store.workspaces.retain(|path| path != &canonical);
        store.stopped.retain(|path| path != &canonical);
    })
}

/// True when the workspace is remembered (supervised or explicitly stopped).
pub(crate) fn is_registered(workspace: &Path) -> anyhow::Result<bool> {
    let canonical = canonical_workspace_path(workspace)?;
    let registry = read_registry()?;
    Ok(registry.workspaces.iter().any(|path| path == &canonical)
        || registry.stopped.iter().any(|path| path == &canonical))
}

pub(crate) fn registered_workspaces() -> anyhow::Result<Vec<String>> {
    Ok(read_registry()?
        .workspaces
        .into_iter()
        .map(CanonicalWorkspacePath::into_string)
        .collect())
}

/// Register the canonical workspace root whose configured runner may be
/// supervised. Enablement and needs-attention state are checked separately
/// when the supervisor builds its desired child set.
pub(crate) fn add_runner(workspace: &Path) -> anyhow::Result<()> {
    let canonical = ensure_workspace_configured(workspace)?;
    feanorfs_agent_core::RunnerStore::open_configured(canonical.as_path())
        .context("open the configured agent runner")?;
    seed_registry_from_recents_if_absent()?;
    update_registry(|store| {
        if !store.runners.iter().any(|path| path == &canonical) {
            store.runners.push(canonical.clone());
        }
        // Re-adding the same runner invalidates every prior stop token. A
        // later removal will create a fresh token, so an old waiter can never
        // acknowledge a new A→removed→A cycle.
        store.runner_stop_tokens.remove(&canonical);
    })
}

pub(crate) fn remove_runner_from_registry(workspace: &Path) -> anyhow::Result<()> {
    let canonical = canonical_workspace_path(workspace)?;
    seed_registry_from_recents_if_absent()?;
    let token = update_registry_checked(|store| -> anyhow::Result<String> {
        // Concurrent stop callers for the same removed runner share the
        // existing durable tombstone.  Replacing it would strand the first
        // waiter and create an avoidable ABA boundary without any re-add.
        if !store.runners.iter().any(|path| path == &canonical) {
            if let Some(existing) = store.runner_stop_tokens.get(&canonical) {
                ensure!(
                    !existing.token.is_empty() && existing.generation != 0,
                    "existing runner stop tombstone is malformed"
                );
                return Ok(existing.token.clone());
            }
        }
        let ack_store = read_runner_reconcile_ack_store()?.unwrap_or_default();
        let already_present = store.runner_stop_tokens.contains_key(&canonical);
        if !already_present && store.runner_stop_tokens.len() >= MAX_RUNNER_STOP_TOMBSTONES {
            prune_runner_stop_tokens(store, &ack_store)?;
            ensure!(
                store.runner_stop_tokens.len() < MAX_RUNNER_STOP_TOMBSTONES,
                "runner stop tombstone capacity ({MAX_RUNNER_STOP_TOMBSTONES}) is full; wait for a durable stop acknowledgement before removing another runner"
            );
        }
        store.runners.retain(|path| path != &canonical);
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).context("generate runner stop token")?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(canonical.as_str().as_bytes());
        hasher.update(&store.mutation_generation.to_le_bytes());
        hasher.update(&nonce);
        let token = hasher.finalize().to_hex().to_string();
        store.runner_stop_tokens.insert(
            canonical.clone(),
            RunnerStopTombstone {
                token: token.clone(),
                generation: store.mutation_generation,
            },
        );
        Ok(token)
    })?;
    remember_runner_stop_token(canonical.into_string(), token);
    Ok(())
}

fn remember_runner_stop_token(canonical: String, token: String) {
    let mut pending = PENDING_RUNNER_STOP_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.insert(canonical, token);
}

pub(super) fn pending_runner_stop_token(canonical: &str) -> Option<String> {
    PENDING_RUNNER_STOP_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(canonical)
        .cloned()
}

pub(super) fn clear_runner_stop_token(canonical: &str, expected: Option<&str>) {
    let mut pending = PENDING_RUNNER_STOP_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if expected.is_none_or(|token| {
        pending
            .get(canonical)
            .is_some_and(|current| current == token)
    }) {
        pending.remove(canonical);
    }
}

pub(super) fn prune_runner_stop_tokens(
    registry: &mut SupervisorRegistry,
    ack_store: &RunnerReconcileAckStore,
) -> anyhow::Result<()> {
    while registry.runner_stop_tokens.len() >= MAX_RUNNER_STOP_TOMBSTONES {
        let oldest = registry
            .runner_stop_tokens
            .iter()
            .filter(|(workspace, tombstone)| {
                ack_store.acks.get(*workspace).is_some_and(|ack| {
                    ack.workspace == **workspace && ack.stop_token == tombstone.token
                })
            })
            .min_by_key(|(workspace, tombstone)| (tombstone.generation, *workspace))
            .map(|(workspace, _)| workspace.clone());
        let Some(oldest) = oldest else {
            anyhow::bail!(
                "runner stop tombstone capacity ({MAX_RUNNER_STOP_TOMBSTONES}) is full and no completed tombstone can be reclaimed"
            );
        };
        registry.runner_stop_tokens.remove(&oldest);
    }
    Ok(())
}

pub(crate) fn is_runner_registered(workspace: &Path) -> anyhow::Result<bool> {
    let canonical = canonical_workspace_path(workspace)?;
    Ok(read_registry_if_present()?
        .runners
        .iter()
        .any(|path| path == &canonical))
}

/// Returns whether a durable supervisor status artifact still establishes
/// runner authority for `workspace` before a visible controller mutates the
/// runner state. A status entry is retained even when its child is stopped: it
/// is evidence that a supervisor may have owned this runner. Reconcile acks do
/// not carry a workspace identity, so they are intentionally not sufficient
/// evidence by themselves; otherwise an unrelated stale ack would make a
/// truly fresh `runner stop` wait forever.
pub(crate) fn runner_stop_authority_exists(workspace: &Path) -> anyhow::Result<bool> {
    let canonical = canonical_workspace_path(workspace)?;
    Ok(read_status()?.is_some_and(|status| status.runners.contains_key(canonical.as_str())))
}

/// Seed the registry from recent workspaces on first use (legacy installs and
/// fresh profiles). Never resurrects workspaces an explicit `service stop` or
/// `feanorfs stop` moved out of supervision.
pub(super) fn seed_registry_from_recents_if_absent() -> anyhow::Result<()> {
    let path = registry_path()?;
    if path.is_file() {
        return Ok(());
    }
    let recent = feanorfs_client::list_recent_workspaces()?;
    let mut workspaces = Vec::new();
    for entry in recent.workspaces {
        let workspace = Path::new(&entry.path);
        if workspace.is_dir()
            && feanorfs_agent_core::workspace_is_configured(workspace)
            && !workspaces
                .iter()
                .any(|path: &CanonicalWorkspacePath| path.as_str() == entry.path)
        {
            workspaces.push(CanonicalWorkspacePath::from_exact_string(entry.path));
        }
    }
    seed_registry_file_if_absent(&path, workspaces)
}

pub(super) fn seed_registry_file_if_absent(
    path: &Path,
    workspaces: Vec<CanonicalWorkspacePath>,
) -> anyhow::Result<()> {
    create_store_dir(path)?;
    let _lock = open_store_lock(path)?;
    if path.is_file() {
        return Ok(());
    }
    save_registry(
        path,
        &SupervisorRegistry {
            workspaces,
            mutation_generation: 1,
            ..SupervisorRegistry::default()
        },
    )
}
