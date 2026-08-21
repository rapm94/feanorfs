//! Conservative fallback materialization for non-Unix, non-Windows targets.
#![cfg(all(not(unix), not(windows)))]

use anyhow::{Context, Result};
use std::path::Path;
use tokio::fs;

use super::model::MaterializationDirectoryProof;
use super::{
    capture_directory_identity, open_regular_no_follow_absolute, same_open_file_identity,
    sync_directory,
};

#[cfg(all(not(unix), not(windows)))]
pub(crate) async fn same_file_identity(left: &Path, right: &Path) -> Result<bool> {
    let left = left.to_path_buf();
    let right = right.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let left = open_regular_no_follow_absolute(&left)?;
        let right = open_regular_no_follow_absolute(&right)?;
        // On Windows this compares volume serial plus file index from the
        // opened handles, never content, mtime, or a pathname.
        same_open_file_identity(&left, &right)
    })
    .await
    .context("join materialization file identity check")?
}

#[cfg(all(not(unix), not(windows)))]
pub(crate) async fn cleanup_materialization_directories(
    base: &Path,
    created_directories: &[MaterializationDirectoryProof],
) -> Result<()> {
    for proof in created_directories.iter().rev() {
        let Some(identity) = proof.identity.as_ref() else {
            // Unsupported non-Unix platforms have no exact directory identity
            // primitive; retaining the directory is the fail-closed choice.
            continue;
        };
        let current = base.join(&proof.path);
        if capture_directory_identity(&current).await?.as_ref() != Some(identity) {
            // A user replacement, rename, or other identity change means this
            // path is no longer proven to be transaction-owned.
            continue;
        }
        match fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        match fs::remove_dir(&current).await {
            Ok(()) => {
                if let Some(parent) = current.parent() {
                    sync_directory(parent).await?;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty
                        | std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
