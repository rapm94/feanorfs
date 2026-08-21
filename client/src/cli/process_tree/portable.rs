//! Conservative fail-closed fallback for targets without native process
//! ownership primitives.
//!
//! These implementations never claim that a PID-only operation is safe: they
//! report "not owned / not terminated / identity mismatch" rather than
//! guessing by age or PID. Windows also uses these group fallbacks (a Job
//! Object, not a process group, is the Windows ownership boundary).

use std::io;

use super::ProcessGroup;

impl ProcessGroup {
    /// Non-Unix targets have no safe PID/group probe until their native
    /// process-ownership primitive is implemented.
    pub(crate) fn exists(&self) -> bool {
        let _ = self;
        false
    }

    /// Requests graceful termination of the exact process group. Unsupported
    /// targets fail closed rather than signaling a guessed PID.
    pub(crate) fn request_termination(&self) -> bool {
        let _ = self;
        false
    }

    /// Forces termination of the exact process group. Unsupported targets
    /// fail closed rather than signaling a guessed PID.
    pub(crate) fn force_termination(&self) -> bool {
        let _ = self;
        false
    }
}

/// No process-group preparation exists for unsupported targets; spawning
/// proceeds without any tree boundary and the ownership checks above stay
/// fail-closed.
pub(super) fn configure_process_group(_command: &mut tokio::process::Command) -> io::Result<()> {
    Ok(())
}

/// Unsupported targets never accept a persisted start identity.
pub(super) fn process_start_matches(pid: u32, process_start_id: &str) -> bool {
    let _ = (pid, process_start_id);
    false
}
