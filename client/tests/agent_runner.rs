//! Real-process coverage for the configured agent runner worker.

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_agent_core::{
    RunnerAttention, RunnerExecutionMode, RunnerInvocation, RunnerPhase, RunnerStore,
};
use feanorfs_client::{do_push_only, load_config, save_config, spawn_agent, SyncCtx};
use feanorfs_common::{AgentInboxQuery, AgentMessageInput, AgentMessageKind, AgentSendResult};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::io::AsRawFd as _;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::Duration;
use support::{
    spawn_test_client_with_server, spawn_test_server, write_workspace_file, TestClient, TestServer,
    TEST_PASSWORD, WORKSPACE_ID,
};
#[cfg(unix)]
use tokio::io::AsyncWriteExt as _;

const AGENT: &str = "runner-worker";
const SECRET_OUTPUT: &str = "runner-private-output-must-not-leak";

// This integration-test executable owns one isolated process profile. Its
// real-process tests must not concurrently mutate that profile or supervise
// children registered in it.
static REAL_PROCESS_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Serialize, Deserialize)]
struct InvocationRecord {
    invocation: RunnerInvocation,
    argv: Vec<String>,
    cwd: PathBuf,
    agent: String,
    agent_dir: PathBuf,
    workspace_root: PathBuf,
}

struct RunnerFixture {
    _server: TestServer,
    client: TestClient,
    config: feanorfs_client::Config,
    helper_program: PathBuf,
    fixed_args: Vec<String>,
}

impl RunnerFixture {
    fn root(&self) -> &Path {
        self.client.workspace.path()
    }

    fn store(&self) -> RunnerStore {
        RunnerStore::open_configured(self.root()).unwrap()
    }

    fn ctx(&self) -> SyncCtx<'_> {
        SyncCtx::from_config(
            &self._server.api,
            &self.client.db,
            self.root(),
            &self.config,
        )
        .unwrap()
    }
}

async fn setup_runner_fixture(configure_runner: bool) -> RunnerFixture {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    spawn_agent(
        client.workspace.path(),
        &client.db,
        &server.api,
        WORKSPACE_ID,
        AGENT,
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();
    let head = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();
    let helper_program = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let fixed_args = vec![
        "--ignored".to_string(),
        "--exact".to_string(),
        "runner_child_helper".to_string(),
        "--nocapture".to_string(),
        "--test-threads=1".to_string(),
    ];
    let fixture = RunnerFixture {
        _server: server,
        client,
        config,
        helper_program,
        fixed_args,
    };
    if configure_runner {
        let store = RunnerStore::configure(
            fixture.root(),
            AGENT,
            &fixture.helper_program,
            fixture.fixed_args.clone(),
            60,
            &head,
        )
        .unwrap();
        store.set_enabled(true).unwrap();
    }
    fixture
}

async fn setup_runner() -> RunnerFixture {
    setup_runner_fixture(true).await
}

async fn setup_runner_workspace() -> RunnerFixture {
    setup_runner_fixture(false).await
}

async fn send_request(fixture: &RunnerFixture, to: &str, body: &str) -> AgentSendResult {
    feanorfs_agent_core::send_message(
        &fixture.ctx(),
        AgentMessageInput {
            to: to.to_string(),
            kind: AgentMessageKind::Request,
            body: body.to_string(),
            about_snapshot: None,
            reply_to: None,
            from: Some("requester".to_string()),
        },
    )
    .await
    .unwrap()
}

fn spawn_worker(
    fixture: &RunnerFixture,
    helper_mode: &str,
    record_path: &Path,
    descendant_path: Option<&Path>,
) -> TestChild {
    let workspace = fixture.root().canonicalize().unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    command
        .args(["service", "runner-run"])
        .arg(&workspace)
        .env("FEANORFS_RUNNER_TEST_MODE", helper_mode)
        .env("FEANORFS_RUNNER_TEST_RECORD", record_path)
        .env("FEANORFS_RUNNER_TEST_CLI", env!("CARGO_BIN_EXE_feanorfs"))
        .env(
            "FEANORFS_RUNNER_TEST_PROFILE",
            std::env::var_os("FEANORFS_HOME").unwrap(),
        )
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    if let Some(path) = descendant_path {
        command.env("FEANORFS_RUNNER_TEST_DESCENDANT", path);
    }
    TestChild::new(command.spawn().unwrap())
}

fn spawn_foreground_worker(
    fixture: &RunnerFixture,
    helper_mode: &str,
    record_path: &Path,
    descendant_path: &Path,
) -> TestChild {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    command
        .args(["agent", "runner", "start", "--foreground"])
        .current_dir(fixture.root())
        .env("FEANORFS_RUNNER_TEST_MODE", helper_mode)
        .env("FEANORFS_RUNNER_TEST_RECORD", record_path)
        .env("FEANORFS_RUNNER_TEST_CLI", env!("CARGO_BIN_EXE_feanorfs"))
        .env(
            "FEANORFS_RUNNER_TEST_PROFILE",
            std::env::var_os("FEANORFS_HOME").unwrap(),
        )
        .env("FEANORFS_RUNNER_TEST_DESCENDANT", descendant_path)
        // The foreground cancellation event targets this process group. The
        // helper and its descendant install a test-only handler below so the
        // descendant remains alive until production Job-object teardown.
        .env("FEANORFS_RUNNER_TEST_IGNORE_CTRL_BREAK", "1")
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        // Keep the foreground worker in its own console process group so the
        // test can deliver the same Ctrl+Break cancellation that Tokio's
        // production shutdown channel receives.
        command
            .as_std_mut()
            .creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    TestChild::new(command.spawn().unwrap())
}

#[cfg(windows)]
fn request_foreground_cancellation(pid: u32) {
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

    // CTRL+BREAK targets a CREATE_NEW_PROCESS_GROUP without affecting the
    // test process. The foreground shutdown channel listens for this event on
    // Windows just as it listens for SIGTERM/SIGINT on Unix.
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    assert_ne!(generated, 0, "deliver Ctrl+Break to foreground runner");
}

#[cfg(windows)]
unsafe extern "system" fn ignore_test_console_control(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};

    match ctrl_type {
        CTRL_BREAK_EVENT | CTRL_C_EVENT => 1,
        _ => 0,
    }
}

