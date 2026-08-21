//! Private process-tree ownership primitives for configured workers.
//!
//! Process groups and process-start identities are security boundaries for the
//! unattended runner.  Keep the platform-specific probes and libc calls here
//! so the runner loop and supervisor cannot grow subtly different PID/group
//! semantics.  The module deliberately exposes intent-level operations only:
//! callers can prepare an owned process group, compare an exact start
//! identity, and request or force termination of that tree. Windows Job
//! Object ownership and exact kernel probes stay behind this semantic seam so
//! callers never understand platform constants.
//!
//! Platform ownership is split by file:
//!
//! - `unix` — process groups, exact start identity probes, exec-gate
//!   descriptor setup, and signals;
//! - `windows` — suspended creation, Job Objects, handles, exact start
//!   identity, and termination;
//! - `portable` — the conservative fail-closed fallback for targets without
//!   native process ownership primitives;
//! - `reaper` — the single persistent Tokio-child reaper shared by the
//!   runner and supervisor;
//! - `tests` — platform-neutral identity/reaper state tests.

pub(crate) mod reaper;

#[cfg(not(unix))]
mod portable;
#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub(crate) use reaper::{ChildReaper, ReadyChildReaper, ReapTicket, CHILD_REAPER};
#[cfg(unix)]
pub(crate) use unix::exec_gate_wait_and_exec;
#[cfg(windows)]
pub(crate) use windows::executable_path;

use std::io;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::os::windows::io::OwnedHandle;

/// The process group created for one configured child.
///
/// On Unix, `leader` is the child PID because the launcher requests a fresh
/// process group in [`configure_process_group`]. Windows uses a Job Object
/// rather than treating a PID as a tree boundary; other targets retain this
/// type as a no-op seam and never claim that a PID-only operation is safe.
///
/// The fields are crate-internal: the platform implementations in [`unix`]
/// probe the exact group and the tests construct fail-closed mismatch
/// fixtures; no caller outside this module mutates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessGroup {
    pub(crate) leader: u32,
    #[cfg(unix)]
    pub(crate) leader_process_start_id: Option<String>,
}

impl ProcessGroup {
    /// Associates a group handle with the freshly spawned child PID.
    pub(crate) fn for_child(pid: u32) -> Self {
        #[cfg(unix)]
        let leader_process_start_id = {
            let identity = process_start_identifier(pid, "process-group-leader");
            process_start_matches(pid, &identity).then_some(identity)
        };
        Self {
            leader: pid,
            #[cfg(unix)]
            leader_process_start_id,
        }
    }

    /// Reconstructs a group from a persisted leader identity.  Orphan
    /// cleanup must use this form before even probing group existence; a fresh
    /// `for_child(pid)` would bind to whatever process currently owns a
    /// recycled PID rather than to the recorded child.
    pub(crate) fn for_child_with_identity(pid: u32, process_start_id: &str) -> Self {
        Self {
            leader: pid,
            #[cfg(unix)]
            leader_process_start_id: Some(process_start_id.to_string()),
        }
    }
}

/// Exact identity of one live process.
///
/// This token is captured from the kernel immediately before a supervisor
/// signals an orphan.  Every signal operation revalidates it first, so a PID
/// recycled between graceful and forceful termination is never targeted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pid: u32,
    process_start_id: String,
}

impl ProcessIdentity {
    /// Captures an exact native start identity for a live process.
    pub(crate) fn capture(pid: u32) -> Option<Self> {
        let process_start_id = process_start_identifier(pid, "");
        process_start_matches(pid, &process_start_id).then_some(Self {
            pid,
            process_start_id,
        })
    }

    /// Returns the PID represented by this exact identity.
    pub(crate) const fn pid(&self) -> u32 {
        self.pid
    }

    /// Revalidates the identity against the current kernel process.
    pub(crate) fn is_current(&self) -> bool {
        process_start_matches(self.pid, &self.process_start_id)
    }

    /// Requests graceful termination, only while the exact identity remains
    /// current.
    #[cfg(unix)]
    pub(crate) fn request_termination(&self) -> bool {
        self.is_current() && self.signal(libc::SIGTERM)
    }

