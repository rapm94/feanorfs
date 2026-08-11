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

use std::io;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{GetLastError, FILETIME, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenThread, QueryFullProcessImageNameW,
    ResumeThread, TerminateProcess, CREATE_SUSPENDED, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
#[cfg(all(test, windows))]
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

#[cfg(all(test, windows))]
static TEST_FORCE_ADOPTION_FAILURE_PID: AtomicU32 = AtomicU32::new(0);

/// The process group created for one configured child.
///
/// On Unix, `leader` is the child PID because the launcher requests a fresh
/// process group in [`configure_process_group`]. Windows uses a Job Object
/// rather than treating a PID as a tree boundary; other targets retain this
/// type as a no-op seam and never claim that a PID-only operation is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessGroup {
    leader: u32,
    #[cfg(unix)]
    leader_process_start_id: Option<String>,
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

/// Native ownership of one configured child and every process it creates.
///
/// Unix callers use the child's dedicated process group. Windows callers use
/// a private Job Object configured with `KILL_ON_JOB_CLOSE`; every child is
/// created suspended, assigned to this Job Object, and resumed only after
/// membership verification. A process-lifetime owner Job Object is inherited
/// before creation, so a supervisor crash also kills a child in the brief
/// interval before private adoption. Other targets retain a zero-sized,
/// fail-closed marker until their native process ownership API is available.
#[allow(dead_code)]
pub(crate) struct ProcessTree {
    #[cfg(unix)]
    group: ProcessGroup,
    #[cfg(windows)]
    job: WindowsJob,
}

/// Parent-controlled startup gate.
///
/// Unix cannot safely block in `pre_exec`: Rust's `Command::spawn` waits for
/// the child to either `exec` or report a pre-exec error over its private
/// error pipe, so a blocking hook deadlocks the parent. The gate therefore
/// installs only nonblocking descriptor setup in `pre_exec`; the child
/// immediately execs FeanorFS's internal `service exec-gate` wrapper. That
/// trusted wrapper blocks after `spawn` has returned, and later execs the
/// configured worker in-place when the parent releases this gate. PID,
/// process-group, stdin, environment, and argv of the configured worker are
/// preserved by the final `exec`.
pub(crate) struct StartupGate {
    #[cfg(unix)]
    release: Option<UnixStream>,
    #[cfg(unix)]
    child_endpoint: Option<UnixStream>,
    released: bool,
}

impl StartupGate {
    const RELEASE_BYTE: u8 = 1;

    /// Installs Unix descriptor setup. Windows creates children suspended and
    /// releases them by resuming the verified primary thread.
    #[allow(dead_code)]
    pub(crate) fn prepare(command: &mut tokio::process::Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;

            let (release, child_endpoint) = UnixStream::pair()?;
            let child_read_fd = child_endpoint.as_raw_fd();
            let child_release_fd = release.as_raw_fd();
            // SAFETY: this hook performs only fcntl/close descriptor setup and
            // returns immediately; it never waits on the release pipe.
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
    #[allow(dead_code)]
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

#[cfg(unix)]
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
    if byte[0] != StartupGate::RELEASE_BYTE {
        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
    }
    drop(release);
    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(program).args(args).exec();
    Err(error)
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

#[allow(dead_code)]
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
            WindowsJob::adopt_child(child).map(|job| Self { job })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    /// Releases a Windows child after durable launch/status publication. Unix
    /// children have already entered the exec'd gate wrapper, so this is a
    /// no-op there.
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
            self.is_current() && terminate_windows_process(self.pid, &self.process_start_id, 1)
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
            self.is_current() && terminate_windows_process(self.pid, &self.process_start_id, 1)
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            false
        }
    }

    #[cfg(unix)]
    fn signal(&self, signal: libc::c_int) -> bool {
        let Ok(pid) = libc::pid_t::try_from(self.pid) else {
            return false;
        };
        // SAFETY: `is_current` was checked immediately before this direct
        // signal; unlike a raw PID caller, the process identity is rechecked
        // again for every escalation step.
        unsafe { libc::kill(pid, signal) == 0 }
    }
}

