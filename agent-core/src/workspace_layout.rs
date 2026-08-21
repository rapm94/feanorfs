//! Global, project-litter-free workspace state layout.

use crate::state::LocalStateV1;
use anyhow::{bail, Context as _, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LEGACY_STATE_DIR: &str = ".feanorfs";
const LEGACY_IGNORE_FILE: &str = ".feanorfsignore";
const DEFAULT_RETENTION_DAYS: u64 = 30;
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const TEMP_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn global_state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("FEANORFS_HOME") {
        let root = PathBuf::from(root);
        ensure_utf8_state_root(&root)?;
        return Ok(root);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME or USERPROFILE environment variable not set")?;
    Ok(PathBuf::from(home).join(".feanorfs"))
}

fn ensure_utf8_state_root(root: &Path) -> Result<()> {
    if root.to_str().is_none() {
        bail!("FEANORFS_HOME is not valid UTF-8 and cannot expose portable state paths");
    }
    Ok(())
}

pub(crate) fn canonical_workspace(workspace: &Path) -> Result<PathBuf> {
    match fs::canonicalize(workspace) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if workspace.is_absolute() {
                Ok(workspace.to_path_buf())
            } else {
                Ok(std::env::current_dir()?.join(workspace))
            }
        }
        Err(error) => Err(error).context("resolve workspace path"),
    }
}

pub fn workspace_state_id(workspace: &Path) -> Result<String> {
    let canonical = canonical_workspace(workspace)?;
    let mut hasher = blake3::Hasher::new_derive_key("feanorfs global workspace state v1");
    let canonical = canonical
        .to_str()
        .context("workspace path is not valid UTF-8 and cannot have a portable state identity")?;
    hasher.update(canonical.as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn workspace_state_path(workspace: &Path) -> Result<PathBuf> {
    workspace_state_path_in(workspace, &global_state_root()?, false)
}

pub(crate) fn legacy_workspace_state_path(workspace: &Path) -> Result<PathBuf> {
    workspace_state_path_in(workspace, &global_state_root()?, true)
}

pub(crate) const MAX_WORKSPACE_STATE_SLOTS: usize = 100_000;
const MAX_WORKSPACE_IDENTITY_BYTES: u64 = 512;

pub(crate) fn workspace_state_path_in(
    workspace: &Path,
    root: &Path,
    allow_mismatched_preferred: bool,
) -> Result<PathBuf> {
    Ok(resolve_workspace_state_in(workspace, root, allow_mismatched_preferred)?.path)
}

struct WorkspaceStateResolution {
    path: PathBuf,
    identity: Option<WorkspaceIdentity>,
    /// The slot already existed on disk at resolution time.
    slot_existed: bool,
    /// The preferred path-only slot (no identity file) whose recorded
    /// location matches the requested canonical path: adopt it once, with
    /// recorded provenance, instead of silently reusing or hard-failing.
    adopt_path_only: bool,
    /// The preferred slot was reused on positive project-local legacy
    /// evidence (`.feanorfs` inside the workspace), not path-only adoption.
    from_legacy: bool,
    /// The matched slot carries a tombstone but the live folder claims its
    /// identity: `ensure` clears the tombstone after revalidation.
    revive: bool,
}

fn ensure_state_path_lease(root: &Path, state: &Path) -> Result<()> {
    let slot = state
        .file_name()
        .and_then(|name| name.to_str())
        .context("global workspace state slot is not UTF-8")?;
    crate::workspace_state_registry::ensure_shared_state_lease(root, slot)
}

fn state_directory_exists(state: &Path) -> Result<bool> {
    match fs::symlink_metadata(state) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => bail!(
            "workspace state is not a regular directory: {}",
            state.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect workspace state"),
    }
}

fn location_matches(slot: &Path, canonical: &str) -> Result<bool> {
    let path = slot.join("location");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => bail!(
            "workspace location is not a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspect workspace location"),
    };
    if metadata.len() > MAX_WORKSPACE_IDENTITY_BYTES {
        bail!("workspace location exceeds bounded size");
    }
    let mut stored = String::with_capacity(metadata.len() as usize);
    fs::File::open(&path)
        .context("open workspace location")?
        .take(MAX_WORKSPACE_IDENTITY_BYTES + 1)
        .read_to_string(&mut stored)
        .context("read workspace location")?;
    Ok(stored.trim() == canonical)
}

fn resolve_workspace_state_in(
    workspace: &Path,
    root: &Path,
    allow_mismatched_preferred: bool,
) -> Result<WorkspaceStateResolution> {
    let workspaces = root.join("workspaces");
    let preferred = workspaces.join(workspace_state_id(workspace)?);
    let preferred_exists = state_directory_exists(&preferred)?;
    let Some(identity) = current_workspace_identity(workspace)? else {
        ensure_state_path_lease(root, &preferred)?;
        if state_directory_exists(&preferred)? != preferred_exists {
            bail!("workspace state changed while its lifetime lease was being acquired");
        }
        return Ok(WorkspaceStateResolution {
            path: preferred,
            identity: None,
            slot_existed: false,
            adopt_path_only: false,
            from_legacy: false,
            revive: false,
        });
    };
    if preferred_exists && identity.matches(read_workspace_identity(&preferred)?.as_deref()) {
        ensure_state_path_lease(root, &preferred)?;
        if !state_directory_exists(&preferred)?
            || !identity.matches(read_workspace_identity(&preferred)?.as_deref())
        {
            bail!("workspace state changed while its lifetime lease was being acquired");
        }
        return Ok(WorkspaceStateResolution {
            revive: crate::workspace_state_registry::slot_tombstoned(&preferred)?,
            path: preferred,
            identity: Some(identity),
            slot_existed: true,
            adopt_path_only: false,
            from_legacy: false,
        });
    }

    // Verified O(1) moved-workspace lookup. The identity file inside the
    // candidate stays authoritative, and the index is trusted only while no
    // slot creation, removal, or rename has happened since it was written:
    // such a mutation could have introduced a second slot claiming the same
    // identity, so a stale index falls through to the bounded scan, which
    // keeps the fail-closed ambiguity check intact.
    let index_fresh = crate::workspace_state_registry::identity_index_fresh(root, &workspaces)?;
    if index_fresh {
        if let Some(candidate) = crate::workspace_state_registry::index_lookup(root, &identity)? {
            if candidate != preferred
                && candidate.is_dir()
                && identity.matches(read_workspace_identity(&candidate)?.as_deref())
            {
                ensure_state_path_lease(root, &candidate)?;
                if !state_directory_exists(&candidate)?
                    || !identity.matches(read_workspace_identity(&candidate)?.as_deref())
                {
                    bail!("workspace state changed while its lifetime lease was being acquired");
                }
                return Ok(WorkspaceStateResolution {
                    path: candidate.clone(),
                    identity: Some(identity),
                    slot_existed: true,
                    adopt_path_only: false,
                    from_legacy: false,
                    revive: crate::workspace_state_registry::slot_tombstoned(&candidate)?,
                });
            }
        }
    }

    // Bounded fallback scan: only reached when the index is missing, corrupt,
    // or stale, and it repairs the index through the next `ensure`.
    let entries = match fs::read_dir(&workspaces) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_state_path_lease(root, &preferred)?;
            if state_directory_exists(&preferred)? {
                bail!("workspace state changed while its lifetime lease was being acquired");
            }
            return Ok(WorkspaceStateResolution {
                path: preferred,
                identity: Some(identity),
                slot_existed: false,
                adopt_path_only: false,
                from_legacy: false,
                revive: false,
            });
        }
        Err(error) => return Err(error).context("search global workspace registry"),
    };
    let mut matches = Vec::new();
    let mut scanned = 0usize;
    for entry in entries {
        let entry = entry.context("read global workspace registry entry")?;
        if !entry
            .file_type()
            .context("inspect global workspace registry entry")?
            .is_dir()
        {
            continue;
        }
        scanned = scanned.saturating_add(1);
        if scanned > MAX_WORKSPACE_STATE_SLOTS {
            bail!("global workspace registry exceeds bounded slot count");
        }
        let candidate = entry.path();
        if crate::workspace_state_registry::slot_tombstoned(&candidate)? {
            // Retired state is inert: resolution can never resurrect it.
            continue;
        }
        if identity.matches(read_workspace_identity(&candidate)?.as_deref()) {
            matches.push(candidate);
            if matches.len() > 1 {
                bail!("workspace identity is ambiguous across multiple state directories");
            }
        }
    }
    if let Some(candidate) = matches.pop() {
        ensure_state_path_lease(root, &candidate)?;
        if !state_directory_exists(&candidate)?
            || crate::workspace_state_registry::slot_tombstoned(&candidate)?
            || !identity.matches(read_workspace_identity(&candidate)?.as_deref())
        {
            bail!("workspace state changed while its lifetime lease was being acquired");
        }
        return Ok(WorkspaceStateResolution {
            path: candidate,
            identity: Some(identity),
            slot_existed: true,
            adopt_path_only: false,
            from_legacy: false,
            revive: false,
        });
    }
    if preferred_exists {
        if allow_mismatched_preferred {
            // Compatibility migration must inspect the exact legacy slot even
            // when its identity is stale, so it can preserve rather than adopt
            // the bytes. Ordinary workspace resolution still fails closed.
            ensure_state_path_lease(root, &preferred)?;
            if !state_directory_exists(&preferred)? {
                bail!("workspace state changed while its lifetime lease was being acquired");
            }
            return Ok(WorkspaceStateResolution {
                path: preferred,
                identity: Some(identity),
                slot_existed: true,
                adopt_path_only: false,
                from_legacy: false,
                revive: false,
            });
        }
        // A project-local legacy store is positive evidence that migration,
        // rather than path reuse, owns this transition. The migration path
        // compares/quarantines the overlapping global bytes before writing.
        if workspace.join(LEGACY_STATE_DIR).is_dir() {
            ensure_state_path_lease(root, &preferred)?;
            if !state_directory_exists(&preferred)? {
                bail!("workspace state changed while its lifetime lease was being acquired");
            }
            return Ok(WorkspaceStateResolution {
                path: preferred,
                identity: Some(identity),
                slot_existed: true,
                adopt_path_only: false,
                from_legacy: true,
                revive: false,
            });
        }
        // Upgrade adoption: a slot from a version that never recorded an
        // identity is adopted exactly once, with written provenance, and only
        // when the recorded location proves it was established for this exact
        // path. Anything else fails closed — never silently.
        if read_workspace_identity(&preferred)?.is_none() {
            let canonical = canonical_workspace(workspace)?
                .to_str()
                .context("workspace path is not valid UTF-8 and cannot be adopted portably")?
                .to_string();
            if location_matches(&preferred, &canonical)? {
                ensure_state_path_lease(root, &preferred)?;
                if !state_directory_exists(&preferred)?
                    || read_workspace_identity(&preferred)?.is_some()
                    || !location_matches(&preferred, &canonical)?
                {
                    bail!("workspace state changed while its lifetime lease was being acquired");
                }
                return Ok(WorkspaceStateResolution {
                    path: preferred,
                    identity: Some(identity),
                    slot_existed: true,
                    adopt_path_only: true,
                    from_legacy: false,
                    revive: false,
                });
            }
        }
        bail!(
            "workspace path now identifies a different folder; refusing to reuse state at {}",
            preferred.display()
        );
    }
    ensure_state_path_lease(root, &preferred)?;
    if state_directory_exists(&preferred)? {
        bail!("workspace state changed while its lifetime lease was being acquired");
    }
    Ok(WorkspaceStateResolution {
        path: preferred,
        identity: Some(identity),
        slot_existed: false,
        adopt_path_only: false,
        from_legacy: false,
        revive: false,
    })
}

