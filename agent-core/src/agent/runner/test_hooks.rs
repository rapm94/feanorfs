//! Test-only pause and contention probes; never compiled into production builds.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::ownership::{RunnerIdentity, CONFIGURE_LOCK};

pub(super) const TEST_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
type TestHookId = u64;

#[cfg(test)]
static NEXT_TEST_HOOK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
pub(super) fn next_test_hook_id() -> TestHookId {
    let id = NEXT_TEST_HOOK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_ne!(id, 0, "runner test hook id space exhausted");
    id
}

#[cfg(test)]
struct LifecycleContentionHook {
    pub(super) id: TestHookId,
    path: PathBuf,
    pub(super) entered: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
static LIFECYCLE_CONTENTION_HOOKS: std::sync::Mutex<Vec<LifecycleContentionHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(super) struct LifecycleContentionProbe {
    pub(super) id: TestHookId,
    pub(super) entered: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl LifecycleContentionProbe {
    pub(super) fn wait(&self, diagnostic: &str) {
        self.entered
            .recv_timeout(TEST_HOOK_TIMEOUT)
            .unwrap_or_else(|error| panic!("{diagnostic} within {TEST_HOOK_TIMEOUT:?}: {error:?}"));
    }
}

#[cfg(test)]
impl Drop for LifecycleContentionProbe {
    fn drop(&mut self) {
        if let Ok(mut hooks) = LIFECYCLE_CONTENTION_HOOKS.lock() {
            hooks.retain(|hook| hook.id != self.id);
        }
    }
}

#[cfg(test)]
pub(super) fn install_lifecycle_contention_hook(base: &Path) -> Result<LifecycleContentionProbe> {
    let path = crate::workspace_layout::workspace_state_path(base)?
        .join("agents")
        .join(CONFIGURE_LOCK);
    let id = next_test_hook_id();
    let (sender, receiver) = std::sync::mpsc::channel();
    LIFECYCLE_CONTENTION_HOOKS
        .lock()
        .map_err(|_| anyhow::anyhow!("runner lifecycle contention hook was poisoned"))?
        .push(LifecycleContentionHook {
            id,
            path: path.clone(),
            entered: sender,
        });
    Ok(LifecycleContentionProbe {
        id,
        entered: receiver,
    })
}

#[cfg(test)]
pub(super) fn notify_lifecycle_lock_contention(path: &Path) {
    // One contention event is observable by every same-path probe. Drain all
    // matches in installation order so parallel observers cannot steal it.
    let matching = {
        let mut hooks = LIFECYCLE_CONTENTION_HOOKS
            .lock()
            .unwrap_or_else(|_| panic!("runner lifecycle contention hooks were poisoned"));
        let mut matching = Vec::new();
        let mut index = 0;
        while index < hooks.len() {
            if hooks[index].path == path {
                matching.push(hooks.remove(index));
            } else {
                index += 1;
            }
        }
        matching
    };
    for hook in matching {
        hook.entered.send(()).unwrap_or_else(|_| {
            panic!(
                "runner lifecycle contention probe {} dropped before notification",
                hook.id
            )
        });
    }
}

#[cfg(test)]
pub(super) struct TestPauseHook {
    pub(super) id: TestHookId,
    pub(super) canonical_base: PathBuf,
    pub(super) agent: String,
    pub(super) entered: std::sync::mpsc::Sender<()>,
    pub(super) release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
pub(super) static OPERATION_GUARD_PAUSE_HOOKS: std::sync::Mutex<Vec<TestPauseHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
static INBOX_ADMISSION_PAUSE_HOOKS: std::sync::Mutex<Vec<TestPauseHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
static STATUS_DISCOVERY_PAUSE_HOOKS: std::sync::Mutex<Vec<TestPauseHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
static STATUS_SNAPSHOT_PAUSE_HOOKS: std::sync::Mutex<Vec<TestPauseHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(super) struct TestPause {
    pub(super) id: TestHookId,
    registry: &'static std::sync::Mutex<Vec<TestPauseHook>>,
    label: &'static str,
    pub(super) entered: std::sync::mpsc::Receiver<()>,
    pub(super) release: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(test)]
impl TestPause {
    pub(super) fn wait(&self, diagnostic: &str) {
        self.wait_with_timeout(diagnostic, TEST_HOOK_TIMEOUT);
    }

    pub(super) fn wait_with_timeout(&self, diagnostic: &str, timeout: std::time::Duration) {
        self.entered
            .recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("{diagnostic} within {timeout:?}: {error:?}"));
    }

    pub(super) fn release(&mut self) -> Result<()> {
        self.release
            .take()
            .with_context(|| format!("runner {} pause was already released", self.label))?
            .send(())
            .map_err(|_| anyhow::anyhow!("runner {} pause receiver was dropped", self.label))
    }
}

#[cfg(test)]
impl Drop for TestPause {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Ok(mut hooks) = self.registry.lock() {
            hooks.retain(|hook| hook.id != self.id);
        }
    }
}

#[cfg(test)]
pub(super) fn install_test_pause(
    registry: &'static std::sync::Mutex<Vec<TestPauseHook>>,
    label: &'static str,
    base: &Path,
    agent: &str,
) -> Result<TestPause> {
    let id = next_test_hook_id();
    let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let canonical_base = fs::canonicalize(base)?;
    registry
        .lock()
        .map_err(|_| anyhow::anyhow!("runner {label} pause hooks were poisoned"))?
        .push(TestPauseHook {
            id,
            canonical_base: canonical_base.clone(),
            agent: agent.to_string(),
            entered: entered_sender,
            release: release_receiver,
        });
    Ok(TestPause {
        id,
        registry,
        label,
        entered: entered_receiver,
        release: Some(release_sender),
    })
}

#[cfg(test)]
pub(super) fn take_test_pause(
    registry: &'static std::sync::Mutex<Vec<TestPauseHook>>,
    label: &str,
    canonical_base: &Path,
    agent: &str,
) -> Option<TestPauseHook> {
    let mut hooks = registry
        .lock()
        .unwrap_or_else(|_| panic!("runner {label} pause hooks were poisoned"));
    hooks
        .iter()
        .position(|hook| hook.canonical_base == canonical_base && hook.agent == agent)
        .map(|index| hooks.remove(index))
}

#[cfg(test)]
pub(super) fn wait_for_test_pause_release(
    hook: TestPauseHook,
    label: &str,
    timeout: std::time::Duration,
) {
    hook.entered.send(()).unwrap_or_else(|_| {
        panic!(
            "runner {label} pause observer {} dropped before worker entry",
            hook.id
        )
    });
    hook.release.recv_timeout(timeout).unwrap_or_else(|error| {
        panic!(
            "runner {label} paused worker {} was not released within {timeout:?}: {error:?}",
            hook.id
        )
    });
}

#[cfg(test)]
pub(super) fn install_operation_guard_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(&OPERATION_GUARD_PAUSE_HOOKS, "operation guard", base, agent)
}