#[cfg(windows)]
struct WindowsJob {
    handle: OwnedHandle,
}

#[cfg(windows)]
impl std::fmt::Debug for WindowsJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("WindowsJob").finish_non_exhaustive()
    }
}

#[cfg(windows)]
impl WindowsJob {
    fn is_empty(&self) -> io::Result<bool> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let mut returned = 0_u32;
        let length = u32::try_from(std::mem::size_of_val(&accounting))
            .expect("Windows Job accounting structure fits u32");
        // SAFETY: `accounting` is a writable C-layout buffer of the exact
        // structure size and the Job handle remains owned for the query.
        if unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle() as HANDLE,
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION)
                    .cast::<core::ffi::c_void>(),
                length,
                &mut returned,
            )
        } == 0
        {
            return Err(io::Error::last_os_error())
                .map_err(|error| io::Error::other(format!("query Job Object processes: {error}")));
        }
        Ok(accounting.ActiveProcesses == 0)
    }

    fn adopt_child(child: &tokio::process::Child) -> io::Result<Self> {
        ensure_owner_job()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child exited before Job Object adoption"))?;
        let raw = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("child exited before Job Object adoption"))?;
        let process = raw as HANDLE;

        // Probe inherited membership before creating the private boundary. A
        // nested assignment is attempted below when the platform supports it;
        // a failure is explicit and fail-closed rather than guessed through
        // process-tree enumeration.
        let mut inherited = 0;
        // SAFETY: `process` is the live handle owned by Tokio's child. The
        // null job handle asks Windows whether it belongs to any job; the
        // output pointer is a writable BOOL-sized local.
        let probe_ok =
            unsafe { IsProcessInJob(process, std::ptr::null_mut(), &mut inherited) } != 0;
        if !probe_ok {
            return Err(io::Error::last_os_error()).map_err(|error| {
                io::Error::other(format!("probe inherited Job Object: {error}"))
            })?;
        }

        // SAFETY: null security attributes/name request an unnamed private
        // Job Object. A null handle is reported as an OS error.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error()).map_err(|error| {
                io::Error::other(format!("create private Job Object: {error}"))
            })?;
        }
        // SAFETY: `CreateJobObjectW` returned a new owned kernel handle. The
        // `OwnedHandle` closes it exactly once when this value is dropped.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        let job = Self { handle };
        if let Err(error) = job.configure_kill_on_close() {
            return Err(error);
        }

        // SAFETY: both handles are valid for the duration of this call. The
        // Job Object remains owned by `job` after assignment.
        if unsafe { AssignProcessToJobObject(job.handle.as_raw_handle() as HANDLE, process) } == 0 {
            let error = io::Error::last_os_error();
            let context = if inherited != 0 {
                "assign child to nested private Job Object"
            } else {
                "assign child to private Job Object"
            };
            return Err(io::Error::other(format!("{context}: {error}")));
        }

        let mut assigned = 0;
        // SAFETY: querying the exact child handle and exact private Job Object
        // verifies that this process is owned by the boundary we retain.
        if unsafe { IsProcessInJob(process, job.handle.as_raw_handle() as HANDLE, &mut assigned) }
            == 0
        {
            return Err(io::Error::last_os_error()).map_err(|error| {
                io::Error::other(format!("verify private Job Object membership: {error}"))
            })?;
        }
        if assigned == 0 {
            return Err(io::Error::other(
                "child was not admitted to the private Job Object",
            ));
        }
        #[cfg(all(test, windows))]
        if TEST_FORCE_ADOPTION_FAILURE_PID
            .compare_exchange(pid, 0, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_ok()
        {
            return Err(io::Error::other("injected Windows Job adoption failure"));
        }
        Ok(job)
    }

    fn release_child(&self, pid: u32) -> io::Result<()> {
        resume_suspended_primary_thread(pid).map_err(|error| {
            io::Error::other(format!("resume adopted child primary thread: {error}"))
        })
    }

    fn configure_kill_on_close(&self) -> io::Result<()> {
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        let length = u32::try_from(std::mem::size_of_val(&limits))
            .expect("Windows Job Object limit structure fits u32");
        // SAFETY: `limits` is a correctly initialized structure and the
        // supplied byte length matches its C representation.
        if unsafe {
            SetInformationJobObject(
                self.handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION)
                    .cast::<core::ffi::c_void>(),
                length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).map_err(|error| {
                io::Error::other(format!("configure Job Object kill-on-close: {error}"))
            })?;
        }
        Ok(())
    }

    fn terminate(&self) -> bool {
        // SAFETY: the handle remains owned by this RAII value and is valid for
        // this call. TerminateJobObject applies to the complete process tree.
        unsafe { TerminateJobObject(self.handle.as_raw_handle() as HANDLE, 1) != 0 }
    }
}

