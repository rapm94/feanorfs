//! The single persistent Tokio-child reaper shared by the agent runner and
//! the supervisor.
//!
//! # Behavior matrix
//!
//! The two historical reapers (`ChildReaper` in `agent_runner.rs` and
//! `SupervisorChildReaper` in `supervisor.rs`) were consolidated here without
//! altering any timeout/retry/graceful/forced sequence. The matrix below
//! records the sequences this module and its callers preserve; the tests in
//! `tests.rs` plus the runner/supervisor lifecycle tests are the
//! characterization harness.
//!
//! | Row | Behavior | Preserved sequence |
//! |-----|----------|--------------------|
//! | 1 | Already-exited child | `enqueue` a child whose kernel exit is already observable; the worker's next `try_wait` returns `Ok(Some)` and the ticket completes. |
//! | 2 | Normal async wait | Worker polls `try_wait`; while `Ok(None)`, the entry is requeued to the back of the queue and the worker sleeps `REAP_RETRY_POLL` (100 ms) — the runner's `DETACHED_REAP_POLL` and the supervisor's 100 ms retry sleep. |
//! | 3 | Graceful then forced termination | Caller policy, not reaper policy: runner `ManagedChild::terminate` uses `TERMINATE_GRACE` (2 s) then force; supervisor `terminate_child` uses `STOP_GRACE` (5 s, 50 ms poll) then `CHILD_REAP_GRACE` (1 s) force reap. Sequences unchanged. |
//! | 4 | Post-kill wait timeout | Runner `force_reap` waits `POST_KILL_REAP_GRACE` (1 s) on the killed child and hands it to the reaper on `TimedOut`/`WaitError`; supervisor hands off after `CHILD_REAP_GRACE` times out or fails. Sequences unchanged. |
//! | 5 | Coordinator startup failure | `ensure_ready` returns an error; the caller retains ownership. Runner: the spawn closure is never invoked. Supervisor: `enqueue_or_wait` falls back to an in-task synchronous `wait` (retrying every 100 ms on wait error) and never drops the live child. |
//! | 6 | First/repeated `try_wait` error | A transient error requeues the entry (`error_requeues`) and sleeps `REAP_RETRY_POLL` before the next attempt; repeated errors keep requeueing until the kernel wait succeeds. |
//! | 7 | Worker panic/restart | A panic inside `reap_one` unwinds the entry guard (requeueing the child), increments `panic_recoveries`, warns, sleeps `REAP_RETRY_POLL`, and the worker continues — the thread never exits permanently. |
//! | 8 | Queue transfer during shutdown | Every caller handoff is a queue push guarded by a ticket; the worker drains the queue even while the caller is shutting down; the supervisor's shutdown loop awaits tickets for up to `STOP_GRACE + CHILD_REAP_GRACE` (6 s, 20 ms poll). |
//! | 9 | Multiple queued children / fairness | Entries are round-robin: a not-yet-exited child is requeued to the back so later entries are not starved (merged from the runner's round-robin; the supervisor's head-of-line single-entry blocking was unified to the same 100 ms cadence per entry). |
//! | 10 | Ticket incomplete/complete visibility | `ReapTicket::is_complete` is false until the kernel wait succeeded (`try_wait` `Ok(Some)` in the worker, or `Child::wait` `Ok` in the synchronous fallback). |
//! | 11 | Process with surviving descendant | Caller policy: Unix process-group / Windows Job-Object termination happens before or around the direct-child handoff (runner `cleanup_process_tree_after_exit`, supervisor `ensure_managed_process_tree_reaped`). Reaper only waits on the direct child. |
//! | 12 | Caller panic/drop while child live | `Drop for ManagedChild` kills and transfers the child; the supervisor's `ChildHandleGuard`/`ShutdownChildGuard` return the exact handle on task cancellation/panic; the reaper's entry guard requeues on unwind; a poisoned queue is recovered via `into_inner`. |
//!
//! # Ownership invariants
//!
//! - A `ReapTicket` completes exactly when the kernel wait on the owned child
//!   succeeded. Callers treat an incomplete ticket as "the child is still
//!   owned somewhere".
//! - `enqueue` (via a `ReadyChildReaper` handle) is an infallible queue
//!   transfer: the handle proves the coordinator thread was started by a
//!   successful `ensure_ready`, and the thread is immortal (panics are
//!   recovered in place), so the transfer can never lose the child.
//! - `enqueue_or_wait` is the fallible-start path: if the coordinator cannot
//!   be established, ownership stays in the caller's `&mut Option<Child>`
//!   slot and the kernel wait happens synchronously in the caller's task.
//!   A cancelled caller task leaves the child in the slot; only a successful
//!   wait empties it.
//! - No live child is ever dropped: every drop path requeues or completes the
//!   kernel wait first.

