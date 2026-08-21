//! Windows handle-anchored activation and recovery primitives.
#![cfg(windows)]

use crate::crypto::seal;
use anyhow::{Context, Result};
use feanorfs_common::is_safe_rel_path;
use std::path::Path;
use tokio::fs;

use super::super::download::MAX_MATERIALIZATION_DIRECTORY_PROOFS;
use super::model::{
    JournalDownload, MaterializationDirectoryIdentity, MaterializationDirectoryProof,
    PublishedDownload,
};
use super::{materialization_mode_matches, portable_mode, same_open_file_identity};

#[cfg(windows)]
pub(crate) fn windows_file_identity(
    file: &std::fs::File,
) -> Result<MaterializationDirectoryIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file owns a valid handle and the output points to writable
    // storage for the duration of this synchronous call.
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the successful call initialized the complete structure.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    anyhow::ensure!(file_index != 0, "Windows file identity has a zero index");
    Ok(MaterializationDirectoryIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index,
    })
}

#[cfg(windows)]
fn open_materialization_file_for_delete(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization path is not a non-reparse regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_materialization_directory_for_delete(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization path is not a non-reparse directory",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn delete_materialization_handle(file: &std::fs::File) -> std::io::Result<()> {
    delete_materialization_handle_with_options(file, false)
}

#[cfg(windows)]
fn delete_materialization_handle_with_options(
    file: &std::fs::File,
    force_basic_disposition: bool,
) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, FileDispositionInfoEx, SetFileInformationByHandle,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    };

    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    if !force_basic_disposition {
        // SAFETY: the handle is owned by `file`, and the disposition structure
        // is initialized for the duration of this synchronous system call.
        let result = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfoEx,
                (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        };
        if result != 0 {
            return Ok(());
        }
    }
    let extended_error = if force_basic_disposition {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "test-forced FileDispositionInfoEx fallback",
        )
    } else {
        std::io::Error::last_os_error()
    };
    let basic = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: same valid handle and initialized fallback structure. This
    // supports filesystems that do not implement FileDispositionInfoEx.
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&basic as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result != 0 {
        return Ok(());
    }
    let fallback_error = std::io::Error::last_os_error();
    Err(std::io::Error::new(
        fallback_error.kind(),
        format!("delete disposition failed ({extended_error}); fallback failed ({fallback_error})"),
    ))
}