#[cfg(windows)]
fn install_test_ctrl_break_handler() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    // This handler exists only in the direct helper/descendant test process.
    // It makes the targeted Ctrl+Break a cancellation signal for the
    // foreground runner, not an independent termination mechanism for the
    // descendant whose Job-object ownership is under test.
    let installed = unsafe { SetConsoleCtrlHandler(Some(ignore_test_console_control), 1) };
    assert_ne!(installed, 0, "install test Ctrl+Break handler");
}

async fn wait_for_records(path: &Path, count: usize) -> Vec<InvocationRecord> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let records = std::fs::read_to_string(path)
            .ok()
            .map(|content| {
                content
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if records.len() >= count {
            return records;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {count} runner invocation record(s)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_idle(store: &RunnerStore) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let status = store.status().unwrap();
        if status.phase == RunnerPhase::Idle
            && status.pending_count == 0
            && status.last_terminal_kind.is_some()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "runner did not complete its request: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop_worker(fixture: &RunnerFixture, child: TestChild) -> std::process::Output {
    fixture.store().set_enabled(false).unwrap();
    child
        .wait_with_output_bounded(Duration::from_secs(10))
        .await
        .expect("runner worker exits after disable")
}

async fn terminals_after(
    fixture: &RunnerFixture,
    cursor: &str,
) -> Vec<feanorfs_common::AgentMessage> {
    feanorfs_agent_core::inbox(
        &fixture.ctx(),
        AgentInboxQuery {
            recipient: "requester".to_string(),
            after: Some(cursor.to_string()),
            limit: 1000,
        },
    )
    .await
    .unwrap()
    .messages
}

async fn run_cli(workspace: &Path, args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    command.args(args).current_dir(workspace).kill_on_drop(true);
    command.output().await.unwrap()
}

/// Own a test-spawned process until it has been waited/reaped.  A panic in a
/// test must not leave a runner worker (or its supervisor) running in the
/// shared isolated profile, so Drop performs bounded TERM→KILL escalation.
struct TestChild {
    child: Option<tokio::process::Child>,
}

impl TestChild {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(tokio::process::Child::id)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .expect("test child was already reaped")
            .try_wait()
    }

    #[cfg(unix)]
    fn signal(&mut self, signal: libc::c_int) {
        let Some(pid) = self.id() else {
            return;
        };
        // Each test-owned command is placed in its own process group, so
        // descendants are included without ever touching the test runner
        // process group. The direct signal is a fallback for platforms or
        // commands that do not expose a process group.
        // SAFETY: `pid` is the exact process spawned and owned here.
        unsafe {
            if libc::kill(-(pid as libc::pid_t), signal) != 0 {
                let _ = libc::kill(pid as libc::pid_t, signal);
            }
        }
    }

    #[cfg(unix)]
    fn terminate(&mut self) {
        self.signal(libc::SIGTERM);
    }

    #[cfg(not(unix))]
    fn terminate(&mut self) {
        let _ = self
            .child
            .as_mut()
            .expect("test child was already reaped")
            .start_kill();
    }

    #[cfg(unix)]
    fn kill(&mut self) {
        self.signal(libc::SIGKILL);
    }

    #[cfg(not(unix))]
    fn kill(&mut self) {
        let _ = self
            .child
            .as_mut()
            .expect("test child was already reaped")
            .start_kill();
    }

    async fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(_)) => {
                    self.child.take();
                    return true;
                }
                Ok(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(None) | Err(_) => return false,
            }
        }
    }

    /// Wait for normal test completion, escalating if a test helper hangs.
    /// The final `wait_with_output` reaps the exact child after escalation.
    async fn wait_with_output_bounded(mut self, timeout: Duration) -> std::io::Result<Output> {
        let mut child = self.child.take().expect("test child was already reaped");
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Err(error) => {
                    handoff_to_persistent_reaper(child);
                    return Err(error);
                }
                Ok(Some(_)) => return child.wait_with_output().await,
                Ok(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(None) => break,
            }
        }

        #[cfg(unix)]
        let pid = child.id();
        #[cfg(unix)]
        if let Some(pid) = pid {
            // SAFETY: this is the exact test-owned child process group.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child.start_kill();
        }
        let grace_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Err(error) => {
                    handoff_to_persistent_reaper(child);
                    return Err(error);
                }
                Ok(Some(_)) => return child.wait_with_output().await,
                Ok(None) if tokio::time::Instant::now() < grace_deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(None) => break,
            }
        }
        #[cfg(unix)]
        if let Some(pid) = pid {
            // SAFETY: the process group still belongs to the exact child.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        let _ = child.start_kill();
        let final_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Err(error) => {
                    handoff_to_persistent_reaper(child);
                    return Err(error);
                }
                Ok(Some(_)) => return child.wait_with_output().await,
                Ok(None) if tokio::time::Instant::now() < final_deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(None) => {
                    handoff_to_persistent_reaper(child);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "test child cleanup handed off to a persistent reaper",
                    ));
                }
            }
        }
    }

    fn reap_blocking(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        let pid = child.id();
        #[cfg(unix)]
        if let Some(pid) = pid {
            // SAFETY: this is the exact test-owned child process group.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child.start_kill();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => {
                    handoff_to_persistent_reaper(child);
                    return;
                }
            }
        }

        #[cfg(unix)]
        if let Some(pid) = pid {
            // SAFETY: the process group remains owned by this test child.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        let _ = child.start_kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => {
                    handoff_to_persistent_reaper(child);
                    return;
                }
            }
        }
        handoff_to_persistent_reaper(child);
    }
}

/// Keep owning an unreaped child after Drop's bounded synchronous cleanup
/// window. A detached reaper thread never drops its `Child` until `try_wait`
/// confirms that the kernel has reaped the exact process.
fn handoff_to_persistent_reaper(child: tokio::process::Child) {
    use std::sync::{Arc, Mutex};

    let slot = Arc::new(Mutex::new(Some(child)));
    let worker_slot = Arc::clone(&slot);
    let spawned = std::thread::Builder::new()
        .name("feanorfs-test-child-reaper".to_string())
        .spawn(move || {
            let child = worker_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(mut child) = child {
                reap_until_waited(&mut child);
            }
        });
    if spawned.is_err() {
        let child = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("persistent test-child reaper lost child ownership");
        let mut child = child;
        reap_until_waited(&mut child);
    }
}

