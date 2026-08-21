//! Validation of the remote plan against the local projection.

use crate::ctx::SyncCtx;
use anyhow::{Context, Result};
use feanorfs_common::{FileState, SyncResponse};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::fs;

use super::SyncMode;

pub(crate) fn validate_cross_direction_structure(response: &SyncResponse) -> Result<()> {
    let remote_changes = response
        .download_required
        .iter()
        .map(|file| file.path.clone())
        .chain(response.delete_local.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    for local_path in &response.upload_required {
        if let Some(remote_path) = related_path(&remote_changes, local_path) {
            return Err(crate::agent::continuous::conflict_failure(format!(
                "concurrent file/directory conflict between local {local_path} and cloud {remote_path}; reconcile the hierarchy explicitly before syncing"
            )));
        }
    }
    Ok(())
}

fn related_path<'a>(paths: &'a std::collections::BTreeSet<String>, path: &str) -> Option<&'a str> {
    if let Some(exact) = paths.get(path) {
        return Some(exact);
    }
    for (index, _) in path.match_indices('/') {
        if let Some(ancestor) = paths.get(&path[..index]) {
            return Some(ancestor);
        }
    }
    let descendant_prefix = format!("{path}/");
    paths
        .range(descendant_prefix.clone()..)
        .next()
        .filter(|candidate| candidate.starts_with(&descendant_prefix))
        .map(String::as_str)
}

pub(crate) async fn validate_final_candidate(
    ctx: &SyncCtx<'_>,
    mode: SyncMode,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
    blocked: &HashSet<String>,
) -> Result<()> {
    let mut projected = local_files.clone();
    if mode != SyncMode::Push {
        for path in &response.delete_local {
            projected.remove(path);
        }
        for file in &response.download_required {
            projected.insert(file.path.clone(), file.clone());
        }
    }
    feanorfs_common::flat_to_tree(&projected)
        .context("sync result contains a file/directory path collision")?;
    if ctx.format_version() < 3 || mode == SyncMode::Pull {
        return Ok(());
    }
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Ok(());
    };
    let snapshots = crate::snapshot::SnapshotEngine::new(ctx);
    let state = snapshots.load_state(&head).await?;
    for path in blocked {
        match state.files.get(path) {
            Some(file) => {
                projected.insert(path.clone(), file.clone());
            }
            None => {
                projected.remove(path);
            }
        }
    }
    snapshots
        .candidate_root(&projected, &state.conflicts)
        .context("pending conflicts would create a file/directory path collision")
        .map_err(crate::agent::continuous::conflict_failure)?;
    Ok(())
}

pub(crate) async fn preflight_download_projection(
    base: &Path,
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
) -> Result<()> {
    validate_local_projection(local_files, response)?;
    let delete_paths: HashSet<&str> = response.delete_local.iter().map(String::as_str).collect();
    for replica_file in &response.download_required {
        ensure_destination_replaceable(
            base,
            &replica_file.path,
            &delete_paths,
            local_files.contains_key(&replica_file.path),
        )
        .await?;
    }
    Ok(())
}

fn validate_local_projection(
    local_files: &HashMap<String, FileState>,
    response: &SyncResponse,
) -> Result<()> {
    let mut projected = local_files.clone();
    for path in &response.delete_local {
        projected.remove(path);
    }
    for file in &response.download_required {
        projected.insert(file.path.clone(), file.clone());
    }
    feanorfs_common::flat_to_tree(&projected)
        .context("remote update would create a file/directory path collision")?;
    Ok(())
}

async fn ensure_destination_replaceable(
    base: &Path,
    path: &str,
    delete_paths: &HashSet<&str>,
    destination_was_tracked: bool,
) -> Result<()> {
    let destination = base.join(path);
    let mut prefix = String::new();
    let components: Vec<&str> = path.split('/').collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        match fs::symlink_metadata(base.join(&prefix)).await {
            Ok(metadata) => {
                if (!metadata.is_dir() || metadata.file_type().is_symlink())
                    && !delete_paths.contains(prefix.as_str())
                {
                    anyhow::bail!(
                        "cannot materialize {path}: ancestor {prefix} is a local file not scheduled for replacement"
                    );
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = match fs::symlink_metadata(&destination).await {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        anyhow::bail!("refusing to replace symlink at {path}");
    }
    if !metadata.is_dir() {
        if !destination_was_tracked {
            anyhow::bail!("refusing to replace untracked local file at {path}");
        }
        return Ok(());
    }
    let mut pending = vec![destination];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = fs::symlink_metadata(entry.path()).await?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(entry.path());
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(base)
                .context("replacement entry escaped workspace")?
                .to_str()
                .context("replacement entry is not valid UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if !delete_paths.contains(relative.as_str()) {
                anyhow::bail!(
                    "cannot replace directory {path}: untracked entry {relative} would be lost"
                );
            }
        }
    }
    Ok(())
}
