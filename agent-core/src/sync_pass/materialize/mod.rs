//! Shared materialization orchestration and platform dispatch for
//! activation and recovery.

use crate::crypto::seal;
use crate::ctx::SyncCtx;
use crate::local::CacheEntry;
use anyhow::{Context, Result};
use feanorfs_common::{is_safe_rel_path, FileState, SyncResponse};
use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;
use tokio::fs;

#[cfg(windows)]
use windows::{open_directory_no_follow_absolute, windows_file_identity};

mod model;
pub(crate) mod portable;
pub(crate) mod unix;
pub(crate) mod windows;

pub(crate) use model::*;
#[cfg(all(not(unix), not(windows)))]
pub(crate) use portable::cleanup_materialization_directories;
#[cfg(unix)]
pub(crate) use unix::open_materialization_anchors;
#[cfg(windows)]
pub(crate) use windows::cleanup_materialization_directories;

#[cfg(not(unix))]
use super::download::MAX_MATERIALIZATION_DIRECTORY_PROOFS;
use super::negotiate::preflight_download_projection;
#[cfg(not(unix))]
use super::rollback::write_materialization_journal;

pub(crate) async fn sync_directory(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    let display_path = path.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        #[cfg(windows)]
        let directory = {
            use std::os::windows::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1 | 0x2 | 0x4)
                .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
                .open(&path)?
        };
        #[cfg(windows)]
        {
            // Windows has no portable directory-entry fsync: FlushFileBuffers
            // on a directory handle returns ERROR_ACCESS_DENIED. Files and the
            // materialization journal are flushed before publication. Keep the
            // directory open/metadata check so missing or inaccessible paths
            // still fail closed.
            if !directory.metadata()?.is_dir() {
                return Err(std::io::Error::other(
                    "directory sync target is not a directory",
                ));
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let directory = std::fs::File::open(&path)?;
            directory.sync_all()
        }
    })
    .await
    .context("join directory sync task")?
    .with_context(|| format!("sync directory {}", display_path.display()))
}

pub(crate) async fn sync_directory_chain(start: &Path, root: &Path) -> Result<()> {
    anyhow::ensure!(
        start.starts_with(root),
        "directory sync escaped transaction root"
    );
    let mut current = start.to_path_buf();
    loop {
        sync_directory(&current).await?;
        if current == root {
            return Ok(());
        }
        anyhow::ensure!(current.pop(), "directory sync escaped transaction root");
    }
}

#[cfg(not(unix))]
pub(crate) async fn sync_file(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let file = fs::OpenOptions::new()
        // FlushFileBuffers requires a write-capable handle on Windows. All
        // callers flush before restoring a placeholder's read-only bit.
        .read(true)
        .write(true)
        .open(path)
        .await;
    #[cfg(not(windows))]
    let file = fs::File::open(path).await;
    file.with_context(|| format!("open {} for durability sync", path.display()))?
        .sync_all()
        .await
        .with_context(|| format!("sync {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) async fn open_materialization_anchors(
    _base: &Path,
    _stage: &Path,
) -> Result<MaterializationAnchors> {
    Ok(MaterializationAnchors {})
}

pub(crate) fn same_open_file_identity(left: &std::fs::File, right: &std::fs::File) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let left = left.metadata()?;
        let right = right.metadata()?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(windows)]
    {
        Ok(windows_file_identity(left)? == windows_file_identity(right)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Ok(false)
    }
}

pub(crate) fn open_regular_no_follow_absolute(path: &Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        anyhow::ensure!(file.metadata()?.is_file(), "path is not a regular file");
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(0x0020_0000)
            .open(path)?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file() && metadata.file_attributes() & 0x0000_0400 == 0,
            "path is not a non-reparse regular file"
        );
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        anyhow::bail!("stable placeholder identity is unsupported on this platform")
    }
}

#[cfg(not(unix))]
pub(crate) async fn capture_directory_identity(
    path: &Path,
) -> Result<Option<MaterializationDirectoryIdentity>> {
    #[cfg(windows)]
    {
        let path = path.to_path_buf();
        return tokio::task::spawn_blocking(move || {
            let directory = open_directory_no_follow_absolute(&path)?;
            Ok(Some(windows_file_identity(&directory)?))
        })
        .await
        .context("join materialization directory identity check")?;
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(None)
    }
}

