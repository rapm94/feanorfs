//! Unix openat/no-follow activation and recovery primitives.
#![cfg(unix)]

use anyhow::{Context, Result};
use std::path::Path;

use super::model::{
    CreatedMaterializationDirectory, JournalDownload, MaterializationAnchors,
    MaterializationBackup, PublishedDownload,
};

#[cfg(unix)]
fn open_directory_at_tracked(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    create: bool,
) -> std::io::Result<(std::fs::File, bool)> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated and `parent` remains open for the call.
    let mut fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    let mut created_here = false;
    if fd < 0 && create && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        // SAFETY: arguments are valid for the duration of this call. The mode
        // is filtered by the process umask just like `create_dir_all`.
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) };
        if created == 0 {
            created_here = true;
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        // SAFETY: same valid parent/name pair as above.
        fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor.
    Ok((unsafe { std::fs::File::from_raw_fd(fd) }, created_here))
}

#[cfg(unix)]
fn open_directory_at(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    create: bool,
) -> std::io::Result<std::fs::File> {
    open_directory_at_tracked(parent, name, create).map(|(directory, _)| directory)
}

#[cfg(unix)]
fn open_regular_at(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    write: bool,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let access = if write { libc::O_RDWR } else { libc::O_RDONLY };
    // SAFETY: `name` is NUL-terminated and `parent` remains open for the call.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            access | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other(
            "materialization entry is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn portable_component(value: &str) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(value.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(unix)]
struct RelativeParentAt {
    parent: std::fs::File,
    final_name: std::ffi::CString,
}

#[cfg(unix)]
fn open_materialization_anchors_blocking(
    base: &Path,
    stage: &Path,
) -> std::io::Result<MaterializationAnchors> {
    use std::os::unix::fs::OpenOptionsExt as _;

    if stage.parent() != Some(base) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization stage is not directly beneath the workspace",
        ));
    }
    let canonical_base = std::fs::canonicalize(base)?;
    let base_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(canonical_base)?;
    let stage_name = stage
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| std::io::Error::other("materialization stage name is not UTF-8"))?;
    let stage_name = portable_component(stage_name)?;
    let stage_file = open_directory_at(&base_file, &stage_name, false)?;
    Ok(MaterializationAnchors {
        base: base_file,
        stage: stage_file,
    })
}

#[cfg(unix)]
pub(crate) async fn open_materialization_anchors(
    base: &Path,
    stage: &Path,
) -> Result<MaterializationAnchors> {
    let base = base.to_path_buf();
    let stage = stage.to_path_buf();
    tokio::task::spawn_blocking(move || open_materialization_anchors_blocking(&base, &stage))
        .await
        .context("join materialization anchor task")?
        .context("open no-follow materialization anchors")
}

#[cfg(unix)]
fn open_relative_parent_at(
    root: &std::fs::File,
    relative: &str,
    create: bool,
) -> std::io::Result<RelativeParentAt> {
    let mut chain = vec![root.try_clone()?];
    let mut components = relative.split('/').peekable();
    let mut final_name = None;
    while let Some(component) = components.next() {
        let component = portable_component(component)?;
        if components.peek().is_none() {
            final_name = Some(component);
            break;
        }
        let parent = chain
            .last()
            .ok_or_else(|| std::io::Error::other("missing relative parent"))?;
        chain.push(open_directory_at(parent, &component, create)?);
    }
    let final_name = final_name
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty path"))?;
    let parent = chain
        .last()
        .ok_or_else(|| std::io::Error::other("missing relative parent"))?
        .try_clone()?;
    Ok(RelativeParentAt { parent, final_name })
}

#[cfg(unix)]
fn same_open_file(left: &std::fs::File, right: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(unix)]
pub(crate) fn unlink_regular_at(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
    expected: &std::fs::File,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let current = open_regular_at(parent, name, false)?;
    if !same_open_file(&current, expected)? {
        return Err(std::io::Error::other(
            "materialized destination identity changed",
        ));
    }
    // SAFETY: the descriptor and NUL-terminated component remain valid.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    parent.sync_all()
}