pub(crate) fn read_workspace_identity(state: &Path) -> Result<Option<String>> {
    let path = state.join("identity");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => bail!(
            "workspace identity is not a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect workspace identity"),
    };
    if metadata.len() > MAX_WORKSPACE_IDENTITY_BYTES {
        bail!("workspace identity exceeds bounded size");
    }
    let mut stored = String::with_capacity(metadata.len() as usize);
    fs::File::open(&path)
        .context("open workspace identity")?
        .take(MAX_WORKSPACE_IDENTITY_BYTES + 1)
        .read_to_string(&mut stored)
        .context("read workspace identity")?;
    if stored.len() as u64 > MAX_WORKSPACE_IDENTITY_BYTES {
        bail!("workspace identity exceeds bounded size");
    }
    Ok(Some(stored.trim().to_string()))
}

#[derive(Debug)]
pub(crate) struct WorkspaceIdentity {
    pub(crate) stable: String,
    pub(crate) compatible_legacy: Option<String>,
}

impl WorkspaceIdentity {
    pub(crate) fn matches(&self, stored: Option<&str>) -> bool {
        stored.is_some_and(|stored| {
            stored == self.stable || self.compatible_legacy.as_deref() == Some(stored)
        })
    }
}

fn revalidate_workspace_identity(
    workspace: &Path,
    expected: Option<&WorkspaceIdentity>,
) -> Result<()> {
    let current = current_workspace_identity(workspace)?;
    let unchanged = match (expected, current.as_ref()) {
        (Some(expected), Some(current)) => expected.stable == current.stable,
        (None, None) => true,
        _ => false,
    };
    if !unchanged {
        bail!("workspace path changed while workspace state was being prepared; refusing to reuse state");
    }
    Ok(())
}

/// The strongest identity a weak filesystem can honestly expose on Unix: the
/// device and inode pair survives as long as the directory object exists, but
/// a deleted-and-recreated folder may reuse the inode, so same-path
/// replacement detection is best-effort there. The `-weak` marker makes that
/// limitation explicit in the recorded identity.
pub(crate) fn unix_weak_identity(dev: u64, ino: u64) -> String {
    format!("unix-weak:{dev}:{ino}")
}