    #[cfg(not(unix))]
    pub(crate) fn request_termination(&self) -> bool {
        #[cfg(windows)]
        {
            self.is_current()
                && windows::terminate_windows_process(self.pid, &self.process_start_id, 1)
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            false
        }
    }

    /// Forces termination, only while the exact identity remains current.
    #[cfg(unix)]
    pub(crate) fn force_termination(&self) -> bool {
        self.is_current() && self.signal(libc::SIGKILL)
    }

    #[cfg(not(unix))]
    pub(crate) fn force_termination(&self) -> bool {
        #[cfg(windows)]
        {
            self.is_current()
                && windows::terminate_windows_process(self.pid, &self.process_start_id, 1)
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            false
        }
    }
}

/// Native ownership of one configured child and every process it creates.
///
/// Unix callers use the child's dedicated process group. Windows callers use
/// a private Job Object configured with `KILL_ON_JOB_CLOSE`; every child is
/// created suspended, assigned to this Job Object, and resumed only after
/// membership verification. A process-lifetime owner Job Object is inherited
/// before creation, so a supervisor crash also kills a child in the brief
/// interval before private adoption. Other targets retain a zero-sized,
/// fail-closed marker until their native process ownership API is available.
pub(crate) struct ProcessTree {
    #[cfg(unix)]
    group: ProcessGroup,
    #[cfg(windows)]
    job: WindowsJob,
}

impl std::fmt::Debug for ProcessTree {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessTree")
            .finish_non_exhaustive()
    }
}

#[cfg(windows)]
fn assert_send<T: Send>() {}

#[cfg(windows)]
const _: fn() = assert_send::<ProcessTree>;

#[cfg(windows)]
use windows::WindowsJob;

impl ProcessTree {
    /// Attaches a freshly spawned child to a private ownership primitive.
    ///
    /// On Windows an inherited Job Object is not silently treated as our
    /// ownership boundary. We probe it with `IsProcessInJob`, attempt the
    /// nested assignment explicitly, and verify membership in the new job.
    /// Systems that reject nested jobs therefore fail closed at adoption time;
    /// callers must terminate/reap the just-spawned child and must not publish
    /// it as running.
    pub(crate) fn adopt_child(child: &tokio::process::Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let pid = child
                .id()
                .ok_or_else(|| io::Error::other("child exited before process-group adoption"))?;
            Ok(Self {
                group: ProcessGroup::for_child(pid),
            })
        }

        #[cfg(windows)]
        {
            windows::WindowsJob::adopt_child(child).map(|job| Self { job })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    /// Releases a Windows child after durable launch/status publication. Unix
    /// children have already entered the exec'd gate wrapper, so this is a
    /// no-op there. Only the Windows startup-gate release and Windows tests
    /// reach it, so other targets keep it compiled-but-dead.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn release_child(&self, child: &tokio::process::Child) -> io::Result<()> {
        #[cfg(windows)]
        {
            let pid = child
                .id()
                .ok_or_else(|| io::Error::other("child exited before startup release"))?;
            self.job.release_child(pid)
        }
        #[cfg(not(windows))]
        {
            let _ = (self, child);
            Ok(())
        }
    }

