//! Descriptor-anchored, no-follow reads beneath a workspace root.

use anyhow::{bail, ensure, Context, Result};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Component, Path};

#[cfg(not(any(unix, windows)))]
use std::path::PathBuf;
#[cfg(any(unix, windows))]
use std::sync::Arc;

/// A reusable anchor for opening regular files beneath one workspace root.
///
/// Unix callers retain an open directory descriptor, so later reads are not
/// resolved again from the process working directory. Other platforms perform
/// no-follow metadata checks for each path component before opening a file.
#[derive(Clone)]
pub struct WorkspaceReadRoot {
    #[cfg(any(unix, windows))]
    root: Arc<File>,
    #[cfg(not(any(unix, windows)))]
    root: PathBuf,
}

impl WorkspaceReadRoot {
    /// Open and retain a workspace directory as the root for later reads.
    pub fn open(base: impl AsRef<Path>) -> Result<Self> {
        let base = base.as_ref();

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            let root = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(base)
                .with_context(|| {
                    format!("open no-follow workspace read root {}", base.display())
                })?;
            Ok(Self {
                root: Arc::new(root),
            })
        }

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;

            let root = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(WINDOWS_SHARE_ALL)
                .custom_flags(WINDOWS_OPEN_REPARSE_POINT | WINDOWS_BACKUP_SEMANTICS)
                .open(base)
                .with_context(|| {
                    format!("open no-follow workspace read root {}", base.display())
                })?;
            require_windows_directory(&root, base, "workspace read root")?;
            Ok(Self {
                root: Arc::new(root),
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let root = if base.is_absolute() {
                base.to_path_buf()
            } else {
                std::env::current_dir()
                    .context("resolve current directory for workspace read root")?
                    .join(base)
            };
            require_directory_without_symlink(&root, "workspace read root")?;
            Ok(Self { root })
        }
    }

    /// Open a regular file at a UTF-8 relative workspace path.
    pub fn open_regular(&self, relative: &str) -> Result<File> {
        ensure!(
            feanorfs_common::is_safe_rel_path(relative),
            "workspace read path must use one canonical portable relative spelling"
        );
        self.open_regular_path(Path::new(relative))
    }

    /// Open a regular file at a lexical relative path beneath this root.
    pub fn open_regular_path(&self, relative: &Path) -> Result<File> {
        let components = validated_relative_components(relative)?;

        #[cfg(unix)]
        {
            self.open_regular_unix(relative, &components)
        }

        #[cfg(windows)]
        {
            self.open_regular_windows(relative, &components)
        }

        #[cfg(not(any(unix, windows)))]
        {
            self.open_regular_checked(relative, &components)
        }
    }

    /// Read one regular file through the retained root with a caller-supplied
    /// byte cap, verifying that descriptor metadata stayed stable.
    pub async fn read_regular_stable(
        &self,
        relative: &str,
        max_bytes: u64,
    ) -> Result<(Vec<u8>, std::fs::Metadata)> {
        use tokio::io::AsyncReadExt as _;

        let source = self.open_regular(relative)?;
        let mut source = tokio::fs::File::from_std(source);
        let before = source.metadata().await?;
        ensure!(
            before.len() <= max_bytes,
            "workspace file {relative} exceeds the permitted read size"
        );
        let mut bytes = Vec::with_capacity(
            usize::try_from(before.len())
                .unwrap_or(usize::MAX)
                .min(8 * 1024 * 1024),
        );
        {
            let limit = max_bytes
                .checked_add(1)
                .context("workspace read limit overflow")?;
            let mut bounded = (&mut source).take(limit);
            bounded.read_to_end(&mut bytes).await?;
        }
        let after = source.metadata().await?;
        ensure!(
            bytes.len() as u64 == before.len()
                && before.len() == after.len()
                && before.modified().ok() == after.modified().ok(),
            "workspace file {relative} changed while it was being read"
        );
        Ok((bytes, before))
    }

