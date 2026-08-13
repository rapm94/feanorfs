pub mod agent;
pub mod agent_runner;
pub mod conflicts;
pub mod events;
pub mod history;
pub mod hub_service;
pub mod hydrate;
pub mod integrator;
pub mod mcp;
pub mod pair;
mod process_tree;
pub mod recovery;
pub mod runner;
pub mod serve;
pub mod service;
pub mod start;
pub mod supervisor;
pub mod sync;
pub mod tray;
pub mod update;
pub mod util;
pub mod workspace;

pub use util::{setup_logging, LoggingMode};

#[cfg(test)]
/// Owns the shared runner fixture slot for the lifetime of a test workspace.
///
/// Runner fixtures also exercise process-wide lifecycle and supervisor test
/// state, so temp directories alone do not isolate their teardown. Keep their
/// lifecycle operations serialized while preserving production lock behavior.
pub(crate) struct RunnerTestWorkspace {
    // `Option` lets `Drop` remove the directory explicitly before releasing
    // the process-wide fixture guard.
    directory: Option<tempfile::TempDir>,
    _serial_guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl RunnerTestWorkspace {
    pub(crate) fn new() -> Self {
        static RUNNER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let serial_guard = RUNNER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            directory: Some(tempfile::tempdir().expect("create isolated runner test workspace")),
            _serial_guard: serial_guard,
        }
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        self.directory
            .as_ref()
            .expect("runner test workspace is available until drop")
            .path()
    }
}

#[cfg(test)]
impl Drop for RunnerTestWorkspace {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            // Match TempDir's best-effort cleanup while retaining the fixture
            // guard until the directory teardown has completed.
            let _ = directory.close();
        }
    }
}

pub use agent::AgentAction;
pub use conflicts::ConflictsAction;
pub use hydrate::HydrateAction;
pub use sync::SyncAction;
pub use workspace::WorkspaceAction;