#[cfg(windows)]
fn verify_materialized_handle(
    file: &mut std::fs::File,
    item: &JournalDownload,
    password: &str,
    check_mode: bool,
) -> Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        anyhow::bail!("materialization destination is not a non-reparse regular file");
    }
    if !item.hydrated {
        if metadata.len() != 0
            || (check_mode && !metadata.permissions().readonly())
            || (check_mode
                && !materialization_mode_matches(portable_mode(&metadata), item.file.mode))
        {
            return Err(crate::agent::continuous::verification_failure(format!(
                "interrupted placeholder {} changed; refusing automatic recovery",
                item.file.path
            )));
        }
        return Ok(());
    }
    let encrypted_hash = if crate::large_file::uses_chunk_transport(item.file.size) {
        file.seek(SeekFrom::Start(0))?;
        crate::large_file::fingerprint_opened(file, password, &item.file.path)?.encrypted_hash
    } else {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(crate::large_file::CHUNK_THRESHOLD_BYTES as usize)
                .min(crate::large_file::CHUNK_THRESHOLD_BYTES as usize),
        );
        file.take(crate::large_file::CHUNK_THRESHOLD_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > crate::large_file::CHUNK_THRESHOLD_BYTES {
            anyhow::bail!("materialization destination grew while it was being checked");
        }
        seal(&bytes, password, &item.file.path)?.0
    };
    let after = file.metadata()?;
    if after.len() != metadata.len() {
        return Err(crate::agent::continuous::retryable_volatility_failure(
            format!(
                "interrupted materialization {} changed while it was being checked",
                item.file.path
            ),
        ));
    }
    if encrypted_hash != item.file.hash
        || after.len() != item.file.size
        || (check_mode && !materialization_mode_matches(portable_mode(&after), item.file.mode))
    {
        return Err(crate::agent::continuous::verification_failure(format!(
            "interrupted materialization {} changed; refusing automatic recovery",
            item.file.path
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn open_verified_published_file(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<std::fs::File> {
    let mut staged_file = open_materialization_file_for_delete(staged)
        .with_context(|| format!("open staged materialization {}", staged.display()))?;
    let mut destination_file = open_materialization_file_for_delete(destination)
        .with_context(|| format!("open published materialization {}", destination.display()))?;
    anyhow::ensure!(
        same_open_file_identity(&staged_file, &destination_file)?,
        "staged and published materializations do not share an identity"
    );
    verify_materialized_handle(&mut staged_file, item, password, false)?;
    verify_materialized_handle(&mut destination_file, item, password, false)?;
    Ok(destination_file)
}

#[cfg(windows)]
fn remove_verified_published_file(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<bool> {
    let mut staged_file = match open_materialization_file_for_delete(staged) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut destination_file = match open_materialization_file_for_delete(destination) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !same_open_file_identity(&staged_file, &destination_file)? {
        return Ok(false);
    }
    verify_materialized_handle(&mut staged_file, item, password, false)?;
    verify_materialized_handle(&mut destination_file, item, password, false)?;
    if destination_file.metadata()?.permissions().readonly() {
        set_readonly_materialization_handle(&destination_file, false)?;
    }
    delete_materialization_handle(&destination_file)?;
    Ok(true)
}

#[cfg(windows)]
pub(crate) async fn open_verified_published_file_async(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<std::fs::File> {
    let staged = staged.to_path_buf();
    let destination = destination.to_path_buf();
    let item = item.clone();
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        open_verified_published_file(&staged, &destination, &item, &password)
    })
    .await
    .context("join published materialization verification task")?
}

#[cfg(windows)]
pub(crate) async fn remove_verified_published_file_async(
    staged: &Path,
    destination: &Path,
    item: &JournalDownload,
    password: &str,
) -> Result<bool> {
    let staged = staged.to_path_buf();
    let destination = destination.to_path_buf();
    let item = item.clone();
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        remove_verified_published_file(&staged, &destination, &item, &password)
    })
    .await
    .context("join interrupted materialization removal task")?
}

#[cfg(windows)]
pub(crate) async fn remove_retained_published_file_async(
    published: &PublishedDownload,
    password: &str,
) -> Result<()> {
    let mut file = published.file.try_clone()?;
    let item = published.expected.clone();
    let password = password.to_owned();
    let check_mode = published.mode_applied;
    tokio::task::spawn_blocking(move || {
        verify_materialized_handle(&mut file, &item, &password, check_mode)?;
        if file.metadata()?.permissions().readonly() {
            set_readonly_materialization_handle(&file, false)?;
        }
        delete_materialization_handle(&file)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("join retained materialization removal task")??;
    Ok(())
}

#[cfg(windows)]
fn open_windows_restore_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    // Ancestor directory handles below are intentionally opened without
    // FILE_SHARE_DELETE to pin the namespace. File handles themselves must
    // share deletion because the destination may already be a hard link to
    // this same inode and Windows rejects a second open otherwise.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization restore source is not a non-reparse regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_restore_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let directory = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialization restore ancestor is not a non-reparse directory",
        ));
    }
    Ok(directory)
}

#[cfg(windows)]
fn open_windows_restore_ancestors(
    root: &Path,
    relative: &str,
) -> std::io::Result<Vec<std::fs::File>> {
    open_windows_restore_ancestors_with_creation(root, relative, &[])
}

#[cfg(windows)]
fn open_windows_restore_ancestors_with_creation(
    root: &Path,
    relative: &str,
    recreate: &[String],
) -> std::io::Result<Vec<std::fs::File>> {
    if !relative.is_empty() {
        if !is_safe_rel_path(relative) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe restore ancestor path",
            ));
        }
    }
    for path in recreate {
        if !is_safe_rel_path(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe restore ancestor allowlist path",
            ));
        }
    }
    let mut current = root.to_path_buf();
    let mut chain = vec![open_windows_restore_directory(&current)?];
    let mut current_relative = String::new();
    for component in relative
        .split('/')
        .filter(|component| !component.is_empty())
    {
        if !current_relative.is_empty() {
            current_relative.push('/');
        }
        current_relative.push_str(component);
        current.push(component);
        let directory = match open_windows_restore_directory(&current) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !recreate.iter().any(|path| path == &current_relative) {
                    return Err(error);
                }
                match std::fs::create_dir(&current) {
                    Ok(()) => open_windows_restore_directory(&current)?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        // A concurrent creator owns this namespace entry. Reopen
                        // it with no-follow semantics and pin the resulting
                        // identity before continuing.
                        open_windows_restore_directory(&current)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        chain.push(directory);
    }
    Ok(chain)
}

