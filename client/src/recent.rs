//! Recently opened workspace folders for the tray switcher.

use crate::workspace_path::CanonicalWorkspacePath;
use anyhow::{Context, Result};
use feanorfs_common::tray_contract::{RecentWorkspaceEntry, RecentWorkspacesResult};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 12;
const MAX_RECENT_STATE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RecentStore {
    #[serde(default)]
    workspaces: Vec<RecentWorkspaceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<String>,
}

fn recent_path() -> Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join("recent.json"))
}

fn create_store_dir(path: &Path) -> Result<()> {
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

fn open_store_lock(path: &Path) -> Result<File> {
    open_store_lock_with(path, false)
}

fn open_store_lock_shared(path: &Path) -> Result<File> {
    open_store_lock_with(path, true)
}

fn try_open_store_lock_shared(path: &Path) -> Result<Option<File>> {
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
        .with_context(|| format!("open recent-workspace lock {}", lock_path.display()))?;
    match fs2::FileExt::try_lock_shared(&lock) {
        Ok(()) => Ok(Some(lock)),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("lock recent workspaces {}", lock_path.display()))
        }
    }
}

fn open_store_lock_with(path: &Path, shared: bool) -> Result<File> {
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
        .with_context(|| format!("open recent-workspace lock {}", lock_path.display()))?;
    if shared {
        fs2::FileExt::lock_shared(&lock)
            .with_context(|| format!("lock recent workspaces {}", lock_path.display()))?;
    } else {
        fs2::FileExt::lock_exclusive(&lock)
            .with_context(|| format!("lock recent workspaces {}", lock_path.display()))?;
    }
    Ok(lock)
}

fn load_store(path: &Path) -> Result<RecentStore> {
    if !path.is_file() {
        return Ok(RecentStore::default());
    }
    let file =
        File::open(path).with_context(|| format!("open recent workspaces {}", path.display()))?;
    let mut content = Vec::new();
    file.take(MAX_RECENT_STATE_BYTES.saturating_add(1))
        .read_to_end(&mut content)
        .with_context(|| format!("read recent workspaces {}", path.display()))?;
    if content.len() as u64 > MAX_RECENT_STATE_BYTES {
        anyhow::bail!("recent workspace state exceeds {MAX_RECENT_STATE_BYTES} byte limit");
    }
    serde_json::from_slice(&content)
        .with_context(|| format!("parse recent workspaces {}", path.display()))
}

/// Private atomic visibility: the tray switcher registry
/// is replaced via a 0o600 temp file and atomic rename, without a
/// parent-directory sync. A crash may revert to the previous registry entry,
/// which only clears the tray's recent-folder list; concurrent writers are
/// serialized by the store lock, and reads are bounded by
/// `MAX_RECENT_STATE_BYTES`.
fn save_store(path: &Path, store: &RecentStore) -> Result<()> {
    let content = serde_json::to_string_pretty(store)?;
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(content.as_bytes())?;
    file.commit()?;
    Ok(())
}

fn update_store<T>(update: impl FnOnce(&mut RecentStore) -> T) -> Result<T> {
    let path = recent_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    let mut store = load_store(&path)?;
    let result = update(&mut store);
    save_store(&path, &store)?;
    Ok(result)
}

/// Build an exact registry entry for `workspace_path`.
///
/// The stored path is the canonical path as exact UTF-8 (never lossy), so a
/// non-UTF-8 actionable path is rejected with a typed error before it can be
/// persisted. The label is a bounded display label for the tray only.
fn workspace_entry(workspace_path: &Path) -> Result<RecentWorkspaceEntry> {
    let config = crate::load_config(workspace_path)?;
    let canonical = CanonicalWorkspacePath::canonicalize(workspace_path)?;
    Ok(RecentWorkspaceEntry {
        path: canonical.as_str().to_owned(),
        workspace_id: config.workspace_id,
        label: canonical.display_label(),
    })
}

fn remove_workspace(store: &mut RecentStore, path: &str) {
    store.workspaces.retain(|workspace| workspace.path != path);
    if store.active.as_deref() == Some(path) {
        store.active = store
            .workspaces
            .first()
            .map(|workspace| workspace.path.clone());
    }
}

fn workspace_is_available(workspace: &RecentWorkspaceEntry) -> bool {
    workspace_is_available_with(workspace, feanorfs_agent_core::workspace_is_configured)
}

fn workspace_is_available_with(
    workspace: &RecentWorkspaceEntry,
    is_configured: impl Fn(&Path) -> bool,
) -> bool {
    // The folder must actually exist: a deleted folder cannot be started even
    // when its global workspace state (config.json under ~/.feanorfs) survived.
    let path = Path::new(&workspace.path);
    path.is_dir() && is_configured(path)
}