pub(crate) fn current_workspace_identity(workspace: &Path) -> Result<Option<WorkspaceIdentity>> {
    #[cfg(unix)]
    {
        let file = match fs::File::open(workspace) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open workspace identity"),
        };
        let metadata = file.metadata().context("read workspace identity")?;
        use std::os::unix::fs::MetadataExt as _;
        let dev = metadata.dev();
        let ino = metadata.ino();
        let created = metadata
            .created()
            .ok()
            .and_then(|created| created.duration_since(UNIX_EPOCH).ok());
        let legacy = created.map(|created| {
            format!(
                "unix:{}:{}:{}:{}",
                dev,
                ino,
                created.as_secs(),
                created.subsec_nanos()
            )
        });
        let stable = match &created {
            Some(created) => {
                #[cfg(target_os = "macos")]
                {
                    match macos_volume_uuid(&file) {
                        Ok(volume_uuid) => {
                            use std::fmt::Write as _;
                            let mut volume = String::with_capacity(32);
                            for byte in volume_uuid {
                                write!(&mut volume, "{byte:02x}")
                                    .expect("writing into a String cannot fail");
                            }
                            format!(
                                "macos-v2:{}:{}:{}:{}",
                                volume,
                                ino,
                                created.as_secs(),
                                created.subsec_nanos()
                            )
                        }
                        Err(_) => legacy.clone().expect("created implies legacy"),
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    match linux_file_identity(&file) {
                        Ok(file_identity) => format!(
                            "linux-v2:{}:{}:{}",
                            file_identity,
                            created.as_secs(),
                            created.subsec_nanos()
                        ),
                        Err(_) => legacy.clone().expect("created implies legacy"),
                    }
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                {
                    legacy.clone().expect("created implies legacy")
                }
            }
            None => {
                // Weak filesystem: no birth time. Use the strongest remaining
                // identity and mark it weak so replacement-detection limits
                // stay explicit instead of silently adopting path-only state.
                #[cfg(target_os = "macos")]
                {
                    match macos_volume_uuid(&file) {
                        Ok(volume_uuid) => {
                            use std::fmt::Write as _;
                            let mut volume = String::with_capacity(32);
                            for byte in volume_uuid {
                                write!(&mut volume, "{byte:02x}")
                                    .expect("writing into a String cannot fail");
                            }
                            format!("macos-weak:{volume}:{ino}:{dev}")
                        }
                        Err(_) => unix_weak_identity(dev, ino),
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    match linux_file_identity(&file) {
                        Ok(file_identity) => format!("linux-v2-weak:{file_identity}:{dev}:{ino}"),
                        Err(_) => unix_weak_identity(dev, ino),
                    }
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                {
                    unix_weak_identity(dev, ino)
                }
            }
        };
        Ok(Some(WorkspaceIdentity {
            compatible_legacy: legacy.filter(|legacy| *legacy != stable),
            stable,
        }))
    }
    #[cfg(windows)]
    {
        let mut options = OpenOptions::new();
        options.read(true);
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS);
        }
        let file = match options.open(workspace) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open workspace identity"),
        };
        let stable = windows_workspace_identity(&file)?;
        Ok(Some(WorkspaceIdentity {
            stable,
            compatible_legacy: None,
        }))
    }
}

#[cfg(windows)]
fn windows_workspace_identity(file: &fs::File) -> Result<String> {
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
        return Err(std::io::Error::last_os_error()).context("read Windows workspace identity");
    }
    // SAFETY: the successful call initialized the complete structure.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    anyhow::ensure!(
        file_index != 0,
        "Windows workspace identity has a zero file index"
    );
    let creation_100ns = (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
        | u64::from(information.ftCreationTime.dwLowDateTime);
    Ok(format!(
        "windows-v2:{:08x}:{:016x}:{}",
        information.dwVolumeSerialNumber, file_index, creation_100ns
    ))
}

#[cfg(target_os = "linux")]
fn linux_file_identity(file: &fs::File) -> std::io::Result<String> {
    use std::fmt::Write as _;
    use std::os::fd::AsRawFd as _;

    const MAX_FILE_HANDLE_BYTES: usize = 128;

    #[repr(C)]
    struct FileHandleBuffer {
        header: libc::file_handle,
        bytes: [libc::c_uchar; MAX_FILE_HANDLE_BYTES],
    }

    const _: () = assert!(
        std::mem::offset_of!(FileHandleBuffer, bytes) == std::mem::size_of::<libc::file_handle>()
    );

    let stats = linux_file_stats(file)?;
    let mut handle = FileHandleBuffer {
        header: libc::file_handle {
            handle_bytes: MAX_FILE_HANDLE_BYTES as libc::c_uint,
            handle_type: 0,
            f_handle: [],
        },
        bytes: [0; MAX_FILE_HANDLE_BYTES],
    };
    let mut mount_id = 0;
    // SAFETY: `file` remains open for the call. `AT_EMPTY_PATH` asks the
    // kernel to identify that descriptor rather than resolving another path.
    // `FileHandleBuffer` is repr(C), starts with the kernel header, and owns
    // the advertised bounded byte capacity immediately after that header.
    let handle_ptr = std::ptr::addr_of_mut!(handle).cast::<libc::file_handle>();
    let result = unsafe {
        libc::name_to_handle_at(
            file.as_raw_fd(),
            c"".as_ptr(),
            handle_ptr,
            &mut mount_id,
            libc::AT_EMPTY_PATH,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let handle_bytes = handle.header.handle_bytes as usize;
    if !(1..=MAX_FILE_HANDLE_BYTES).contains(&handle_bytes) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "kernel returned an invalid file-handle length",
        ));
    }

    // `fsid_t` has platform-private fields, so encode its fully initialized
    // native bytes. This state is local to one machine and therefore does not
    // need a cross-architecture representation.
    let filesystem_id = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!(stats.f_fsid).cast::<u8>(),
            std::mem::size_of_val(&stats.f_fsid),
        )
    };
    if filesystem_id.iter().all(|byte| *byte == 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "kernel returned an empty filesystem ID",
        ));
    }

    let mut filesystem_hex = String::with_capacity(filesystem_id.len() * 2);
    for byte in filesystem_id {
        write!(&mut filesystem_hex, "{byte:02x}").expect("writing into a String cannot fail");
    }
    let mut handle_hex = String::with_capacity(handle_bytes * 2);
    for byte in &handle.bytes[..handle_bytes] {
        write!(&mut handle_hex, "{byte:02x}").expect("writing into a String cannot fail");
    }
    Ok(format!(
        "{}:{}:{}",
        filesystem_hex, handle.header.handle_type, handle_hex
    ))
}

