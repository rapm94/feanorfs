//! Bounded opaque head-change waiting for the authenticated head route.
//!
//! This is a transport wakeup about an opaque CAS value, not an agent-aware
//! server feature. Waiters are keyed only by opaque workspace id and hold no
//! plaintext names, paths, or content. Capacity is bounded globally and per
//! workspace, wait durations are capped below the client read-idle timeout,
//! and waiters are notified only after a head swap is durably accepted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};

/// Hard cap for one requested wait. Must stay below the 60-second client
/// HTTP read-idle bound so a healthy wait always finishes before the
/// transport itself would abort the request.
pub(crate) const MAX_HEAD_WAIT_MS: u64 = 30_000;

/// Global concurrent waiter bound; exhaustion fails the request cleanly.
const MAX_GLOBAL_WAITERS: usize = 256;

/// Per-workspace concurrent waiter bound.
pub(super) const MAX_WORKSPACE_WAITERS: usize = 16;

// Never recycle an identity while an old notified handle can still be dropped.
// Exhaustion fails admission rather than wrapping into a live registration.
static NEXT_WAITER_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct WorkspaceWaiters {
    senders: HashMap<u64, oneshot::Sender<()>>,
    #[cfg(test)]
    waiting: std::collections::HashSet<u64>,
}

/// In-memory notification registry for opaque head waiters.
pub(crate) struct HeadWaiters {
    inner: Mutex<HashMap<String, WorkspaceWaiters>>,
    capacity: Arc<Semaphore>,
}

impl HeadWaiters {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity: Arc::new(Semaphore::new(MAX_GLOBAL_WAITERS)),
        }
    }
}

impl HeadWaiters {
    /// Registers one waiter for `workspace_id`, returning a receiver that
    /// resolves after the next durable head swap and a permit that releases
    /// the global slot when dropped (including client disconnect).
    ///
    /// Returns `None` when the global or per-workspace waiter bound is
    /// exhausted, so the handler can fail the request cleanly.
    pub(super) fn register(self: &Arc<Self>, workspace_id: &str) -> Option<RegisteredWaiter> {
        let permit = match Arc::clone(&self.capacity).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return None,
        };
        let waiter_id = NEXT_WAITER_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut inner = match self.inner.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let entry = inner.entry(workspace_id.to_string()).or_default();
            if entry.senders.len() >= MAX_WORKSPACE_WAITERS {
                return None;
            }
            entry.senders.insert(waiter_id, sender);
        }
        Some(RegisteredWaiter {
            receiver,
            _permit: permit,
            registry: Arc::clone(self),
            workspace_id: workspace_id.to_string(),
            waiter_id,
        })
    }

    /// Releases the per-workspace registration for one waiter.
    fn unregister(&self, workspace_id: &str, waiter_id: u64) {
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut remove = false;
        if let Some(entry) = inner.get_mut(workspace_id) {
            entry.senders.remove(&waiter_id);
            #[cfg(test)]
            entry.waiting.remove(&waiter_id);
            remove = entry.senders.is_empty();
        }
        if remove {
            inner.remove(workspace_id);
        }
    }

    /// Test synchronization observes actual receiver polling, after the route's
    /// post-registration head recheck, rather than merely a queued request.
    #[cfg(test)]
    pub(super) fn waiting_count(&self, workspace_id: &str) -> usize {
        let inner = self.inner.lock().unwrap();
        inner
            .get(workspace_id)
            .map_or(0, |entry| entry.waiting.len())
    }

    /// Wakes every waiter for `workspace_id` after a durable head swap.
    /// Waiters re-read the head themselves; a rejected CAS never calls this.
    pub(super) fn notify(&self, workspace_id: &str) {
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = inner.remove(workspace_id) {
            for sender in entry.senders.into_values() {
                let _ = sender.send(());
            }
        }
    }
}

/// One exact registered waiter. Dropping it after timeout, disconnect, or
/// cancellation removes its sender and releases both capacity bounds.
pub(super) struct RegisteredWaiter {
    receiver: oneshot::Receiver<()>,
    _permit: OwnedSemaphorePermit,
    registry: Arc<HeadWaiters>,
    workspace_id: String,
    waiter_id: u64,
}

impl RegisteredWaiter {
    pub(super) fn receiver(&mut self) -> &mut oneshot::Receiver<()> {
        #[cfg(test)]
        if let Some(entry) = self
            .registry
            .inner
            .lock()
            .unwrap()
            .get_mut(&self.workspace_id)
        {
            if entry.senders.contains_key(&self.waiter_id) {
                entry.waiting.insert(self.waiter_id);
            }
        }
        &mut self.receiver
    }
}

impl Drop for RegisteredWaiter {
    fn drop(&mut self) {
        self.registry.unregister(&self.workspace_id, self.waiter_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn global_capacity_is_bounded() {
        let registry = Arc::new(HeadWaiters::new());
        let mut handles = Vec::new();
        for index in 0..MAX_GLOBAL_WAITERS {
            handles.push(
                registry
                    .register(&format!("ws-{index}"))
                    .expect("waiter registered"),
            );
        }
        assert!(
            registry.register("ws-overflow").is_none(),
            "global bound enforced"
        );
        drop(handles);
        assert!(registry.register("ws-after").is_some(), "capacity released");
    }

    #[tokio::test]
    async fn per_workspace_capacity_is_bounded() {
        let registry = Arc::new(HeadWaiters::new());
        let mut handles = Vec::new();
        for _ in 0..MAX_WORKSPACE_WAITERS {
            handles.push(registry.register("ws-a").expect("waiter registered"));
        }
        assert!(
            registry.register("ws-a").is_none(),
            "per-workspace bound enforced"
        );
        // A different workspace keeps independent capacity.
        assert!(registry.register("ws-b").is_some());
        drop(handles);
    }

    #[tokio::test]
    async fn notify_wakes_only_the_target_workspace() {
        let registry = Arc::new(HeadWaiters::new());
        let mut a = registry.register("ws-a").unwrap();
        let mut b = registry.register("ws-b").unwrap();
        registry.notify("ws-a");
        assert!(a.receiver().try_recv().is_ok(), "workspace a waiter woken");
        assert!(
            b.receiver().try_recv().is_err(),
            "workspace b waiter untouched"
        );
    }

    #[test]
    fn dropping_notified_waiter_preserves_new_registration() {
        let registry = Arc::new(HeadWaiters::new());
        let old = registry.register("ws-a").unwrap();
        registry.notify("ws-a");
        let mut new = registry.register("ws-a").unwrap();
        drop(old);
        assert_eq!(
            new.receiver().try_recv(),
            Err(oneshot::error::TryRecvError::Empty),
            "old drop must not close the new sender"
        );
        registry.notify("ws-a");
        assert_eq!(new.receiver().try_recv(), Ok(()));
    }

    #[test]
    fn timeout_or_disconnect_removes_exact_sender() {
        let registry = Arc::new(HeadWaiters::new());
        for _ in 0..MAX_WORKSPACE_WAITERS * 4 {
            drop(registry.register("quiet-workspace").unwrap());
        }
        let inner = registry.inner.lock().unwrap();
        assert!(
            !inner.contains_key("quiet-workspace"),
            "closed waiters must not accumulate in quiet workspaces"
        );
    }
}