pub(crate) async fn revalidate_materialization_inputs(
    ctx: &SyncCtx<'_>,
    response: &SyncResponse,
    local_files: &HashMap<String, FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
    placeholder_anchors: &HashMap<String, std::fs::File>,
) -> Result<()> {
    let read_root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    let mut touched = response
        .delete_local
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    touched.extend(
        response
            .download_required
            .iter()
            .map(|item| item.path.as_str()),
    );
    for path in touched {
        revalidate_materialization_input_at(
            ctx,
            path,
            &ctx.base.join(path),
            local_files.get(path),
            cached_entries,
            placeholder_anchors.get(path),
            &read_root,
        )
        .await?;
    }
    preflight_download_projection(ctx.base, local_files, response).await
}

pub(crate) async fn revalidate_materialization_input_at(
    ctx: &SyncCtx<'_>,
    path: &str,
    actual_path: &Path,
    expected: Option<&FileState>,
    cached_entries: &HashMap<String, CacheEntry>,
    placeholder_anchor: Option<&std::fs::File>,
    read_root: &crate::workspace_read::WorkspaceReadRoot,
) -> Result<()> {
    match expected {
        Some(expected) if !expected.deleted => {
            if let Some(placeholder) = cached_entries
                .get(path)
                .filter(|entry| !entry.hydrated && entry.encrypted_hash == expected.hash)
            {
                let anchor = placeholder_anchor
                    .with_context(|| format!("missing staged placeholder identity for {path}"))?;
                let worktree_path = ctx.base.join(path);
                let current = if actual_path == worktree_path {
                    read_root.open_regular(path).with_context(|| {
                        format!("reopen local placeholder {path} before activation")
                    })?
                } else {
                    open_regular_no_follow_absolute(actual_path).with_context(|| {
                        format!("reopen moved local placeholder {path} before activation")
                    })?
                };
                if !same_open_file_identity(anchor, &current)? {
                    anyhow::bail!(
                        "local placeholder {path} was replaced while downloads were staged"
                    );
                }
                let metadata = current.metadata()?;
                let observed_mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|duration| i64::try_from(duration.as_millis()).ok())
                    .unwrap_or(0);
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.len() != 0
                    || !materialization_mode_matches(portable_mode(&metadata), expected.mode)
                    || !metadata.permissions().readonly()
                    || observed_mtime != placeholder.mtime
                {
                    return Err(crate::agent::continuous::retryable_volatility_failure(
                        format!("local placeholder {path} changed while downloads were staged"),
                    ));
                }
            } else {
                let actual = fingerprint_path(ctx, actual_path, path, expected.size).await?;
                if actual.hash != expected.hash
                    || actual.size != expected.size
                    || actual.mode != expected.mode
                {
                    return Err(crate::agent::continuous::retryable_volatility_failure(
                        format!("local path {path} changed while downloads were staged"),
                    ));
                }
            }
        }
        _ => match fs::symlink_metadata(actual_path).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => anyhow::bail!("untracked local path {path} appeared during download"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

pub(crate) fn materialization_mode_matches(observed: u32, expected: u32) -> bool {
    #[cfg(unix)]
    {
        observed == expected
    }
    #[cfg(not(unix))]
    {
        let _ = (observed, expected);
        true
    }
}

pub(crate) fn portable_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 != 0 {
            return feanorfs_common::EXECUTABLE_MODE;
        }
    }
    0
}