    #[cfg(unix)]
    fn open_regular_unix(&self, relative: &Path, components: &[&OsStr]) -> Result<File> {
        let (file_name, directories) = components
            .split_last()
            .expect("validated paths have a final component");
        let mut parent = self
            .root
            .try_clone()
            .context("duplicate workspace read root descriptor")?;

        for component in directories.iter().copied() {
            let name = unix_component(component, relative)?;
            parent = open_at(
                &parent,
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
            .with_context(|| {
                format!(
                    "open no-follow directory component {:?} while resolving workspace path {}",
                    component,
                    relative.display()
                )
            })?;
        }

        let name = unix_component(file_name, relative)?;
        let file = open_at(
            &parent,
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
        .with_context(|| format!("open no-follow workspace file {}", relative.display()))?;
        require_regular(&file, relative)?;
        Ok(file)
    }

    #[cfg(windows)]
    fn open_regular_windows(&self, relative: &Path, components: &[&OsStr]) -> Result<File> {
        let (file_name, directories) = components
            .split_last()
            .expect("validated paths have a final component");
        let mut parent = self
            .root
            .try_clone()
            .context("duplicate workspace read root handle")?;
        for component in directories.iter().copied() {
            parent = open_relative_windows(&parent, component, true).with_context(|| {
                format!(
                    "open no-follow directory component {:?} while resolving workspace path {}",
                    component,
                    relative.display()
                )
            })?;
            require_windows_directory(&parent, relative, "workspace path component")?;
        }
        let file = open_relative_windows(&parent, file_name, false)
            .with_context(|| format!("open no-follow workspace file {}", relative.display()))?;
        require_regular(&file, relative)?;
        Ok(file)
    }

    #[cfg(not(any(unix, windows)))]
    fn open_regular_checked(&self, relative: &Path, components: &[&OsStr]) -> Result<File> {
        require_directory_without_symlink(&self.root, "workspace read root")?;

        let mut candidate = self.root.clone();
        for (index, component) in components.iter().enumerate() {
            candidate.push(component);
            let metadata = std::fs::symlink_metadata(&candidate).with_context(|| {
                format!(
                    "inspect workspace path component {} while resolving {}",
                    candidate.display(),
                    relative.display()
                )
            })?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "workspace path {} contains a symlink at {}",
                relative.display(),
                candidate.display()
            );
            if index + 1 == components.len() {
                ensure!(
                    metadata.file_type().is_file(),
                    "workspace path {} is not a regular file",
                    relative.display()
                );
            } else {
                ensure!(
                    metadata.file_type().is_dir(),
                    "workspace path {} has a non-directory ancestor at {}",
                    relative.display(),
                    candidate.display()
                );
            }
        }

        let file = File::open(&candidate)
            .with_context(|| format!("open checked workspace file {}", relative.display()))?;
        require_regular(&file, relative)?;
        Ok(file)
    }
}

fn validated_relative_components(path: &Path) -> Result<Vec<&OsStr>> {
    ensure!(
        !path.as_os_str().is_empty(),
        "workspace read path must not be empty"
    );
    validate_raw_relative_spelling(path)?;

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value),
            Component::CurDir => bail!(
                "workspace read path {} contains a current-directory component",
                path.display()
            ),
            Component::ParentDir => bail!(
                "workspace read path {} contains parent traversal",
                path.display()
            ),
            Component::RootDir | Component::Prefix(_) => {
                bail!("workspace read path {} must be relative", path.display())
            }
        }
    }
    ensure!(
        !components.is_empty(),
        "workspace read path {} has no file component",
        path.display()
    );
    Ok(components)
}

#[cfg(unix)]
fn validate_raw_relative_spelling(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = path.as_os_str().as_bytes();
    ensure!(
        !bytes.starts_with(b"/") && !bytes.ends_with(b"/"),
        "workspace read path {} must be an unaliased relative path",
        path.display()
    );
    ensure!(
        bytes
            .split(|byte| *byte == b'/')
            .all(|component| !component.is_empty() && component != b"." && component != b".."),
        "workspace read path {} contains an empty or dot component",
        path.display()
    );
    Ok(())
}