#[cfg(test)]
pub(super) fn pause_operation_guard_if_requested(base: &Path, agent: &str) {
    let canonical_base = fs::canonicalize(base)
        .unwrap_or_else(|error| panic!("canonicalize runner operation pause key: {error}"));
    if let Some(hook) = take_test_pause(
        &OPERATION_GUARD_PAUSE_HOOKS,
        "operation guard",
        &canonical_base,
        agent,
    ) {
        wait_for_test_pause_release(hook, "operation guard", TEST_HOOK_TIMEOUT);
    }
}

#[cfg(test)]
pub(super) fn install_inbox_admission_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(&INBOX_ADMISSION_PAUSE_HOOKS, "inbox admission", base, agent)
}

#[cfg(test)]
pub(super) fn pause_inbox_admission_if_requested(identity: &RunnerIdentity) {
    if let Some(hook) = take_test_pause(
        &INBOX_ADMISSION_PAUSE_HOOKS,
        "inbox admission",
        &identity.canonical_workspace,
        &identity.agent,
    ) {
        wait_for_test_pause_release(hook, "inbox admission", TEST_HOOK_TIMEOUT);
    }
}

#[cfg(test)]
pub(super) fn install_status_discovery_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(
        &STATUS_DISCOVERY_PAUSE_HOOKS,
        "status discovery",
        base,
        agent,
    )
}

#[cfg(test)]
pub(super) fn pause_status_discovery_if_requested(base: &Path, agent: &str) {
    let canonical_base = fs::canonicalize(base)
        .unwrap_or_else(|error| panic!("canonicalize runner status discovery key: {error}"));
    if let Some(hook) = take_test_pause(
        &STATUS_DISCOVERY_PAUSE_HOOKS,
        "status discovery",
        &canonical_base,
        agent,
    ) {
        wait_for_test_pause_release(hook, "status discovery", TEST_HOOK_TIMEOUT);
    }
}

#[cfg(test)]
pub(super) fn install_status_snapshot_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(&STATUS_SNAPSHOT_PAUSE_HOOKS, "status snapshot", base, agent)
}

#[cfg(test)]
pub(super) fn pause_status_snapshot_if_requested(base: &Path, agent: &str) {
    let canonical_base = fs::canonicalize(base)
        .unwrap_or_else(|error| panic!("canonicalize runner status snapshot key: {error}"));
    if let Some(hook) = take_test_pause(
        &STATUS_SNAPSHOT_PAUSE_HOOKS,
        "status snapshot",
        &canonical_base,
        agent,
    ) {
        wait_for_test_pause_release(hook, "status snapshot", TEST_HOOK_TIMEOUT);
    }
}