fn reap_until_waited(child: &mut tokio::process::Child) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) | Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        self.reap_blocking();
    }
}

#[cfg(debug_assertions)]
static MANUAL_SUPERVISOR_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(debug_assertions)]
struct ManualSupervisor {
    child: TestChild,
    // Every manual supervisor in this process shares the isolated global
    // profile (and therefore its single supervisor instance lock). Hold the
    // async guard for the complete child lifetime, including bounded teardown,
    // so libtest's parallel tests cannot race two supervisor processes.
    _serial_guard: tokio::sync::MutexGuard<'static, ()>,
}

#[cfg(debug_assertions)]
impl ManualSupervisor {
    async fn spawn(fixture: &RunnerFixture, mode: &str, record_path: &Path) -> Self {
        Self::spawn_for_workspace(fixture.root(), mode, record_path).await
    }

    async fn spawn_for_workspace(workspace: &Path, mode: &str, record_path: &Path) -> Self {
        let serial_guard = MANUAL_SUPERVISOR_SERIAL.lock().await;
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
        command
            .args(["service", "supervise"])
            .current_dir(workspace)
            .env("FEANORFS_RUNNER_TEST_MODE", mode)
            .env("FEANORFS_RUNNER_TEST_RECORD", record_path)
            .env("FEANORFS_RUNNER_TEST_CLI", env!("CARGO_BIN_EXE_feanorfs"))
            .env(
                "FEANORFS_RUNNER_TEST_PROFILE",
                std::env::var_os("FEANORFS_HOME").unwrap(),
            )
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        Self {
            child: TestChild::new(command.spawn().unwrap()),
            _serial_guard: serial_guard,
        }
    }

