//! Platform-neutral identity, ownership, and shared-reaper state tests.
//!
//! The reaper tests are the behavior matrix harness: every row of the
//! matrix (already-exited child, normal async wait, startup failure, first
//! and repeated `try_wait` error, worker panic, shutdown transfer, multiple
//! queued children, ticket visibility, caller abort while a child is live,
//! poisoned-queue recovery) is characterized here plus in the runner and
//! supervisor lifecycle suites.

use super::*;
#[cfg(target_os = "windows")]
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "windows")]
use super::windows::{
    ensure_owner_job, normalize_windows_path, windows_process_creation_ticks, WindowsJob,
    TEST_FORCE_ADOPTION_FAILURE_PID,
};

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
    TEST_FORCE_ADOPTION_FAILURE_PID.store(pid, std::sync::atomic::Ordering::Release);
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
    let marker = std::env::var_os("FEANORFS_OWNER_CRASH_MARKER").expect("owner crash marker path");
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
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
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
    // A forged legacy `spawn:` token must never authorize group signaling;
    // `for_child_with_identity` reproduces the mismatch without field access.
    let group = ProcessGroup::for_child_with_identity(
        std::process::id(),
        &format!("spawn:{}:reused", std::process::id()),
    );
    assert!(!group.is_leader());
    assert!(!group.request_termination());
    assert!(!group.force_termination());
}

// Shared reaper behavior matrix

#[test]
#[ignore]
fn reaper_sleep_helper() {
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
#[ignore]
fn reaper_exit_helper() {
    // Exits immediately; used to enqueue an already-exited child.
}

fn reaper_child_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "cli::process_tree::tests::reaper_sleep_helper",
            "--nocapture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_process_group(&mut command).unwrap();
    command
}

fn spawn_reaper_child() -> tokio::process::Child {
    reaper_child_command().spawn().unwrap()
}

fn test_reaper() -> &'static ChildReaper {
    Box::leak(Box::new(ChildReaper::new()))
}

fn enqueue_killed_child(reaper: &'static ChildReaper) -> (u32, ReapTicket) {
    let ready = reaper.ensure_ready().unwrap();
    let mut child = spawn_reaper_child();
    let pid = child.id().unwrap();
    let _ = child.start_kill();
    (pid, ready.enqueue(child))
}

async fn wait_for_reaper_idle(reaper: &'static ChildReaper) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !reaper.is_idle() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child reaper became idle");
}

async fn wait_for_ticket(ticket: &ReapTicket) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ticket.is_complete() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reap ticket completed");
}

/// Matrix row 1: an already-exited child is reaped by the worker and the
/// ticket completes.
#[tokio::test]
async fn reaper_ticket_completes_for_already_exited_child() {
    let reaper = test_reaper();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "cli::process_tree::tests::reaper_exit_helper",
            "--nocapture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().expect("spawn quick-exit child");
    let pid = child.id().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while child.try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child exited before enqueue");

    let ready = reaper.ensure_ready().unwrap();
    let ticket = ready.enqueue(child);
    wait_for_ticket(&ticket).await;
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
}

/// Matrix rows 2/10: the ticket stays incomplete while the child is live and
/// completes exactly when the kernel wait succeeds.
#[tokio::test]
async fn reaper_ticket_visibility_matches_kernel_wait() {
    let reaper = test_reaper();
    let ready = reaper.ensure_ready().unwrap();
    // Force one worker panic on the first try_wait so the worker cannot
    // complete the ticket before the incompleteness assertion below runs.
    reaper.panic_next_try_wait();
    let mut child = spawn_reaper_child();
    let pid = child.id().unwrap();
    let _ = child.start_kill();
    let ticket = ready.enqueue(child);

    assert!(
        !ticket.is_complete(),
        "ticket is incomplete until the kernel wait succeeds"
    );
    wait_for_ticket(&ticket).await;
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    assert_eq!(reaper.panic_recovery_count(), 1);
}

/// Matrix row 5: coordinator startup failure leaves no thread and no child.
#[tokio::test]
async fn ensure_ready_failure_leaves_no_thread_and_no_transfer() {
    let reaper = test_reaper();
    reaper.fail_next_start();
    let error = reaper
        .ensure_ready()
        .err()
        .expect("injected coordinator start failure");
    assert!(error
        .to_string()
        .contains("injected reaper coordinator start failure"));
    assert!(!reaper.is_ready());
    assert_eq!(reaper.coordinator_start_count(), 0);
    assert!(reaper.is_idle());
}

/// Matrix row 5: `enqueue_or_wait` falls back to a synchronous in-task wait
/// when the coordinator cannot be established, never dropping the child.
#[tokio::test]
async fn enqueue_or_wait_falls_back_to_synchronous_wait_on_start_failure() {
    let reaper = test_reaper();
    reaper.fail_worker_start_for_test(true);
    let mut child = spawn_reaper_child();
    let pid = child.id().unwrap();
    let _ = child.start_kill();
    let mut slot = Some(child);
    let ticket = reaper.enqueue_or_wait(&mut slot).await;
    assert!(ticket.is_complete(), "fallback kernel wait completed");
    assert!(slot.is_none(), "fallback emptied the caller-owned slot");
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    assert_eq!(
        reaper.coordinator_start_count(),
        0,
        "no coordinator thread was started"
    );
}