use std::collections::VecDeque;
use std::io;
use std::process::ExitStatus;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Poll interval between `try_wait` retries on a not-yet-exited child, and
/// the recovery pause after a worker panic. Was `DETACHED_REAP_POLL` (100 ms)
/// in the runner and the 100 ms retry sleep in the supervisor; the value is
/// part of the recorded behavior matrix and must not change.
const REAP_RETRY_POLL: Duration = Duration::from_millis(100);

/// Completion signal for one reaped child.
///
/// Completion means the kernel wait succeeded (see the ownership invariants
/// above). The ticket is cloneable: the reaper entry and every interested
/// caller hold a copy; dropping a ticket never affects the reaper.
#[derive(Clone)]
pub(crate) struct ReapTicket {
    completed: Arc<AtomicBool>,
}

impl ReapTicket {
    pub(crate) fn new() -> Self {
        Self {
            completed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.completed.load(AtomicOrdering::Acquire)
    }

    /// Marks the ticket complete.
    ///
    /// The reaper worker calls this only after a successful kernel wait. The
    /// supervisor's orphan cleanup uses it as its own verified-process-gone
    /// marker (the "kernel wait" is the exact-identity termination sequence);
    /// no other caller completes a ticket.
    pub(crate) fn complete(&self) {
        self.completed.store(true, AtomicOrdering::Release);
    }
}

/// One queued handoff: the owned child plus the ticket that completes when it
/// is reaped.
struct ReaperEntry {
    child: Option<tokio::process::Child>,
    ticket: ReapTicket,
}

impl ReaperEntry {
    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child
            .as_mut()
            .expect("reaper entry retains its child handle")
    }

    fn reaped(&mut self) {
        self.child = None;
        self.ticket.complete();
    }
}

/// Guard held by the worker while it polls one entry. Dropping it with the
/// child still present requeues the entry to the back of the queue, which is
/// the round-robin fairness behavior (matrix row 9) and the panic/error
/// recovery path (rows 6-7).
struct ReaperEntryGuard {
    reaper: &'static ChildReaper,
    entry: Option<ReaperEntry>,
}