#[cfg(windows)]
static OWNER_JOB: OnceLock<OwnedHandle> = OnceLock::new();

#[cfg(windows)]
static OWNER_JOB_INIT: OnceLock<Mutex<()>> = OnceLock::new();

/// Establishes a process-lifetime Job Object before any worker is created.
///
/// Windows does not make `CreateProcess` plus `AssignProcessToJobObject` one
/// atomic operation. Assigning the current process first makes every newly
/// created suspended worker inherit this kill-on-close boundary. If the
/// supervisor or foreground runner dies between process creation and private
/// adoption, the kernel closes this retained handle and kills the suspended
/// worker before it can execute user code. If nested jobs are unavailable,
/// assignment fails and callers must not spawn.
#[cfg(windows)]
fn ensure_owner_job() -> io::Result<()> {
    if OWNER_JOB.get().is_some() {
        return Ok(());
    }
    let lock = OWNER_JOB_INIT.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if OWNER_JOB.get().is_some() {
        return Ok(());
    }

    let current = unsafe { GetCurrentProcess() };
    let mut inherited = 0;
    // SAFETY: `current` is the pseudo-handle for this process and `inherited`
    // is a writable BOOL-sized local.
    if unsafe { IsProcessInJob(current, std::ptr::null_mut(), &mut inherited) } == 0 {
        return Err(io::Error::last_os_error()).map_err(|error| {
            io::Error::other(format!("probe supervisor Job Object membership: {error}"))
        });
    }

    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error()).map_err(|error| {
            io::Error::other(format!("create supervisor owner Job Object: {error}"))
        });
    }
    // SAFETY: `CreateJobObjectW` returned a new owned handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw as _) };
    let owner = WindowsJob { handle };
    owner.configure_kill_on_close()?;
    // SAFETY: `current` and the owner Job handle remain valid for this call.
    // A failure here includes unsupported/non-nested inherited Job Objects;
    // dropping `owner` closes an empty boundary and no child is spawned.
    if unsafe { AssignProcessToJobObject(owner.handle.as_raw_handle() as HANDLE, current) } == 0 {
        let context = if inherited != 0 {
            "assign supervisor to nested owner Job Object"
        } else {
            "assign supervisor to owner Job Object"
        };
        return Err(io::Error::last_os_error())
            .map_err(|error| io::Error::other(format!("{context}: {error}")));
    }
    let mut assigned = 0;
    // SAFETY: verifies that this process is owned by the exact boundary whose
    // handle is retained below.
    if unsafe {
        IsProcessInJob(
            current,
            owner.handle.as_raw_handle() as HANDLE,
            &mut assigned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).map_err(|error| {
            io::Error::other(format!("verify supervisor owner Job Object: {error}"))
        });
    }
    if assigned == 0 {
        return Err(io::Error::other(
            "supervisor was not admitted to owner Job Object",
        ));
    }
    OWNER_JOB
        .set(owner.handle)
        .map_err(|_| io::Error::other("supervisor owner Job Object initialized concurrently"))
}

#[cfg(windows)]
const RESUME_ATTEMPTS: usize = 50;

