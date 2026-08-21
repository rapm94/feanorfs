//! Windows process ownership: suspended creation, private Job Object
//! adoption and termination, handles, and exact start identity.
//!
//! Every unsafe block documents its invariant in place:
//! kernel handles are converted to `OwnedHandle` exactly once and closed on
//! drop, the process-lifetime owner Job Object is established before any
//! worker is created, children stay suspended until private membership is
//! verified, and every signal revalidates the exact creation token first.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use windows_sys::Win32::Foundation::{GetLastError, FILETIME, HANDLE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenThread, QueryFullProcessImageNameW,
    ResumeThread, TerminateProcess, CREATE_SUSPENDED, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
};

use std::os::windows::process::CommandExt as _;

/// Test seam: a PID whose private Job adoption must fail (fail-closed proof).
#[cfg(test)]
pub(super) static TEST_FORCE_ADOPTION_FAILURE_PID: AtomicU32 = AtomicU32::new(0);

/// A private Job Object owning one configured child and every descendant it
/// creates. Configured with `KILL_ON_JOB_CLOSE`: dropping the last handle
/// terminates the complete tree, so ownership is kernel-enforced rather than
/// PID-scanned.
pub(super) struct WindowsJob {
    handle: OwnedHandle,
}

impl std::fmt::Debug for WindowsJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("WindowsJob").finish_non_exhaustive()
    }
}

impl WindowsJob {
    pub(super) fn is_empty(&self) -> io::Result<bool> {
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

    pub(super) fn adopt_child(child: &tokio::process::Child) -> io::Result<Self> {
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
        #[cfg(test)]
        if TEST_FORCE_ADOPTION_FAILURE_PID
            .compare_exchange(pid, 0, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_ok()
        {
            return Err(io::Error::other("injected Windows Job adoption failure"));
        }
        Ok(job)
    }

    pub(super) fn release_child(&self, pid: u32) -> io::Result<()> {
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

    pub(super) fn terminate(&self) -> bool {
        // SAFETY: the handle remains owned by this RAII value and is valid for
        // this call. TerminateJobObject applies to the complete process tree.
        unsafe { TerminateJobObject(self.handle.as_raw_handle() as HANDLE, 1) != 0 }
    }
}

static OWNER_JOB: OnceLock<OwnedHandle> = OnceLock::new();

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
pub(super) fn ensure_owner_job() -> io::Result<()> {
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

const RESUME_ATTEMPTS: usize = 50;

/// Finds the only primary thread of a process created with
/// `CREATE_SUSPENDED`, opens it, and resumes it exactly once. A suspended
/// process cannot run user code, so the ToolHelp snapshot cannot observe a
/// thread tree that races with assignment. Multiple matching threads are
/// rejected rather than guessing which one is primary.
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

/// Prepares a Windows child for fail-closed ownership: establishes the
/// process-lifetime owner Job Object and asks CreateProcess for a suspended
/// primary thread. `ProcessTree::adopt_child` assigns/verifies the private
/// Job Object and resumes that thread before any worker code can execute.
pub(super) fn configure_process_group(command: &mut tokio::process::Command) -> io::Result<()> {
    ensure_owner_job()?;
    command.as_std_mut().creation_flags(CREATE_SUSPENDED);
    Ok(())
}

pub(super) fn windows_process_creation_ticks(pid: u32) -> Option<u64> {
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

pub(super) fn windows_process_start_matches(pid: u32, process_start_id: &str) -> bool {
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

/// Returns the kernel-reported Win32 executable image for an exact PID.
///
/// The result is intentionally kept private to lifecycle ownership checks;
/// callers must compare it through [`super::executable_matches`] so
/// extended-prefix, separator, and case normalization are identical on both
/// sides.
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

pub(super) fn normalize_windows_path(path: Option<&Path>) -> Option<String> {
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

/// Kernel executable identity of a live PID, normalized the same way as the
/// persisted path identity so in-place upgrades and case differences compare
/// equal.
pub(super) fn executable_identity_for_pid(pid: u32) -> Option<String> {
    normalize_windows_path(executable_path(pid).as_deref()).map(|value| format!("path:{value}"))
}

/// Terminates a process only while its kernel creation token still matches
/// the exact expected identity.
///
/// # Invariants
/// - The handle is opened with query+terminate rights and the creation token
///   is re-read through the same kernel handle immediately before signaling,
///   so a recycled PID is never targeted.
/// - The returned `OwnedHandle` closes exactly once on drop.
pub(super) fn terminate_windows_process(pid: u32, expected_start_id: &str, exit_code: u32) -> bool {
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