    async fn wait_until_ready(&mut self) {
        let pid = self.child.id().expect("manual supervisor pid");
        let status_path = feanorfs_agent_core::global_state_root()
            .unwrap()
            .join("supervisor-status.json");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => panic!("manual supervisor exited before readiness: {status}"),
                Ok(None) => {}
                Err(error) => panic!("inspect manual supervisor: {error}"),
            }
            if let Ok(bytes) = std::fs::read(&status_path) {
                if serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|status| status["pid"].as_u64())
                    == Some(u64::from(pid))
                {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "manual supervisor did not publish a live status snapshot"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_until_runner_stop_reconciled(&mut self, workspace: &Path) {
        let pid = self.child.id().expect("manual supervisor pid");
        let canonical = workspace
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let state_root = feanorfs_agent_core::global_state_root().unwrap();
        let registry_path = state_root.join("supervisor.json");
        let ack_path = state_root.join("supervisor-runner-ack.json");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    panic!("manual supervisor exited before reconciliation: {status}")
                }
                Ok(None) => {}
                Err(error) => panic!("inspect manual supervisor: {error}"),
            }
            let registry = std::fs::read(&registry_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let ack_store = std::fs::read(&ack_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let registry_generation = registry
                .as_ref()
                .and_then(|value| value["mutation_generation"].as_u64());
            let tombstone = registry
                .as_ref()
                .and_then(|value| value["runner_stop_tokens"].get(&canonical));
            let ack = ack_store
                .as_ref()
                .and_then(|value| value["acks"].get(&canonical));
            if let (Some(registry_generation), Some(tombstone), Some(ack)) =
                (registry_generation, tombstone, ack)
            {
                let stop_token = tombstone["token"].as_str().unwrap_or_default();
                if registry_generation > 0
                    && tombstone["generation"]
                        .as_u64()
                        .is_some_and(|value| value > 0)
                    && !stop_token.is_empty()
                    && ack["pid"].as_u64() == Some(u64::from(pid))
                    && ack["workspace"].as_str() == Some(canonical.as_str())
                    // Acknowledgements are runner-scoped. An unrelated registry
                    // mutation may advance the current generation after this
                    // runner was reconciled, so equality would create a race.
                    && ack["registry_generation"]
                        .as_u64()
                        .is_some_and(|value| value > 0)
                    && ack["generation"].as_u64().is_some_and(|value| value > 0)
                    && ack["stop_token"].as_str() == Some(stop_token)
                {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "manual supervisor did not reconcile the stopped runner"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn shutdown(mut self) {
        self.child.terminate();
        if self.child.wait_for_exit(Duration::from_secs(7)).await {
            return;
        }
        self.child.kill();
        assert!(
            self.child.wait_for_exit(Duration::from_secs(2)).await,
            "manual supervisor did not stop within the bounded cleanup escalation"
        );
    }
}

#[cfg(debug_assertions)]
async fn run_cli_with_manual_supervisor(workspace: &Path, args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    command
        .args(args)
        .current_dir(workspace)
        .env("FEANORFS_TEST_MANUAL_SUPERVISOR", "1")
        .kill_on_drop(true);
    command.output().await.unwrap()
}

fn assert_cli_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "CLI failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn test_child_drop_reaps_a_hanging_process() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    use std::os::unix::process::CommandExt as _;

    let mut command = tokio::process::Command::new("/bin/sleep");
    command
        .arg("30")
        .kill_on_drop(true)
        .as_std_mut()
        .process_group(0);
    let child = TestChild::new(command.spawn().unwrap());
    let pid = child.id().unwrap();

    drop(child);

    assert!(
        !feanorfs_agent_core::lock::pid_alive(pid),
        "dropping a test child must terminate and reap it"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn persistent_reaper_handoff_reaps_after_bounded_cleanup() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    use std::os::unix::process::CommandExt as _;

    let mut command = tokio::process::Command::new("/bin/sleep");
    command
        .arg("30")
        .kill_on_drop(true)
        .as_std_mut()
        .process_group(0);
    let child = command.spawn().unwrap();
    let pid = child.id().unwrap();
    // SAFETY: this is the exact process group just spawned for this test.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    handoff_to_persistent_reaper(child);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while feanorfs_agent_core::lock::pid_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !feanorfs_agent_core::lock::pid_alive(pid),
        "persistent reaper must retain ownership until the child is waited"
    );
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn visible_runner_cli_reconfigures_redacts_resets_and_preserves_agent_on_remove() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    spawn_agent(
        client.workspace.path(),
        &client.db,
        &server.api,
        WORKSPACE_ID,
        AGENT,
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let supervisor_record = tempfile::tempdir().unwrap();
    let mut supervisor = ManualSupervisor::spawn_for_workspace(
        client.workspace.path(),
        "publish_result",
        &supervisor_record.path().join("supervisor.ndjson"),
    )
    .await;
    supervisor.wait_until_ready().await;
    let current_head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let program = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let program_text = program.to_str().unwrap();
    let first_arg = "--private-fixed-one";
    let setup = run_cli(
        client.workspace.path(),
        &[
            "--json",
            "agent",
            "runner",
            "setup",
            AGENT,
            "--timeout",
            "60",
            "--",
            program_text,
            first_arg,
        ],
    )
    .await;
    assert_cli_success(&setup);
    let setup_stdout = String::from_utf8(setup.stdout).unwrap();
    assert!(!setup_stdout.contains(program_text));
    assert!(!setup_stdout.contains(first_arg));
    let setup_json: serde_json::Value = serde_json::from_str(&setup_stdout).unwrap();
    assert_eq!(setup_json["action"], "setup");
    assert_eq!(setup_json["runner"]["agent"], AGENT);
    assert_eq!(setup_json["runner"]["enabled"], false);
    assert_eq!(setup_json["supervisor"]["registered"], false);

    let store = RunnerStore::open_configured(client.workspace.path()).unwrap();
    let session = store
        .execution_session(client.workspace.path(), RunnerExecutionMode::Foreground)
        .unwrap();
    session
        .admit_inbox(&feanorfs_common::AgentInboxResult {
            cursor: "b".repeat(64),
            cursor_reset: false,
            messages: vec![feanorfs_common::AgentMessage {
                message_id: "1".repeat(64),
                from: "requester".to_string(),
                to: AGENT.to_string(),
                kind: AgentMessageKind::Request,
                body: "private request body must not leak".to_string(),
                about_snapshot: current_head,
                reply_to: None,
                created_at_ms: 1,
            }],
        })
        .unwrap();
    drop(session);
    drop(store);

    let second_arg = "--private-fixed-two";
    let reconfigure = run_cli(
        client.workspace.path(),
        &[
            "--json",
            "agent",
            "runner",
            "setup",
            AGENT,
            "--timeout",
            "120",
            "--",
            program_text,
            second_arg,
        ],
    )
    .await;
    assert_cli_success(&reconfigure);
    let reconfigure_stdout = String::from_utf8(reconfigure.stdout).unwrap();
    assert!(!reconfigure_stdout.contains(program_text));
    assert!(!reconfigure_stdout.contains(second_arg));
    assert!(!reconfigure_stdout.contains("private request body"));
    let store = RunnerStore::open_configured(client.workspace.path()).unwrap();
    assert_eq!(store.config().unwrap().fixed_args, vec![second_arg]);
    assert_eq!(store.config().unwrap().timeout_secs, 120);
    assert_eq!(store.status().unwrap().pending_count, 1);
    assert!(!store.status().unwrap().enabled);
    drop(store);

    let mut offline = config.clone();
    offline.server_url = "http://127.0.0.1:1".to_string();
    save_config(client.workspace.path(), &offline).unwrap();
    let status = run_cli(
        client.workspace.path(),
        &["--json", "agent", "runner", "status"],
    )
    .await;
    assert_cli_success(&status);
    let status_stdout = String::from_utf8(status.stdout).unwrap();
    assert!(!status_stdout.contains(program_text));
    assert!(!status_stdout.contains(second_arg));
    assert!(!status_stdout.contains("private request body"));
    let status_json: serde_json::Value = serde_json::from_str(&status_stdout).unwrap();
    assert_eq!(status_json["runner"]["pending_count"], 1);
    assert_eq!(status_json["supervisor"]["state"], "not_installed");

    save_config(client.workspace.path(), &config).unwrap();
    let reset = run_cli(
        client.workspace.path(),
        &["--json", "agent", "runner", "reset", "--discard-pending"],
    )
    .await;
    assert_cli_success(&reset);
    assert_eq!(
        RunnerStore::open_configured(client.workspace.path())
            .unwrap()
            .status()
            .unwrap()
            .pending_count,
        0
    );

    let agent_dir = feanorfs_agent_core::agent_dir(client.workspace.path(), AGENT).unwrap();
    let agent_root = agent_dir.parent().unwrap();
    let base_ref = agent_root.join("state/base-snapshot");
    let base_before = std::fs::read(&base_ref).unwrap();
    let worktree_marker = agent_dir.join("runner-remove-preserves-worktree");
    std::fs::write(&worktree_marker, b"preserved").unwrap();
    let runtime_marker = agent_root.join("state/runtime/runner-remove-preserves-runtime");
    std::fs::create_dir_all(runtime_marker.parent().unwrap()).unwrap();
    std::fs::write(&runtime_marker, b"preserved").unwrap();

    let remove = run_cli(
        client.workspace.path(),
        &["--json", "agent", "runner", "remove", "--discard-pending"],
    )
    .await;
    assert_cli_success(&remove);
    assert!(feanorfs_agent_core::runner_status(client.workspace.path())
        .unwrap()
        .is_none());
    assert_eq!(std::fs::read(&worktree_marker).unwrap(), b"preserved");
    assert_eq!(std::fs::read(&base_ref).unwrap(), base_before);
    assert_eq!(std::fs::read(&runtime_marker).unwrap(), b"preserved");
    supervisor.shutdown().await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn visible_runner_stop_is_idempotent_on_an_empty_profile() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner_workspace().await;
    let output = run_cli(fixture.root(), &["--json", "agent", "runner", "stop"]).await;
    assert_cli_success(&output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["action"], "stop");
    assert!(value["runner"].is_null());
    assert_eq!(value["supervisor"]["registered"], false);
    assert_eq!(value["supervisor"]["state"], "not_installed");
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn visible_runner_setup_is_fresh_without_supervisor_authority() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner_workspace().await;
    let program = fixture.helper_program.to_str().unwrap();
    let mut args = vec![
        "--json",
        "agent",
        "runner",
        "setup",
        AGENT,
        "--timeout",
        "60",
        "--",
        program,
    ];
    args.extend(fixture.fixed_args.iter().map(String::as_str));
    let output = run_cli(fixture.root(), &args).await;
    assert_cli_success(&output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["action"], "setup");
    assert_eq!(value["runner"]["agent"], AGENT);
    assert_eq!(value["runner"]["enabled"], false);
    assert_eq!(value["supervisor"]["registered"], false);
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn visible_runner_stop_on_disabled_configured_runner_without_authority_succeeds() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner_workspace().await;
    let program = fixture.helper_program.to_str().unwrap();
    let mut setup_args = vec![
        "--json",
        "agent",
        "runner",
        "setup",
        AGENT,
        "--timeout",
        "60",
        "--",
        program,
    ];
    setup_args.extend(fixture.fixed_args.iter().map(String::as_str));
    assert_cli_success(&run_cli(fixture.root(), &setup_args).await);

    // The runner is configured but remains disabled, and no supervisor has
    // ever registered or reported it. A stop must not wait for an impossible
    // reconciliation acknowledgement merely because the local config exists.
    let output = run_cli(fixture.root(), &["--json", "agent", "runner", "stop"]).await;
    assert_cli_success(&output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["action"], "stop");
    assert_eq!(value["runner"]["enabled"], false);
    assert_eq!(value["supervisor"]["registered"], false);
    assert_eq!(value["supervisor"]["state"], "not_installed");
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn visible_runner_repeated_setup_without_supervisor_authority_succeeds() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner_workspace().await;
    let program = fixture.helper_program.to_str().unwrap();
    let mut setup_args = vec![
        "--json",
        "agent",
        "runner",
        "setup",
        AGENT,
        "--timeout",
        "60",
        "--",
        program,
    ];
    setup_args.extend(fixture.fixed_args.iter().map(String::as_str));

    assert_cli_success(&run_cli(fixture.root(), &setup_args).await);
    let output = run_cli(fixture.root(), &setup_args).await;
    assert_cli_success(&output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["action"], "setup");
    assert_eq!(value["runner"]["agent"], AGENT);
    assert_eq!(value["runner"]["enabled"], false);
    assert_eq!(value["supervisor"]["registered"], false);
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn visible_runner_fresh_setup_waits_for_a_stale_registry_entry() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner_workspace().await;
    let temp = tempfile::tempdir().unwrap();
    let mut supervisor = ManualSupervisor::spawn(
        &fixture,
        "publish_result",
        &temp.path().join("supervisor.ndjson"),
    )
    .await;
    supervisor.wait_until_ready().await;

    let registry_path = feanorfs_agent_core::global_state_root()
        .unwrap()
        .join("supervisor.json");
    let canonical = fixture
        .root()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::write(
        &registry_path,
        serde_json::json!({
            "workspaces": [],
            "stopped": [],
            "runners": [canonical],
        })
        .to_string(),
    )
    .unwrap();

    let program = fixture.helper_program.to_str().unwrap();
    let mut args = vec![
        "--json",
        "agent",
        "runner",
        "setup",
        AGENT,
        "--timeout",
        "60",
        "--",
        program,
    ];
    args.extend(fixture.fixed_args.iter().map(String::as_str));
    let output = run_cli(fixture.root(), &args).await;
    assert_cli_success(&output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["action"], "setup");
    assert_eq!(value["supervisor"]["registered"], false);
    let registry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    assert!(registry["runners"].as_array().unwrap().is_empty());
    supervisor.shutdown().await;
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "current_thread")]
async fn visible_runner_stop_prevents_resurrection_across_manual_supervisor_restart() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    // The manual supervisor and visible CLI commands share the isolated
    // profile; ManualSupervisor's process-local guard serializes this test
    // with the other tests that own a supervisor child.
    let fixture = setup_runner_workspace().await;
    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("manual-supervisor-invocations.ndjson");
    let mut supervisor = ManualSupervisor::spawn(&fixture, "publish_result", &record_path).await;
    supervisor.wait_until_ready().await;

    let program = fixture.helper_program.to_str().unwrap();
    let mut setup_args = vec![
        "--json".to_string(),
        "agent".to_string(),
        "runner".to_string(),
        "setup".to_string(),
        AGENT.to_string(),
        "--timeout".to_string(),
        "60".to_string(),
        "--".to_string(),
        program.to_string(),
    ];
    setup_args.extend(fixture.fixed_args.iter().cloned());
    let setup_refs = setup_args.iter().map(String::as_str).collect::<Vec<_>>();
    assert_cli_success(&run_cli_with_manual_supervisor(fixture.root(), &setup_refs).await);

    assert_cli_success(
        &run_cli_with_manual_supervisor(fixture.root(), &["--json", "agent", "runner", "start"])
            .await,
    );
    let first_request = send_request(&fixture, AGENT, "first supervised request").await;
    let first_records = wait_for_records(&record_path, 1).await;
    wait_for_idle(&fixture.store()).await;
    assert_eq!(
        first_records[0].invocation.message.message_id,
        first_request.message_id
    );

    assert_cli_success(
        &run_cli_with_manual_supervisor(fixture.root(), &["--json", "agent", "runner", "stop"])
            .await,
    );
    assert!(!fixture.store().status().unwrap().enabled);

    supervisor.shutdown().await;
    let mut restarted = ManualSupervisor::spawn(&fixture, "publish_result", &record_path).await;
    restarted.wait_until_ready().await;
    restarted
        .wait_until_runner_stop_reconciled(fixture.root())
        .await;

    let deferred_request = send_request(&fixture, AGENT, "must remain stopped").await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        std::fs::read_to_string(&record_path)
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .count(),
        1,
        "a stopped runner must not resurrect when its supervisor restarts"
    );
    assert!(!fixture.store().status().unwrap().enabled);

    assert_cli_success(
        &run_cli_with_manual_supervisor(fixture.root(), &["--json", "agent", "runner", "start"])
            .await,
    );
    let records = wait_for_records(&record_path, 2).await;
    wait_for_idle(&fixture.store()).await;
    assert_eq!(
        records[1].invocation.message.message_id, deferred_request.message_id,
        "a later visible start must restore supervised execution"
    );

    assert_cli_success(
        &run_cli_with_manual_supervisor(fixture.root(), &["--json", "agent", "runner", "stop"])
            .await,
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn invocation_contract_and_child_published_terminal_complete() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    let request = send_request(&fixture, AGENT, "perform the configured task").await;
    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let child = spawn_worker(&fixture, "publish_result", &record_path, None);
    let records = wait_for_records(&record_path, 1).await;
    wait_for_idle(&fixture.store()).await;
    let output = stop_worker(&fixture, child).await;
    assert!(
        output.status.success(),
        "worker stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let record = &records[0];
    assert_eq!(record.invocation.schema_version, 1);
    assert_eq!(record.invocation.agent, AGENT);
    assert_eq!(record.invocation.message.message_id, request.message_id);
    assert_eq!(
        record.invocation.message.body,
        "perform the configured task"
    );
    assert_eq!(&record.argv[1..], fixture.fixed_args);
    assert_eq!(
        record.cwd,
        feanorfs_agent_core::agent_dir(fixture.root(), AGENT)
            .unwrap()
            .canonicalize()
            .unwrap()
    );
    assert_eq!(record.agent, AGENT);
    assert_eq!(record.agent_dir, record.cwd);
    assert_eq!(
        record.workspace_root,
        fixture.root().canonicalize().unwrap()
    );
    assert_eq!(
        fixture.helper_program,
        std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
    );
    assert_eq!(
        fixture.store().status().unwrap().last_terminal_kind,
        Some(AgentMessageKind::Result)
    );
}

#[tokio::test]
async fn generic_blocked_fallback_never_leaks_process_output() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    let request = send_request(&fixture, AGENT, "fallback request").await;
    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let child = spawn_worker(&fixture, "no_terminal", &record_path, None);
    wait_for_records(&record_path, 1).await;
    wait_for_idle(&fixture.store()).await;
    let output = stop_worker(&fixture, child).await;
    let captured = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!captured.contains(SECRET_OUTPUT));

    let terminal = terminals_after(&fixture, &request.message_id)
        .await
        .into_iter()
        .find(|message| message.reply_to.as_deref() == Some(request.message_id.as_str()))
        .unwrap();
    assert_eq!(terminal.kind, AgentMessageKind::Blocked);
    assert_eq!(
        terminal.body,
        "runner blocked: process exited without a correlated terminal"
    );
    assert!(!terminal.body.contains(SECRET_OUTPUT));
}

#[tokio::test]
async fn direct_requests_execute_sequentially_and_broadcast_is_ignored() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    send_request(&fixture, AGENT, "first direct").await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    send_request(&fixture, "*", "broadcast request").await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    send_request(&fixture, AGENT, "second direct").await;
    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let child = spawn_worker(&fixture, "no_terminal", &record_path, None);
    let records = wait_for_records(&record_path, 2).await;
    wait_for_idle(&fixture.store()).await;
    stop_worker(&fixture, child).await;