#[cfg(windows)]
fn validate_raw_relative_spelling(path: &Path) -> Result<()> {
    use unicode_normalization::UnicodeNormalization as _;

    let raw = path
        .to_str()
        .with_context(|| format!("workspace read path {} is not Unicode", path.display()))?;
    ensure!(
        raw.nfc().eq(raw.chars()),
        "workspace read path {} must use its exact NFC spelling",
        path.display()
    );
    let portable = raw.replace('\\', "/");
    ensure!(
        feanorfs_common::is_safe_rel_path(&portable),
        "workspace read path {} is not a safe portable path",
        path.display()
    );
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_raw_relative_spelling(path: &Path) -> Result<()> {
    use unicode_normalization::UnicodeNormalization as _;

    let raw = path
        .to_str()
        .with_context(|| format!("workspace read path {} is not Unicode", path.display()))?;
    let portable = raw.replace('\\', "/").nfc().collect::<String>();
    ensure!(
        raw == portable,
        "workspace read path {} must use its exact NFC forward-slash spelling",
        path.display()
    );
    ensure!(
        feanorfs_common::is_safe_rel_path(&portable),
        "workspace read path {} is not a safe portable path",
        path.display()
    );
    Ok(())
}

#[cfg(unix)]
fn unix_component(component: &OsStr, relative: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt as _;

    std::ffi::CString::new(component.as_bytes()).with_context(|| {
        format!(
            "workspace read path {} contains a NUL byte",
            relative.display()
        )
    })
}

#[cfg(unix)]
fn open_at(parent: &File, name: &std::ffi::CStr, flags: libc::c_int) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    // SAFETY: `name` is NUL-terminated and `parent` remains open for the call.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful `openat` returned a new descriptor owned by this call.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn require_regular(file: &File, relative: &Path) -> Result<()> {
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd as _;

    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `metadata` points to writable storage for one `stat`, and `file`
    // owns a valid descriptor for the duration of the call.
    let result = unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) };
    if result < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("inspect opened workspace file {}", relative.display()));
    }
    // SAFETY: `fstat` succeeded and initialized the complete structure.
    let metadata = unsafe { metadata.assume_init() };
    ensure!(
        metadata.st_mode & libc::S_IFMT == libc::S_IFREG,
        "workspace path {} is not a regular file",
        relative.display()
    );
    Ok(())
}

#[cfg(windows)]
const WINDOWS_SHARE_ALL: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;
#[cfg(windows)]
const WINDOWS_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const WINDOWS_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const WINDOWS_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(windows)]
#[repr(C)]
struct WindowsUnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsObjectAttributes {
    length: u32,
    root_directory: windows_sys::Win32::Foundation::HANDLE,
    object_name: *mut WindowsUnicodeString,
    attributes: u32,
    security_descriptor: *mut std::ffi::c_void,
    security_quality_of_service: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[repr(C)]
union WindowsIoStatusValue {
    status: i32,
    pointer: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsIoStatusBlock {
    value: WindowsIoStatusValue,
    information: usize,
}

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    #[link_name = "NtCreateFile"]
    fn nt_create_file(
        file_handle: *mut windows_sys::Win32::Foundation::HANDLE,
        desired_access: u32,
        object_attributes: *mut WindowsObjectAttributes,
        io_status_block: *mut WindowsIoStatusBlock,
        allocation_size: *mut i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *mut std::ffi::c_void,
        ea_length: u32,
    ) -> i32;