impl ReaperEntryGuard {
    fn new(reaper: &'static ChildReaper, entry: ReaperEntry) -> Self {
        Self {
            reaper,
            entry: Some(entry),
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.entry
            .as_mut()
            .expect("reaper guard retains its entry")
            .child_mut()
    }

    fn reaped(&mut self) {
        if let Some(entry) = self.entry.as_mut() {
            // Completes the ticket exactly when the kernel wait succeeded and
            // clears the child so the guard drop does not requeue it.
            entry.reaped();
        }
        #[cfg(test)]
        self.reaper.processing.store(false, AtomicOrdering::Release);
    }
}

impl Drop for ReaperEntryGuard {
    fn drop(&mut self) {
        // Requeue only while the entry still owns a live child. After
        // `reaped()` the child is gone and the ticket is complete, so the
        // guard must not push an empty entry back into the queue.
        let child_present = self
            .entry
            .as_ref()
            .is_some_and(|entry| entry.child.is_some());
        if child_present {
            let entry = self
                .entry
                .take()
                .expect("requeue guard entry was checked above");
            let mut pending = self.reaper.pending();
            pending.push_back(entry);
            #[cfg(test)]
            self.reaper.processing.store(false, AtomicOrdering::Release);
            drop(pending);
            self.reaper.wake.notify_one();
        }
    }
}

/// One persistent Tokio-child reaper.
///
/// Exactly one definition of this type exists in the crate; both callers share
/// the `CHILD_REAPER` static.
pub(crate) struct ChildReaper {
    pending: Mutex<VecDeque<ReaperEntry>>,
    wake: Condvar,
    initialization: Mutex<()>,
    ready: AtomicBool,
    #[cfg(test)]
    processing: AtomicBool,
    #[cfg(test)]
    forced_start_failures: AtomicUsize,
    #[cfg(test)]
    forced_try_wait_errors: AtomicUsize,
    #[cfg(test)]
    forced_try_wait_panics: AtomicUsize,
    #[cfg(test)]
    coordinator_starts: AtomicUsize,
    #[cfg(test)]
    panic_recoveries: AtomicUsize,
    #[cfg(test)]
    transfers: AtomicUsize,
    #[cfg(test)]
    error_requeues: AtomicUsize,
    #[cfg(test)]
    fail_next_enqueue: AtomicBool,
    #[cfg(test)]
    fail_worker_start: AtomicBool,
}

pub(crate) static CHILD_REAPER: ChildReaper = ChildReaper::new();

/// Handle proving the coordinator thread is live. Obtained from a successful
/// `ensure_ready`; `enqueue` through this handle is an infallible transfer.
#[derive(Clone, Copy)]
pub(crate) struct ReadyChildReaper(&'static ChildReaper);

impl ChildReaper {
    pub(crate) const fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            initialization: Mutex::new(()),
            ready: AtomicBool::new(false),
            #[cfg(test)]
            processing: AtomicBool::new(false),
            #[cfg(test)]
            forced_start_failures: AtomicUsize::new(0),
            #[cfg(test)]
            forced_try_wait_errors: AtomicUsize::new(0),
            #[cfg(test)]
            forced_try_wait_panics: AtomicUsize::new(0),
            #[cfg(test)]
            coordinator_starts: AtomicUsize::new(0),
            #[cfg(test)]
            panic_recoveries: AtomicUsize::new(0),
            #[cfg(test)]
            transfers: AtomicUsize::new(0),
            #[cfg(test)]
            error_requeues: AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_enqueue: AtomicBool::new(false),
            #[cfg(test)]
            fail_worker_start: AtomicBool::new(false),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, VecDeque<ReaperEntry>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts the coordinator thread on first use. On error the caller
    /// retains ownership of any child it holds: the runner must not spawn,
    /// and the supervisor falls back to `enqueue_or_wait`'s synchronous wait.
    pub(crate) fn ensure_ready(&'static self) -> io::Result<ReadyChildReaper> {
        #[cfg(test)]
        if take_test_counter(&self.forced_start_failures) {
            return Err(io::Error::other(
                "injected reaper coordinator start failure",
            ));
        }
        #[cfg(test)]
        if self.fail_worker_start.load(AtomicOrdering::Acquire) {
            return Err(io::Error::other(
                "supervisor reaper worker start failure injected by test",
            ));
        }
        if !self.ready.load(AtomicOrdering::Acquire) {
            let _initialization = self
                .initialization
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !self.ready.load(AtomicOrdering::Acquire) {
                let thread = std::thread::Builder::new()
                    .name("feanorfs-child-reaper".to_string())
                    .spawn(move || self.run())?;
                #[cfg(test)]
                self.coordinator_starts.fetch_add(1, AtomicOrdering::SeqCst);
                drop(thread);
                self.ready.store(true, AtomicOrdering::Release);
            }
        }
        Ok(ReadyChildReaper(self))
    }

    /// Infallible queue transfer for callers holding a `ReadyChildReaper`
    /// handle (the runner). The coordinator is live, so this cannot fail and
    /// never drops the child.
    fn enqueue(&'static self, child: tokio::process::Child) -> ReapTicket {
        let ticket = ReapTicket::new();
        self.enqueue_with_ticket(child, &ticket);
        ticket
    }

    fn enqueue_with_ticket(&'static self, child: tokio::process::Child, ticket: &ReapTicket) {
        let mut pending = self.pending();
        pending.push_back(ReaperEntry {
            child: Some(child),
            ticket: ticket.clone(),
        });
        #[cfg(test)]
        self.transfers.fetch_add(1, AtomicOrdering::SeqCst);
        drop(pending);
        self.wake.notify_one();
    }

    /// Fallible-start handoff used by the supervisor: transfer the child to
    /// the coordinator, or — if the coordinator cannot be established —
    /// retain ownership in `child_slot` and wait synchronously in this task
    /// until the kernel reports exit. Infallible from the caller's
    /// perspective: no live child is dropped, and the returned ticket
    /// completes exactly when the kernel wait succeeded.
    pub(crate) async fn enqueue_or_wait(
        &'static self,
        child_slot: &mut Option<tokio::process::Child>,
    ) -> ReapTicket {
        let ticket = ReapTicket::new();
        #[cfg(test)]
        let primary_failed = self.fail_next_enqueue.swap(false, AtomicOrdering::AcqRel);
        #[cfg(not(test))]
        let primary_failed = false;

        if !primary_failed && self.ensure_ready().is_ok() {
            let child = child_slot
                .take()
                .expect("reaper enqueue owns a live child handle");
            self.enqueue_with_ticket(child, &ticket);
            return ticket;
        }

        // An unavailable coordinator cannot be allowed to turn the child back
        // into a fallible return value. Retain ownership locally and wait on
        // the original Tokio handle until the kernel reports it reaped.
        // `Child::wait` never signals by a guessed PID, and retries preserve
        // the handle even if an unusual wait error is transient.
        if primary_failed {
            tracing::warn!("child reaper enqueue failure; synchronously retaining child");
        } else {
            tracing::warn!("child reaper unavailable; synchronously retaining child");
        }
        loop {
            match child_slot
                .as_mut()
                .expect("synchronous reaper fallback retains child handle")
                .wait()
                .await
            {
                Ok(_) => {
                    // The kernel wait completed while this future still
                    // retained the handle in the caller's slot. Drop only
                    // that now-reaped handle; cancellation before this point
                    // leaves it available for the caller's ownership guard.
                    child_slot.take();
                    ticket.complete();
                    break;
                }
                Err(error) => {
                    tracing::warn!("child synchronous reap failed; retaining child: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        ticket
    }

    fn wait_for_entry(&'static self) -> ReaperEntryGuard {
        let mut pending = self.pending();
        let entry = loop {
            if let Some(entry) = pending.pop_front() {
                break entry;
            }
            pending = self
                .wake
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        };
        #[cfg(test)]
        self.processing.store(true, AtomicOrdering::Release);
        ReaperEntryGuard::new(self, entry)
    }

    fn try_wait(&self, child: &mut tokio::process::Child) -> io::Result<Option<ExitStatus>> {
        #[cfg(test)]
        if take_test_counter(&self.forced_try_wait_panics) {
            panic!("injected reaper wait panic");
        }
        #[cfg(test)]
        if take_test_counter(&self.forced_try_wait_errors) {
            return Err(io::Error::other("injected reaper wait failure"));
        }
        child.try_wait()
    }

    fn reap_one(&'static self) {
        let mut entry = self.wait_for_entry();
        let retry = match self.try_wait(entry.child_mut()) {
            Ok(Some(_)) => {
                entry.reaped();
                false
            }
            Ok(None) => true,
            Err(_) => {
                #[cfg(test)]
                self.error_requeues.fetch_add(1, AtomicOrdering::SeqCst);
                true
            }
        };
        if retry {
            drop(entry);
            std::thread::sleep(REAP_RETRY_POLL);
        }
    }

    /// Immortal worker: a panic inside `reap_one` unwinds the entry guard
    /// (requeueing the child) and is recovered here; the thread never exits,
    /// so ownership can never be stranded.
    fn run(&'static self) -> ! {
        loop {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reap_one())).is_err() {
                #[cfg(test)]
                self.panic_recoveries.fetch_add(1, AtomicOrdering::SeqCst);
                tracing::warn!("feanorfs child reaper recovered from child-processing panic");
                std::thread::sleep(REAP_RETRY_POLL);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_start(&self) {
        self.forced_start_failures
            .fetch_add(1, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn coordinator_start_count(&self) -> usize {
        self.coordinator_starts.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(AtomicOrdering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_try_wait(&self) {
        self.forced_try_wait_errors
            .fetch_add(1, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn panic_next_try_wait(&self) {
        self.forced_try_wait_panics
            .fetch_add(1, AtomicOrdering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn transfer_count(&self) -> usize {
        self.transfers.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn error_requeue_count(&self) -> usize {
        self.error_requeues.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn panic_recovery_count(&self) -> usize {
        self.panic_recoveries.load(AtomicOrdering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        let pending = self.pending();
        let empty = pending.is_empty();
        let processing = self.processing.load(AtomicOrdering::Acquire);
        drop(pending);
        empty && !processing
    }

    #[cfg(test)]
    pub(crate) fn fail_next_enqueue_for_test(&self) {
        self.fail_next_enqueue.store(true, AtomicOrdering::Release);
    }

    #[cfg(test)]
    pub(crate) fn set_fail_next_enqueue(&self, fail: bool) {
        self.fail_next_enqueue.store(fail, AtomicOrdering::Release);
    }

    #[cfg(test)]
    pub(crate) fn fail_worker_start_for_test(&self, fail: bool) {
        self.fail_worker_start.store(fail, AtomicOrdering::Release);
    }

    #[cfg(test)]
    pub(crate) fn poison_pending_for_test(&self) {
        let _guard = self.pending.lock().unwrap();
        panic!("injected reaper queue poison");
    }
}

impl ReadyChildReaper {
    /// Transfers an owned child to the live coordinator. Infallible (see
    /// module invariants); returns the ticket that completes on kernel reap.
    pub(crate) fn enqueue(self, child: tokio::process::Child) -> ReapTicket {
        self.0.enqueue(child)
    }
}

#[cfg(test)]
fn take_test_counter(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(AtomicOrdering::SeqCst, AtomicOrdering::SeqCst, |count| {
            count.checked_sub(1)
        })
        .is_ok()
}