    let bodies = records
        .iter()
        .map(|record| record.invocation.message.body.as_str())
        .collect::<Vec<_>>();
    assert_eq!(bodies, ["first direct", "second direct"]);
}

#[tokio::test]
async fn disable_cancels_current_child_and_completes_durable_state() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    let request = send_request(&fixture, AGENT, "long-running request").await;
    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let descendant_path = temp.path().join("descendant.pid");
    let child = spawn_worker(&fixture, "hang_tree", &record_path, Some(&descendant_path));
    let worker_pid = child.id().expect("supervised runner worker pid");
    wait_for_records(&record_path, 1).await;
    let descendant = wait_for_descendant(&descendant_path).await;

    fixture.store().set_enabled(false).unwrap();
    let output = child
        .wait_with_output_bounded(Duration::from_secs(10))
        .await
        .expect("disabled runner exits");
    assert!(output.status.success());
    let status = fixture.store().status().unwrap();
    assert!(!status.enabled);
    assert_eq!(status.phase, RunnerPhase::Idle);
    assert_eq!(status.pending_count, 0);
    assert!(status.active_message_id.is_none());
    assert_eq!(status.last_terminal_kind, Some(AgentMessageKind::Blocked));
    assert!(status.attention.is_none());
    let terminal = terminals_after(&fixture, &request.message_id)
        .await
        .into_iter()
        .find(|message| message.reply_to.as_deref() == Some(request.message_id.as_str()))
        .unwrap();
    assert_eq!(terminal.body, "runner blocked: execution cancelled");

    assert_process_dies(worker_pid).await;
    assert_process_dies(descendant).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn foreground_invocation_cancellation_kills_term_ignoring_descendant() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    let supervisor_record = tempfile::tempdir().unwrap();
    let mut supervisor = ManualSupervisor::spawn(
        &fixture,
        "publish_result",
        &supervisor_record.path().join("supervisor.ndjson"),
    )
    .await;
    supervisor.wait_until_ready().await;
    let request = send_request(&fixture, AGENT, "foreground cancellation").await;
    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let descendant_path = temp.path().join("descendant.pid");
    let child = spawn_foreground_worker(&fixture, "hang_tree", &record_path, &descendant_path);
    let worker_pid = child.id().unwrap();
    let records = wait_for_records(&record_path, 1).await;
    let descendant = wait_for_descendant(&descendant_path).await;
    assert_eq!(records[0].invocation.message.message_id, request.message_id);

    #[cfg(unix)]
    {
        // SAFETY: the foreground CLI is this test's exact child process. Its
        // installed SIGTERM handler converts the signal into runner
        // cancellation.
        assert_eq!(
            unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGTERM) },
            0
        );
    }
    #[cfg(windows)]
    request_foreground_cancellation(worker_pid);
    let output = child
        .wait_with_output_bounded(Duration::from_secs(10))
        .await
        .expect("foreground runner exits after cancellation");
    assert!(
        output.status.success(),
        "foreground runner stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_process_dies(worker_pid).await;
    assert_process_dies(descendant).await;

    let status = fixture.store().status().unwrap();
    assert!(!status.enabled);
    assert_eq!(status.phase, RunnerPhase::Idle);
    assert_eq!(status.pending_count, 0);
    assert_eq!(status.last_terminal_kind, Some(AgentMessageKind::Blocked));
    let terminal = terminals_after(&fixture, &request.message_id)
        .await
        .into_iter()
        .find(|message| message.reply_to.as_deref() == Some(request.message_id.as_str()))
        .unwrap();
    assert_eq!(terminal.body, "runner blocked: execution cancelled");
    supervisor.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn normal_exit_terminates_remaining_process_group_before_completion() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    let request = send_request(&fixture, AGENT, "normal-exit process tree").await;
    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let descendant_path = temp.path().join("descendant.pid");
    let child = spawn_worker(&fixture, "exit_tree", &record_path, Some(&descendant_path));
    wait_for_records(&record_path, 1).await;
    let descendant = wait_for_descendant(&descendant_path).await;

    wait_for_idle(&fixture.store()).await;
    assert!(
        !feanorfs_agent_core::lock::pid_alive(descendant),
        "runner completed before terminating descendant {descendant}"
    );
    let output = stop_worker(&fixture, child).await;
    assert!(
        output.status.success(),
        "worker stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let terminal = terminals_after(&fixture, &request.message_id)
        .await
        .into_iter()
        .find(|message| message.reply_to.as_deref() == Some(request.message_id.as_str()))
        .unwrap();
    assert_eq!(
        terminal.body,
        "runner blocked: process exited without a correlated terminal"
    );
}

#[tokio::test]
async fn startup_ambiguous_checkpoint_never_relaunches() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    let request = send_request(&fixture, AGENT, "must not replay").await;
    let store = fixture.store();
    let session = store
        .execution_session(fixture.root(), RunnerExecutionMode::Supervised)
        .unwrap();
    let inbox = feanorfs_agent_core::inbox(
        &fixture.ctx(),
        AgentInboxQuery {
            recipient: AGENT.to_string(),
            after: Some(store.committed_cursor().unwrap()),
            limit: 1000,
        },
    )
    .await
    .unwrap();
    session.admit_inbox(&inbox).unwrap();
    let launch = session.begin_next(&inbox.cursor).unwrap();
    assert_eq!(launch.message_id, request.message_id);
    drop(session);
    drop(store);

    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let child = spawn_worker(&fixture, "no_terminal", &record_path, None);
    let output = child
        .wait_with_output_bounded(Duration::from_secs(10))
        .await
        .expect("ambiguous startup exits");
    assert!(output.status.success());
    assert!(!record_path.exists());
    let status = fixture.store().status().unwrap();
    assert_eq!(status.phase, RunnerPhase::NeedsAttention);
    assert_eq!(status.attention, Some(RunnerAttention::AmbiguousExecution));
    assert_eq!(
        status.active_message_id.as_deref(),
        Some(request.message_id.as_str())
    );
    assert_eq!(status.pending_count, 1);
}

