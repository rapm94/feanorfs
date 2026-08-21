use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::process_tree;
use crate::cli::process_tree::{ReapTicket, CHILD_REAPER};
use feanorfs_client::workspace_path::CanonicalWorkspacePath;

use super::*;

static ACK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Wrap a literal registry identity for tests.
fn cwp(path: &str) -> CanonicalWorkspacePath {
    CanonicalWorkspacePath::from_exact_string(path.to_owned())
}

static RUNNER_FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
static REAPER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(unix)]
struct ReaperTestReset;

#[cfg(unix)]
impl Drop for ReaperTestReset {
    fn drop(&mut self) {
        TEST_FORCE_REAP_TIMEOUT.store(false, AtomicOrdering::Release);
        TEST_TERMINATION_GRACE_MILLIS.store(0, AtomicOrdering::Release);
        CHILD_REAPER.set_fail_next_enqueue(false);
        CHILD_REAPER.fail_worker_start_for_test(false);
        TEST_SHUTDOWN_PANIC_ONCE.store(false, AtomicOrdering::Release);
    }
}

fn id(ch: char) -> String {
    std::iter::repeat_n(ch, 64).collect()
}

fn configured_runner_fixture() -> (
    crate::cli::RunnerTestWorkspace,
    PathBuf,
    feanorfs_agent_core::RunnerStore,
) {
    let dir = crate::cli::RunnerTestWorkspace::new();
    let workspace = dir.path().canonicalize().unwrap();
    let fixture_sequence = RUNNER_FIXTURE_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    feanorfs_client::save_config(
        &workspace,
        &feanorfs_client::Config {
            server_url: "http://127.0.0.1:1".to_string(),
            workspace_id: format!("supervisor-runner-test-{fixture_sequence}"),
            encryption_password: Some("e".repeat(64)),
            server_password: None,
            tls_ca_pem: None,
            format_version: 3,
            hub_local: false,
            relay: None,
        },
    )
    .unwrap();
    let worktree = feanorfs_agent_core::agent_dir(&workspace, "worker").unwrap();
    let root = worktree.parent().unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::write(root.join("state/base-snapshot"), id('a')).unwrap();
    let program = std::env::current_exe().unwrap().canonicalize().unwrap();
    let store = feanorfs_agent_core::RunnerStore::configure(
        &workspace,
        "worker",
        &program,
        vec!["--fixed".to_string()],
        60,
        &id('a'),
    )
    .unwrap();
    (dir, workspace, store)
}

#[cfg(target_os = "windows")]
fn release_test_suspended_child(children: &mut BTreeMap<String, ManagedChild>, key: &str) {
    let managed = children
        .get_mut(key)
        .expect("test reconcile spawned the managed child");
    let tree = managed
        .process_tree
        .as_ref()
        .expect("test child has a private Windows Job Object");
    let child = managed
        .child
        .as_ref()
        .expect("test child retains its process handle");
    tree.release_child(child)
        .expect("resume adopted suspended test child");
}

fn spawn_long_running_test_child() -> tokio::process::Child {
    #[cfg(unix)]
    let mut command = {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args(["/C", "ping 127.0.0.1 -n 31 >NUL"]);
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn cross-platform long-running child")
}

#[cfg(unix)]
async fn spawn_term_ignoring_child() -> tokio::process::Child {
    use tokio::io::AsyncReadExt as _;

    // Ignored SIGTERM forces terminate_child through its bounded force
    // path. The readiness byte proves the trap is installed before the
    // caller can signal the child, and the builtin loop leaves no helper
    // descendant behind.
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "trap '' TERM; printf 1; while :; do :; done"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn termination test child");
    let mut ready = [0_u8; 1];
    tokio::time::timeout(
        Duration::from_secs(2),
        child
            .stdout
            .as_mut()
            .expect("termination test child has readiness pipe")
            .read_exact(&mut ready),
    )
    .await
    .expect("termination test child became ready")
    .expect("read termination test child readiness");
    assert_eq!(ready, [b'1']);
    child.stdout.take();
    child
}

#[cfg(unix)]
async fn assert_pid_reaped(pid: u32) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while feanorfs_agent_core::lock::pid_alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("termination test child was reaped");
}

mod child;
mod r#loop;
mod migration;
mod platform;
mod registry;
mod status;