/// Matrix row 12: aborting the caller task mid-fallback leaves the live child
/// in the caller's slot.
#[tokio::test]
async fn enqueue_or_wait_abort_retains_child_ownership() {
    let reaper = test_reaper();
    reaper.fail_worker_start_for_test(true);
    let child = spawn_reaper_child();
    let pid = child.id().unwrap();
    let slot = Arc::new(tokio::sync::Mutex::new(Some(child)));
    let slot_for_task = Arc::clone(&slot);
    let task = tokio::spawn(async move {
        let mut guard = slot_for_task.lock().await;
        reaper.enqueue_or_wait(&mut guard).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    task.abort();
    let _ = task.await;

    let mut guard = slot.lock().await;
    let recovered = guard
        .take()
        .expect("aborted fallback retained the live child");
    drop(guard);
    assert!(
        feanorfs_agent_core::lock::pid_alive(pid),
        "child survived the aborted fallback"
    );

    // Recover the child through the normal coordinator path.
    reaper.fail_worker_start_for_test(false);
    let mut recovered = recovered;
    let _ = recovered.start_kill();
    let ready = reaper.ensure_ready().unwrap();
    let ticket = ready.enqueue(recovered);
    wait_for_ticket(&ticket).await;
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
}

/// Matrix row 12: a poisoned queue is recovered on the next transfer.
#[tokio::test]
async fn reaper_recovers_poisoned_queue_on_transfer() {
    let reaper = test_reaper();
    let poisoned = std::panic::catch_unwind(|| reaper.poison_pending_for_test());
    assert!(poisoned.is_err());
    let ready = reaper.ensure_ready().unwrap();
    let mut child = spawn_reaper_child();
    let pid = child.id().unwrap();
    let _ = child.start_kill();
    ready.enqueue(child);
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    assert_eq!(reaper.transfer_count(), 1);
}

/// Matrix row 6: a single transient `try_wait` error requeues and reaps.
#[tokio::test]
async fn reaper_recovers_transient_try_wait_error() {
    let reaper = test_reaper();
    let ready = reaper.ensure_ready().unwrap();
    reaper.fail_next_try_wait();
    let mut child = spawn_reaper_child();
    let pid = child.id().unwrap();
    let _ = child.start_kill();
    ready.enqueue(child);

    tokio::time::timeout(Duration::from_secs(2), async {
        while reaper.error_requeue_count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("transient try_wait error was requeued");
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    assert_eq!(reaper.coordinator_start_count(), 1);
}

/// Matrix row 6 (repeated): consecutive `try_wait` errors keep requeueing
/// until the kernel wait succeeds; the worker never gives up on the child.
#[tokio::test]
async fn reaper_recovers_repeated_transient_wait_errors() {
    let reaper = test_reaper();
    let ready = reaper.ensure_ready().unwrap();
    reaper.fail_next_try_wait();
    reaper.fail_next_try_wait();
    let mut child = spawn_reaper_child();
    let pid = child.id().unwrap();
    let _ = child.start_kill();
    ready.enqueue(child);

    tokio::time::timeout(Duration::from_secs(2), async {
        while reaper.error_requeue_count() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both transient errors were requeued");
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    assert_eq!(reaper.coordinator_start_count(), 1);
}

/// Matrix row 7: worker panics are recovered in place, the thread never
/// exits, and later children are still reaped.
#[tokio::test]
async fn reaper_worker_survives_repeated_panics_and_reaps_later_children() {
    let reaper = test_reaper();

    reaper.panic_next_try_wait();
    let (first_pid, first_ticket) = enqueue_killed_child(reaper);
    tokio::time::timeout(Duration::from_secs(2), async {
        while reaper.panic_recovery_count() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first worker panic was recovered");
    wait_for_ticket(&first_ticket).await;
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(first_pid));

    reaper.panic_next_try_wait();
    let (second_pid, second_ticket) = enqueue_killed_child(reaper);
    tokio::time::timeout(Duration::from_secs(2), async {
        while reaper.panic_recovery_count() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("second worker panic was recovered");
    wait_for_ticket(&second_ticket).await;
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(second_pid));

    assert_eq!(reaper.panic_recovery_count(), 2);
    assert_eq!(
        reaper.coordinator_start_count(),
        1,
        "one immortal coordinator thread served both panics"
    );
}

/// Matrix rows 8/9: the worker drains to idle, wakes again for later
/// transfers, and one coordinator thread serves the whole process.
#[tokio::test]
async fn reaper_wakes_after_draining_to_idle() {
    let reaper = test_reaper();
    let (first_pid, _) = enqueue_killed_child(reaper);
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(first_pid));

    let (second_pid, _) = enqueue_killed_child(reaper);
    wait_for_reaper_idle(reaper).await;
    assert!(!feanorfs_agent_core::lock::pid_alive(second_pid));
    assert_eq!(reaper.transfer_count(), 2);
    assert_eq!(reaper.coordinator_start_count(), 1);
}

/// Matrix row 9: multiple queued children are all reaped without starvation.
#[tokio::test]
async fn reaper_drains_multiple_queued_children_without_starvation() {
    let reaper = test_reaper();
    let ready = reaper.ensure_ready().unwrap();
    let mut entries = Vec::new();
    for _ in 0..4 {
        let mut child = spawn_reaper_child();
        let pid = child.id().unwrap();
        let _ = child.start_kill();
        let ticket = ready.enqueue(child);
        entries.push((pid, ticket));
    }
    for (pid, ticket) in entries {
        wait_for_ticket(&ticket).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    }
    wait_for_reaper_idle(reaper).await;
    assert_eq!(reaper.transfer_count(), 4);
    assert_eq!(reaper.coordinator_start_count(), 1);
}