fn forget_unavailable_from_store(store: &mut RecentStore) -> usize {
    forget_unavailable_from_store_with(store, workspace_is_available)
}

fn forget_unavailable_from_store_with(
    store: &mut RecentStore,
    is_available: impl Fn(&RecentWorkspaceEntry) -> bool,
) -> usize {
    let before = store.workspaces.len();
    store.workspaces.retain(is_available);
    if store.active.as_ref().is_some_and(|active| {
        !store
            .workspaces
            .iter()
            .any(|workspace| &workspace.path == active)
    }) {
        store.active = store
            .workspaces
            .first()
            .map(|workspace| workspace.path.clone());
    }
    before.saturating_sub(store.workspaces.len())
}

fn result_from_store(store: &RecentStore) -> RecentWorkspacesResult {
    let workspaces = store
        .workspaces
        .iter()
        .take(MAX_RECENT)
        .cloned()
        .collect::<Vec<_>>();
    let active = store.active.as_ref().and_then(|active| {
        workspaces
            .iter()
            .any(|workspace| &workspace.path == active)
            .then(|| active.clone())
    });
    RecentWorkspacesResult { active, workspaces }
}

fn list_recent_workspaces_at(path: &Path) -> Result<RecentWorkspacesResult> {
    let Some(parent) = path.parent() else {
        return Ok(result_from_store(&RecentStore::default()));
    };

    // A first read must not create the global state root, its permissions, or
    // a lock/store file. Once the root exists, the shared lock provides the
    // same linearization point as writers while preserving atomic replacement.
    match fs::symlink_metadata(parent) {
        Ok(_) => {
            let _lock = open_store_lock_shared(path)?;
            Ok(result_from_store(&load_store(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(result_from_store(&RecentStore::default()))
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect recent workspace state root {}", parent.display())),
    }
}

fn try_list_recent_workspaces_at(path: &Path) -> Result<Option<RecentWorkspacesResult>> {
    let Some(parent) = path.parent() else {
        return Ok(Some(result_from_store(&RecentStore::default())));
    };

    match fs::symlink_metadata(parent) {
        Ok(_) => {
            let Some(_lock) = try_open_store_lock_shared(path)? else {
                return Ok(None);
            };
            Ok(Some(result_from_store(&load_store(path)?)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Some(result_from_store(&RecentStore::default())))
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect recent workspace state root {}", parent.display())),
    }
}

/// Register a workspace folder in `~/.feanorfs/recent.json` and mark it active.
pub fn register_workspace(workspace_path: &Path) -> Result<()> {
    let entry = workspace_entry(workspace_path)?;
    update_store(|store| {
        store
            .workspaces
            .retain(|workspace| workspace.path != entry.path);
        store.active = Some(entry.path.clone());
        store.workspaces.insert(0, entry);
        store.workspaces.truncate(MAX_RECENT);
    })
}

pub fn set_active_workspace(workspace_path: &Path) -> Result<()> {
    let entry = workspace_entry(workspace_path)?;
    update_store(|store| {
        if !store
            .workspaces
            .iter()
            .any(|workspace| workspace.path == entry.path)
        {
            store.workspaces.insert(0, entry.clone());
            store.workspaces.truncate(MAX_RECENT);
        }
        store.active = Some(entry.path);
    })
}

/// Remove a workspace from the tray without deleting its files or FeanorFS metadata.
pub fn unregister_workspace(workspace_path: &Path) -> Result<RecentWorkspacesResult> {
    // `canonicalize_keep_raw` preserves the legacy ability to remove an entry
    // whose folder no longer exists while still rejecting non-UTF-8 paths
    // with a typed error instead of lossy-mangling the registry identity.
    let path = CanonicalWorkspacePath::canonicalize_keep_raw(workspace_path)?;
    update_store(|store| {
        remove_workspace(store, path.as_str());
        result_from_store(store)
    })
}

pub fn list_recent_workspaces() -> Result<RecentWorkspacesResult> {
    let path = recent_path()?;
    list_recent_workspaces_at(&path)
}

/// Best-effort bounded projection for recurring desktop refreshes.
///
/// A contended writer returns `Ok(None)` immediately so the tray can retain
/// its last good folder list instead of leaving the status process pending.
pub fn try_list_recent_workspaces() -> Result<Option<RecentWorkspacesResult>> {
    let path = recent_path()?;
    try_list_recent_workspaces_at(&path)
}

/// Explicitly remove tray entries whose folder is missing or whose workspace
/// config is unavailable. A deleted folder is always unavailable, even when
/// its global workspace state survives under `~/.feanorfs`.
///
/// This changes only the recent-workspace registry; it never touches workspace
/// files, credentials, services, hubs, or remote snapshots.
pub fn forget_unavailable_workspaces() -> Result<RecentWorkspacesResult> {
    update_store(|store| {
        forget_unavailable_from_store(store);
        result_from_store(store)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> RecentWorkspaceEntry {
        RecentWorkspaceEntry {
            path: path.into(),
            workspace_id: format!("id-{path}"),
            label: path.trim_start_matches('/').into(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_workspace_is_rejected_before_registry_write() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = recent_path().unwrap();
        let before = fs::read(&path).ok();
        let non_utf8 = PathBuf::from(std::ffi::OsString::from_vec(vec![b'w', 0x81]));

        let error = unregister_workspace(&non_utf8).unwrap_err();

        // The typed rejection happens before the store lock, load, or write:
        // the registry file (and any content it had) is untouched.
        assert!(error.to_string().contains("not valid UTF-8"));
        assert_eq!(fs::read(&path).ok(), before);
    }

    #[test]
    fn register_workspace_round_trips_exact_canonical_path_through_recent_json() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        crate::save_config(
            &workspace,
            &crate::Config {
                server_url: "http://127.0.0.1:1".to_string(),
                workspace_id: "recent-roundtrip-id".to_string(),
                encryption_password: Some("e".repeat(64)),
                server_password: None,
                tls_ca_pem: None,
                format_version: 3,
                hub_local: false,
                relay: None,
            },
        )
        .unwrap();

        register_workspace(&workspace).unwrap();

        // The persisted entry is the exact canonical string — nothing lossy.
        let path = recent_path().unwrap();
        let listed = list_recent_workspaces_at(&path).unwrap();
        assert_eq!(listed.active.as_deref(), Some(workspace.to_str().unwrap()));
        assert_eq!(listed.workspaces.len(), 1);
        assert_eq!(listed.workspaces[0].path, workspace.to_str().unwrap());
        assert_eq!(listed.workspaces[0].workspace_id, "recent-roundtrip-id");

        // Reloading the raw file yields the same exact entry (UTF-8 round-trip).
        let store: RecentStore = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(store.workspaces[0].path, workspace.to_str().unwrap());

        // set_active_workspace keeps the identity stable.
        set_active_workspace(&workspace).unwrap();
        let listed = list_recent_workspaces_at(&path).unwrap();
        assert_eq!(listed.active.as_deref(), Some(workspace.to_str().unwrap()));
        assert_eq!(listed.workspaces[0].path, workspace.to_str().unwrap());
    }

    #[test]
    fn removing_active_workspace_selects_next_recent_workspace() {
        let mut store = RecentStore {
            active: Some("/one".into()),
            workspaces: vec![entry("/one"), entry("/two")],
        };

        remove_workspace(&mut store, "/one");

        assert_eq!(store.active.as_deref(), Some("/two"));
        assert_eq!(store.workspaces.len(), 1);
        assert_eq!(store.workspaces[0].path, "/two");
    }

    #[test]
    fn removing_inactive_workspace_preserves_active_workspace() {
        let mut store = RecentStore {
            active: Some("/one".into()),
            workspaces: vec![entry("/one"), entry("/two")],
        };

        remove_workspace(&mut store, "/two");

        assert_eq!(store.active.as_deref(), Some("/one"));
        assert_eq!(store.workspaces.len(), 1);
        assert_eq!(store.workspaces[0].path, "/one");
    }

    #[test]
    fn removing_last_workspace_clears_active_workspace() {
        let mut store = RecentStore {
            active: Some("/one".into()),
            workspaces: vec![entry("/one")],
        };

        remove_workspace(&mut store, "/one");

        assert!(store.active.is_none());
        assert!(store.workspaces.is_empty());
    }

    #[test]
    fn malformed_recent_state_fails_instead_of_being_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recent.json");
        fs::write(&path, b"{not-json").unwrap();

        let error = list_recent_workspaces_at(&path).unwrap_err();

        assert!(error.to_string().contains("parse recent workspaces"));
        assert_eq!(fs::read(&path).unwrap(), b"{not-json");
    }

    #[test]
    fn missing_state_root_read_is_empty_without_creating_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let path = root.join("recent.json");

        let result = list_recent_workspaces_at(&path).unwrap();

        assert!(result.active.is_none());
        assert!(result.workspaces.is_empty());
        assert!(!root.exists());
        assert!(!path.exists());
        assert!(!path.with_extension("lock").exists());
    }

    #[test]
    fn listing_preserves_store_projection_and_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recent.json");
        let store = RecentStore {
            active: Some("/second".into()),
            workspaces: vec![entry("/second"), entry("/first")],
        };
        fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();

        let listed = list_recent_workspaces_at(&path).unwrap();

        assert_eq!(listed.active.as_deref(), store.active.as_deref());
        assert_eq!(listed.workspaces.len(), store.workspaces.len());
        for (listed, stored) in listed.workspaces.iter().zip(store.workspaces.iter()) {
            assert_eq!(listed.path, stored.path);
            assert_eq!(listed.workspace_id, stored.workspace_id);
            assert_eq!(listed.label, stored.label);
        }
    }

    #[test]
    fn listing_caps_legacy_entries_and_drops_an_out_of_projection_active_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recent.json");
        let workspaces = (0..MAX_RECENT + 3)
            .map(|index| entry(&format!("/{index}")))
            .collect::<Vec<_>>();
        let store = RecentStore {
            active: Some(format!("/{}", MAX_RECENT + 1)),
            workspaces,
        };
        fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();

        let listed = list_recent_workspaces_at(&path).unwrap();

        assert_eq!(listed.workspaces.len(), MAX_RECENT);
        assert!(listed.active.is_none());
    }

    #[test]
    fn listing_rejects_oversized_recent_state_before_parsing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recent.json");
        fs::write(&path, vec![b' '; MAX_RECENT_STATE_BYTES as usize + 1]).unwrap();

        let error = list_recent_workspaces_at(&path).unwrap_err();

        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn best_effort_listing_skips_a_contended_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recent.json");
        let _writer = open_store_lock(&path).unwrap();

        assert!(try_list_recent_workspaces_at(&path).unwrap().is_none());
    }

    #[test]
    fn shared_readers_coexist_while_exclusive_writer_is_blocked() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recent.json");
        let first_reader = open_store_lock_shared(&path).unwrap();
        let lock_path = path.with_extension("lock");
        let second_reader = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();

        assert!(fs2::FileExt::try_lock_shared(&second_reader).is_ok());

        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&writer).is_err());

        drop(first_reader);
        drop(second_reader);
        assert!(fs2::FileExt::try_lock_exclusive(&writer).is_ok());
    }

    #[test]
    fn forgetting_missing_folder_removes_entry_even_when_state_survives() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("deleted");
        fs::create_dir_all(&folder).unwrap();
        // The folder is now gone, but simulate surviving global workspace
        // state: the availability check must still treat it as unavailable
        // (a checker that claims the path is configured).
        fs::remove_dir_all(&folder).unwrap();
        let mut store = RecentStore {
            active: None,
            workspaces: vec![entry(&folder.to_string_lossy())],
        };

        let removed = forget_unavailable_from_store_with(&mut store, |workspace| {
            workspace_is_available_with(workspace, |_| true)
        });

        assert_eq!(removed, 1);
        assert!(store.workspaces.is_empty());
    }

    #[test]
    fn forgetting_existing_folder_keeps_configured_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("present");
        fs::create_dir_all(&folder).unwrap();
        let mut store = RecentStore {
            active: None,
            workspaces: vec![entry(&folder.to_string_lossy())],
        };

        let removed = forget_unavailable_from_store_with(&mut store, |workspace| {
            workspace_is_available_with(workspace, |_| true)
        });

        assert_eq!(removed, 0);
        assert_eq!(store.workspaces.len(), 1);
    }

    #[test]
    fn forgetting_unavailable_entries_preserves_configured_workspaces() {
        let directory = tempfile::tempdir().unwrap();
        let configured = directory.path().join("configured");
        fs::create_dir_all(configured.join(".feanorfs")).unwrap();
        fs::write(configured.join(".feanorfs/config.json"), b"{}").unwrap();
        let missing = directory.path().join("missing");
        let mut store = RecentStore {
            active: Some(missing.to_string_lossy().into_owned()),
            workspaces: vec![
                entry(&missing.to_string_lossy()),
                entry(&configured.to_string_lossy()),
            ],
        };

        let removed = forget_unavailable_from_store(&mut store);

        assert_eq!(removed, 1);
        assert_eq!(store.workspaces.len(), 1);
        assert_eq!(
            store.workspaces[0].path,
            configured.to_string_lossy().as_ref()
        );
        assert_eq!(
            store.active.as_deref(),
            Some(configured.to_string_lossy().as_ref())
        );
    }
}