#[tokio::test]
async fn startup_running_checkpoint_never_relaunches() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    let fixture = setup_runner().await;
    let request = send_request(&fixture, AGENT, "running request must not replay").await;
    let store = fixture.store();
    let session = store
        .execution_session(fixture.root(), RunnerExecutionMode::Supervised)
        .unwrap();
    let inbox = feanorfs_agent_core::inbox(
        &fixture.ctx(),
        AgentInboxQuery {
            recipient: AGENT.to_string(),
            after: Some(store.committed_cursor().unwrap()),
            limit: 1000,
        },
    )
    .await
    .unwrap();
    session.admit_inbox(&inbox).unwrap();
    let launch = session.begin_next(&inbox.cursor).unwrap();
    session
        .mark_spawned(
            &launch.message_id,
            std::process::id(),
            "persisted-running-child",
        )
        .unwrap();
    assert_eq!(store.status().unwrap().phase, RunnerPhase::Running);
    drop(session);
    drop(store);

    let temp = tempfile::tempdir().unwrap();
    let record_path = temp.path().join("invocations.ndjson");
    let child = spawn_worker(&fixture, "no_terminal", &record_path, None);
    let output = child
        .wait_with_output_bounded(Duration::from_secs(10))
        .await
        .expect("ambiguous running startup exits");
    assert!(output.status.success());
    assert!(!record_path.exists());
    let status = fixture.store().status().unwrap();
    assert_eq!(status.phase, RunnerPhase::NeedsAttention);
    assert_eq!(status.attention, Some(RunnerAttention::AmbiguousExecution));
    assert_eq!(
        status.active_message_id.as_deref(),
        Some(request.message_id.as_str())
    );
    assert_eq!(status.pending_count, 1);
}

