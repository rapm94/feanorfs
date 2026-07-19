use anyhow::{Context, Result};
use feanorfs_common::is_valid_hash;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;

const KEEP_LAST: usize = 50;

pub(crate) async fn prune(base: &Path) -> Result<()> {
    let retention = crate::workspace_layout::retention_age();
    let state = crate::workspace_layout::ensure_workspace_state(base)?;
    let manifests_dir = state.join("manifests");
    let mut entries = match fs::read_dir(&manifests_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read local manifests"),
    };
    let refs = referenced_snapshots(&state).await?;
    let mut manifests = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|id| is_valid_hash(id))
        else {
            continue;
        };
        let modified = entry.metadata().await?.modified()?;
        manifests.push((modified, id.to_string(), path));
    }
    manifests.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let available: HashSet<_> = manifests.iter().map(|(_, id, _)| id.clone()).collect();
    if !refs.is_subset(&available) {
        return Ok(());
    }
    let mut live = HashSet::new();
    for (index, (modified, id, path)) in manifests.into_iter().enumerate() {
        let recent = modified.elapsed().is_ok_and(|age| age <= retention);
        if index < KEEP_LAST || recent || refs.contains(&id) {
            let manifest = fs::read_to_string(path).await?;
            live.extend(
                manifest
                    .lines()
                    .filter(|hash| is_valid_hash(hash))
                    .map(str::to_string),
            );
        } else {
            fs::remove_file(path).await?;
        }
    }
    let objects_dir = state.join("objects");
    let mut objects = match fs::read_dir(&objects_dir).await {
        Ok(objects) => objects,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read local object cache"),
    };
    while let Some(entry) = objects.next_entry().await? {
        let path = entry.path();
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if is_valid_hash(id) && !live.contains(id) {
            fs::remove_file(path).await?;
        }
    }
    Ok(())
}

async fn referenced_snapshots(state: &Path) -> Result<HashSet<String>> {
    let mut refs = HashSet::new();
    for path in [state.join("refs/workspace"), state.join("refs/last-synced")] {
        if let Ok(id) = fs::read_to_string(path).await {
            let id = id.trim();
            if is_valid_hash(id) {
                refs.insert(id.to_string());
            }
        }
    }
    let agents = state.join("agents");
    let mut entries = match fs::read_dir(agents).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(refs),
        Err(error) => return Err(error).context("read agent refs"),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path: PathBuf = entry.path().join("state/base-snapshot");
        if let Ok(id) = fs::read_to_string(path).await {
            let id = id.trim();
            if is_valid_hash(id) {
                refs.insert(id.to_string());
            }
        }
    }
    Ok(refs)
}