#[cfg(target_os = "linux")]
fn linux_file_stats(file: &fs::File) -> std::io::Result<libc::statfs> {
    use std::os::fd::AsRawFd as _;

    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `file` stays open and `stats` points to enough writable memory
    // for the kernel's complete `statfs` result.
    if unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: A successful `fstatfs` initialized the complete result.
    Ok(unsafe { stats.assume_init() })
}

#[cfg(all(target_os = "linux", test))]
fn linux_file_is_btrfs(file: &fs::File) -> std::io::Result<bool> {
    Ok(linux_file_stats(file)?.f_type as u64 == libc::BTRFS_SUPER_MAGIC as u64)
}

#[cfg(target_os = "macos")]
fn macos_volume_uuid(file: &fs::File) -> std::io::Result<[u8; 16]> {
    use std::os::fd::AsRawFd as _;

    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_VOL_INFO: u32 = 0x8000_0000;
    const ATTR_VOL_UUID: u32 = 0x0004_0000;

    #[repr(C)]
    struct AttrList {
        bitmap_count: u16,
        reserved: u16,
        common_attr: u32,
        volume_attr: u32,
        directory_attr: u32,
        file_attr: u32,
        fork_attr: u32,
    }

    #[repr(C)]
    struct VolumeUuidBuffer {
        length: u32,
        uuid: [u8; 16],
    }

    unsafe extern "C" {
        fn fgetattrlist(
            fd: libc::c_int,
            attributes: *mut AttrList,
            buffer: *mut libc::c_void,
            buffer_size: libc::size_t,
            options: libc::c_ulong,
        ) -> libc::c_int;
    }

    let mut attributes = AttrList {
        bitmap_count: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        common_attr: 0,
        volume_attr: ATTR_VOL_INFO | ATTR_VOL_UUID,
        directory_attr: 0,
        file_attr: 0,
        fork_attr: 0,
    };
    let mut buffer = VolumeUuidBuffer {
        length: 0,
        uuid: [0; 16],
    };
    let result = unsafe {
        fgetattrlist(
            file.as_raw_fd(),
            &mut attributes,
            (&mut buffer as *mut VolumeUuidBuffer).cast(),
            std::mem::size_of::<VolumeUuidBuffer>(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if buffer.length as usize != std::mem::size_of::<VolumeUuidBuffer>() || buffer.uuid == [0; 16] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "filesystem did not return a stable volume UUID",
        ));
    }
    Ok(buffer.uuid)
}

pub(crate) fn workspace_identity_matches(workspace: &Path, stored: &str) -> Result<bool> {
    Ok(current_workspace_identity(workspace)?
        .is_some_and(|identity| identity.matches(Some(stored))))
}

pub fn workspace_is_configured(workspace: &Path) -> bool {
    workspace_state_path(workspace).is_ok_and(|state| state.join("config.json").is_file())
        || workspace
            .join(LEGACY_STATE_DIR)
            .join("config.json")
            .is_file()
}

/// Return the global workspace state directory, migrating the legacy in-project
/// directory and ignore policy on first access.
pub fn ensure_workspace_state(workspace: &Path) -> Result<PathBuf> {
    ensure_workspace_state_in(workspace, &global_state_root()?)
}

pub(crate) fn ensure_workspace_state_in(workspace: &Path, root: &Path) -> Result<PathBuf> {
    let resolution = resolve_workspace_state_in(workspace, root, false)?;
    let WorkspaceStateResolution {
        path: state,
        identity,
        slot_existed,
        adopt_path_only,
        from_legacy,
        revive,
    } = resolution;
    let workspaces = state
        .parent()
        .context("global workspace state has no parent")?;
    private_dir(workspaces)?;
    let id = workspace_state_id(workspace)?;
    let state_slot = state
        .file_name()
        .and_then(|name| name.to_str())
        .context("global workspace state slot is not UTF-8")?;
    let lock = workspaces.join(format!(".{state_slot}.migration.lock"));
    let _guard = MigrationLock::acquire(&lock)?;
    revalidate_workspace_identity(workspace, identity.as_ref())?;

    let legacy = workspace.join(LEGACY_STATE_DIR);
    let legacy_is_global_root = paths_resolve_to_same_location(&legacy, root);
    // Path-hash migration moves state bytes (rename or verified copy), so it
    // is serialized against every other live process's full-lifetime state
    // lease and fails closed while any of them holds the slot.
    let _exclusive = if !legacy_is_global_root && legacy.exists() {
        Some(
            crate::workspace_state_registry::ExclusiveStateLease::try_acquire(root, state_slot)
                .with_context(|| {
                    format!(
                        "serialize path-hash migration for workspace-state slot {}",
                        state_slot
                    )
                })?,
        )
    } else {
        None
    };
    if !legacy_is_global_root {
        recover_overlapping_state(root, &id, &legacy, &state)?;
    }
    if legacy.exists() && !legacy_is_global_root {
        migrate_legacy_state(&legacy, &state, workspaces, &id)?;
        relocate_conflict_paths(&state, &legacy, &state)?;
    } else {
        private_dir(&state)?;
    }
    migrate_agent_layouts(&state)?;

    import_legacy_ignore(workspace, &state)?;
    revalidate_workspace_identity(workspace, identity.as_ref())?;
    write_location(&state, workspace)?;
    let mut index_record = None;
    if let Some(identity) = identity.as_ref() {
        write_private(&state.join("identity"), identity.stable.as_bytes())?;
        let canonical = canonical_workspace(workspace)?
            .to_str()
            .context("workspace path is not valid UTF-8 and cannot be recorded portably")?
            .to_string();
        if revive {
            // The live folder claims a tombstoned slot and the identity was
            // revalidated above: retirement is revoked, not deleted around.
            crate::workspace_state_registry::clear_slot_tombstone(&state)?;
            tracing::info!("Revived retired workspace state at {}", state.display());
        }
        if !slot_existed || adopt_path_only || from_legacy {
            // Prospective provenance, recorded the first time a slot becomes
            // identity-bound. Adoption and legacy migration are explicit and
            // marked; fresh slots carry no adoption marker.
            let provenance = crate::workspace_state_registry::SlotProvenance {
                version: 1,
                established_unix_ns: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_nanos() as u64)
                    .unwrap_or_default(),
                identity: identity.stable.clone(),
                canonical_path: canonical.clone(),
                adopted_from: if adopt_path_only {
                    Some("absent-identity".to_string())
                } else if from_legacy {
                    Some("legacy-migration".to_string())
                } else {
                    None
                },
            };
            crate::workspace_state_registry::write_slot_provenance(&state, &provenance)?;
        }
        index_record = Some((identity.stable.clone(), canonical));
    }
    maintain_workspace(workspace, &state)?;
    revalidate_workspace_identity(workspace, identity.as_ref())?;
    // Removing the migration lock mutates the workspaces directory. Publish
    // the index afterward so its freshness stamp represents the final slot
    // layout rather than becoming stale as `ensure` returns.
    drop(_guard);
    if let Some((identity, canonical)) = index_record {
        crate::workspace_state_registry::upsert_index_entry(
            root, &identity, &canonical, state_slot,
        )?;
    }
    crate::workspace_state_registry::maybe_sweep_retired_state(root);
    Ok(state)
}