async fn wait_for_descendant(path: &Path) -> u32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            if let Ok(pid) = value.parse() {
                return pid;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "descendant did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_process_dies(pid: u32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while feanorfs_agent_core::lock::pid_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!feanorfs_agent_core::lock::pid_alive(pid));
}

/// The production `service exec-gate` wrapper must block only after
/// `spawn()` has returned.  Releasing it then performs an in-place exec, so
/// PID, process group, stdin, and the target argv remain unchanged.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn exec_gate_release_preserves_process_identity_and_io() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    use std::os::unix::process::CommandExt as _;

    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("released.txt");
    let script =
        r#"IFS= read -r line; printf '%s|%s|%s|%s\n' "$$" "$0" "$1" "$line" > "$2"; sleep 1"#;
    let (mut release, child_endpoint) = UnixStream::pair().unwrap();
    let child_fd = child_endpoint.as_raw_fd();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    command
        .args([
            "service",
            "exec-gate",
            &child_fd.to_string(),
            "/bin/sh",
            "--",
            "-c",
            script,
            "gate-target",
            "argv-ok",
            marker.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The wrapper's child endpoint must survive its first exec; the release
    // writer remains parent-owned and is never inherited by the child.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
        command.as_std_mut().process_group(0);
    }
    let mut child = command.spawn().unwrap();
    drop(child_endpoint);
    let pid = child.id().unwrap();
    let group = unsafe { libc::getpgid(pid as libc::pid_t) };
    assert_eq!(group, pid as libc::pid_t);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!marker.exists(), "target must not execute before release");

    release.write_all(&[1]).unwrap();
    drop(release);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"stdin-ok\n").await.unwrap();
    stdin.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    let fields = std::fs::read_to_string(marker).unwrap();
    let fields = fields.trim_end().split('|').collect::<Vec<_>>();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0], pid.to_string());
    assert_eq!(&fields[1..], ["gate-target", "argv-ok", "stdin-ok"]);
}

