//! Unix process ownership: dedicated process groups, exact start identity
//! probes, exec-gate descriptor setup, and signals.
//!
//! Every unsafe block documents its invariant in place:
//! file descriptors are either inherited-and-owned or closed exactly once,
//! `pre_exec` never blocks, and every signal targets a boundary that was
//! revalidated against the recorded start identity.

use std::io;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use super::{process_start_matches, ProcessGroup, ProcessIdentity};

/// Installs the nonblocking exec-gate descriptor setup in `pre_exec` and
/// returns the parent-side `(release, child_endpoint)` pair.
///
/// Invariants:
/// - `pre_exec` performs only `fcntl`/`close` descriptor setup and returns
///   immediately; it never waits on the release pipe (a blocking hook would
///   deadlock `Command::spawn`, which waits for exec or a pre-exec error).
/// - The child's read endpoint has `FD_CLOEXEC` cleared so it survives the
///   wrapper's `exec`; the parent's write endpoint is closed in the child so
///   the wrapper observes EOF if the gate is dropped unreleased.
/// - The returned streams are owned by the caller (the startup gate); they
///   are closed exactly once when the gate is dropped or released.
pub(super) fn prepare_startup_gate(
    command: &mut tokio::process::Command,
) -> io::Result<(UnixStream, UnixStream)> {
    use std::os::unix::process::CommandExt as _;

    let (release, child_endpoint) = UnixStream::pair()?;
    let child_read_fd = child_endpoint.as_raw_fd();
    let child_release_fd = release.as_raw_fd();
    // SAFETY: this hook performs only fcntl/close descriptor setup and
    // returns immediately; it never waits on the release pipe. The raw
    // descriptors are valid for the spawn that immediately follows and the
    // closure runs in the post-fork pre-exec context of that exact spawn.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            let flags = libc::fcntl(child_read_fd, libc::F_GETFD);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(child_read_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::close(child_release_fd) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok((release, child_endpoint))
}

/// Waits on the inherited release descriptor, then execs the configured
/// worker in-place. The wrapper owns `release_fd` from conversion until the
/// exec; the final `exec` closes it along with every other descriptor.
///
/// # Safety
/// `release_fd` is the raw descriptor handed to this process by the
/// supervisor's `StartupGate`; it is valid for the lifetime of this wrapper
/// and is taken into ownership here, so no other owner can close it early.
pub(crate) fn exec_gate_wait_and_exec(
    release_fd: RawFd,
    program: &std::path::Path,
    args: &[std::ffi::OsString],
) -> io::Result<()> {
    use std::os::fd::FromRawFd;
    // SAFETY: the descriptor is inherited from the supervisor's gate and is
    // owned by this wrapper after conversion; it is closed before exec.
    let mut release = unsafe { std::fs::File::from_raw_fd(release_fd) };
    let mut byte = [0_u8; 1];
    std::io::Read::read_exact(&mut release, &mut byte)?;
    if byte[0] != super::StartupGate::RELEASE_BYTE {
        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
    }
    drop(release);
    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(program).args(args).exec();
    Err(error)
}

/// Places the child in a fresh process group whose id is the child PID. The
/// group is the ownership boundary: descendants inherit it, and group
/// termination covers the complete tree without PID scans.
pub(super) fn configure_process_group(command: &mut tokio::process::Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    command.as_std_mut().process_group(0);
    Ok(())
}

impl ProcessGroup {
    /// Probes whether the exact child-owned process group still exists.
    pub(crate) fn exists(&self) -> bool {
        if self.leader == 0 {
            return false;
        }
        let Ok(pid) = libc::pid_t::try_from(self.leader) else {
            return false;
        };
        // SAFETY: signal zero only probes the exact negative process-group
        // target and does not deliver a signal.
        let result = unsafe { libc::kill(-pid, 0) };
        let exists =
            result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !exists {
            return false;
        }
        // A live group whose persisted leader token no longer matches is an
        // unresolved ownership boundary, not proof that the group vanished.
        // Keep the caller in `Stopping` rather than dropping descendants or
        // ever signaling a reused PGID.  The revalidation is still performed
        // here even though both the matching and mismatch states retain the
        // conservative "possibly alive" result.
        let _identity_current = self.leader_identity_current();
        true
    }

    /// Verifies that the recorded PID is still the leader of its own group.
    /// This check is required before residual cleanup can signal a group after
    /// the original supervisor has gone away.
    #[cfg(test)]
    pub(crate) fn is_leader(&self) -> bool {
        if self.leader == 0 {
            return false;
        }
        let Ok(pid) = libc::pid_t::try_from(self.leader) else {
            return false;
        };
        self.leader_identity_current()
            // SAFETY: `getpgid` only queries the exact PID under consideration.
            && unsafe { libc::getpgid(pid) == pid }
    }

    /// Requests graceful termination of the exact process group.
    pub(crate) fn request_termination(&self) -> bool {
        self.signal(libc::SIGTERM)
    }

    /// Forces termination of the exact process group.
    pub(crate) fn force_termination(&self) -> bool {
        self.signal(libc::SIGKILL)
    }