#[cfg(windows)]
pub(crate) fn derive_windows_restore_ancestors(
    original_paths: &[String],
    downloads: &[JournalDownload],
) -> Result<Vec<String>> {
    let mut ancestors = std::collections::BTreeSet::new();
    for download in downloads {
        let destination = &download.file.path;
        let destination_depth = destination.split('/').count();
        let prefix = format!("{destination}/");
        for original in original_paths {
            if !original.starts_with(&prefix) {
                continue;
            }
            let components = original.split('/').collect::<Vec<_>>();
            let parent_depth = components.len().saturating_sub(1);
            for depth in destination_depth..=parent_depth {
                let candidate = components[..depth].join("/");
                if ancestors.insert(candidate)
                    && ancestors.len() > MAX_MATERIALIZATION_DIRECTORY_PROOFS
                {
                    anyhow::bail!("materialization restore ancestor allowlist exceeds bound");
                }
            }
        }
    }
    Ok(ancestors.into_iter().collect())
}

#[cfg(windows)]
pub(crate) async fn windows_backup_exists_for_download(
    stage: &Path,
    download_path: &str,
    original_paths: &[String],
) -> Result<bool> {
    let prefix = format!("{download_path}/");
    for original in original_paths
        .iter()
        .filter(|original| original.as_str() == download_path || original.starts_with(&prefix))
    {
        let backup = stage.join("backup").join(original);
        match fs::symlink_metadata(&backup).await {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                return Ok(true)
            }
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                // A directory is only the container for descendant backups;
                // the exact original path has no file backup at this level.
            }
            Ok(_) => anyhow::bail!("materialization backup {original} changed type"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowsPublicationRecovery {
    NotPublished,
    Published,
    Ambiguous,
}

#[cfg(windows)]
pub(crate) fn classify_windows_publication_recovery(
    staged_exists: bool,
    destination_exists: bool,
    backup_exists: bool,
) -> WindowsPublicationRecovery {
    if staged_exists && destination_exists {
        return WindowsPublicationRecovery::Published;
    }
    if backup_exists {
        WindowsPublicationRecovery::Ambiguous
    } else {
        WindowsPublicationRecovery::NotPublished
    }
}

#[cfg(all(test, windows))]
mod windows_publication_recovery_tests {
    use super::{classify_windows_publication_recovery, WindowsPublicationRecovery};

    #[test]
    fn publication_progress_truth_table_is_conservative() {
        assert_eq!(
            classify_windows_publication_recovery(true, false, true),
            WindowsPublicationRecovery::Ambiguous
        );
        assert_eq!(
            classify_windows_publication_recovery(true, false, false),
            WindowsPublicationRecovery::NotPublished
        );
        assert_eq!(
            classify_windows_publication_recovery(false, false, true),
            WindowsPublicationRecovery::Ambiguous
        );
        assert_eq!(
            classify_windows_publication_recovery(false, true, true),
            WindowsPublicationRecovery::Ambiguous
        );
        assert_eq!(
            classify_windows_publication_recovery(true, true, true),
            WindowsPublicationRecovery::Published
        );
        assert_eq!(
            classify_windows_publication_recovery(true, true, false),
            WindowsPublicationRecovery::Published
        );
    }
}

#[cfg(windows)]
fn set_readonly_materialization_handle(
    file: &std::fs::File,
    readonly: bool,
) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    let mut permissions = metadata.permissions();
    permissions.set_readonly(readonly);
    file.set_permissions(permissions)
}

#[cfg(windows)]
fn force_basic_delete_disposition_for_tests(base: &Path) -> bool {
    #[cfg(debug_assertions)]
    {
        crate::workspace_layout::ensure_workspace_state(base)
            .map(|state| state.join("test-force-basic-delete").is_file())
            .unwrap_or(false)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = base;
        false
    }
}

#[cfg(windows)]
fn restore_windows_backup_blocking(
    base: &Path,
    stage: &Path,
    relative: &str,
    readonly: bool,
    recreate_ancestors: &[String],
) -> std::io::Result<bool> {
    let parent = relative.rsplit_once('/').map_or("", |(parent, _)| parent);
    let backup_relative = if parent.is_empty() {
        "backup".to_owned()
    } else {
        format!("backup/{parent}")
    };
    // Open and retain every ancestor without delete sharing before opening
    // either file.  This rejects junctions/reparse points and prevents an
    // ancestor replacement from redirecting the later path operations.
    let _backup_ancestors = match open_windows_restore_ancestors(stage, &backup_relative) {
        Ok(chain) => chain,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let backup_path = stage.join("backup").join(relative);
    let original_path = base.join(relative);
    match std::fs::symlink_metadata(&backup_path) {
        Ok(metadata) if metadata.is_dir() => {
            // A directory here is the container for descendant backups (for
            // example stage/backup/d when restoring d/f), not a backed-up file
            // for this exact journal path. Validate it as a non-reparse
            // directory and leave it for the descendant restore.
            open_windows_restore_directory(&backup_path)?;
            return Ok(false);
        }
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "materialization backup changed type",
            ))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    }
    let _original_ancestors =
        open_windows_restore_ancestors_with_creation(base, parent, recreate_ancestors)?;
    let force_basic_delete = force_basic_delete_disposition_for_tests(base);
    let backup = open_windows_restore_file(&backup_path)?;
    let backup_identity = windows_file_identity(&backup)
        .map_err(|error| std::io::Error::other(format!("read backup identity: {error:#}")))?;
    // The extended disposition API ignores this bit, but the fallback
    // FILE_DISPOSITION_INFO path does not. Clear it through the retained
    // handle before linking/deleting, then apply the recorded intent to the
    // restored destination below.
    if backup.metadata()?.permissions().readonly() {
        set_readonly_materialization_handle(&backup, false)?;
    }

    let existing = match open_windows_restore_file(&original_path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(existing) = existing {
        if same_open_file_identity(&backup, &existing).map_err(|error| {
            std::io::Error::other(format!("compare restore identity: {error:#}"))
        })? {
            delete_materialization_handle_with_options(&backup, force_basic_delete)?;
            set_readonly_materialization_handle(&existing, readonly)?;
            return Ok(true);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "materialization restore destination is occupied",
        ));
    }

    // The final destination was absent under an anchored, non-reparse parent.
    // hard_link is no-clobber: a concurrent creator causes AlreadyExists and
    // leaves both the destination and the backup intact for a later retry.
    std::fs::hard_link(&backup_path, &original_path)?;
    let restored = match open_windows_restore_file(&original_path) {
        Ok(file) => file,
        Err(error) => return Err(error),
    };
    let restored_identity = windows_file_identity(&restored)
        .map_err(|error| std::io::Error::other(format!("read restored identity: {error:#}")))?;
    if restored_identity != backup_identity {
        return Err(std::io::Error::other(
            "restored materialization identity changed",
        ));
    }
    delete_materialization_handle_with_options(&backup, force_basic_delete)?;
    set_readonly_materialization_handle(&restored, readonly)?;
    Ok(true)
}