/// Closing the release owner is fail-closed: the wrapper observes EOF and
/// exits without ever executing the configured target.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn exec_gate_owner_drop_prevents_target_execution() {
    let _serial_guard = REAL_PROCESS_SERIAL.lock().await;
    use std::os::unix::process::CommandExt as _;

    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("not-released.txt");
    let (release, child_endpoint) = UnixStream::pair().unwrap();
    let child_fd = child_endpoint.as_raw_fd();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    command
        .args([
            "service",
            "exec-gate",
            &child_fd.to_string(),
            "/bin/sh",
            "--",
            "-c",
            "printf started > \"$1\"",
            "gate-target",
            marker.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.as_std_mut().pre_exec(move || {
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(release);
    drop(child_endpoint);
    tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!marker.exists());
}

#[test]
#[ignore]
fn runner_child_helper() {
    #[cfg(windows)]
    if std::env::var_os("FEANORFS_RUNNER_TEST_IGNORE_CTRL_BREAK").is_some() {
        install_test_ctrl_break_handler();
    }
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes).unwrap();
    let invocation: RunnerInvocation = serde_json::from_slice(&bytes).unwrap();
    let record = InvocationRecord {
        invocation: invocation.clone(),
        argv: std::env::args().collect(),
        cwd: std::env::current_dir().unwrap().canonicalize().unwrap(),
        agent: std::env::var("FEANORFS_AGENT").unwrap(),
        agent_dir: PathBuf::from(std::env::var_os("FEANORFS_AGENT_DIR").unwrap()),
        workspace_root: PathBuf::from(std::env::var_os("FEANORFS_WORKSPACE_ROOT").unwrap()),
    };
    let record_path = PathBuf::from(std::env::var_os("FEANORFS_RUNNER_TEST_RECORD").unwrap());
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(record_path)
        .unwrap();
    writeln!(output, "{}", serde_json::to_string(&record).unwrap()).unwrap();
    output.flush().unwrap();

    match std::env::var("FEANORFS_RUNNER_TEST_MODE").unwrap().as_str() {
        "publish_result" => publish_result(&invocation),
        "no_terminal" => {
            println!("{SECRET_OUTPUT}");
            eprintln!("{SECRET_OUTPUT}");
        }
        "hang" => std::thread::sleep(Duration::from_secs(30)),
        "hang_tree" => {
            let descendant_path =
                PathBuf::from(std::env::var_os("FEANORFS_RUNNER_TEST_DESCENDANT").unwrap());
            let mut descendant = Command::new(std::env::current_exe().unwrap());
            descendant.args([
                "--ignored",
                "--exact",
                "runner_descendant_helper",
                "--nocapture",
            ]);
            #[cfg(windows)]
            if std::env::var_os("FEANORFS_RUNNER_TEST_IGNORE_CTRL_BREAK").is_some() {
                use std::os::windows::process::CommandExt as _;
                use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

                // Keep the descendant inside the inherited production Job
                // Object, but isolate its console process group from the
                // foreground worker's targeted Ctrl+Break. Its eventual
                // death therefore comes from Job teardown, not the console
                // event itself.
                descendant.creation_flags(CREATE_NEW_PROCESS_GROUP);
            }
            let mut descendant = descendant.spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !descendant_path.is_file() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(descendant_path.is_file(), "descendant did not become ready");
            std::thread::sleep(Duration::from_secs(30));
            let _ = descendant.wait();
        }
        "exit_tree" => {
            let descendant_path =
                PathBuf::from(std::env::var_os("FEANORFS_RUNNER_TEST_DESCENDANT").unwrap());
            let descendant = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "runner_descendant_helper",
                    "--nocapture",
                ])
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !descendant_path.is_file() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(descendant_path.is_file(), "descendant did not become ready");
            drop(descendant);
        }
        mode => panic!("unknown runner helper mode {mode}"),
    }
}

fn publish_result(invocation: &RunnerInvocation) {
    let cli = std::env::var_os("FEANORFS_RUNNER_TEST_CLI").unwrap();
    let profile = std::env::var_os("FEANORFS_RUNNER_TEST_PROFILE").unwrap();
    let status = Command::new(cli)
        .args(["agent", "send"])
        .arg(&invocation.message.from)
        .args(["--kind", "result", "--reply-to"])
        .arg(&invocation.message.message_id)
        .args(["--from"])
        .arg(&invocation.agent)
        .arg("configured child completed")
        .current_dir(std::env::var_os("FEANORFS_AGENT_DIR").unwrap())
        .env("FEANORFS_HOME", profile)
        .env(
            "FEANORFS_WORKSPACE_ROOT",
            std::env::var_os("FEANORFS_WORKSPACE_ROOT").unwrap(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore]
fn runner_descendant_helper() {
    #[cfg(windows)]
    if std::env::var_os("FEANORFS_RUNNER_TEST_IGNORE_CTRL_BREAK").is_some() {
        install_test_ctrl_break_handler();
    }
    #[cfg(unix)]
    // SAFETY: this single-threaded test helper intentionally ignores SIGTERM
    // before announcing readiness so the worker must exercise KILL escalation.
    unsafe {
        assert_ne!(libc::signal(libc::SIGTERM, libc::SIG_IGN), libc::SIG_ERR);
    }
    if let Some(path) = std::env::var_os("FEANORFS_RUNNER_TEST_DESCENDANT") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
    }
    std::thread::sleep(Duration::from_secs(30));
}