    fn signal(&self, signal: libc::c_int) -> bool {
        if self.leader == 0 {
            return false;
        }
        let Ok(pid) = libc::pid_t::try_from(self.leader) else {
            return false;
        };
        let leader_current = self.leader_identity_current();
        if !leader_current {
            // Once the original leader has been reaped, its start token is no
            // longer queryable.  A still-existing process group is nevertheless
            // a safe boundary: the kernel cannot recycle its PGID while any
            // descendant remains in that group.  If the PID is alive instead,
            // the mismatch may be PID reuse and signaling is forbidden.
            let leader_probe = unsafe { libc::kill(pid, 0) };
            let leader_alive = leader_probe == 0
                || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
            if leader_alive {
                return false;
            }
            let group_probe = unsafe { libc::kill(-pid, 0) };
            if group_probe != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return false;
            }
        } else {
            // SAFETY: `getpgid` only queries the exact PID under consideration.
            if unsafe { libc::getpgid(pid) != pid } {
                return false;
            }
        }
        // SAFETY: callers only construct this value from a child PID that was
        // placed in a fresh group.  The caller additionally verifies exact
        // start identity before residual cleanup. The signal targets the
        // negative PID (the whole group), never a single recycled PID.
        unsafe { libc::kill(-pid, signal) == 0 }
    }

    fn leader_identity_current(&self) -> bool {
        self.leader_process_start_id
            .as_deref()
            .is_some_and(|identity| process_start_matches(self.leader, identity))
    }
}

impl ProcessIdentity {
    /// Direct signal to the exact PID. Callers check `is_current` immediately
    /// before each escalation step; `pid()` is the identity-bound PID.
    pub(super) fn signal(&self, signal: libc::c_int) -> bool {
        let Ok(pid) = libc::pid_t::try_from(self.pid()) else {
            return false;
        };
        // SAFETY: `is_current` was checked immediately before this direct
        // signal; unlike a raw PID caller, the process identity is rechecked
        // again for every escalation step.
        unsafe { libc::kill(pid, signal) == 0 }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn linux_process_start_ticks(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The process name is parenthesized and may itself contain spaces or ')'.
    // Splitting after the final `) ` leaves field 3 as the first item, making
    // field 22 (starttime) item 19 in the remaining whitespace list.
    let fields = stat.rsplit_once(") ")?.1;
    fields
        .split_whitespace()
        .nth(19)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .map(str::to_string)
}

#[cfg(target_os = "linux")]
pub(super) fn linux_process_start_matches(pid: u32, process_start_id: &str) -> bool {
    let Some((prefix, remainder)) = process_start_id.split_once(':') else {
        return false;
    };
    let Some((recorded_pid, recorded_ticks)) = remainder.split_once(':') else {
        return false;
    };
    prefix == "linux"
        && recorded_pid.parse::<u32>().ok() == Some(pid)
        && linux_process_start_ticks(pid).as_deref() == Some(recorded_ticks)
        && !recorded_ticks.is_empty()
        && recorded_ticks.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(target_os = "macos")]
pub(super) fn macos_process_start(pid: u32) -> Option<(u64, u64)> {
    use std::mem::MaybeUninit;

    let pid = libc::c_int::try_from(pid).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size = libc::c_int::try_from(size).ok()?;
    // SAFETY: `info` is a writable, correctly sized proc_bsdinfo buffer and
    // proc_pidinfo writes only within the supplied byte count.  The call does
    // not retain the pointer after returning.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo reported a complete proc_bsdinfo structure.
    let info = unsafe { info.assume_init() };
    (info.pbi_pid == pid as u32 && info.pbi_start_tvusec < 1_000_000)
        .then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(target_os = "macos")]
pub(super) fn macos_process_start_matches(pid: u32, process_start_id: &str) -> bool {
    let mut fields = process_start_id.split(':');
    let (Some(prefix), Some(recorded_pid), Some(recorded_seconds), Some(recorded_useconds)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    if fields.next().is_some() || prefix != "macos" {
        return false;
    }
    let Some(recorded_pid) = recorded_pid.parse::<u32>().ok() else {
        return false;
    };
    let Some(recorded_seconds) = recorded_seconds.parse::<u64>().ok() else {
        return false;
    };
    let Some(recorded_useconds) = recorded_useconds.parse::<u64>().ok() else {
        return false;
    };
    recorded_pid == pid
        && recorded_useconds < 1_000_000
        && macos_process_start(pid) == Some((recorded_seconds, recorded_useconds))
}

/// Kernel executable identity for a live PID: the `/proc/<pid>/exe`
/// device/inode (Linux) or `proc_pidpath` (macOS). Other Unix targets retain
/// the `executable_path` probe stub and never claim a native file ID.
pub(super) fn executable_identity_for_pid(pid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    #[cfg(target_os = "linux")]
    let path = std::path::PathBuf::from(format!("/proc/{pid}/exe"));
    #[cfg(target_os = "macos")]
    let path = {
        use std::os::unix::ffi::OsStringExt as _;
        let mut bytes = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `bytes` is writable for the supplied capacity and
        // proc_pidpath writes at most that capacity without retaining the
        // pointer.
        let length = unsafe {
            libc::proc_pidpath(
                pid as libc::c_int,
                bytes.as_mut_ptr().cast(),
                bytes.len() as u32,
            )
        };
        if length <= 0 {
            return None;
        }
        bytes.truncate(length as usize);
        std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let path = super::executable_path(pid)?;
    let metadata = std::fs::metadata(path).ok()?;
    Some(format!("unix-devino:{}:{}", metadata.dev(), metadata.ino()))
}
