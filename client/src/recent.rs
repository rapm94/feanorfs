//! Recently opened workspace folders for the tray switcher.

use anyhow::{Context, Result};
use feanorfs_common::tray_contract::{RecentWorkspaceEntry, RecentWorkspacesResult};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RecentStore {
    #[serde(default)]
    workspaces: Vec<RecentWorkspaceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<String>,
}

fn recent_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME or USERPROFILE environment variable not set")?;
    Ok(PathBuf::from(home).join(".feanorfs").join("recent.json"))
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
    fs2::FileExt::lock_exclusive(&lock)
        .with_context(|| format!("lock recent workspaces {}", lock_path.display()))?;
    Ok(lock)
}

fn load_store(path: &Path) -> Result<RecentStore> {
    if !path.is_file() {
        return Ok(RecentStore::default());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("read recent workspaces {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("parse recent workspaces {}", path.display()))
}

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

fn workspace_label(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace")
        .to_string()
}

fn canonical_path_string(workspace_path: &Path) -> String {
    workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn workspace_entry(workspace_path: &Path) -> Result<RecentWorkspaceEntry> {
    let config = crate::load_config(workspace_path)?;
    let canonical = workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.to_path_buf());
    Ok(RecentWorkspaceEntry {
        path: canonical.to_string_lossy().into_owned(),
        workspace_id: config.workspace_id,
        label: workspace_label(&canonical),
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
    Path::new(&workspace.path)
        .join(".feanorfs")
        .join("config.json")
        .is_file()
}

fn forget_unavailable_from_store(store: &mut RecentStore) -> usize {
    let before = store.workspaces.len();
    store.workspaces.retain(workspace_is_available);
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
    RecentWorkspacesResult {
        active: store.active.clone(),
        workspaces: store.workspaces.clone(),
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
    let path = canonical_path_string(workspace_path);
    update_store(|store| {
        remove_workspace(store, &path);
        result_from_store(store)
    })
}

pub fn list_recent_workspaces() -> Result<RecentWorkspacesResult> {
    let path = recent_path()?;
    create_store_dir(&path)?;
    let _lock = open_store_lock(&path)?;
    Ok(result_from_store(&load_store(&path)?))
}

/// Explicitly remove tray entries whose workspace config is unavailable.
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

        let error = load_store(&path).unwrap_err();

        assert!(error.to_string().contains("parse recent workspaces"));
        assert_eq!(fs::read(&path).unwrap(), b"{not-json");
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