    #[link_name = "RtlNtStatusToDosError"]
    fn rtl_nt_status_to_dos_error(status: i32) -> u32;
}

#[cfg(windows)]
fn open_relative_windows(
    parent: &File,
    component: &OsStr,
    directory: bool,
) -> std::io::Result<File> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};

    let mut wide = component.encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Windows workspace path component",
        ));
    }
    let byte_len = wide
        .len()
        .checked_mul(2)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows workspace path component is too long",
            )
        })?;
    let mut name = WindowsUnicodeString {
        length: byte_len,
        maximum_length: byte_len,
        buffer: wide.as_mut_ptr(),
    };
    let mut attributes = WindowsObjectAttributes {
        length: std::mem::size_of::<WindowsObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &mut name,
        // Case-insensitive matching is the filesystem's ordinary behavior;
        // DONT_REPARSE makes any reparse point fail rather than redirect.
        attributes: 0x0000_0040 | 0x0000_1000,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut io_status = std::mem::MaybeUninit::<WindowsIoStatusBlock>::uninit();
    let mut handle = std::ptr::null_mut();
    let create_options = 0x0020_0000 // FILE_OPEN_REPARSE_POINT
        | 0x0000_0020 // FILE_SYNCHRONOUS_IO_NONALERT
        | 0x0000_4000 // FILE_OPEN_FOR_BACKUP_INTENT
        | if directory { 0x0000_0001 } else { 0x0000_0040 };
    // SAFETY: every pointer refers to initialized storage that remains alive
    // for the synchronous call; a successful call returns one owned handle.
    let status = unsafe {
        nt_create_file(
            &mut handle,
            0x0010_0000 | 0x0000_0001 | 0x0000_0080,
            &mut attributes,
            io_status.as_mut_ptr(),
            std::ptr::null_mut(),
            0,
            WINDOWS_SHARE_ALL,
            1, // FILE_OPEN
            create_options,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: conversion has no memory-safety preconditions.
        let code = unsafe { rtl_nt_status_to_dos_error(status) };
        return Err(std::io::Error::from_raw_os_error(code as i32));
    }
    if handle.is_null() {
        return Err(std::io::Error::other(
            "NtCreateFile succeeded without returning a handle",
        ));
    }
    // SAFETY: `NtCreateFile` returned a new handle owned by this call.
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn require_windows_directory(file: &File, path: &Path, description: &str) -> Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    let metadata = file
        .metadata()
        .with_context(|| format!("inspect opened {description} {}", path.display()))?;
    ensure!(
        metadata.file_attributes() & WINDOWS_ATTRIBUTE_REPARSE_POINT == 0,
        "{description} {} is a reparse point",
        path.display()
    );
    ensure!(
        metadata.file_type().is_dir(),
        "{description} {} is not a directory",
        path.display()
    );
    Ok(())
}

#[cfg(windows)]
fn require_regular(file: &File, relative: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    let metadata = file
        .metadata()
        .with_context(|| format!("inspect opened workspace file {}", relative.display()))?;
    ensure!(
        metadata.file_attributes() & WINDOWS_ATTRIBUTE_REPARSE_POINT == 0,
        "workspace path {} is a reparse point",
        relative.display()
    );
    ensure!(
        metadata.file_type().is_file(),
        "workspace path {} is not a regular file",
        relative.display()
    );
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn require_regular(file: &File, relative: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect opened workspace file {}", relative.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "workspace path {} is not a regular file",
        relative.display()
    );
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn require_directory_without_symlink(path: &Path, description: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect {description} {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "{description} {} is a symlink",
        path.display()
    );
    ensure!(
        metadata.file_type().is_dir(),
        "{description} {} is not a directory",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn rejects_non_relative_and_non_file_paths() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        std::fs::write(workspace.path().join("file.txt"), b"contents")?;
        std::fs::create_dir(workspace.path().join("directory"))?;
        let root = WorkspaceReadRoot::open(workspace.path())?;

        assert!(root.open_regular("").is_err());
        assert!(root.open_regular(".").is_err());
        assert!(root.open_regular("../file.txt").is_err());
        assert!(root.open_regular("dir//file.txt").is_err());
        assert!(root.open_regular("dir/./file.txt").is_err());
        assert!(root.open_regular("file.txt/").is_err());
        assert!(root.open_regular_path(Path::new("dir//file.txt")).is_err());
        assert!(root.open_regular_path(Path::new("dir/./file.txt")).is_err());
        assert!(root.open_regular_path(Path::new("file.txt/")).is_err());
        assert!(root
            .open_regular_path(&workspace.path().join("file.txt"))
            .is_err());
        assert!(root.open_regular("directory").is_err());

        let mut file = root.clone().open_regular("file.txt")?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        assert_eq!(contents, "contents");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_aliases_devices_and_streams() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        std::fs::create_dir(workspace.path().join("directory"))?;
        std::fs::write(workspace.path().join("directory/file"), b"safe")?;
        let root = WorkspaceReadRoot::open(workspace.path())?;
        assert!(root.open_regular("directory/file").is_ok());
        assert!(root
            .open_regular_path(&std::path::PathBuf::from("directory").join("file"))
            .is_ok());
        for path in [
            r".. \outside\secret",
            r"directory.\file",
            r"directory \file",
            r"file.txt:stream",
            r"NUL",
            r"CON.txt",
            r"directory\AUX",
        ] {
            assert!(root.open_regular_path(Path::new(path)).is_err(), "{path}");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_file_and_directory_reparse_points() -> Result<()> {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let sandbox = tempfile::tempdir()?;
        let workspace = sandbox.path().join("workspace");
        let outside = sandbox.path().join("outside");
        std::fs::create_dir(&workspace)?;
        std::fs::create_dir(&outside)?;
        std::fs::write(outside.join("secret.txt"), b"outside")?;
        if let Err(error) = symlink_file(outside.join("secret.txt"), workspace.join("file-link")) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return Ok(());
            }
            return Err(error.into());
        }
        symlink_dir(&outside, workspace.join("directory-link"))?;

        let root = WorkspaceReadRoot::open(&workspace)?;
        assert!(root.open_regular("file-link").is_err());
        assert!(root.open_regular("directory-link/secret.txt").is_err());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn concurrent_final_reparse_swaps_never_read_outside_bytes() -> Result<()> {
        use std::os::windows::fs::symlink_file;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let sandbox = tempfile::tempdir()?;
        let workspace = sandbox.path().join("workspace");
        let outside = sandbox.path().join("secret.txt");
        std::fs::create_dir(&workspace)?;
        std::fs::write(&outside, b"outside-secret")?;
        let victim = workspace.join("victim.txt");
        let alternate = workspace.join("alternate.txt");
        let swap = workspace.join("swap.txt");
        std::fs::write(&victim, b"safe")?;
        if let Err(error) = symlink_file(&outside, &alternate) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return Ok(());
            }
            return Err(error.into());
        }
        let root = WorkspaceReadRoot::open(&workspace)?;
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            while !writer_stop.load(Ordering::Relaxed) {
                if std::fs::rename(&victim, &swap).is_err() {
                    continue;
                }
                if std::fs::rename(&alternate, &victim).is_err() {
                    let _ = std::fs::rename(&swap, &victim);
                    continue;
                }
                if std::fs::rename(&swap, &alternate).is_err() {
                    break;
                }
            }
        });
        let mut escaped = false;
        for _ in 0..2_000 {
            if let Ok(mut file) = root.open_regular("victim.txt") {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                if bytes == b"outside-secret" {
                    escaped = true;
                    break;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("workspace swap thread panicked");
        assert!(!escaped, "workspace read followed a swapped reparse point");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_final_and_ancestor_symlinks_to_outside() -> Result<()> {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir()?;
        let workspace = sandbox.path().join("workspace");
        let outside = sandbox.path().join("outside");
        std::fs::create_dir(&workspace)?;
        std::fs::create_dir(&outside)?;
        std::fs::write(outside.join("secret.txt"), b"outside")?;
        symlink(outside.join("secret.txt"), workspace.join("file-link"))?;
        symlink(&outside, workspace.join("directory-link"))?;

        let root = WorkspaceReadRoot::open(&workspace)?;
        assert!(root.open_regular("file-link").is_err());
        assert!(root.open_regular("directory-link/secret.txt").is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_root_and_fifo_without_blocking() -> Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir()?;
        let workspace = sandbox.path().join("workspace");
        let linked = sandbox.path().join("linked-workspace");
        std::fs::create_dir(&workspace)?;
        symlink(&workspace, &linked)?;
        assert!(WorkspaceReadRoot::open(&linked).is_err());

        let fifo = workspace.join("pipe");
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes())?;
        // SAFETY: `fifo_name` is a valid NUL-terminated pathname.
        if unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let root = WorkspaceReadRoot::open(&workspace)?;
        assert!(root.open_regular("pipe").is_err());
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn root_descriptor_survives_root_path_replacement() -> Result<()> {
        let sandbox = tempfile::tempdir()?;
        let workspace = sandbox.path().join("workspace");
        let moved = sandbox.path().join("moved-workspace");
        std::fs::create_dir(&workspace)?;
        std::fs::write(workspace.join("file.txt"), b"anchored")?;
        let root = WorkspaceReadRoot::open(&workspace)?;

        std::fs::rename(&workspace, &moved)?;
        std::fs::create_dir(&workspace)?;
        std::fs::write(workspace.join("file.txt"), b"replacement")?;

        let mut opened = root.open_regular("file.txt")?;
        let mut contents = Vec::new();
        opened.read_to_end(&mut contents)?;
        assert_eq!(contents, b"anchored");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn opened_file_survives_pathname_replacement() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let path = workspace.path().join("file.txt");
        let replacement = workspace.path().join("replacement.txt");
        std::fs::write(&path, b"original")?;
        std::fs::write(&replacement, b"replacement")?;

        let root = WorkspaceReadRoot::open(workspace.path())?;
        let mut opened = root.open_regular("file.txt")?;
        std::fs::rename(&replacement, &path)?;

        let mut contents = Vec::new();
        opened.read_to_end(&mut contents)?;
        assert_eq!(contents, b"original");
        assert_eq!(std::fs::read(&path)?, b"replacement");
        Ok(())
    }
}