/// Finds the only primary thread of a process created with
/// `CREATE_SUSPENDED`, opens it, and resumes it exactly once. A suspended
/// process cannot run user code, so the ToolHelp snapshot cannot observe a
/// thread tree that races with assignment. Multiple matching threads are
/// rejected rather than guessing which one is primary.
#[cfg(windows)]
fn resume_suspended_primary_thread(pid: u32) -> io::Result<()> {
    for _ in 0..RESUME_ATTEMPTS {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot as isize == -1 {
            return Err(io::Error::last_os_error()).map_err(|error| {
                io::Error::other(format!("snapshot suspended child threads: {error}"))
            });
        }
        // SAFETY: `CreateToolhelp32Snapshot` returned an owned snapshot
        // handle. `OwnedHandle` closes it exactly once.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as _) };
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut thread_id = None;
        let mut more =
            unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                if thread_id.replace(entry.th32ThreadID).is_some() {
                    return Err(io::Error::other(
                        "suspended child exposed multiple primary threads",
                    ));
                }
            }
            more = unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) } != 0;
        }
        let Some(thread_id) = thread_id else {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        };
        let raw_thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        if raw_thread.is_null() {
            return Err(io::Error::last_os_error()).map_err(|error| {
                io::Error::other(format!("open suspended child primary thread: {error}"))
            });
        }
        // SAFETY: `OpenThread` returned a new owned handle.
        let thread = unsafe { OwnedHandle::from_raw_handle(raw_thread as _) };
        // SAFETY: the exact thread handle is valid and the process was created
        // with one suspension count. A different count is ambiguous and fails
        // closed so the caller terminates the still-owned Job tree.
        let previous = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
        if previous == u32::MAX {
            return Err(io::Error::last_os_error()).map_err(|error| {
                io::Error::other(format!("resume suspended child primary thread: {error}"))
            });
        }
        if previous != 1 {
            return Err(io::Error::other(format!(
                "unexpected suspended child thread count {previous}"
            )));
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "suspended child primary thread was not discoverable",
    ))
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

    /// Probes whether the exact child-owned process group still exists.
    #[cfg(unix)]
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

    /// Non-Unix targets have no safe PID/group probe until their native
    /// process-ownership primitive is implemented.
    #[cfg(not(unix))]
    pub(crate) fn exists(&self) -> bool {
        let _ = self;
        false
    }

    /// Verifies that the recorded PID is still the leader of its own group.
    /// This check is required before residual cleanup can signal a group after
    /// the original supervisor has gone away.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub(crate) fn is_leader(&self) -> bool {
        if self.leader == 0 {
            return false;
        }
        let Ok(pid) = libc::pid_t::try_from(self.leader) else {
            return false;
        };
        // SAFETY: `getpgid` only queries the exact PID under consideration.
        self.leader_identity_current()
            // SAFETY: `getpgid` only queries the exact PID under consideration.
            && unsafe { libc::getpgid(pid) == pid }
    }

    #[cfg(not(unix))]
    #[allow(dead_code)]
    pub(crate) fn is_leader(&self) -> bool {
        let _ = self;
        false
    }

    /// Requests graceful termination of the exact process group.
    #[cfg(unix)]
    pub(crate) fn request_termination(&self) -> bool {
        self.signal(libc::SIGTERM)
    }

    #[cfg(not(unix))]
    pub(crate) fn request_termination(&self) -> bool {
        let _ = self;
        false
    }

    /// Forces termination of the exact process group.
    #[cfg(unix)]
    pub(crate) fn force_termination(&self) -> bool {
        self.signal(libc::SIGKILL)
    }

    #[cfg(not(unix))]
    pub(crate) fn force_termination(&self) -> bool {
        let _ = self;
        false
    }

    #[cfg(unix)]
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
        // start identity before residual cleanup.
        unsafe { libc::kill(-pid, signal) == 0 }
    }

    #[cfg(unix)]
    fn leader_identity_current(&self) -> bool {
        self.leader_process_start_id
            .as_deref()
            .is_some_and(|identity| process_start_matches(self.leader, identity))
    }
}