    /// Requests termination of every process in this tree.
    pub(crate) fn request_termination(&self) -> bool {
        #[cfg(unix)]
        {
            self.group.request_termination()
        }
        #[cfg(windows)]
        {
            self.job.terminate()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    /// Forces termination of every process in this tree.
    pub(crate) fn force_termination(&self) -> bool {
        #[cfg(unix)]
        {
            self.group.force_termination()
        }
        #[cfg(windows)]
        {
            self.job.terminate()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    /// Indicates whether a Unix process group still has a member. Windows
    /// Job Objects are intentionally not probed by PID/group scans; closing
    /// the retained handle is the ownership operation.
    #[cfg(unix)]
    pub(crate) fn exists(&self) -> bool {
        self.group.exists()
    }

    /// Queries the kernel-owned Job Object for active processes. Unlike a
    /// PID scan this remains valid after the direct root exits and covers all
    /// descendants in the private Job.
    #[cfg(windows)]
    pub(crate) fn is_empty(&self) -> io::Result<bool> {
        self.job.is_empty()
    }
}

/// Parent-controlled startup gate.
///
/// Unix cannot safely block in `pre_exec`: Rust's `Command::spawn` waits for
/// the child to either `exec` or report a pre-exec error over its private
/// error pipe, so a blocking hook deadlocks the parent. The gate therefore
/// installs only nonblocking descriptor setup in `pre_exec` (see
/// [`unix::prepare_startup_gate`]); the child immediately execs FeanorFS's
/// internal `service exec-gate` wrapper. That trusted wrapper blocks after
/// `spawn` has returned, and later execs the configured worker in-place when
/// the parent releases this gate. PID, process-group, stdin, environment, and
/// argv of the configured worker are preserved by the final `exec`.
pub(crate) struct StartupGate {
    #[cfg(unix)]
    release: Option<UnixStream>,
    #[cfg(unix)]
    child_endpoint: Option<UnixStream>,
    released: bool,
}

impl StartupGate {
    pub(super) const RELEASE_BYTE: u8 = 1;

    /// Installs Unix descriptor setup. Windows creates children suspended and
    /// releases them by resuming the verified primary thread.
    #[cfg_attr(test, allow(dead_code))] // production callers gate the gate on `not(test)`
    pub(crate) fn prepare(command: &mut tokio::process::Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let (release, child_endpoint) = unix::prepare_startup_gate(command)?;
            Ok(Self {
                release: Some(release),
                child_endpoint: Some(child_endpoint),
                released: false,
            })
        }

        #[cfg(not(unix))]
        {
            let _ = command;
            Ok(Self { released: false })
        }
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            #[cfg(unix)]
            release: None,
            #[cfg(unix)]
            child_endpoint: None,
            released: true,
        }
    }

    /// FD passed to the internal wrapper on Unix.
    #[cfg(unix)]
    #[cfg_attr(test, allow(dead_code))] // only the production launch path reads it
    pub(crate) fn release_fd(&self) -> RawFd {
        self.child_endpoint
            .as_ref()
            .expect("startup gate child endpoint retained")
            .as_raw_fd()
    }

    /// Releases the gate after durable identity/status publication.
    pub(crate) fn release(
        &mut self,
        #[cfg(windows)] tree: Option<&ProcessTree>,
        #[cfg(windows)] child: Option<&tokio::process::Child>,
    ) -> io::Result<()> {
        if self.released {
            return Ok(());
        }
        #[cfg(unix)]
        {
            drop(self.child_endpoint.take());
            let Some(release) = self.release.as_mut() else {
                return Err(io::Error::other("startup gate release endpoint missing"));
            };
            std::io::Write::write_all(release, &[Self::RELEASE_BYTE])?;
            std::io::Write::flush(release)?;
            self.release.take();
        }
        #[cfg(windows)]
        {
            let Some(tree) = tree else {
                return Err(io::Error::other("startup gate process tree missing"));
            };
            let Some(child) = child else {
                return Err(io::Error::other("startup gate child handle missing"));
            };
            tree.release_child(child)?;
        }
        self.released = true;
        Ok(())
    }
}

impl Drop for StartupGate {
    fn drop(&mut self) {
        // Dropping an unreleased Unix gate closes the writer. The trusted
        // wrapper observes EOF and exits before executing the worker.
        #[cfg(unix)]
        if !self.released {
            self.release.take();
            self.child_endpoint.take();
        }
    }
}

/// Prepares a configured child for fail-closed ownership.
///
/// Unix requests a fresh process group. Windows first establishes the
/// process-lifetime owner Job Object, then asks CreateProcess for a suspended
/// primary thread; [`ProcessTree::adopt_child`] assigns/verifies the private
/// Job Object and resumes that thread before any worker code can execute.
pub(crate) fn configure_process_group(command: &mut tokio::process::Command) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix::configure_process_group(command)
    }
    #[cfg(windows)]
    {
        return windows::configure_process_group(command);
    }
    #[cfg(not(any(unix, windows)))]
    {
        portable::configure_process_group(command)
    }
}