#[cfg(unix)]
pub(crate) fn remove_created_directories_at(
    directories: &[CreatedMaterializationDirectory],
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    for created in directories.iter().rev() {
        let current = match open_directory_at(&created.parent, &created.name, false) {
            Ok(current) => current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !same_open_file(&current, &created.directory)? {
            return Err(std::io::Error::other(format!(
                "created materialization directory {} changed identity",
                created.path
            )));
        }
        // SAFETY: this removes only the proven directory entry from its opened parent.
        if unsafe {
            libc::unlinkat(
                created.parent.as_raw_fd(),
                created.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.kind(),
                std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
            ) {
                continue;
            }
            return Err(error);
        }
        created.parent.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn backup_original_no_follow_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::AsRawFd as _;

    let original = open_relative_parent_at(&base, relative, false)?;
    let backup_name = portable_component("backup")?;
    let backup_root = open_directory_at(&stage, &backup_name, true)?;
    let backup = open_relative_parent_at(&backup_root, relative, true)?;
    // SAFETY: both directory descriptors and the shared final component remain valid.
    if unsafe {
        libc::renameat(
            original.parent.as_raw_fd(),
            original.final_name.as_ptr(),
            backup.parent.as_raw_fd(),
            backup.final_name.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok((original.parent, backup.parent))
}

#[cfg(unix)]
pub(crate) async fn backup_original_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<(std::fs::File, std::fs::File)> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || backup_original_no_follow_blocking(base, stage, &relative))
        .await
        .context("join no-follow backup task")?
        .with_context(|| format!("back up local path {display}"))
}

#[cfg(unix)]
pub(crate) async fn sync_backup_directories(backup: &MaterializationBackup) -> Result<()> {
    let original = backup.original_parent.try_clone()?;
    let staged = backup.backup_parent.try_clone()?;
    tokio::task::spawn_blocking(move || {
        original.sync_all()?;
        staged.sync_all()
    })
    .await
    .context("join backup directory sync task")?
    .context("sync no-follow backup directories")
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupRecoveryState {
    Missing,
    DestinationMissing,
    AlreadyRestored,
    DestinationOccupied,
}

#[cfg(unix)]
fn inspect_backup_recovery_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<BackupRecoveryState> {
    use std::os::fd::AsRawFd as _;

    let backup_name = portable_component("backup")?;
    let backup_root = match open_directory_at(&stage, &backup_name, false) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackupRecoveryState::Missing)
        }
        Err(error) => return Err(error),
    };
    let backup = match open_relative_parent_at(&backup_root, relative, false) {
        Ok(backup) => backup,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(BackupRecoveryState::Missing)
        }
        Err(error) => return Err(error),
    };
    let source = open_regular_at(&backup.parent, &backup.final_name, false)?;
    let original = match open_relative_parent_at(&base, relative, false) {
        Ok(original) => original,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(BackupRecoveryState::DestinationMissing)
        }
        Err(error) => return Err(error),
    };
    match open_regular_at(&original.parent, &original.final_name, false) {
        Ok(current) if same_open_file(&source, &current)? => {
            // SAFETY: remove only the redundant staged hard link.
            if unsafe { libc::unlinkat(backup.parent.as_raw_fd(), backup.final_name.as_ptr(), 0) }
                != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            backup.parent.sync_all()?;
            Ok(BackupRecoveryState::AlreadyRestored)
        }
        Ok(_) => Ok(BackupRecoveryState::DestinationOccupied),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(BackupRecoveryState::DestinationMissing)
        }
        // Any non-missing entry that cannot be opened as the backed-up regular
        // file is occupied. Recovery will remove it only if it is a proven
        // empty transaction-created directory; symlinks and special files stay.
        Err(_) => Ok(BackupRecoveryState::DestinationOccupied),
    }
}

