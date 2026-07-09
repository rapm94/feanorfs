//! Recently opened workspace folders for the tray switcher.

use anyhow::{Context, Result};
use feanorfs_common::tray_contract::{RecentWorkspaceEntry, RecentWorkspacesResult};
use serde::{Deserialize, Serialize};
use std::fs;
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

fn load_store() -> Result<RecentStore> {
    let path = recent_path()?;
    if !path.is_file() {
        return Ok(RecentStore::default());
    }
    let content = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&content).unwrap_or_default())
}

fn save_store(store: &RecentStore) -> Result<()> {
    let path = recent_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(store)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &content)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn workspace_label(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace")
        .to_string()
}

/// Register a workspace folder in `~/.feanorfs/recent.json` and mark it active.
pub fn register_workspace(workspace_path: &Path) -> Result<()> {
    let config = crate::load_config(workspace_path)?;
    let canonical = workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.to_path_buf());
    let path_str = canonical.to_string_lossy().into_owned();
    let entry = RecentWorkspaceEntry {
        path: path_str.clone(),
        workspace_id: config.workspace_id.clone(),
        label: workspace_label(&canonical),
    };

    let mut store = load_store()?;
    store.workspaces.retain(|w| w.path != path_str);
    store.workspaces.insert(0, entry);
    store.workspaces.truncate(MAX_RECENT);
    store.active = Some(path_str);
    save_store(&store)
}

pub fn set_active_workspace(workspace_path: &Path) -> Result<()> {
    let canonical = workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.to_path_buf());
    let path_str = canonical.to_string_lossy().into_owned();
    let mut store = load_store()?;
    if !store.workspaces.iter().any(|w| w.path == path_str) {
        register_workspace(workspace_path)?;
        return Ok(());
    }
    store.active = Some(path_str);
    save_store(&store)
}

pub fn list_recent_workspaces() -> Result<RecentWorkspacesResult> {
    let store = load_store()?;
    Ok(RecentWorkspacesResult {
        active: store.active,
        workspaces: store.workspaces,
    })
}