/// Returns a bounded opaque process-start identity for `pid`.
///
/// Linux uses the kernel's `/proc/<pid>/stat` start-time ticks.  macOS uses
/// `proc_pidinfo(PROC_PIDTBSDINFO)` and stores the kernel start seconds and
/// microseconds.  Both identities include the PID so a recycled PID cannot
/// match.  If the native probe fails, retain the historical `spawn:` marker;
/// validation intentionally rejects that legacy form, making cleanup
/// fail-closed rather than guessing by age.
pub(crate) fn process_start_identifier(pid: u32, session_id: &str) -> String {
    #[cfg(target_os = "linux")]
    if let Some(start_ticks) = unix::linux_process_start_ticks(pid) {
        return format!("linux:{pid}:{start_ticks}");
    }

    #[cfg(target_os = "macos")]
    if let Some((seconds, microseconds)) = unix::macos_process_start(pid) {
        return format!("macos:{pid}:{seconds}:{microseconds}");
    }

    #[cfg(target_os = "windows")]
    if let Some(creation_ticks) = windows::windows_process_creation_ticks(pid) {
        return format!("windows:{creation_ticks}");
    }

    // This preserves the old persisted shape for platforms without a native
    // exact probe.  `process_start_matches` rejects it on every platform.
    format!("spawn:{pid}:{session_id}")
}

/// Validates a persisted process-start identity against the exact live PID.
///
/// Legacy `spawn:<pid>:<session>` values are deliberately never accepted:
/// session IDs are correlation values, not kernel process identities.
pub(crate) fn process_start_matches(pid: u32, process_start_id: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        unix::linux_process_start_matches(pid, process_start_id)
    }
    #[cfg(target_os = "macos")]
    {
        unix::macos_process_start_matches(pid, process_start_id)
    }
    #[cfg(target_os = "windows")]
    {
        return windows::windows_process_start_matches(pid, process_start_id);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        portable::process_start_matches(pid, process_start_id)
    }
}

/// Captures a stable executable identity from a configured path.  Unix
/// device/inode pairs remain valid when an in-place upgrade unlinks the old
/// pathname while a worker still has the old image mapped.  Other platforms
/// retain a normalized path fallback until a native file-ID probe is
/// available.
pub(crate) fn executable_identity_for_path(path: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = std::fs::metadata(path).ok()?;
        Some(format!("unix-devino:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        return windows::normalize_windows_path(Some(path)).map(|value| format!("path:{value}"));
    }
    #[cfg(not(any(unix, windows)))]
    {
        return std::fs::canonicalize(path)
            .ok()
            .map(|value| format!("path:{}", value.display()));
    }
}

/// Captures the executable identity of an exact live process.  On Linux the
/// `/proc/<pid>/exe` descriptor resolves to the mapped inode even when its
/// display path ends in ` (deleted)`.
pub(crate) fn executable_identity_for_pid(pid: u32) -> Option<String> {
    #[cfg(unix)]
    {
        unix::executable_identity_for_pid(pid)
    }
    #[cfg(windows)]
    {
        return windows::executable_identity_for_pid(pid);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        None
    }
}

/// Compares a persisted executable identity to the exact live process.
pub(crate) fn executable_identity_matches(pid: u32, expected: &str) -> bool {
    executable_identity_for_pid(pid).as_deref() == Some(expected)
}

/// Compares an expected executable with the kernel image of `pid`.
///
/// Only the supervisor's Windows orphan path calls this today; the
/// non-Windows branch is retained unchanged (through the `executable_path`
/// probe stub) so other-Unix targets keep the same conservative semantics if
/// a caller appears.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn executable_matches(pid: u32, expected: &std::path::Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        return windows::normalize_windows_path(windows::executable_path(pid).as_deref())
            == windows::normalize_windows_path(Some(expected));
    }
    #[cfg(not(target_os = "windows"))]
    {
        executable_path(pid).and_then(|actual| std::fs::canonicalize(actual).ok())
            == std::fs::canonicalize(expected).ok()
    }
}

/// Non-Windows probe stub kept for the legacy `executable_matches` branch and
/// other-Unix executable identity capture; it never claims a native image.
/// On Linux/macOS it is reached only through the (dead, `cfg_attr`-allowed)
/// `executable_matches` arm, whose body still counts as a use.
#[cfg(not(target_os = "windows"))]
pub(crate) fn executable_path(_pid: u32) -> Option<std::path::PathBuf> {
    None
}
