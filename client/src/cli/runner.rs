//! Visible lifecycle controller for the workspace's configured agent runner.

use anyhow::{ensure, Context as _};
use clap::Subcommand;
use feanorfs_agent_core::{RunnerAttention, RunnerPhase, RunnerStatus, RunnerStore};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use super::supervisor::{self, ServiceState};
use super::util::output_json;

const DEFAULT_TIMEOUT_SECS: u64 = 3_600;
const MIN_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 86_400;
const MAX_PROGRAM_BYTES: usize = 16 * 1024;
const MAX_ARGS: usize = 128;
const MAX_ARG_BYTES: usize = 8 * 1024;
const MAX_ARGV_BYTES: usize = 64 * 1024;
const CONTROL_LOCK_FILE: &str = "runner-control.lock";

#[derive(Debug, Subcommand)]
pub enum RunnerAction {
    /// Configure a disabled runner for an existing spawned agent.
    Setup {
        agent: String,
        /// Maximum duration of each configured invocation.
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
        /// Program and fixed arguments. Must follow `--`.
        #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Start the configured runner under the single supervisor.
    Start {
        /// Run the same worker loop in this terminal instead of the supervisor.
        #[arg(long)]
        foreground: bool,
    },
    /// Stop supervised execution while preserving runner state and the agent.
    Stop,
    /// Show redacted local runner and supervisor state.
    Status,
    /// Discard pending or ambiguous work and advance to the current workspace head.
    Reset {
        #[arg(long, required = true)]
        discard_pending: bool,
    },
    /// Remove only runner configuration and state, preserving the spawned agent.
    Remove {
        #[arg(long, required = true)]
        discard_pending: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RunnerSupervisorProjection {
    registered: bool,
    state: ServiceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RunnerControlResult {
    action: &'static str,
    runner: Option<RunnerStatus>,
    supervisor: RunnerSupervisorProjection,
}

pub async fn run(current_dir: &Path, action: RunnerAction, json: bool) -> anyhow::Result<()> {
    let workspace = configured_workspace(current_dir)?;
    match action {
        RunnerAction::Setup {
            agent,
            timeout,
            command,
        } => {
            let result = {
                let _control = acquire_runner_control_lock(&workspace).await?;
                let status = setup_locked(&workspace, &agent, timeout, command).await?;
                result("setup", &workspace, Some(status))?
            };
            print_result(result, json, None)
        }
        RunnerAction::Start { foreground } => {
            if foreground {
                {
                    let _control = acquire_runner_control_lock(&workspace).await?;
                    let status = require_runner(&workspace)?.status()?;
                    reject_attention(&status)?;
                    stop_locked(&workspace)?;
                }
                // The handoff is now durable (disabled and unregistered), so
                // do not retain the CLI-control lock while the terminal owns
                // the long-running foreground worker. The runner lifetime
                // lease remains the final authority over the small handoff
                // race with another launcher.
                super::agent_runner::run_worker(
                    &workspace,
                    feanorfs_agent_core::RunnerExecutionMode::Foreground,
                )
                .await?;
                let status = require_runner(&workspace)?.status()?;
                print_result(result("start", &workspace, Some(status))?, json, None)
            } else {
                let result = {
                    let _control = acquire_runner_control_lock(&workspace).await?;
                    start_supervised_locked(&workspace)?;
                    let status = require_runner(&workspace)?.status()?;
                    result("start", &workspace, Some(status))?
                };
                print_result(result, json, None)
            }
        }
        RunnerAction::Stop => {
            let result = {
                let _control = acquire_runner_control_lock(&workspace).await?;
                let status = stop_locked(&workspace)?;
                result("stop", &workspace, status)?
            };
            print_result(result, json, None)
        }
        RunnerAction::Status => {
            let status = feanorfs_agent_core::runner_status(&workspace)?;
            print_result(result("status", &workspace, status)?, json, None)
        }
        RunnerAction::Reset { discard_pending } => {
            ensure!(
                discard_pending,
                "runner reset requires `--discard-pending`; ambiguous work is not replayed"
            );
            let result = {
                let _control = acquire_runner_control_lock(&workspace).await?;
                let store = require_stopped_runner(&workspace)?;
                let config = feanorfs_client::load_config(&workspace)?;
                let api = feanorfs_client::open_api_client(&workspace, &config).await?;
                let head = api
                    .get_head(&config.workspace_id)
                    .await?
                    .context("the workspace has no current snapshot head")?;
                let status = store.reset_to_current_cursor(&head, true)?;
                result("reset", &workspace, Some(status))?
            };
            print_result(
                result,
                json,
                Some("Pending and ambiguous runner work was discarded and will not be replayed."),
            )
        }
        RunnerAction::Remove { discard_pending } => {
            ensure!(
                discard_pending,
                "runner removal requires `--discard-pending`; ambiguous work is not replayed"
            );
            let result = {
                let _control = acquire_runner_control_lock(&workspace).await?;
                require_runner(&workspace)?;
                stop_locked(&workspace)?;
                feanorfs_agent_core::remove_configured(&workspace, true)?;
                result("remove", &workspace, None)?
            };
            print_result(
                result,
                json,
                Some(
                    "Runner state was removed. The agent worktree, base snapshot, and runtime were preserved.",
                ),
            )
        }
    }
}

async fn setup_locked(
    workspace: &Path,
    agent: &str,
    timeout_secs: u64,
    command: Vec<String>,
) -> anyhow::Result<RunnerStatus> {
    let configured = feanorfs_agent_core::runner_status(workspace)?;
    // Capture the pre-mutation authority before creating a fresh runner.  A
    // truly fresh workspace has no configured runner, no registry entry, and
    // no durable supervisor artifact to wait for; stale registry/status state
    // remains fail-closed and must still be acknowledged after removal.
    let registered_before = supervisor::is_runner_registered(workspace)?;
    let authority_before = supervisor::runner_stop_authority_exists(workspace)?;
    if let Some(status) = &configured {
        ensure!(
            status.agent == agent,
            "runner '{}' is already configured; run `feanorfs agent runner remove --discard-pending` before configuring runner '{agent}'",
            status.agent
        );
    }
    let (program, fixed_args) = validate_setup_inputs(workspace, agent, timeout_secs, command)?;

    let store = if configured.is_some() {
        stop_locked(workspace)?;
        RunnerStore::reconfigure(workspace, agent, &program, fixed_args, timeout_secs)?
    } else {
        let config = feanorfs_client::load_config(workspace)?;
        let api = feanorfs_client::open_api_client(workspace, &config).await?;
        let head = api
            .get_head(&config.workspace_id)
            .await?
            .context("the workspace has no current snapshot head")?;
        let store =
            RunnerStore::configure(workspace, agent, &program, fixed_args, timeout_secs, &head)?;
        supervisor::remove_runner_from_registry(workspace)?;
        if registered_before || authority_before {
            supervisor::wait_for_runner_stopped(workspace)?;
        }
        store
    };
    store.status()
}

/// The only supervisor operations that the visible control transaction uses.
/// Keeping this seam small makes failure compensation testable without giving
/// the controller another durable authority.
trait RunnerSupervisorOps {
    fn is_registered(&mut self, workspace: &Path) -> anyhow::Result<bool>;
    fn runner_authority_exists(&mut self, _workspace: &Path) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn add_runner(&mut self, workspace: &Path) -> anyhow::Result<()>;
    fn remove_runner(&mut self, workspace: &Path) -> anyhow::Result<()>;
    fn set_enabled(&mut self, store: &RunnerStore, enabled: bool) -> anyhow::Result<RunnerStatus>;
    fn ensure_supervisor_running(&mut self) -> anyhow::Result<()>;
    fn wait_for_runner_child(&mut self, workspace: &str) -> anyhow::Result<()>;
    fn wait_for_runner_stopped(&mut self, workspace: &Path) -> anyhow::Result<()>;
}

#[derive(Default)]
struct SystemRunnerSupervisorOps;

impl RunnerSupervisorOps for SystemRunnerSupervisorOps {
    fn is_registered(&mut self, workspace: &Path) -> anyhow::Result<bool> {
        supervisor::is_runner_registered(workspace)
    }

    fn runner_authority_exists(&mut self, workspace: &Path) -> anyhow::Result<bool> {
        supervisor::runner_stop_authority_exists(workspace)
    }

    fn add_runner(&mut self, workspace: &Path) -> anyhow::Result<()> {
        supervisor::add_runner(workspace)
    }

    fn remove_runner(&mut self, workspace: &Path) -> anyhow::Result<()> {
        supervisor::remove_runner_from_registry(workspace)
    }

    fn set_enabled(&mut self, store: &RunnerStore, enabled: bool) -> anyhow::Result<RunnerStatus> {
        store.set_enabled(enabled)
    }

    fn ensure_supervisor_running(&mut self) -> anyhow::Result<()> {
        supervisor::ensure_supervisor_running().map(|_| ())
    }

    fn wait_for_runner_child(&mut self, workspace: &str) -> anyhow::Result<()> {
        supervisor::wait_for_runner_child(workspace, supervisor::RUNNER_READY_TIMEOUT)
    }

    fn wait_for_runner_stopped(&mut self, workspace: &Path) -> anyhow::Result<()> {
        supervisor::wait_for_runner_stopped(workspace)
    }
}

#[derive(Clone, Copy)]
struct StartIntent {
    enabled: bool,
    registered: bool,
}

fn start_supervised_locked(workspace: &Path) -> anyhow::Result<()> {
    let mut ops = SystemRunnerSupervisorOps;
    start_supervised_with_ops(workspace, &mut ops)
}

fn start_supervised_with_ops(
    workspace: &Path,
    ops: &mut impl RunnerSupervisorOps,
) -> anyhow::Result<()> {
    let workspace_key = workspace
        .to_str()
        .context("canonical runner workspace path must be valid UTF-8")?;
    let store = require_runner(workspace)?;
    let status = store.status()?;
    reject_attention(&status)?;
    let prior = StartIntent {
        enabled: status.enabled,
        registered: ops.is_registered(workspace)?,
    };
    let mut registered_by_this_start = false;
    let mut enabled_by_this_start = false;

    if !prior.registered {
        if let Err(error) = ops.add_runner(workspace) {
            // A mutator may persist its change before reporting an error
            // (for example, an atomic write can succeed while a final read
            // fails). Re-read the durable registry before deciding whether
            // rollback owns the registration.
            let primary = match ops.is_registered(workspace) {
                Ok(registered) => {
                    registered_by_this_start = registered;
                    error
                }
                Err(observe_error) => {
                    // The pre-call intent was unregistered. If observation
                    // is itself unavailable, conservatively attempt the
                    // safe idempotent unregister and report both failures.
                    registered_by_this_start = true;
                    anyhow::anyhow!(
                        "{error:#}; could not observe runner registration after add failure: {observe_error:#}"
                    )
                }
            };
            return Err(compensate_start_failure(
                workspace,
                &store,
                ops,
                prior,
                registered_by_this_start,
                enabled_by_this_start,
                primary,
            ));
        }
        registered_by_this_start = true;
    }
    if !prior.enabled {
        if let Err(error) = ops.set_enabled(&store, true) {
            // RunnerStore persists enablement before its final status read,
            // so an Err does not prove that admission stayed disabled.
            let primary = match store.status() {
                Ok(status) => {
                    enabled_by_this_start = status.enabled;
                    error
                }
                Err(observe_error) => {
                    // The pre-call intent was disabled. Prefer a best-effort
                    // disable over leaving an unobserved admission enabled.
                    enabled_by_this_start = true;
                    anyhow::anyhow!(
                        "{error:#}; could not observe runner enablement after enable failure: {observe_error:#}"
                    )
                }
            };
            return Err(compensate_start_failure(
                workspace,
                &store,
                ops,
                prior,
                registered_by_this_start,
                enabled_by_this_start,
                primary,
            ));
        }
        enabled_by_this_start = true;
    }
    if let Err(error) = ops.ensure_supervisor_running() {
        return Err(compensate_start_failure(
            workspace,
            &store,
            ops,
            prior,
            registered_by_this_start,
            enabled_by_this_start,
            error,
        ));
    }
    if let Err(error) = ops.wait_for_runner_child(workspace_key) {
        return Err(compensate_start_failure(
            workspace,
            &store,
            ops,
            prior,
            registered_by_this_start,
            enabled_by_this_start,
            error,
        ));
    }
    Ok(())
}

fn compensate_start_failure(
    workspace: &Path,
    store: &RunnerStore,
    ops: &mut impl RunnerSupervisorOps,
    prior: StartIntent,
    registered_by_this_start: bool,
    enabled_by_this_start: bool,
    primary: anyhow::Error,
) -> anyhow::Error {
    let mut rollback_errors = Vec::new();
    let mut admission_disabled = !enabled_by_this_start;

    // A pre-existing enabled runner is already an admission intent. Never
    // tear it down just because this invocation failed later. If this
    // invocation enabled admission, disable it before removing a registry
    // entry it added. That restores even a pre-existing enabled-but-
    // unregistered state without leaving a newly admitted worker unregistered.
    if enabled_by_this_start {
        match ops.set_enabled(store, false) {
            Ok(_) => admission_disabled = true,
            Err(error) => {
                rollback_errors.push(format!("disable runner: {error:#}"));
                admission_disabled = store.status().is_ok_and(|status| !status.enabled);
            }
        }
    }
    if registered_by_this_start && admission_disabled {
        if let Err(error) = ops.remove_runner(workspace) {
            match ops.is_registered(workspace) {
                Ok(false) => {}
                Ok(true) => rollback_errors.push(format!("unregister runner: {error:#}")),
                Err(observe_error) => rollback_errors.push(format!(
                    "unregister runner: {error:#}; could not observe registration after unregister failure: {observe_error:#}"
                )),
            }
        }
    }
    if enabled_by_this_start || registered_by_this_start {
        if let Err(error) = ops.wait_for_runner_stopped(workspace) {
            rollback_errors.push(format!("wait for runner stop: {error:#}"));
        }
    }

    // Verify the complete durable intent, including asymmetric states that
    // can exist after a previous interrupted lifecycle operation. This turns
    // an effect-uncertain mutator into an explicit needs-attention error
    // instead of silently claiming that rollback restored the prior state.
    match store.status() {
        Ok(status) if status.enabled != prior.enabled => rollback_errors.push(format!(
            "runner enablement is {}, expected {} after rollback",
            status.enabled, prior.enabled
        )),
        Ok(_) => {}
        Err(error) => rollback_errors.push(format!("verify runner enablement: {error:#}")),
    }
    match ops.is_registered(workspace) {
        Ok(registered) if registered != prior.registered => rollback_errors.push(format!(
            "runner registration is {}, expected {} after rollback",
            registered, prior.registered
        )),
        Ok(_) => {}
        Err(error) => rollback_errors.push(format!("verify runner registration: {error:#}")),
    }

    if rollback_errors.is_empty() {
        anyhow::anyhow!(
            "supervised runner start failed: {primary:#}; rolled back this invocation's changes"
        )
    } else {
        anyhow::anyhow!(
            "supervised runner start failed: {primary:#}; rollback also failed: {}",
            rollback_errors.join("; ")
        )
    }
}

fn stop_locked(workspace: &Path) -> anyhow::Result<Option<RunnerStatus>> {
    let mut ops = SystemRunnerSupervisorOps;
    stop_with_ops(workspace, &mut ops)
}

fn stop_with_ops(
    workspace: &Path,
    ops: &mut impl RunnerSupervisorOps,
) -> anyhow::Result<Option<RunnerStatus>> {
    let status = feanorfs_agent_core::runner_status(workspace)?;
    let registered_before = ops.is_registered(workspace)?;
    let authority_before = ops.runner_authority_exists(workspace)?;
    // A configured RunnerStatus is client-owned state, not evidence that the
    // supervisor ever admitted this workspace.  Fresh setup creates a
    // disabled configured runner before any registry/status authority exists;
    // waiting for an acknowledgement in that state can only time out.  Keep
    // the pre-mutation registry and workspace-specific supervisor status as
    // the fail-closed authority evidence instead.
    let wait_for_stop = registered_before || authority_before;
    let mut errors = Vec::new();
    let mut admission_disabled = !status.as_ref().is_some_and(|status| status.enabled);
    if status.as_ref().is_some_and(|status| status.enabled) {
        let store = RunnerStore::open_configured(workspace)?;
        match ops.set_enabled(&store, false) {
            Ok(_) => admission_disabled = true,
            Err(error) => match store.status() {
                Ok(status) if !status.enabled => admission_disabled = true,
                Ok(_) => {
                    // Admission is still live, so unregistering it would
                    // violate the stop ordering. Never re-enable on failure.
                    errors.push(format!("disable runner: {error:#}"));
                }
                Err(observe_error) => errors.push(format!(
                    "disable runner: {error:#}; could not observe enablement after disable failure: {observe_error:#}"
                )),
            },
        }
    }
    // Stop is intentionally biased toward admission shutdown. If removing the
    // registry entry or waiting for the child fails, do not re-enable it.
    if admission_disabled {
        if let Err(error) = ops.remove_runner(workspace) {
            match ops.is_registered(workspace) {
                Ok(false) => {}
                Ok(true) => errors.push(format!("unregister runner: {error:#}")),
                Err(observe_error) => errors.push(format!(
                    "unregister runner: {error:#}; could not observe registration after unregister failure: {observe_error:#}"
                )),
            }
        }
        if wait_for_stop {
            if let Err(error) = ops.wait_for_runner_stopped(workspace) {
                errors.push(format!("wait for runner stop: {error:#}"));
            }
        }
    }
    if errors.is_empty() {
        feanorfs_agent_core::runner_status(workspace)
    } else {
        anyhow::bail!("runner stop failed: {}", errors.join("; "))
    }
}

/// Per-workspace serialization for visible runner-controller commands. This
/// is deliberately neither the agent-core runner lifecycle/lifetime lease nor
/// the global supervisor registry lock: it protects the cross-store intent
/// transaction spanning runner state, registry state, and readiness waits.
struct RunnerControlGuard(File);

impl Drop for RunnerControlGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

async fn acquire_runner_control_lock(workspace: &Path) -> anyhow::Result<RunnerControlGuard> {
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || acquire_runner_control_lock_blocking(&workspace))
        .await
        .context("join runner control lock acquisition")?
}

fn acquire_runner_control_lock_blocking(workspace: &Path) -> anyhow::Result<RunnerControlGuard> {
    let state = feanorfs_agent_core::workspace_state_path(workspace)?;
    let metadata = fs::symlink_metadata(&state)
        .with_context(|| format!("inspect runner control state directory {}", state.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "runner control state directory is not a real directory"
    );
    let path = state.join(CONTROL_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "runner control lock is not a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("open runner control lock {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    fs2::FileExt::lock_exclusive(&file)
        .with_context(|| format!("acquire runner control lock {}", path.display()))?;
    Ok(RunnerControlGuard(file))
}

fn result(
    action: &'static str,
    workspace: &Path,
    runner: Option<RunnerStatus>,
) -> anyhow::Result<RunnerControlResult> {
    let registered = supervisor::is_runner_registered(workspace)?;
    let state = if registered {
        supervisor::status_for_runner(workspace)?
    } else {
        ServiceState::NotInstalled
    };
    Ok(RunnerControlResult {
        action,
        runner,
        supervisor: RunnerSupervisorProjection { registered, state },
    })
}

fn print_result(
    result: RunnerControlResult,
    json: bool,
    consequence: Option<&str>,
) -> anyhow::Result<()> {
    if json {
        return output_json(&result);
    }
    match &result.runner {
        Some(status) => {
            println!(
                "Runner '{}': {} ({}, {} pending)",
                status.agent,
                phase_label(status.phase),
                if status.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.pending_count
            );
            if let Some(attention) = status.attention {
                println!("Needs attention: {}", attention_label(attention));
                println!(
                    "Stop it, inspect the agent worktree, then run `feanorfs agent runner reset --discard-pending`; ambiguous work is not replayed."
                );
            }
        }
        None => println!("No agent runner is configured."),
    }
    println!(
        "Supervisor: {}{}",
        service_state_label(result.supervisor.state),
        if result.supervisor.registered {
            " (registered)"
        } else {
            ""
        }
    );
    if let Some(consequence) = consequence {
        println!("{consequence}");
    }
    Ok(())
}

fn configured_workspace(path: &Path) -> anyhow::Result<PathBuf> {
    let workspace = path
        .canonicalize()
        .with_context(|| format!("workspace folder does not exist: {}", path.display()))?;
    ensure!(workspace.is_dir(), "workspace path must be a directory");
    ensure!(
        workspace.to_str().is_some(),
        "canonical runner workspace path must be valid UTF-8"
    );
    let config = feanorfs_client::load_config(&workspace)
        .context("agent runners require an existing configured FeanorFS workspace")?;
    ensure!(
        config.format_version == 3,
        "agent runners require a format-v3 workspace"
    );
    Ok(workspace)
}

fn require_runner(workspace: &Path) -> anyhow::Result<RunnerStore> {
    RunnerStore::open_configured(workspace)
        .context("no agent runner is configured; run `feanorfs agent runner setup` first")
}

fn require_stopped_runner(workspace: &Path) -> anyhow::Result<RunnerStore> {
    let store = require_runner(workspace)?;
    ensure!(
        !store.status()?.enabled && !supervisor::is_runner_registered(workspace)?,
        "stop the runner with `feanorfs agent runner stop` before resetting it"
    );
    Ok(store)
}

fn reject_attention(status: &RunnerStatus) -> anyhow::Result<()> {
    ensure!(
        status.phase != RunnerPhase::NeedsAttention,
        "runner needs attention ({}); stop it, inspect the agent worktree, then run `feanorfs agent runner reset --discard-pending`; ambiguous work is not replayed",
        status
            .attention
            .map(attention_label)
            .unwrap_or("unknown runner state")
    );
    Ok(())
}

fn validate_setup_inputs(
    workspace: &Path,
    agent: &str,
    timeout_secs: u64,
    command: Vec<String>,
) -> anyhow::Result<(PathBuf, Vec<String>)> {
    feanorfs_agent_core::validate_name(agent)?;
    ensure!(
        (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout_secs),
        "runner timeout must be between {MIN_TIMEOUT_SECS} and {MAX_TIMEOUT_SECS} seconds"
    );
    let (program, fixed_args) = command
        .split_first()
        .context("runner setup requires a program after `--`")?;
    ensure!(
        fixed_args.len() <= MAX_ARGS,
        "runner has too many fixed arguments"
    );
    let mut argv_bytes = 0usize;
    for arg in fixed_args {
        ensure!(
            arg.len() <= MAX_ARG_BYTES,
            "runner fixed argument is too large"
        );
        ensure!(!arg.contains('\0'), "runner fixed argument contains NUL");
        argv_bytes = argv_bytes.saturating_add(arg.len());
    }
    ensure!(
        argv_bytes <= MAX_ARGV_BYTES,
        "runner fixed argv exceeds its size bound"
    );

    let program = which::which(program).context("resolve runner program through PATH")?;
    let program = program
        .canonicalize()
        .context("canonicalize runner program")?;
    let program_text = program
        .to_str()
        .context("runner program path must be valid UTF-8")?;
    ensure!(
        !program_text.is_empty()
            && program_text.len() <= MAX_PROGRAM_BYTES
            && !program_text.chars().any(char::is_control),
        "runner program path is invalid or too large"
    );
    let metadata = std::fs::metadata(&program).context("inspect runner program")?;
    ensure!(metadata.is_file(), "runner program must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "runner program must be executable"
        );
    }

    let agents = feanorfs_agent_core::workspace_state_path(workspace)?.join("agents");
    let agent_dir = feanorfs_agent_core::agent_dir(workspace, agent)?;
    let agent_root = agent_dir
        .parent()
        .context("configured agent worktree has no owned root")?;
    let agent_state = agent_root.join("state");
    for (path, label) in [
        (agents.as_path(), "agents directory"),
        (agent_root, "agent root"),
        (agent_dir.as_path(), "agent worktree"),
        (agent_state.as_path(), "agent state directory"),
    ] {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("spawn agent '{agent}' before configuring its runner"))?;
        ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "{label} is not a real directory"
        );
    }
    let canonical_agents = agents.canonicalize()?;
    let canonical_root = agent_root.canonicalize()?;
    ensure!(
        canonical_root == canonical_agents.join(agent),
        "agent root is reached through a filesystem alias"
    );
    ensure!(
        agent_dir.canonicalize()? == canonical_root.join("worktree")
            && agent_state.canonicalize()? == canonical_root.join("state"),
        "agent workspace is reached through a filesystem alias"
    );
    let base_ref = feanorfs_agent_core::paths::agent_base_ref(workspace, agent)?;
    let base_metadata = std::fs::symlink_metadata(&base_ref)
        .context("configured agent base snapshot is missing")?;
    ensure!(
        base_metadata.file_type().is_file()
            && !base_metadata.file_type().is_symlink()
            && base_metadata.len() <= 128,
        "configured agent base snapshot is invalid"
    );
    let base = std::fs::read_to_string(base_ref)?;
    ensure!(
        feanorfs_common::is_valid_hash(base.trim()),
        "configured agent base snapshot must contain one full snapshot id"
    );
    Ok((program, fixed_args.to_vec()))
}

const fn phase_label(phase: RunnerPhase) -> &'static str {
    match phase {
        RunnerPhase::Idle => "idle",
        RunnerPhase::Launching => "launching",
        RunnerPhase::Running => "running",
        RunnerPhase::NeedsAttention => "needs attention",
    }
}

const fn attention_label(attention: RunnerAttention) -> &'static str {
    match attention {
        RunnerAttention::CursorReset => "inbox cursor reset",
        RunnerAttention::PendingOverflow => "pending queue overflow",
        RunnerAttention::AmbiguousExecution => "ambiguous prior execution",
        RunnerAttention::DeliveryUnknown => "terminal delivery unknown",
        RunnerAttention::PreparationFailed => "refresh preparation failed",
    }
}

const fn service_state_label(state: ServiceState) -> &'static str {
    match state {
        ServiceState::NotInstalled => "not registered",
        ServiceState::Running => "running",
        ServiceState::Stopped => "stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn id(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn configured_runner_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        feanorfs_client::save_config(
            &workspace,
            &feanorfs_client::Config {
                server_url: "http://127.0.0.1:1".to_string(),
                workspace_id: "runner-control-test".to_string(),
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
        let agent_root = worktree.parent().unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(agent_root.join("state")).unwrap();
        std::fs::write(agent_root.join("state/base-snapshot"), id('a')).unwrap();
        let program = std::env::current_exe().unwrap().canonicalize().unwrap();
        RunnerStore::configure(&workspace, "worker", &program, vec![], 60, &id('a')).unwrap();
        (dir, workspace)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InjectedFailure {
        Add,
        AddAfterMutation,
        Enable,
        EnableAfterMutation,
        Disable,
        DisableAfterMutation,
        Ensure,
        Ready,
        Remove,
        RemoveAfterMutation,
        StopWait,
    }

    #[derive(Default)]
    struct FakeSupervisorState {
        registered: bool,
        runner_authority: bool,
        failures: Vec<InjectedFailure>,
        calls: Vec<&'static str>,
    }

    #[derive(Clone)]
    struct FakeSupervisorOps {
        state: Arc<Mutex<FakeSupervisorState>>,
    }

    impl FakeSupervisorOps {
        fn new(registered: bool, failure: Option<InjectedFailure>) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeSupervisorState {
                    registered,
                    runner_authority: false,
                    failures: failure.into_iter().collect(),
                    calls: Vec::new(),
                })),
            }
        }

        fn with_failures(registered: bool, failures: &[InjectedFailure]) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeSupervisorState {
                    registered,
                    runner_authority: false,
                    failures: failures.to_vec(),
                    calls: Vec::new(),
                })),
            }
        }

        fn with_runner_authority(self, runner_authority: bool) -> Self {
            self.state.lock().unwrap().runner_authority = runner_authority;
            self
        }

        fn registered(&self) -> bool {
            self.state.lock().unwrap().registered
        }

        fn calls(&self) -> Vec<&'static str> {
            self.state.lock().unwrap().calls.clone()
        }

        fn fail(&self, step: InjectedFailure) -> anyhow::Result<()> {
            let mut state = self.state.lock().unwrap();
            if let Some(index) = state.failures.iter().position(|failure| *failure == step) {
                state.failures.remove(index);
                anyhow::bail!("injected {step:?} failure");
            }
            Ok(())
        }

        fn take_failure(&self, step: InjectedFailure) -> bool {
            let mut state = self.state.lock().unwrap();
            let Some(index) = state.failures.iter().position(|failure| *failure == step) else {
                return false;
            };
            state.failures.remove(index);
            true
        }

        fn record(&self, operation: &'static str) {
            self.state.lock().unwrap().calls.push(operation);
        }
    }

    impl RunnerSupervisorOps for FakeSupervisorOps {
        fn is_registered(&mut self, _workspace: &Path) -> anyhow::Result<bool> {
            self.record("registered");
            Ok(self.registered())
        }

        fn runner_authority_exists(&mut self, _workspace: &Path) -> anyhow::Result<bool> {
            self.record("authority");
            Ok(self.state.lock().unwrap().runner_authority)
        }

        fn add_runner(&mut self, _workspace: &Path) -> anyhow::Result<()> {
            self.record("add");
            self.fail(InjectedFailure::Add)?;
            self.state.lock().unwrap().registered = true;
            if self.take_failure(InjectedFailure::AddAfterMutation) {
                anyhow::bail!("injected AddAfterMutation failure");
            }
            Ok(())
        }

        fn remove_runner(&mut self, _workspace: &Path) -> anyhow::Result<()> {
            self.record("remove");
            self.fail(InjectedFailure::Remove)?;
            self.state.lock().unwrap().registered = false;
            if self.take_failure(InjectedFailure::RemoveAfterMutation) {
                anyhow::bail!("injected RemoveAfterMutation failure");
            }
            Ok(())
        }

        fn set_enabled(
            &mut self,
            store: &RunnerStore,
            enabled: bool,
        ) -> anyhow::Result<RunnerStatus> {
            self.record(if enabled { "enable" } else { "disable" });
            self.fail(if enabled {
                InjectedFailure::Enable
            } else {
                InjectedFailure::Disable
            })?;
            let status = store.set_enabled(enabled)?;
            let after = if enabled {
                InjectedFailure::EnableAfterMutation
            } else {
                InjectedFailure::DisableAfterMutation
            };
            if self.take_failure(after) {
                anyhow::bail!("injected {after:?} failure");
            }
            Ok(status)
        }

        fn ensure_supervisor_running(&mut self) -> anyhow::Result<()> {
            self.record("ensure");
            self.fail(InjectedFailure::Ensure)
        }

        fn wait_for_runner_child(&mut self, _workspace: &str) -> anyhow::Result<()> {
            self.record("ready");
            self.fail(InjectedFailure::Ready)
        }

        fn wait_for_runner_stopped(&mut self, _workspace: &Path) -> anyhow::Result<()> {
            self.record("stopped");
            self.fail(InjectedFailure::StopWait)
        }
    }

    #[test]
    fn attention_labels_cover_every_persisted_attention_state() {
        assert_eq!(
            attention_label(RunnerAttention::CursorReset),
            "inbox cursor reset"
        );
        assert_eq!(
            attention_label(RunnerAttention::PendingOverflow),
            "pending queue overflow"
        );
        assert_eq!(
            attention_label(RunnerAttention::AmbiguousExecution),
            "ambiguous prior execution"
        );
        assert_eq!(
            attention_label(RunnerAttention::DeliveryUnknown),
            "terminal delivery unknown"
        );
        assert_eq!(
            attention_label(RunnerAttention::PreparationFailed),
            "refresh preparation failed"
        );
    }

    #[test]
    fn status_json_is_redacted() {
        let result = RunnerControlResult {
            action: "status",
            runner: Some(RunnerStatus {
                configured: true,
                enabled: false,
                agent: "worker".to_string(),
                phase: RunnerPhase::Idle,
                pending_count: 0,
                active_message_id: None,
                active_session_id: None,
                active_started_at_ms: None,
                active_spawned_at_ms: None,
                last_terminal_kind: None,
                last_terminal_message_id: None,
                attention: None,
                updated_at_ms: 1,
                inbox_failure_count: 0,
            }),
            supervisor: RunnerSupervisorProjection {
                registered: false,
                state: ServiceState::NotInstalled,
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("program"));
        assert!(!json.contains("argv"));
        assert!(!json.contains("output"));
        assert!(!json.contains("body"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["runner"]["agent"],
            "worker"
        );
    }

    #[test]
    fn supervised_start_rolls_back_every_new_intent_on_injected_failure() {
        for failure in [
            InjectedFailure::Add,
            InjectedFailure::Enable,
            InjectedFailure::Ensure,
            InjectedFailure::Ready,
        ] {
            let (_dir, workspace) = configured_runner_fixture();
            let mut ops = FakeSupervisorOps::new(false, Some(failure));
            let error = start_supervised_with_ops(&workspace, &mut ops).unwrap_err();
            assert!(error.to_string().contains("injected"));
            assert!(
                !RunnerStore::open_configured(&workspace)
                    .unwrap()
                    .status()
                    .unwrap()
                    .enabled,
                "{failure:?} left supervised admission enabled"
            );
            assert!(
                !ops.registered(),
                "{failure:?} left a runner registered without admission"
            );
            if !matches!(failure, InjectedFailure::Add) {
                assert!(ops.calls().contains(&"stopped"));
            }
        }
    }

    #[test]
    fn supervised_start_reconciles_mutation_before_error() {
        for failure in [
            InjectedFailure::AddAfterMutation,
            InjectedFailure::EnableAfterMutation,
        ] {
            let (_dir, workspace) = configured_runner_fixture();
            let mut ops = FakeSupervisorOps::new(false, Some(failure));

            let error = start_supervised_with_ops(&workspace, &mut ops).unwrap_err();

            assert!(error.to_string().contains("injected"));
            assert!(
                !RunnerStore::open_configured(&workspace)
                    .unwrap()
                    .status()
                    .unwrap()
                    .enabled,
                "{failure:?} left supervised admission enabled after a write-then-error"
            );
            assert!(
                !ops.registered(),
                "{failure:?} left a runner registered after a write-then-error"
            );
            assert!(ops.calls().contains(&"registered"));
        }
    }

    #[test]
    fn supervised_start_preserves_preexisting_start_intent_on_failure() {
        let (_dir, workspace) = configured_runner_fixture();
        let store = RunnerStore::open_configured(&workspace).unwrap();
        store.set_enabled(true).unwrap();
        let mut ops = FakeSupervisorOps::new(true, Some(InjectedFailure::Ensure));

        let error = start_supervised_with_ops(&workspace, &mut ops).unwrap_err();

        assert!(error.to_string().contains("injected Ensure failure"));
        assert!(store.status().unwrap().enabled);
        assert!(ops.registered());
        assert!(!ops.calls().contains(&"disable"));
        assert!(!ops.calls().contains(&"remove"));
    }

    #[test]
    fn supervised_start_restores_asymmetric_prior_intent_on_failure() {
        for (enabled, registered) in [(false, true), (true, false)] {
            let (_dir, workspace) = configured_runner_fixture();
            let store = RunnerStore::open_configured(&workspace).unwrap();
            if enabled {
                store.set_enabled(true).unwrap();
            }
            let mut ops = FakeSupervisorOps::new(registered, Some(InjectedFailure::Ensure));

            let error = start_supervised_with_ops(&workspace, &mut ops).unwrap_err();

            assert!(error.to_string().contains("injected Ensure failure"));
            assert_eq!(store.status().unwrap().enabled, enabled);
            assert_eq!(ops.registered(), registered);
        }
    }

    #[test]
    fn rollback_reports_independent_stop_wait_failure_without_restoring_admission() {
        let (_dir, workspace) = configured_runner_fixture();
        let mut ops = FakeSupervisorOps::with_failures(
            false,
            &[InjectedFailure::Ready, InjectedFailure::StopWait],
        );
        let error = start_supervised_with_ops(&workspace, &mut ops).unwrap_err();
        assert!(error.to_string().contains("injected Ready failure"));
        assert!(error.to_string().contains("wait for runner stop"));
        assert!(
            !RunnerStore::open_configured(&workspace)
                .unwrap()
                .status()
                .unwrap()
                .enabled
        );
        assert!(!ops.registered());
    }

    #[test]
    fn rollback_reports_unregister_failure_after_disabling_admission() {
        let (_dir, workspace) = configured_runner_fixture();
        let mut ops = FakeSupervisorOps::with_failures(
            false,
            &[InjectedFailure::Ready, InjectedFailure::Remove],
        );

        let error = start_supervised_with_ops(&workspace, &mut ops).unwrap_err();

        assert!(error.to_string().contains("injected Ready failure"));
        assert!(error.to_string().contains("unregister runner"));
        assert!(
            !RunnerStore::open_configured(&workspace)
                .unwrap()
                .status()
                .unwrap()
                .enabled
        );
        assert!(ops.registered());
    }

    #[test]
    fn stop_keeps_admission_disabled_when_unregister_or_wait_fails() {
        for failure in [InjectedFailure::Remove, InjectedFailure::StopWait] {
            let (_dir, workspace) = configured_runner_fixture();
            let store = RunnerStore::open_configured(&workspace).unwrap();
            store.set_enabled(true).unwrap();
            let mut ops = FakeSupervisorOps::new(true, Some(failure));

            let error = stop_with_ops(&workspace, &mut ops).unwrap_err();

            assert!(error.to_string().contains("injected"));
            assert!(!store.status().unwrap().enabled);
            assert!(!ops.calls().contains(&"enable"));
            assert_eq!(ops.registered(), failure == InjectedFailure::Remove);
        }
    }

    #[test]
    fn stop_reconciles_unregister_write_then_error_without_reenabling() {
        let (_dir, workspace) = configured_runner_fixture();
        let store = RunnerStore::open_configured(&workspace).unwrap();
        store.set_enabled(true).unwrap();
        let mut ops = FakeSupervisorOps::new(true, Some(InjectedFailure::RemoveAfterMutation));

        stop_with_ops(&workspace, &mut ops).expect("durable unregister succeeded before the error");
        assert!(!store.status().unwrap().enabled);
        assert!(!ops.registered(), "a removed runner must stay unregistered");
    }

    #[test]
    fn fresh_stop_without_runner_or_registration_skips_ack_wait() {
        let (_dir, workspace) = configured_runner_fixture();
        let store = RunnerStore::open_configured(&workspace).unwrap();
        std::fs::remove_file(store.path()).unwrap();
        let mut ops = FakeSupervisorOps::new(false, None);

        stop_with_ops(&workspace, &mut ops).unwrap();

        assert!(!ops.calls().contains(&"stopped"));
        assert!(!ops.registered());
    }

    #[test]
    fn disabled_configured_stop_without_supervisor_authority_skips_ack_wait() {
        let (_dir, workspace) = configured_runner_fixture();
        let store = RunnerStore::open_configured(&workspace).unwrap();
        assert!(!store.status().unwrap().enabled);
        let mut ops = FakeSupervisorOps::new(false, None);

        stop_with_ops(&workspace, &mut ops).unwrap();

        assert!(!ops.calls().contains(&"stopped"));
        assert!(!ops.registered());
        assert!(
            !RunnerStore::open_configured(&workspace)
                .unwrap()
                .status()
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn fresh_stop_with_stale_registration_retains_ack_wait() {
        let (_dir, workspace) = configured_runner_fixture();
        let store = RunnerStore::open_configured(&workspace).unwrap();
        std::fs::remove_file(store.path()).unwrap();
        let mut ops = FakeSupervisorOps::new(true, None);

        stop_with_ops(&workspace, &mut ops).unwrap();

        assert!(ops.calls().contains(&"stopped"));
        assert!(!ops.registered());
    }

    #[test]
    fn stale_status_authority_without_registration_retains_ack_wait() {
        let (_dir, workspace) = configured_runner_fixture();
        let mut ops = FakeSupervisorOps::new(false, None).with_runner_authority(true);

        stop_with_ops(&workspace, &mut ops).unwrap();

        assert!(ops.calls().contains(&"authority"));
        assert!(ops.calls().contains(&"stopped"));
        assert!(!ops.registered());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runner_control_lock_serializes_concurrent_start_and_stop() {
        let (_dir, workspace) = configured_runner_fixture();
        let start_ops = FakeSupervisorOps::new(false, None);
        let stop_ops = start_ops.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let start_workspace = workspace.clone();
        let start_ops_in_task = start_ops.clone();
        let start = tokio::spawn(async move {
            let _control = acquire_runner_control_lock(&start_workspace).await.unwrap();
            started_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let mut ops = start_ops_in_task;
            start_supervised_with_ops(&start_workspace, &mut ops)
        });
        started_rx.await.unwrap();

        let stop_entered = Arc::new(AtomicBool::new(false));
        let stop_entered_in_task = Arc::clone(&stop_entered);
        let stop_workspace = workspace.clone();
        let stop = tokio::spawn(async move {
            let _control = acquire_runner_control_lock(&stop_workspace).await.unwrap();
            stop_entered_in_task.store(true, Ordering::Release);
            let mut ops = stop_ops;
            stop_with_ops(&stop_workspace, &mut ops)
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!stop_entered.load(Ordering::Acquire));
        release_tx.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            start.await.unwrap().unwrap();
            stop.await.unwrap().unwrap();
        })
        .await
        .expect("concurrent start/stop control commands must not deadlock");

        let status = RunnerStore::open_configured(&workspace)
            .unwrap()
            .status()
            .unwrap();
        assert!(!status.enabled);
        assert!(stop_entered.load(Ordering::Acquire));
        assert!(!start_ops.registered());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runner_control_lock_serializes_concurrent_starts() {
        let (_dir, workspace) = configured_runner_fixture();
        let first_ops = FakeSupervisorOps::new(false, None);
        let second_ops = first_ops.clone();
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let first_workspace = workspace.clone();
        let first_ops_in_task = first_ops.clone();
        let first = tokio::spawn(async move {
            let _control = acquire_runner_control_lock(&first_workspace).await.unwrap();
            first_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let mut ops = first_ops_in_task;
            start_supervised_with_ops(&first_workspace, &mut ops)
        });
        first_rx.await.unwrap();

        let second_entered = Arc::new(AtomicBool::new(false));
        let second_entered_in_task = Arc::clone(&second_entered);
        let second_workspace = workspace.clone();
        let second = tokio::spawn(async move {
            let _control = acquire_runner_control_lock(&second_workspace)
                .await
                .unwrap();
            second_entered_in_task.store(true, Ordering::Release);
            let mut ops = second_ops;
            start_supervised_with_ops(&second_workspace, &mut ops)
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!second_entered.load(Ordering::Acquire));
        release_tx.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();
        })
        .await
        .expect("concurrent starts must not deadlock");

        let status = RunnerStore::open_configured(&workspace)
            .unwrap()
            .status()
            .unwrap();
        assert!(status.enabled);
        assert!(second_entered.load(Ordering::Acquire));
        assert!(
            first_ops.registered(),
            "successful starts must not leave enabled admission unregistered"
        );
    }
}