#[cfg(windows)]
pub(crate) async fn restore_windows_backup(
    base: &Path,
    stage: &Path,
    relative: &str,
    readonly: bool,
    recreate_ancestors: &[String],
) -> Result<bool> {
    let base = base.to_path_buf();
    let stage = stage.to_path_buf();
    let relative = relative.to_owned();
    let recreate_ancestors = recreate_ancestors.to_vec();
    tokio::task::spawn_blocking(move || {
        restore_windows_backup_blocking(&base, &stage, &relative, readonly, &recreate_ancestors)
    })
    .await
    .context("join Windows materialization backup restoration task")?
    .map_err(Into::into)
}

#[cfg(windows)]
fn cleanup_materialization_directories_windows_blocking(
    base: &Path,
    proofs: &[MaterializationDirectoryProof],
) -> std::io::Result<()> {
    for proof in proofs.iter().rev() {
        let Some(identity) = proof.identity.as_ref() else {
            continue;
        };
        let parent = proof.path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let _ancestors = match open_windows_restore_ancestors(base, parent) {
            Ok(chain) => chain,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        let current = base.join(&proof.path);
        let directory = match open_materialization_directory_for_delete(&current) {
            Ok(directory) => directory,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        if windows_file_identity(&directory)
            .map_err(|error| std::io::Error::other(format!("read directory identity: {error:#}")))?
            != *identity
        {
            continue;
        }
        match delete_materialization_handle(&directory) {
            Ok(()) => {
                if let Some(parent_handle) = _ancestors.last() {
                    let _ = parent_handle.sync_all();
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) async fn cleanup_materialization_directories(
    base: &Path,
    created_directories: &[MaterializationDirectoryProof],
) -> Result<()> {
    let base = base.to_path_buf();
    let created_directories = created_directories.to_vec();
    tokio::task::spawn_blocking(move || {
        cleanup_materialization_directories_windows_blocking(&base, &created_directories)
    })
    .await
    .context("join Windows materialization directory cleanup task")?
    .map_err(Into::into)
}

#[cfg(windows)]
pub(crate) fn open_directory_no_follow_absolute(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(0x0200_0000 | 0x0020_0000)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.file_attributes() & 0x0000_0400 == 0,
        "path is not a non-reparse directory"
    );
    Ok(file)
}