#[cfg(not(unix))]
pub(crate) async fn ensure_materialization_ancestors(
    base: &Path,
    path: &str,
    stage: &Path,
    journal: &mut MaterializationJournal,
) -> Result<Vec<MaterializationDirectoryProof>> {
    anyhow::ensure!(is_safe_rel_path(path), "unsafe materialization path {path}");
    let components = path.split('/').collect::<Vec<_>>();
    let mut current = base.to_path_buf();
    let mut relative = String::new();
    let mut created = Vec::new();

    for component in components.iter().take(components.len().saturating_sub(1)) {
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(component);
        current.push(component);
        let existing = match fs::symlink_metadata(&current).await {
            Ok(metadata) => Some(metadata),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                None
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(metadata) = existing {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                continue;
            }
            anyhow::bail!(
                "materialization path {path} traverses a non-directory or symlink ancestor"
            );
        }

        if journal.created_directories.len() >= MAX_MATERIALIZATION_DIRECTORY_PROOFS {
            anyhow::bail!("materialization directory proof count exceeds bound");
        }

        match fs::create_dir(&current).await {
            Ok(()) => {
                // Capture immediately after creation. If the platform cannot
                // provide an exact identity (unsupported non-Unix target),
                // keep the proof explicitly unowned so rollback will not
                // guess at ownership. Errors leave the just-created directory
                // unowned as well and abort activation.
                let identity = capture_directory_identity(&current)
                    .await
                    .with_context(|| {
                        format!("capture created materialization directory {relative}")
                    })?;
                let proof = MaterializationDirectoryProof {
                    path: relative.clone(),
                    identity,
                };
                journal.created_directories.push(proof.clone());
                // Persist every proof as soon as it is captured. A later
                // ancestor failure must not make earlier transaction-owned
                // directories unknowable to crash recovery.
                write_materialization_journal(stage, journal).await?;
                created.push(proof);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A creator racing this activation owns the namespace entry;
                // re-inspect it and deliberately record no proof. This is the
                // key boundary that prevents rollback from deleting a user's
                // directory merely because it appeared after the first check.
                match fs::symlink_metadata(&current).await {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Ok(_) => anyhow::bail!(
                        "materialization path {path} traverses a non-directory or symlink ancestor"
                    ),
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create materialization directory {relative}"))
            }
        }
    }
    Ok(created)
}

pub(crate) async fn fingerprint_local_path(
    ctx: &SyncCtx<'_>,
    path: &str,
    size: u64,
) -> Result<FileState> {
    fingerprint_path(ctx, &ctx.base.join(path), path, size).await
}

pub(crate) async fn fingerprint_path(
    ctx: &SyncCtx<'_>,
    actual_path: &Path,
    logical_path: &str,
    size: u64,
) -> Result<FileState> {
    let relative = actual_path.strip_prefix(ctx.base).with_context(|| {
        format!(
            "local path {} is outside its workspace read root",
            actual_path.display()
        )
    })?;
    let root = crate::workspace_read::WorkspaceReadRoot::open(ctx.base)?;
    let mut file = root.open_regular_path(relative).with_context(|| {
        format!("open local path {logical_path} through its workspace read root")
    })?;
    let metadata = file.metadata()?;
    let hash = if crate::large_file::uses_chunk_transport(size) {
        crate::large_file::fingerprint_opened(&mut file, ctx.password_str(), logical_path)?
            .encrypted_hash
    } else {
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(crate::large_file::CHUNK_THRESHOLD_BYTES as usize)
                .min(crate::large_file::CHUNK_THRESHOLD_BYTES as usize),
        );
        (&mut file)
            .take(crate::large_file::CHUNK_THRESHOLD_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > crate::large_file::CHUNK_THRESHOLD_BYTES {
            anyhow::bail!("local path {logical_path} grew while it was being read");
        }
        seal(&bytes, ctx.password_str(), logical_path)?.0
    };
    let after = file.metadata()?;
    if metadata.len() != after.len() || metadata.modified().ok() != after.modified().ok() {
        return Err(crate::agent::continuous::retryable_volatility_failure(
            format!("local path {logical_path} changed while it was being read"),
        ));
    }
    Ok(FileState {
        path: logical_path.to_string(),
        hash,
        size: metadata.len(),
        mtime: 0,
        deleted: false,
        mode: portable_mode(&metadata),
    })
}

pub(crate) async fn validate_worktree_ancestors(base: &Path, path: &str) -> Result<()> {
    anyhow::ensure!(is_safe_rel_path(path), "unsafe materialization path {path}");
    let mut current = base.to_path_buf();
    let components = path.split('/').collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component);
        match fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => anyhow::bail!(
                "materialization path {path} traverses a non-directory or symlink ancestor"
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                break
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) async fn remove_empty_destination_tree(destination: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(destination).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mut pending = vec![destination.to_path_buf()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        directories.push(directory.clone());
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = fs::symlink_metadata(entry.path()).await?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(entry.path());
            } else {
                anyhow::bail!(
                    "refusing to replace non-empty directory {}",
                    destination.display()
                );
            }
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        fs::remove_dir(&directory)
            .await
            .with_context(|| format!("remove empty directory {}", directory.display()))?;
        if let Some(parent) = directory.parent() {
            sync_directory(parent).await?;
        }
    }
    Ok(())
}