fn paths_resolve_to_same_location(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn recover_overlapping_state(root: &Path, id: &str, legacy: &Path, state: &Path) -> Result<()> {
    if !legacy.exists() || !state.exists() {
        return Ok(());
    }
    if trees_equal(legacy, state)? {
        fs::remove_dir_all(legacy).context("finish interrupted workspace-state migration")?;
        return Ok(());
    }

    // The project-local directory is the state an older installed client could
    // still be updating. Preserve the partial/stale global copy, then migrate
    // that active legacy state. No bytes are discarded.
    let quarantine = root.join("quarantine");
    private_dir(&quarantine)?;
    let destination = unique_child(&quarantine, &format!("workspace-{id}-global"));
    fs::rename(state, &destination).with_context(|| {
        format!(
            "quarantine conflicting global workspace state at {}",
            destination.display()
        )
    })?;
    tracing::warn!(
        "Preserved conflicting global workspace state at {} before migrating {}",
        destination.display(),
        legacy.display()
    );
    Ok(())
}

fn migrate_legacy_state(legacy: &Path, state: &Path, workspaces: &Path, id: &str) -> Result<()> {
    if !legacy.is_dir() {
        bail!(
            "legacy FeanorFS state is not a directory: {}",
            legacy.display()
        );
    }
    match fs::rename(legacy, state) {
        Ok(()) => return Ok(()),
        Err(rename_error) => tracing::debug!(
            "Direct workspace-state move failed ({}); using verified copy fallback",
            rename_error
        ),
    }

    let staging = unique_child(workspaces, &format!(".{id}.migrating"));
    let result = (|| -> Result<()> {
        copy_tree(legacy, &staging)?;
        if !trees_equal(legacy, &staging)? {
            bail!("workspace-state copy verification failed");
        }
        fs::rename(&staging, state).context("publish copied global workspace state")?;
        fs::remove_dir_all(legacy).context("remove verified legacy workspace state")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result.with_context(|| {
        format!(
            "move workspace state out of the project from {} to {}",
            legacy.display(),
            state.display()
        )
    })
}

pub(crate) fn unique_child(parent: &Path, stem: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    parent.join(format!("{stem}-{stamp}-{}", std::process::id()))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    private_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(&target, entry.metadata()?.permissions())?;
        } else {
            bail!(
                "workspace state contains unsupported symlink or special file: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn trees_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_entries = sorted_entries(left)?;
    let right_entries = sorted_entries(right)?;
    if left_entries.len() != right_entries.len() {
        return Ok(false);
    }
    for ((left_name, left_kind), (right_name, right_kind)) in
        left_entries.into_iter().zip(right_entries)
    {
        if left_name != right_name || left_kind != right_kind {
            return Ok(false);
        }
        let left_path = left.join(&left_name);
        let right_path = right.join(&right_name);
        match left_kind {
            EntryKind::Directory => {
                if !trees_equal(&left_path, &right_path)? {
                    return Ok(false);
                }
            }
            EntryKind::File => {
                if fs::metadata(&left_path)?.len() != fs::metadata(&right_path)?.len()
                    || !files_equal(&left_path, &right_path)?
                {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
}

fn sorted_entries(directory: &Path) -> Result<Vec<(std::ffi::OsString, EntryKind)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let kind = if kind.is_dir() {
            EntryKind::Directory
        } else if kind.is_file() {
            EntryKind::File
        } else {
            bail!(
                "unsupported workspace-state entry: {}",
                entry.path().display()
            );
        };
        entries.push((entry.file_name(), kind));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn relocate_conflict_paths(state: &Path, old_root: &Path, new_root: &Path) -> Result<()> {
    let path = state.join("local_state.json");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read migrated local conflict state"),
    };
    let mut local = LocalStateV1::from_json(&content)?;
    let mut changed = false;
    for record in local.conflict_registry.values_mut() {
        let current = Path::new(&record.conflict_dir);
        if let Ok(relative) = current.strip_prefix(old_root) {
            let relocated = new_root.join(relative);
            record.conflict_dir = relocated
                .to_str()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "relocated conflict directory is not valid UTF-8: {}",
                        relocated.display()
                    )
                })?
                .to_string();
            changed = true;
        }
    }
    if changed {
        write_private(&path, local.to_json()?.as_bytes())?;
    }
    Ok(())
}

fn migrate_agent_layouts(state: &Path) -> Result<()> {
    let agents = state.join("agents");
    let entries = match fs::read_dir(&agents) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read legacy agent workspaces"),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let root = entry.path();
        let worktree = root.join("worktree");
        if worktree.is_dir() {
            continue;
        }
        let staging = root.join("worktree.migrating");
        private_dir(&staging)?;
        let children = fs::read_dir(&root)?
            .filter_map(std::result::Result::ok)
            .map(|child| child.path())
            .collect::<Vec<_>>();
        for child in children {
            let name = child.file_name().and_then(|name| name.to_str());
            if matches!(
                name,
                Some("state" | "worktree" | "worktree.migrating" | "legacy-state")
            ) {
                continue;
            }
            if name == Some(LEGACY_STATE_DIR) {
                let base = child.join("base-snapshot");
                let destination = root.join("state/base-snapshot");
                if base.is_file() && !destination.exists() {
                    if let Some(parent) = destination.parent() {
                        private_dir(parent)?;
                    }
                    fs::copy(&base, &destination)?;
                }
                fs::rename(&child, root.join("legacy-state"))
                    .context("preserve legacy agent cache outside its worktree")?;
                continue;
            }
            let destination = staging.join(
                child
                    .file_name()
                    .context("legacy agent entry has no file name")?,
            );
            fs::rename(&child, destination).context("move legacy agent content into worktree")?;
        }
        fs::rename(&staging, &worktree).context("publish migrated agent worktree")?;
    }
    Ok(())
}

fn import_legacy_ignore(workspace: &Path, state: &Path) -> Result<()> {
    let legacy = workspace.join(LEGACY_IGNORE_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    if !fs::symlink_metadata(&legacy)?.file_type().is_file() {
        bail!(
            "legacy project-local ignore policy is not a regular file: {}",
            legacy.display()
        );
    }
    let content = fs::read(&legacy).context("read legacy project-local ignore policy")?;
    let destination = state.join("ignore");
    if destination.exists() && fs::read(&destination)? != content {
        let previous = unique_child(state, "ignore.previous");
        fs::rename(&destination, &previous)
            .context("preserve previous global workspace ignore policy")?;
        tracing::warn!(
            "Preserved a differing global ignore policy at {} before importing the active legacy policy",
            previous.display()
        );
    }
    write_private(&destination, &content)?;
    fs::remove_file(&legacy).context("remove migrated project-local ignore policy")?;
    Ok(())
}

fn write_location(state: &Path, workspace: &Path) -> Result<()> {
    let canonical = canonical_workspace(workspace)?;
    let canonical = canonical
        .to_str()
        .context("workspace path is not valid UTF-8 and cannot be persisted portably")?;
    write_private(&state.join("location"), canonical.as_bytes())
}

/// Private durable replacement: writes `bytes` to `path`
/// with mode 0o600 via a temp file, data sync, atomic rename, and — on Unix —
/// a parent-directory sync so the rename survives power loss. A parent-sync
/// failure reports the write as committed-but-durability-uncertain (the rename
/// already happened); callers must not roll back committed state.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("private state file has no parent")?;
    private_dir(parent)?;
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    file.commit()?;
    #[cfg(unix)]
    {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if let Err(error) = fs::File::open(parent).and_then(|dir| dir.sync_all()) {
                return Err(crate::durable::durability_uncertain(parent, error));
            }
        }
    }
    Ok(())
}

struct MigrationLock(PathBuf);

impl MigrationLock {
    fn acquire(path: &Path) -> Result<Self> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self(path.to_path_buf()));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::read_to_string(path)
                        .ok()
                        .and_then(|pid| pid.trim().parse::<u32>().ok())
                        .is_none_or(|pid| !crate::lock::pid_alive(pid));
                    if stale {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        bail!("workspace state migration is already running");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn retention_age() -> Duration {
    let days = std::env::var("FEANORFS_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|days| *days > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    Duration::from_secs(days.saturating_mul(24 * 60 * 60))
}

pub fn maintain_workspace_state(state: &Path) -> Result<()> {
    let retention = retention_age();
    purge_old_children(&state.join("tmp"), TEMP_RETENTION)?;
    purge_old_children(&state.join("recovery"), retention)?;
    rotate_log(&state.join("feanorfs.log"), retention)?;
    Ok(())
}

fn maintain_workspace(workspace: &Path, state: &Path) -> Result<()> {
    let stamp = state.join("maintenance.stamp");
    if fs::metadata(&stamp)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAINTENANCE_INTERVAL)
    {
        return Ok(());
    }
    maintain_workspace_state(state)?;
    purge_stale_worktree_temps(workspace, TEMP_RETENTION)?;
    write_private(&stamp, b"workspace maintenance v1\n")
}

fn purge_stale_worktree_temps(workspace: &Path, max_age: Duration) -> Result<()> {
    fn visit(directory: &Path, now: SystemTime, max_age: Duration) -> Result<()> {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if matches!(name.as_ref(), ".git" | ".jj" | ".feanorfs") {
                    continue;
                }
                visit(&entry.path(), now, max_age)?;
            } else if kind.is_file()
                && name.starts_with(".feanorfs-tmp-")
                && entry
                    .metadata()?
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age > max_age)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
    visit(workspace, SystemTime::now(), max_age)
}

fn purge_old_children(directory: &Path, max_age: Duration) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() <= max_age {
            continue;
        }
        let path = entry.path();
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn rotate_log(log: &Path, retention: Duration) -> Result<()> {
    let lock_path = log.with_extension("log.lock");
    let mut lock_options = OpenOptions::new();
    lock_options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        lock_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = lock_options.open(lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }

    let rotated = log.with_extension("log.old");
    if fs::metadata(&rotated).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
        fs::remove_file(&rotated)?;
    }
    #[cfg(unix)]
    if rotated.exists() {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&rotated, fs::Permissions::from_mode(0o600))?;
    }
    let Ok(metadata) = fs::metadata(log) else {
        return Ok(());
    };
    let expired = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > retention);
    if metadata.len() <= MAX_LOG_BYTES && !expired {
        return Ok(());
    }
    match fs::remove_file(&rotated) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if metadata.len() > MAX_LOG_BYTES {
        // Never retain an oversized generation produced by an older or
        // uncooperative writer. Active descriptors may finish their current
        // record on the unlinked inode; bounded writers reopen the live path.
        fs::remove_file(log)?;
    } else {
        fs::rename(log, &rotated)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(rotated, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ConflictRecordV1;
    use feanorfs_common::ConflictKind;

    #[test]
    fn fresh_state_creates_no_project_metadata() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();

        let state = ensure_workspace_state_in(&project, global.path()).unwrap();

        assert!(state.starts_with(global.path().join("workspaces")));
        assert!(state.join("location").is_file());
        assert!(!project.join(LEGACY_STATE_DIR).exists());
        assert!(!project.join(LEGACY_IGNORE_FILE).exists());
    }

    #[test]
    fn workspace_containing_global_state_does_not_migrate_the_root_into_itself() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("home");
        let global = workspace.join(LEGACY_STATE_DIR);
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join("global-sentinel"), b"keep").unwrap();

        let state = ensure_workspace_state_in(&workspace, &global).unwrap();

        assert_eq!(fs::read(global.join("global-sentinel")).unwrap(), b"keep");
        assert!(state.starts_with(global.join("workspaces")));
        assert!(state.join("location").is_file());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_identity_uses_volume_uuid_and_accepts_only_the_exact_legacy_device() {
        let project = tempfile::tempdir().unwrap();
        let identity = current_workspace_identity(project.path())
            .unwrap()
            .expect("temporary directory has a birth identity");
        assert!(identity.stable.starts_with("macos-v2:"));
        assert!(identity.matches(Some(&identity.stable)));

        let legacy = identity
            .compatible_legacy
            .as_deref()
            .expect("macOS identity retains the exact legacy representation");
        assert!(identity.matches(Some(legacy)));
        let (prefix, rest) = legacy.split_once(':').unwrap();
        let (device, tail) = rest.split_once(':').unwrap();
        let changed_device = device.parse::<u64>().unwrap().saturating_add(1);
        let mismatched = format!("{prefix}:{changed_device}:{tail}");
        assert!(!identity.matches(Some(&mismatched)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_identity_uses_file_handle_and_accepts_only_the_exact_legacy_device() {
        let fixture_root = std::env::var_os("FEANORFS_TEST_BTRFS_ROOT");
        let project = match fixture_root.as_ref() {
            Some(root) => tempfile::Builder::new().tempdir_in(root).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let file = fs::File::open(project.path()).unwrap();
        if !linux_file_is_btrfs(&file).unwrap() {
            assert!(
                fixture_root.is_none(),
                "FEANORFS_TEST_BTRFS_ROOT must identify a Btrfs filesystem"
            );
            return;
        }
        linux_file_identity(&file)
            .expect("Btrfs exposes an unprivileged descriptor-anchored file identity");
        let identity = current_workspace_identity(project.path())
            .unwrap()
            .expect("temporary directory has a birth identity");
        assert!(identity.stable.starts_with("linux-v2:"));
        assert!(identity.matches(Some(&identity.stable)));

        let global = match fixture_root.as_ref() {
            Some(root) => tempfile::Builder::new().tempdir_in(root).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let state = ensure_workspace_state_in(project.path(), global.path()).unwrap();
        fs::write(state.join("config.json"), b"preserved capability").unwrap();

        let legacy = identity
            .compatible_legacy
            .as_deref()
            .expect("Btrfs identity retains the exact legacy representation");
        assert!(identity.matches(Some(legacy)));
        let (prefix, rest) = legacy.split_once(':').unwrap();
        let (device, tail) = rest.split_once(':').unwrap();
        let changed_device = device.parse::<u64>().unwrap().saturating_add(1);
        let mismatched = format!("{prefix}:{changed_device}:{tail}");
        assert!(!identity.matches(Some(&mismatched)));

        fs::write(state.join("identity"), legacy).unwrap();
        let upgraded = ensure_workspace_state_in(project.path(), global.path()).unwrap();
        assert_eq!(upgraded, state);
        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"preserved capability"
        );
        assert_eq!(
            fs::read_to_string(state.join("identity")).unwrap(),
            identity.stable
        );

        fs::write(state.join("identity"), &mismatched).unwrap();
        let error = ensure_workspace_state_in(project.path(), global.path()).unwrap_err();
        assert!(error.to_string().contains("different folder"));
        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"preserved capability"
        );
        assert_eq!(
            fs::read_to_string(state.join("identity")).unwrap(),
            mismatched
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_identity_revalidation_rejects_folder_substitution() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let expected = current_workspace_identity(first.path())
            .unwrap()
            .expect("first directory has a birth identity");

        let error = revalidate_workspace_identity(second.path(), Some(&expected)).unwrap_err();
        assert!(error.to_string().contains("workspace path changed"));
    }

    /// `write_private` is the private durable replacement
    /// policy — 0o600 mode, atomic replace, no temp leftovers, and new bytes
    /// fully visible after success (all-or-nothing rename).
    #[test]
    fn write_private_is_private_durable_replacement() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("state");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("location");

        write_private(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        write_private(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let leftovers = fs::read_dir(&parent)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(".location.") || name.starts_with(".feanorfs-tmp-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn legacy_state_and_ignore_rules_move_out_of_project() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        let legacy = project.join(LEGACY_STATE_DIR);
        fs::create_dir_all(legacy.join("conflicts/1")).unwrap();
        fs::write(legacy.join("config.json"), b"legacy config").unwrap();
        fs::write(project.join(LEGACY_IGNORE_FILE), b"server-data/\n").unwrap();
        let mut local = LocalStateV1::default();
        local.conflict_registry.insert(
            "src/lib.rs".into(),
            ConflictRecordV1 {
                path: "src/lib.rs".into(),
                kind: ConflictKind::EditEdit,
                conflict_dir: legacy.join("conflicts/1").to_string_lossy().into_owned(),
                opened_at: 1,
                status: crate::state::ConflictRecordStatus::Pending,
                conflict_fingerprint: None,
            },
        );
        fs::write(legacy.join("local_state.json"), local.to_json().unwrap()).unwrap();

        let state = ensure_workspace_state_in(&project, global.path()).unwrap();

        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"legacy config"
        );
        assert_eq!(fs::read(state.join("ignore")).unwrap(), b"server-data/\n");
        assert!(!legacy.exists());
        assert!(!project.join(LEGACY_IGNORE_FILE).exists());
        let migrated =
            LocalStateV1::from_json(&fs::read_to_string(state.join("local_state.json")).unwrap())
                .unwrap();
        assert!(migrated.conflict_registry["src/lib.rs"]
            .conflict_dir
            .starts_with(&state.to_string_lossy().into_owned()));
    }

    #[test]
    fn conflicting_partial_global_state_is_quarantined_and_legacy_wins() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        let legacy = project.join(LEGACY_STATE_DIR);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("config.json"), b"active legacy").unwrap();
        let state = workspace_state_path_in(&project, global.path(), false).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("config.json"), b"partial global").unwrap();

        let state = ensure_workspace_state_in(&project, global.path()).unwrap();

        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"active legacy"
        );
        assert!(!legacy.exists());
        let quarantine = global.path().join("quarantine");
        let preserved = fs::read_dir(quarantine)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(preserved.len(), 1);
        assert_eq!(
            fs::read(preserved[0].join("config.json")).unwrap(),
            b"partial global"
        );
    }

    #[test]
    fn renamed_folder_keeps_its_existing_global_state() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let original = project_parent.path().join("before");
        let renamed = project_parent.path().join("after");
        fs::create_dir(&original).unwrap();
        let state = ensure_workspace_state_in(&original, global.path()).unwrap();
        fs::write(state.join("config.json"), b"configured").unwrap();

        fs::rename(&original, &renamed).unwrap();
        let relocated = ensure_workspace_state_in(&renamed, global.path()).unwrap();

        assert_eq!(relocated, state);
        assert_eq!(
            fs::read(relocated.join("config.json")).unwrap(),
            b"configured"
        );
        assert_eq!(
            fs::read_to_string(relocated.join("location")).unwrap(),
            fs::canonicalize(renamed).unwrap().to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_path_identity_mismatch_never_inherits_existing_state() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();
        let state = ensure_workspace_state_in(&project, global.path()).unwrap();
        fs::write(state.join("config.json"), b"old capability").unwrap();
        fs::write(state.join("identity"), b"unix:0:0:0:0").unwrap();

        let error = ensure_workspace_state_in(&project, global.path()).unwrap_err();
        assert!(error.to_string().contains("different folder"));
        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"old capability"
        );
    }

    #[test]
    fn moved_lookup_rejects_duplicate_identity_matches() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let original = project_parent.path().join("before");
        let renamed = project_parent.path().join("after");
        fs::create_dir(&original).unwrap();
        let state = ensure_workspace_state_in(&original, global.path()).unwrap();
        let duplicate = global.path().join("workspaces").join("f".repeat(64));
        fs::create_dir(&duplicate).unwrap();
        fs::copy(state.join("identity"), duplicate.join("identity")).unwrap();
        fs::rename(&original, &renamed).unwrap();

        let error = workspace_state_path_in(&renamed, global.path(), false).unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn verified_copy_preserves_nested_bytes() {
        let source = tempfile::tempdir().unwrap();
        let destination_parent = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("objects/nested")).unwrap();
        fs::write(source.path().join("objects/nested/blob"), b"ciphertext").unwrap();
        let destination = destination_parent.path().join("copy");

        copy_tree(source.path(), &destination).unwrap();

        assert!(trees_equal(source.path(), &destination).unwrap());
    }

    #[test]
    fn legacy_agent_content_is_separated_from_agent_metadata() {
        let state = tempfile::tempdir().unwrap();
        let agent = state.path().join("agents/coder");
        fs::create_dir_all(agent.join("src")).unwrap();
        fs::create_dir_all(agent.join(".feanorfs")).unwrap();
        fs::write(agent.join("src/lib.rs"), b"agent work").unwrap();
        fs::write(agent.join(".feanorfs/base-snapshot"), b"snapshot-id").unwrap();
        fs::write(agent.join(".feanorfs/local_state.json"), b"cache").unwrap();

        migrate_agent_layouts(state.path()).unwrap();

        assert_eq!(
            fs::read(agent.join("worktree/src/lib.rs")).unwrap(),
            b"agent work"
        );
        assert_eq!(
            fs::read(agent.join("state/base-snapshot")).unwrap(),
            b"snapshot-id"
        );
        assert!(agent.join("legacy-state/local_state.json").is_file());
        assert!(!agent.join("worktree/.feanorfs").exists());
    }

    #[test]
    fn retention_removes_expired_children_and_rotates_oversized_log() {
        let state = tempfile::tempdir().unwrap();
        fs::create_dir(state.path().join("tmp")).unwrap();
        fs::write(state.path().join("tmp/old"), b"temporary").unwrap();
        std::thread::sleep(Duration::from_millis(2));
        purge_old_children(&state.path().join("tmp"), Duration::ZERO).unwrap();
        assert!(!state.path().join("tmp/old").exists());

        let log = state.path().join("feanorfs.log");
        let file = fs::File::create(&log).unwrap();
        file.set_len(MAX_LOG_BYTES + 1).unwrap();
        rotate_log(&log, Duration::from_secs(u64::MAX)).unwrap();
        assert!(!log.exists());
        assert!(!state.path().join("feanorfs.log.old").exists());
    }

    #[test]
    fn log_maintenance_skips_a_writer_holding_the_shared_rotation_lock() {
        let state = tempfile::tempdir().unwrap();
        let log = state.path().join("feanorfs.log");
        let file = fs::File::create(&log).unwrap();
        file.set_len(MAX_LOG_BYTES + 1).unwrap();
        drop(file);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(log.with_extension("log.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();

        rotate_log(&log, Duration::from_secs(u64::MAX)).unwrap();
        assert!(log.exists(), "maintenance must not race an active writer");
    }

    #[cfg(unix)]
    #[test]
    fn same_path_folder_replacement_fails_closed_with_real_folders() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();
        let state = ensure_workspace_state_in(&project, global.path()).unwrap();
        fs::write(state.join("config.json"), b"old capability").unwrap();

        fs::remove_dir_all(&project).unwrap();
        fs::create_dir(&project).unwrap();

        let error = ensure_workspace_state_in(&project, global.path()).unwrap_err();
        assert!(error.to_string().contains("different folder"));
        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"old capability"
        );
    }

    #[test]
    fn path_only_slot_is_adopted_once_with_provenance() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();
        let state = workspace_state_path_in(&project, global.path(), false).unwrap();
        fs::create_dir_all(&state).unwrap();
        // Simulate a slot written by a version that never recorded identity
        // or provenance, with a recorded location for this exact path.
        fs::write(state.join("config.json"), b"preserved capability").unwrap();
        fs::write(
            state.join("location"),
            fs::canonicalize(&project)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();

        let adopted = ensure_workspace_state_in(&project, global.path()).unwrap();

        assert_eq!(adopted, state);
        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"preserved capability"
        );
        assert!(state.join("identity").is_file());
        assert!(state.join("provenance.json").is_file());
        let provenance: crate::workspace_state_registry::SlotProvenance =
            serde_json::from_slice(&fs::read(state.join("provenance.json")).unwrap()).unwrap();
        assert_eq!(provenance.adopted_from.as_deref(), Some("absent-identity"));
        // Second access is a plain identity match, no second adoption.
        let again = ensure_workspace_state_in(&project, global.path()).unwrap();
        assert_eq!(again, state);
    }

    #[test]
    fn path_only_slot_without_a_matching_location_never_adopts() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();
        let state = workspace_state_path_in(&project, global.path(), false).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("config.json"), b"preserved capability").unwrap();
        // No location file: nothing proves this slot belongs to this path.

        let error = ensure_workspace_state_in(&project, global.path()).unwrap_err();
        assert!(error.to_string().contains("different folder"));
        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"preserved capability"
        );
    }

    #[test]
    fn path_only_slot_loses_to_an_identity_matching_slot_without_silent_adoption() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();
        let state = workspace_state_path_in(&project, global.path(), false).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("location"),
            fs::canonicalize(&project)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        let identity = current_workspace_identity(&project)
            .unwrap()
            .expect("folder has an identity");
        let competing = global.path().join("workspaces").join("f".repeat(64));
        fs::create_dir(&competing).unwrap();
        fs::write(competing.join("identity"), identity.stable.as_bytes()).unwrap();

        // Exact identity wins over the unauthenticated path-only slot, and
        // the path-only bytes are preserved without being silently adopted.
        let resolved = ensure_workspace_state_in(&project, global.path()).unwrap();
        assert_eq!(resolved, competing);
        assert!(competing.join("identity").is_file());
        assert!(!state.join("identity").exists());
    }

    #[test]
    fn weak_identity_formats_are_stable_and_exact() {
        let weak = unix_weak_identity(2049, 17);
        assert_eq!(weak, "unix-weak:2049:17");
        let identity = WorkspaceIdentity {
            stable: weak.clone(),
            compatible_legacy: None,
        };
        assert!(identity.matches(Some(&weak)));
        assert!(!identity.matches(Some("unix-weak:2049:18")));
        // A weak identity never claims a legacy identity it cannot verify.
        assert!(!identity.matches(Some("unix:2049:17:1:0")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_detects_same_path_replacement() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();
        let state = ensure_workspace_state_in(&project, global.path()).unwrap();
        fs::write(state.join("config.json"), b"old capability").unwrap();
        let stored = fs::read_to_string(state.join("identity")).unwrap();
        assert!(stored.starts_with("windows-v2:"));

        fs::remove_dir_all(&project).unwrap();
        fs::create_dir(&project).unwrap();

        let error = ensure_workspace_state_in(&project, global.path()).unwrap_err();
        assert!(error.to_string().contains("different folder"));
        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"old capability"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_workspace_paths_and_utf8_symlink_aliases_fail_closed() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first = directory
            .path()
            .join(OsString::from_vec(b"workspace-\xff".to_vec()));
        let second = directory
            .path()
            .join(OsString::from_vec(b"workspace-\xfe".to_vec()));
        assert!(workspace_state_id(&first).is_err());
        assert!(workspace_state_id(&second).is_err());

        // Some filesystems (including common macOS APFS configurations)
        // reject non-UTF-8 names before FeanorFS can observe them. Platforms
        // that permit such names also exercise the canonicalized symlink case.
        if fs::create_dir(&first).is_ok() {
            let alias = directory.path().join("utf8-alias");
            symlink(&first, &alias).unwrap();
            assert!(workspace_state_id(&alias).is_err());
        }
    }
}