#[cfg(unix)]
pub(crate) async fn inspect_backup_recovery(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<BackupRecoveryState> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    tokio::task::spawn_blocking(move || inspect_backup_recovery_blocking(base, stage, &relative))
        .await
        .context("join no-follow backup inspection task")?
        .context("inspect no-follow materialization backup")
}

#[cfg(unix)]
fn restore_backup_no_follow_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd as _;

    let backup_name = portable_component("backup")?;
    let backup_root = open_directory_at(&stage, &backup_name, false)?;
    let backup = open_relative_parent_at(&backup_root, relative, false)?;
    let original = open_relative_parent_at(&base, relative, true)?;
    let source = open_regular_at(&backup.parent, &backup.final_name, false)?;
    // No-clobber restoration: a concurrent entry is retained and recovery fails closed.
    if unsafe {
        libc::linkat(
            backup.parent.as_raw_fd(),
            backup.final_name.as_ptr(),
            original.parent.as_raw_fd(),
            original.final_name.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let restored = match open_regular_at(&original.parent, &original.final_name, false) {
        Ok(restored) => restored,
        Err(error) => {
            // SAFETY: remove only the entry just linked into the anchored parent.
            unsafe {
                libc::unlinkat(original.parent.as_raw_fd(), original.final_name.as_ptr(), 0);
            }
            return Err(error);
        }
    };
    if !same_open_file(&source, &restored)? {
        // SAFETY: same anchored destination as above.
        unsafe {
            libc::unlinkat(original.parent.as_raw_fd(), original.final_name.as_ptr(), 0);
        }
        return Err(std::io::Error::other("restored backup identity changed"));
    }
    original.parent.sync_all()?;
    // SAFETY: the backup identity remains opened and the name is anchored.
    if unsafe { libc::unlinkat(backup.parent.as_raw_fd(), backup.final_name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    backup.parent.sync_all()?;
    Ok(restored)
}

#[cfg(unix)]
pub(crate) async fn restore_backup_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<std::fs::File> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || restore_backup_no_follow_blocking(base, stage, &relative))
        .await
        .context("join no-follow backup restoration task")?
        .with_context(|| format!("restore materialization backup {display}"))
}

#[cfg(unix)]
fn remove_current_regular_no_follow_blocking(
    base: std::fs::File,
    relative: &str,
) -> std::io::Result<bool> {
    let destination = match open_relative_parent_at(&base, relative, false) {
        Ok(destination) => destination,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let current = open_regular_at(&destination.parent, &destination.final_name, false)?;
    unlink_regular_at(&destination.parent, &destination.final_name, &current)?;
    Ok(true)
}

#[cfg(unix)]
pub(crate) async fn remove_current_regular_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<bool> {
    let base = anchors.base.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || remove_current_regular_no_follow_blocking(base, &relative))
        .await
        .context("join no-follow current-file removal task")?
        .with_context(|| format!("remove interrupted materialization {display}"))
}

#[cfg(unix)]
fn remove_recovered_publication_no_follow_blocking(
    base: std::fs::File,
    stage: std::fs::File,
    relative: &str,
) -> std::io::Result<bool> {
    let new_name = portable_component("new")?;
    let new_root = match open_directory_at(&stage, &new_name, false) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let staged = match open_relative_parent_at(&new_root, relative, false) {
        Ok(staged) => staged,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let destination = match open_relative_parent_at(&base, relative, false) {
        Ok(destination) => destination,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let source = open_regular_at(&staged.parent, &staged.final_name, false)?;
    let current = open_regular_at(&destination.parent, &destination.final_name, false)?;
    if !same_open_file(&source, &current)? {
        return Ok(false);
    }
    unlink_regular_at(&destination.parent, &destination.final_name, &source)?;
    Ok(true)
}

#[cfg(unix)]
pub(crate) async fn remove_recovered_publication_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<bool> {
    let base = anchors.base.try_clone()?;
    let stage = anchors.stage.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || {
        remove_recovered_publication_no_follow_blocking(base, stage, &relative)
    })
    .await
    .context("join no-follow recovered publication task")?
    .with_context(|| format!("remove interrupted materialization {display}"))
}

#[cfg(unix)]
fn remove_empty_relative_directory_at(
    base: std::fs::File,
    relative: &str,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    let target = match open_relative_parent_at(&base, relative, false) {
        Ok(target) => target,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let directory = match open_directory_at(&target.parent, &target.final_name, false) {
        Ok(directory) => directory,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let current = open_directory_at(&target.parent, &target.final_name, false)?;
    if !same_open_file(&directory, &current)? {
        return Err(std::io::Error::other(
            "materialization directory changed identity",
        ));
    }
    // SAFETY: remove only this opened, empty directory from its anchored parent.
    if unsafe {
        libc::unlinkat(
            target.parent.as_raw_fd(),
            target.final_name.as_ptr(),
            libc::AT_REMOVEDIR,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
        ) {
            return Ok(false);
        }
        return Err(error);
    }
    target.parent.sync_all()?;
    Ok(true)
}

#[cfg(unix)]
async fn remove_empty_directory_no_follow(
    anchors: &MaterializationAnchors,
    relative: &str,
) -> Result<bool> {
    let base = anchors.base.try_clone()?;
    let relative = relative.to_string();
    let display = relative.clone();
    tokio::task::spawn_blocking(move || remove_empty_relative_directory_at(base, &relative))
        .await
        .context("join no-follow directory removal task")?
        .with_context(|| format!("remove empty materialization directory {display}"))
}

#[cfg(unix)]
pub(crate) async fn remove_created_descendants_for_backup(
    anchors: &MaterializationAnchors,
    backup_path: &str,
    downloads: &[JournalDownload],
) -> Result<()> {
    let prefix = format!("{backup_path}/");
    let mut directories = std::collections::BTreeSet::new();
    for item in downloads {
        if !item.file.path.starts_with(&prefix) {
            continue;
        }
        let mut current = item.file.path.as_str();
        while let Some((parent, _)) = current.rsplit_once('/') {
            if parent.len() < backup_path.len() {
                break;
            }
            directories.insert(parent.to_string());
            if parent == backup_path {
                break;
            }
            current = parent;
        }
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| left.cmp(right))
    });
    for directory in directories {
        let _ = remove_empty_directory_no_follow(anchors, &directory).await?;
    }
    let _ = remove_empty_directory_no_follow(anchors, backup_path).await?;
    Ok(())
}

#[cfg(unix)]
fn publish_staged_no_follow_blocking(
    base: &Path,
    stage: &Path,
    relative: &str,
) -> std::io::Result<(
    std::fs::File,
    Vec<std::fs::File>,
    Vec<CreatedMaterializationDirectory>,
)> {
    use std::os::fd::AsRawFd as _;

    let anchors = open_materialization_anchors_blocking(base, stage)?;
    let new_name = portable_component("new")?;
    let mut source_parent = open_directory_at(&anchors.stage, &new_name, false)?;
    let mut destination_chain = vec![anchors.base];
    let mut created_directories = Vec::new();
    let result = (|| {
        let mut components = relative.split('/').peekable();
        let mut final_name = None;
        let mut prefix = String::new();
        while let Some(component_text) = components.next() {
            let component = portable_component(component_text)?;
            if components.peek().is_none() {
                final_name = Some(component);
                break;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component_text);
            source_parent = open_directory_at(&source_parent, &component, false)?;
            let destination_parent = destination_chain
                .last()
                .ok_or_else(|| std::io::Error::other("missing destination directory"))?;
            let (directory, created) =
                open_directory_at_tracked(destination_parent, &component, true).map_err(
                    |error| {
                        std::io::Error::new(
                            error.kind(),
                            format!("no-follow destination ancestor rejected: {error}"),
                        )
                    },
                )?;
            if created {
                created_directories.push(CreatedMaterializationDirectory {
                    parent: destination_parent.try_clone()?,
                    directory: directory.try_clone()?,
                    name: component.clone(),
                    path: prefix.clone(),
                });
            }
            destination_chain.push(directory);
        }
        let final_name = final_name
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty path"))?;
        let source = open_regular_at(&source_parent, &final_name, false)?;
        let destination_parent = destination_chain
            .last()
            .ok_or_else(|| std::io::Error::other("missing destination directory"))?;

        // SAFETY: both descriptors and the NUL-terminated component remain valid.
        if unsafe {
            libc::linkat(
                source_parent.as_raw_fd(),
                final_name.as_ptr(),
                destination_parent.as_raw_fd(),
                final_name.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let destination = match open_regular_at(destination_parent, &final_name, true) {
            Ok(file) => file,
            Err(error) => {
                // SAFETY: removes only the entry in the opened destination directory.
                unsafe {
                    libc::unlinkat(destination_parent.as_raw_fd(), final_name.as_ptr(), 0);
                }
                return Err(error);
            }
        };
        if !same_open_file(&source, &destination)? {
            // SAFETY: same anchored destination as above.
            unsafe {
                libc::unlinkat(destination_parent.as_raw_fd(), final_name.as_ptr(), 0);
            }
            return Err(std::io::Error::other(
                "staged download changed during no-follow publication",
            ));
        }
        Ok(destination)
    })();
    match result {
        Ok(destination) => Ok((destination, destination_chain, created_directories)),
        Err(error) => {
            let _ = remove_created_directories_at(&created_directories);
            Err(error)
        }
    }
}

#[cfg(unix)]
pub(crate) async fn publish_staged_no_follow(
    base: &Path,
    stage: &Path,
    relative: &str,
) -> Result<(
    std::fs::File,
    Vec<std::fs::File>,
    Vec<CreatedMaterializationDirectory>,
)> {
    let base = base.to_path_buf();
    let stage = stage.to_path_buf();
    let relative = relative.to_string();
    let display_relative = relative.clone();
    tokio::task::spawn_blocking(move || publish_staged_no_follow_blocking(&base, &stage, &relative))
        .await
        .context("join no-follow publication task")?
        .with_context(|| format!("publish staged download {display_relative}"))
}

#[cfg(unix)]
pub(crate) async fn sync_published_directories(published: &PublishedDownload) -> Result<()> {
    let directories = published
        .directory_chain
        .iter()
        .map(std::fs::File::try_clone)
        .collect::<std::io::Result<Vec<_>>>()?;
    tokio::task::spawn_blocking(move || {
        for directory in directories.iter().rev() {
            directory.sync_all()?;
        }
        Ok::<_, std::io::Error>(())
    })
    .await
    .context("join published-directory sync task")??;
    Ok(())
}

#[cfg(unix)]
pub(crate) async fn apply_materialized_file_state(
    file: &std::fs::File,
    mode: u32,
    readonly: bool,
) -> Result<i64> {
    let file = file.try_clone()?;
    tokio::task::spawn_blocking(move || -> Result<i64> {
        use std::os::unix::fs::PermissionsExt as _;

        let metadata = file.metadata()?;
        let mut permissions = metadata.permissions();
        let current = permissions.mode();
        let updated = if mode == feanorfs_common::EXECUTABLE_MODE {
            current | 0o111
        } else {
            current & !0o111
        };
        permissions.set_mode(updated);
        if readonly {
            permissions.set_readonly(true);
        }
        file.set_permissions(permissions)?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        Ok(metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(0))
    })
    .await
    .context("join materialized-file sync task")?
}