/// Prepares a configured child for fail-closed ownership.
///
/// Unix requests a fresh process group. Windows first establishes the
/// process-lifetime owner Job Object, then asks CreateProcess for a suspended
/// primary thread; [`ProcessTree::adopt_child`] assigns/verifies the private
/// Job Object and resumes that thread before any worker code can execute.
#[cfg(unix)]
pub(crate) fn configure_process_group(command: &mut tokio::process::Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    command.as_std_mut().process_group(0);
    Ok(())
}

#[cfg(windows)]
pub(crate) fn configure_process_group(command: &mut tokio::process::Command) -> io::Result<()> {
    ensure_owner_job()?;
    command.as_std_mut().creation_flags(CREATE_SUSPENDED);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn configure_process_group(_command: &mut tokio::process::Command) -> io::Result<()> {
    Ok(())
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
    if let Some(start_ticks) = linux_process_start_ticks(pid) {
        return format!("linux:{pid}:{start_ticks}");
    }

    #[cfg(target_os = "macos")]
    if let Some((seconds, microseconds)) = macos_process_start(pid) {
        return format!("macos:{pid}:{seconds}:{microseconds}");
    }

    #[cfg(target_os = "windows")]
    if let Some(creation_ticks) = windows_process_creation_ticks(pid) {
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
#[cfg(target_os = "linux")]
pub(crate) fn process_start_matches(pid: u32, process_start_id: &str) -> bool {
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
pub(crate) fn process_start_matches(pid: u32, process_start_id: &str) -> bool {
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

#[cfg(target_os = "windows")]
pub(crate) fn process_start_matches(pid: u32, process_start_id: &str) -> bool {
    let Some(digits) = process_start_id.strip_prefix("windows:") else {
        return false;
    };
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let Ok(recorded) = digits.parse::<u64>() else {
        return false;
    };
    recorded > 0
        && recorded.to_string() == digits
        && windows_process_creation_ticks(pid) == Some(recorded)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) fn process_start_matches(pid: u32, process_start_id: &str) -> bool {
    let _ = (pid, process_start_id);
    false
}

#[cfg(target_os = "linux")]
fn linux_process_start_ticks(pid: u32) -> Option<String> {
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

#[cfg(target_os = "macos")]
fn macos_process_start(pid: u32) -> Option<(u64, u64)> {
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

#[cfg(target_os = "windows")]
fn windows_process_creation_ticks(pid: u32) -> Option<u64> {
    // PROCESS_QUERY_LIMITED_INFORMATION is sufficient for both GetProcessTimes
    // and QueryFullProcessImageNameW and avoids requesting broad process access.
    // SAFETY: OpenProcess returns an owned kernel handle or null.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    // SAFETY: `OpenProcess` returned a new owned handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process as _) };
    windows_process_creation_ticks_from_handle(process.as_raw_handle() as HANDLE)
}

#[cfg(target_os = "windows")]
fn windows_process_creation_ticks_from_handle(process: HANDLE) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all FILETIME pointers refer to writable locals and the process
    // handle is valid for the duration of this call.
    let ok = unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    Some((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

/// Returns the kernel-reported Win32 executable image for an exact PID.
///
/// The result is intentionally kept private to lifecycle ownership checks;
/// callers must compare it through [`executable_matches`] so extended-prefix,
/// separator, and case normalization are identical on both sides.
#[cfg(target_os = "windows")]
pub(crate) fn executable_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: OpenProcess returns an owned kernel handle or null.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    // SAFETY: `OpenProcess` returned a new owned handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process as _) };
    let mut capacity = 32_u32;
    let mut buffer = vec![0_u16; capacity as usize];
    let length = loop {
        let mut length = capacity;
        // SAFETY: the buffer is writable for `capacity` UTF-16 code units and
        // the process handle is valid for this call.
        let ok = unsafe {
            QueryFullProcessImageNameW(
                process.as_raw_handle() as HANDLE,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &mut length,
            )
        };
        if ok != 0 {
            break Some(length);
        }
        let error = unsafe { GetLastError() };
        // ERROR_INSUFFICIENT_BUFFER is 122. Retry with a bounded doubling so
        // malformed or hostile processes cannot force an unbounded allocation.
        if error != 122 || capacity >= 32 * 1024 {
            break None;
        }
        capacity = capacity.saturating_mul(2);
        buffer.resize(capacity as usize, 0);
    };
    let length = length?;
    let value = String::from_utf16(&buffer[..length as usize]).ok()?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub(crate) fn executable_path(_pid: u32) -> Option<std::path::PathBuf> {
    None
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
        return normalize_windows_path(Some(path)).map(|value| format!("path:{value}"));
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
        let path = executable_path(pid)?;
        let metadata = std::fs::metadata(path).ok()?;
        Some(format!("unix-devino:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        return normalize_windows_path(executable_path(pid).as_deref())
            .map(|value| format!("path:{value}"));
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
#[allow(dead_code)]
pub(crate) fn executable_matches(pid: u32, expected: &std::path::Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        return normalize_windows_path(executable_path(pid).as_deref())
            == normalize_windows_path(Some(expected));
    }
    #[cfg(not(target_os = "windows"))]
    {
        executable_path(pid).and_then(|actual| std::fs::canonicalize(actual).ok())
            == std::fs::canonicalize(expected).ok()
    }
}

#[cfg(target_os = "windows")]
fn normalize_windows_path(path: Option<&Path>) -> Option<String> {
    let path = path?;
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut value = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        value = format!(r"\\{rest}");
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        value = rest.to_string();
    }
    while value.len() > 3 && value.ends_with('\\') {
        value.pop();
    }
    Some(value.to_lowercase())
}

#[cfg(windows)]
fn terminate_windows_process(pid: u32, expected_start_id: &str, exit_code: u32) -> bool {
    // Request both query and terminate rights so the creation token is
    // re-read through the same kernel handle immediately before signaling.
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE;
    // SAFETY: OpenProcess returns an owned handle or null; the handle is
    // immediately revalidated against the exact expected PID identity.
    let process = unsafe { OpenProcess(access, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: `OpenProcess` returned a new owned handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process as _) };
    if windows_process_creation_ticks_from_handle(process.as_raw_handle() as HANDLE)
        .map(|ticks| format!("windows:{ticks}"))
        .as_deref()
        != Some(expected_start_id)
    {
        return false;
    }
    // TerminateProcess is declared in the same Threading feature set.
    let ok = unsafe { TerminateProcess(process.as_raw_handle() as HANDLE, exit_code) };
    ok != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_malformed_ids_fail_closed() {
        let pid = std::process::id();
        assert!(!process_start_matches(pid, &format!("spawn:{pid}:session")));
        assert!(!process_start_matches(pid, ""));
        assert!(!process_start_matches(pid, "linux:not-a-pid:1"));
        assert!(!process_start_matches(pid, "macos:1:2:3:extra"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_identity_matches_exact_live_pid_and_rejects_mismatch() {
        let pid = std::process::id();
        let id = process_start_identifier(pid, "session");
        assert!(id.starts_with("linux:"));
        assert!(process_start_matches(pid, &id));
        assert!(!process_start_matches(pid.saturating_add(1), &id));
        assert!(!process_start_matches(pid, &id.replace("linux:", "spawn:")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_identity_matches_exact_live_pid_and_rejects_mismatch() {
        let pid = std::process::id();
        let id = process_start_identifier(pid, "session");
        assert!(id.starts_with("macos:"));
        assert!(process_start_matches(pid, &id));
        assert!(!process_start_matches(pid.saturating_add(1), &id));

        let mut fields = id.split(':').collect::<Vec<_>>();
        let useconds = fields
            .pop()
            .expect("macOS identity contains microseconds")
            .parse::<u64>()
            .expect("microseconds are numeric");
        let mismatched_useconds = (useconds.saturating_add(1)).to_string();
        fields.push(&mismatched_useconds);
        let mismatch = fields.join(":");
        assert!(!process_start_matches(pid, &mismatch));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn executable_identity_survives_in_place_unlink_of_mapped_image() {
        let temp = tempfile::tempdir().expect("identity tempdir");
        let source = std::path::Path::new("/bin/sleep");
        let source = source
            .is_file()
            .then_some(source)
            .or_else(|| {
                std::path::Path::new("/usr/bin/sleep")
                    .is_file()
                    .then_some(std::path::Path::new("/usr/bin/sleep"))
            })
            .expect("sleep executable");
        let copied = temp.path().join("worker");
        std::fs::copy(source, &copied).expect("copy worker image");
        let expected = executable_identity_for_path(&copied).expect("path identity");
        let mut child = std::process::Command::new(&copied)
            .arg("5")
            .spawn()
            .expect("spawn copied worker");
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !executable_identity_matches(pid, &expected) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(executable_identity_matches(pid, &expected));
        std::fs::remove_file(&copied).expect("unlink old worker path");
        assert!(
            executable_identity_matches(pid, &expected),
            "mapped old image must retain its stable device/inode identity"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_identity_is_exact_creation_ticks_and_rejects_legacy_tokens() {
        let pid = std::process::id();
        let id = process_start_identifier(pid, "session");
        assert!(id.starts_with("windows:"));
        assert!(process_start_matches(pid, &id));
        assert!(!process_start_matches(pid, &format!("spawn:{pid}:session")));
        assert!(!process_start_matches(pid, "windows:01"));
        assert!(!process_start_matches(pid, "windows:+1"));
        assert!(!process_start_matches(pid, "windows:0"));

        let ticks = id
            .strip_prefix("windows:")
            .expect("Windows identity contains creation ticks")
            .parse::<u64>()
            .expect("Windows creation ticks are numeric");
        let mismatched_ticks = if ticks == u64::MAX {
            ticks - 1
        } else {
            ticks + 1
        };
        assert!(!process_start_matches(
            pid,
            &format!("windows:{mismatched_ticks}")
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_executable_normalization_is_case_and_prefix_insensitive() {
        assert_eq!(
            normalize_windows_path(Some(Path::new(r"\\?\C:\FeanorFS\bin.exe"))),
            Some(r"c:\feanorfs\bin.exe".to_string())
        );
        assert_eq!(
            normalize_windows_path(Some(Path::new(r"C:/FeanorFS/bin.exe"))),
            Some(r"c:\feanorfs\bin.exe".to_string())
        );
        assert_eq!(
            normalize_windows_path(Some(Path::new(r"\\?\UNC\Server\Share\bin.exe"))),
            Some(r"\\server\share\bin.exe".to_string())
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn windows_suspended_launch_helper() {
        let marker =
            std::env::var_os("FEANORFS_SUSPENDED_MARKER").expect("suspended launch marker path");
        std::fs::write(marker, b"started").expect("write suspended launch marker");
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_child_stays_suspended_until_job_assignment() {
        let temp = tempfile::tempdir().expect("suspended launch tempdir");
        let marker = temp.path().join("started");
        let mut command = tokio::process::Command::new(
            std::env::current_exe().expect("suspended launch test executable"),
        );
        command
            .args([
                "--ignored",
                "--exact",
                "cli::process_tree::tests::windows_suspended_launch_helper",
                "--nocapture",
            ])
            .env("FEANORFS_SUSPENDED_MARKER", &marker)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if configure_process_group(&mut command).is_err() {
            // A test runner already inside a non-nestable Job Object is an
            // explicit fail-closed platform condition; no unsuspended child
            // is allowed, so this test has nothing safe to execute.
            return;
        }
        let child = command.spawn().expect("spawn suspended launch helper");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!marker.exists(), "child ran before private Job assignment");
        let tree = WindowsJob::adopt_child(&child).expect("adopt suspended child");
        tree.release_child(child.id().expect("suspended child pid"))
            .expect("release adopted child");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child resumed after Job assignment");
        assert!(tree.terminate());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if tree.is_empty().expect("query Job Object process count") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Job Object became empty after force termination");
        drop(tree);
        let mut child = child;
        let _ = child.wait().await;
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_adoption_failure_leaves_suspended_child_unrun() {
        let temp = tempfile::tempdir().expect("adoption failure tempdir");
        let marker = temp.path().join("started");
        let mut command = tokio::process::Command::new(
            std::env::current_exe().expect("adoption failure test executable"),
        );
        command
            .args([
                "--ignored",
                "--exact",
                "cli::process_tree::tests::windows_suspended_launch_helper",
                "--nocapture",
            ])
            .env("FEANORFS_SUSPENDED_MARKER", &marker)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if configure_process_group(&mut command).is_err() {
            return;
        }
        let mut child = command.spawn().expect("spawn suspended adoption helper");
        let pid = child.id().expect("suspended adoption helper pid");
        TEST_FORCE_ADOPTION_FAILURE_PID.store(pid, AtomicOrdering::Release);
        let result = WindowsJob::adopt_child(&child);
        assert!(result.is_err());
        tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .expect("failed adoption child was reaped")
            .expect("failed adoption child wait");
        assert!(!marker.exists(), "failed adoption resumed user code");
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn windows_owner_crash_helper() {
        let pid_path = std::env::var_os("FEANORFS_OWNER_CRASH_PID").expect("owner crash pid path");
        let marker =
            std::env::var_os("FEANORFS_OWNER_CRASH_MARKER").expect("owner crash marker path");
        ensure_owner_job().expect("establish owner Job before crash test");
        let executable = std::env::current_exe().expect("owner crash executable");
        let mut child = std::process::Command::new(executable);
        child
            .args([
                "--ignored",
                "--exact",
                "cli::process_tree::tests::windows_suspended_launch_helper",
                "--nocapture",
            ])
            .env("FEANORFS_SUSPENDED_MARKER", marker)
            .creation_flags(CREATE_SUSPENDED);
        let child = child.spawn().expect("spawn owner-crash suspended child");
        std::fs::write(pid_path, child.id().to_string()).expect("record owner-crash child pid");
        // Drop all Rust state through process teardown. The owner Job handle
        // closes and must kill this still-suspended child before it can run.
        std::process::exit(0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn owner_job_closes_suspended_child_on_process_crash() {
        let temp = tempfile::tempdir().expect("owner crash tempdir");
        let pid_path = temp.path().join("child.pid");
        let marker = temp.path().join("started");
        let executable = std::env::current_exe().expect("owner crash test executable");
        let mut helper = std::process::Command::new(executable);
        helper
            .args([
                "--ignored",
                "--exact",
                "cli::process_tree::tests::windows_owner_crash_helper",
                "--nocapture",
            ])
            .env("FEANORFS_OWNER_CRASH_PID", &pid_path)
            .env("FEANORFS_OWNER_CRASH_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let status = helper
            .spawn()
            .expect("spawn owner crash helper")
            .wait()
            .unwrap();
        assert!(status.success());
        let pid = std::fs::read_to_string(&pid_path)
            .expect("owner crash child pid")
            .trim()
            .parse::<u32>()
            .expect("owner crash child pid format");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if windows_process_creation_ticks(pid).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(windows_process_creation_ticks(pid).is_none());
        assert!(!marker.exists(), "owner crash child executed user code");
    }

    #[cfg(unix)]
    #[test]
    fn process_group_for_live_process_is_exact_leader() {
        let group = ProcessGroup::for_child(std::process::id());
        // The test harness is not expected to own its own group, so this only
        // checks that probing is bounded and does not claim an arbitrary group.
        let _ = group.exists();
    }

    #[cfg(unix)]
    #[test]
    fn process_group_identity_mismatch_fails_closed_before_signal() {
        let mut group = ProcessGroup::for_child(std::process::id());
        group.leader_process_start_id = Some(format!("spawn:{}:reused", std::process::id()));
        assert!(!group.is_leader());
        assert!(!group.request_termination());
        assert!(!group.force_termination());
    }
}
