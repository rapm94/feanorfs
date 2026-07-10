use anyhow::{Context, Result};
use feanorfs_common::is_valid_hash;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;

const KEEP_LAST: usize = 50;
const RETENTION: Duration = Duration::from_secs(30 * 86_400);

pub(crate) async fn prune(base: &Path) -> Result<()> {
    let manifests_dir = base.join(".feanorfs/manifests");
    let mut entries = match fs::read_dir(&manifests_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read local manifests"),
    };
    let refs = referenced_snapshots(base).await?;
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
        let recent = modified.elapsed().is_ok_and(|age| age <= RETENTION);
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
    let objects_dir = base.join(".feanorfs/objects");
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

async fn referenced_snapshots(base: &Path) -> Result<HashSet<String>> {
    let mut refs = HashSet::new();
    for path in [
        base.join(".feanorfs/refs/workspace"),
        base.join(".feanorfs/refs/last-synced"),
    ] {
        if let Ok(id) = fs::read_to_string(path).await {
            let id = id.trim();
            if is_valid_hash(id) {
                refs.insert(id.to_string());
            }
        }
    }
    let agents = base.join(".feanorfs/agents");
    let mut entries = match fs::read_dir(agents).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(refs),
        Err(error) => return Err(error).context("read agent refs"),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path: PathBuf = entry.path().join(".feanorfs/base-snapshot");
        if let Ok(id) = fs::read_to_string(path).await {
            let id = id.trim();
            if is_valid_hash(id) {
                refs.insert(id.to_string());
            }
        }
    }
    Ok(refs)
}
