//! Durable, body-free state machine for one unattended agent runner.

use crate::durable::{
    atomic_overwrite, create_lock_acquire_exclusive, open_lock_exclusive, open_lock_shared,
};
use crate::paths::validate_name;
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{
    is_valid_hash, AgentInboxResult, AgentMessage, AgentMessageKind, AGENT_INBOX_MAX_LIMIT,
    AGENT_MESSAGE_MAX_BODY_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

const RUNNER_STATE_SCHEMA_VERSION: u32 = 2;
const RUNNER_INVOCATION_SCHEMA_VERSION: u32 = 1;
const STATE_FILE: &str = "runner-state.json";
const STATE_LOCK: &str = "runner-state.lock";
const LIFETIME_LOCK: &str = "runner-lifetime.lock";
const CONFIGURE_LOCK: &str = "runner-configure.lock";
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PROGRAM_BYTES: usize = 16 * 1024;
const MAX_ARGS: usize = 128;
const MAX_ARG_BYTES: usize = 8 * 1024;
const MAX_ARGV_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 32;
const MAX_COMPLETED: usize = 10_000;
const MAX_FAILURES: u32 = 1_000_000;
const MAX_PROCESS_START_ID_BYTES: usize = 512;
const MAX_AGENT_ENTRIES: usize = 10_000;
const CONFIGURED_READ_ATTEMPTS: usize = 4;
#[cfg(test)]
const TEST_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Trusted runner configuration. Unlike [`RunnerStatus`], this exposes argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub agent: String,
    pub program: PathBuf,
    pub fixed_args: Vec<String>,
    pub timeout_secs: u64,
    pub configured_at_ms: i64,
}

/// Selects which lifecycle is allowed to consume the next queued request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerExecutionMode {
    /// A persisted, enabled runner owned by the background supervisor.
    Supervised,
    /// An explicit terminal-owned run while background execution is disabled.
    Foreground,
}

/// Stable, non-secret runner phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPhase {
    Idle,
    Launching,
    Running,
    NeedsAttention,
}

/// Categorical fail-closed reason; bodies and process output are never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerAttention {
    CursorReset,
    PendingOverflow,
    AmbiguousExecution,
    DeliveryUnknown,
    PreparationFailed,
}

/// Public, redacted runner status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerStatus {
    pub configured: bool,
    pub enabled: bool,
    pub agent: String,
    pub phase: RunnerPhase,
    pub pending_count: usize,
    pub active_message_id: Option<String>,
    pub active_session_id: Option<String>,
    pub active_started_at_ms: Option<i64>,
    pub active_spawned_at_ms: Option<i64>,
    pub last_terminal_kind: Option<AgentMessageKind>,
    pub last_terminal_message_id: Option<String>,
    pub attention: Option<RunnerAttention>,
    pub updated_at_ms: i64,
    pub inbox_failure_count: u32,
}

/// Read-only identity of the configured process group for an active request.
///
/// This is intentionally separate from [`RunnerStatus`]: process metadata is
/// used only for exact local orphan cleanup and is never part of public status
/// output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerProcessMetadata {
    pub pid: u32,
    pub process_start_id: String,
}

/// Result of admitting one inbox read without retaining any message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerAdmission {
    pub admitted: usize,
    pub ignored: usize,
    pub cursor_advanced: bool,
    pub needs_attention: bool,
}

/// Persisted pre-spawn checkpoint returned to the process launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerLaunch {
    pub message_id: String,
    pub session_id: String,
    pub reply_cursor: String,
}

/// Bounded stdin document for the configured child. This type is never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInvocation {
    pub schema_version: u32,
    pub session_id: String,
    pub agent: String,
    pub message: AgentMessage,
}

impl RunnerInvocation {
    pub fn new(launch: &RunnerLaunch, agent: &str, message: AgentMessage) -> Result<Self> {
        validate_name(agent)?;
        validate_session_id(&launch.session_id)?;
        validate_message(&message)?;
        ensure!(
            message.message_id == launch.message_id,
            "runner invocation message does not match the active checkpoint"
        );
        ensure!(
            message.to == agent && message.kind == AgentMessageKind::Request,
            "runner invocation must contain a direct request for the configured agent"
        );
        Ok(Self {
            schema_version: RUNNER_INVOCATION_SCHEMA_VERSION,
            session_id: launch.session_id.clone(),
            agent: agent.to_string(),
            message,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActivePhase {
    Launching,
    Running,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveRequest {
    message_id: String,
    session_id: String,
    phase: ActivePhase,
    reply_cursor: String,
    started_at_ms: i64,
    spawned_at_ms: Option<i64>,
    pid: Option<u32>,
    process_start_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LastTerminal {
    kind: AgentMessageKind,
    message_id: String,
    observed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerRuntime {
    enabled: bool,
    committed_cursor: String,
    staged_cursor: Option<String>,
    pending: Vec<String>,
    active: Option<ActiveRequest>,
    completed_ids: Vec<String>,
    last_terminal: Option<LastTerminal>,
    attention: Option<RunnerAttention>,
    updated_at_ms: i64,
    inbox_failure_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerState {
    schema_version: u32,
    generation_id: String,
    config: RunnerConfig,
    runtime: RunnerRuntime,
}

#[derive(Deserialize)]
struct SchemaProbe {
    schema_version: u32,
}

/// Lock-backed durable runner state for the workspace's sole configured agent.
#[derive(Debug)]
pub struct RunnerStore {
    state_path: PathBuf,
    lock_path: PathBuf,
    identity: RunnerIdentity,
}

/// Owns the exact configured runner lease for one launcher lifecycle.
#[derive(Debug)]
#[must_use = "dropping the execution session releases the runner lifetime lease"]
pub struct RunnerExecutionSession<'store> {
    store: &'store RunnerStore,
    lease: RunnerLifetimeLock,
    mode: RunnerExecutionMode,
}

impl RunnerStore {
    /// Creates the workspace's only runner at the supplied current head.
    pub fn configure(
        base: &Path,
        agent: &str,
        canonical_absolute_program: &Path,
        fixed_args: Vec<String>,
        timeout_secs: u64,
        initial_head_cursor: &str,
    ) -> Result<Self> {
        validate_name(agent)?;
        validate_nonempty_cursor(initial_head_cursor)?;
        validate_program(canonical_absolute_program)?;
        validate_args(&fixed_args)?;
        ensure!(
            (60..=86_400).contains(&timeout_secs),
            "runner timeout must be between 60 and 86400 seconds"
        );
        let _lifecycle_guard = RunnerLifecycleLock::acquire(base)?;
        validate_configured_baseline(base, agent)?;
        ensure!(
            find_configured(base)?.is_none(),
            "an agent runner is already configured; use reconfigure to update it"
        );

        let generation_id = new_generation_id()?;
        let identity = RunnerIdentity::capture(base, agent, &generation_id)?;
        let _lifetime_guard =
            RunnerLifetimeLock::try_acquire_internal(base, agent, true, &generation_id)?;
        let dir = runner_dir_path(base, agent)?;
        let store = Self::at_dir(&dir, identity, true)?;
        let _lock = create_lock_acquire_exclusive(&store.lock_path)?;
        set_private_file(&store.lock_path)?;
        let now = now_ms();
        let state = RunnerState {
            schema_version: RUNNER_STATE_SCHEMA_VERSION,
            generation_id,
            config: RunnerConfig {
                agent: agent.to_string(),
                program: canonical_absolute_program.to_path_buf(),
                fixed_args,
                timeout_secs,
                configured_at_ms: now,
            },
            runtime: RunnerRuntime {
                enabled: false,
                committed_cursor: initial_head_cursor.to_string(),
                staged_cursor: None,
                pending: Vec::new(),
                active: None,
                completed_ids: Vec::new(),
                last_terminal: None,
                attention: None,
                updated_at_ms: now,
                inbox_failure_count: 0,
            },
        };
        validate_state(&state)?;
        store.write_unlocked(&state)?;
        Ok(store)
    }

    /// Updates trusted argv/timeout while preserving all runtime state.
    pub fn reconfigure(
        base: &Path,
        agent: &str,
        canonical_absolute_program: &Path,
        fixed_args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<Self> {
        validate_name(agent)?;
        validate_program(canonical_absolute_program)?;
        validate_args(&fixed_args)?;
        ensure!(
            (60..=86_400).contains(&timeout_secs),
            "runner timeout must be between 60 and 86400 seconds"
        );
        let _lifecycle_guard = RunnerLifecycleLock::acquire(base)?;
        validate_configured_baseline(base, agent)?;
        let configured = find_configured(base)?.context("no agent runner is configured")?;
        ensure!(
            configured == agent,
            "runner '{configured}' is configured; it cannot be replaced by runner '{agent}'"
        );

        let mut store = Self::open_existing(base, agent)?;
        let _lifetime_guard = RunnerLifetimeLock::try_acquire_internal(
            base,
            agent,
            false,
            &store.identity.generation_id,
        )?;
        let _lock = open_lock_exclusive(&store.lock_path)?;
        let mut state = store.load_unlocked()?;
        ensure!(
            !state.runtime.active.as_ref().is_some_and(|active| {
                matches!(active.phase, ActivePhase::Launching | ActivePhase::Running)
            }),
            "cannot reconfigure a runner with a launching or running request"
        );
        let now = now_ms();
        let generation_id = new_generation_id()?;
        state.generation_id.clone_from(&generation_id);
        state.config = RunnerConfig {
            agent: agent.to_string(),
            program: canonical_absolute_program.to_path_buf(),
            fixed_args,
            timeout_secs,
            configured_at_ms: now,
        };
        state.runtime.enabled = false;
        state.runtime.updated_at_ms = now;
        validate_state(&state)?;
        store.write_unlocked(&state)?;
        store.identity = RunnerIdentity::capture(base, agent, &generation_id)?;
        Ok(store)
    }

    /// Opens the sole configured runner without creating state.
    pub fn open_configured(base: &Path) -> Result<Self> {
        let agent = find_configured(base)?.context("no agent runner is configured")?;
        Self::open_existing(base, &agent)
    }

    /// Reads runner status without inventing an unconfigured status record.
    pub fn status_if_configured(base: &Path) -> Result<Option<RunnerStatus>> {
        Ok(read_configured_state(base)?.map(|state| status_from_state(&state)))
    }

    fn at_dir(dir: &Path, identity: RunnerIdentity, create: bool) -> Result<Self> {
        if create {
            ensure_private_dir(dir)?;
        } else {
            ensure_existing_real_dir(dir, "runner state directory")?;
        }
        let state_path = dir.join(STATE_FILE);
        let lock_path = dir.join(STATE_LOCK);
        ensure_regular_or_missing(&state_path, "runner state")?;
        ensure_regular_or_missing(&lock_path, "runner state lock")?;
        if create && !lock_path.exists() {
            let _lock = create_lock_acquire_exclusive(&lock_path)?;
            set_private_file(&lock_path)?;
        }
        ensure!(lock_path.is_file(), "runner state lock is missing");
        Ok(Self {
            state_path,
            lock_path,
            identity,
        })
    }

    fn open_existing(base: &Path, agent: &str) -> Result<Self> {
        validate_agent_layout(base, agent)?;
        let dir = runner_dir_path(base, agent)?;
        ensure_existing_real_dir(&dir, "runner state directory")?;
        let state_path = dir.join(STATE_FILE);
        let lock_path = dir.join(STATE_LOCK);
        ensure_regular_or_missing(&state_path, "runner state")?;
        ensure_regular_or_missing(&lock_path, "runner state lock")?;
        ensure!(lock_path.is_file(), "runner state lock is missing");
        let _lock = open_lock_shared(&lock_path).context("lock runner state while opening")?;
        let state = read_runner_state_unlocked(&state_path)?;
        ensure!(
            state.config.agent == agent,
            "runner state agent '{}' does not match its directory identity '{}'",
            state.config.agent,
            agent
        );
        let identity = RunnerIdentity::capture(base, agent, &state.generation_id)?;
        Ok(Self {
            state_path,
            lock_path,
            identity,
        })
    }

    pub fn path(&self) -> &Path {
        &self.state_path
    }

    pub fn config(&self) -> Result<RunnerConfig> {
        Ok(self.load()?.config)
    }

    pub fn status(&self) -> Result<RunnerStatus> {
        Ok(status_from_state(&self.load()?))
    }

    /// Returns the exact persisted process identity for an active spawned
    /// request. Launching requests and idle runners have no process metadata.
    pub fn active_process_metadata(&self) -> Result<Option<RunnerProcessMetadata>> {
        let state = self.load()?;
        Ok(state.runtime.active.and_then(|active| {
            Some(RunnerProcessMetadata {
                pid: active.pid?,
                process_start_id: active.process_start_id?,
            })
        }))
    }

    /// Acquires the exact configuration lease used by every execution-state
    /// transition. Reacquiring after an interrupted active session marks that
    /// request ambiguous before returning.
    pub fn execution_session(
        &self,
        base: &Path,
        mode: RunnerExecutionMode,
    ) -> Result<RunnerExecutionSession<'_>> {
        let lease = RunnerLifetimeLock::try_acquire_exact_store(base, self)?
            .context("this runner store is not the configured agent for the workspace")?;
        let state = self.load()?;
        validate_execution_mode(mode, state.runtime.enabled)?;
        let session = RunnerExecutionSession {
            store: self,
            lease,
            mode,
        };
        session.store.checkpoint_startup_locked()?;
        Ok(session)
    }

    // State-machine unit tests exercise persistence transitions directly;
    // production callers can reach them only through RunnerExecutionSession.
    #[cfg(test)]
    fn begin_next(&self, mode: RunnerExecutionMode, reply_cursor: &str) -> Result<RunnerLaunch> {
        self.begin_next_locked(mode, reply_cursor)
    }

    #[cfg(test)]
    fn mark_spawned(
        &self,
        message_id: &str,
        pid: u32,
        process_start_id: &str,
    ) -> Result<RunnerStatus> {
        self.mark_spawned_locked(message_id, pid, process_start_id)
    }

    #[cfg(test)]
    fn observe_terminals(
        &self,
        request: &AgentMessage,
        messages: &[AgentMessage],
    ) -> Result<Option<RunnerStatus>> {
        self.observe_terminals_locked(request, messages)
    }

    #[cfg(test)]
    fn checkpoint_startup(&self) -> Result<RunnerStatus> {
        self.checkpoint_startup_locked()
    }

    #[cfg(test)]
    fn record_delivery_unknown(&self, message_id: &str, session_id: &str) -> Result<RunnerStatus> {
        self.record_delivery_unknown_locked(message_id, session_id)
    }

    pub fn committed_cursor(&self) -> Result<String> {
        Ok(self.load()?.runtime.committed_cursor)
    }

    /// Enables background admission only while holding the exact runner lease.
    /// Disabling is intentionally a state-only stop signal so it remains
    /// available to a supervisor whose execution session owns that lease.
    pub fn set_enabled(&self, enabled: bool) -> Result<RunnerStatus> {
        let _control_lease = if enabled {
            Some(self.acquire_control_lease()?)
        } else {
            None
        };
        self.update(|state| {
            state.runtime.enabled = enabled;
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    #[cfg(test)]
    fn admit_inbox(&self, result: &AgentInboxResult) -> Result<RunnerAdmission> {
        let mode = if self.load()?.runtime.enabled {
            RunnerExecutionMode::Supervised
        } else {
            RunnerExecutionMode::Foreground
        };
        self.admit_inbox_locked(mode, result)
    }

    fn admit_inbox_locked(
        &self,
        mode: RunnerExecutionMode,
        result: &AgentInboxResult,
    ) -> Result<RunnerAdmission> {
        validate_inbox(result)?;
        #[cfg(test)]
        pause_inbox_admission_if_requested(&self.identity);
        let mut admission = RunnerAdmission {
            admitted: 0,
            ignored: result.messages.len(),
            cursor_advanced: false,
            needs_attention: false,
        };
        self.update(|state| {
            validate_execution_mode(mode, state.runtime.enabled)?;
            if result.cursor_reset {
                state.runtime.attention = Some(RunnerAttention::CursorReset);
                state.runtime.updated_at_ms = now_ms();
                admission.needs_attention = true;
                return Ok(());
            }
            ensure!(
                state.runtime.attention.is_none(),
                "runner needs attention before more inbox messages can be admitted"
            );

            let mut known: HashSet<String> = state.runtime.pending.iter().cloned().collect();
            known.extend(state.runtime.completed_ids.iter().cloned());
            if let Some(active) = &state.runtime.active {
                known.insert(active.message_id.clone());
            }
            let mut accepted = result
                .messages
                .iter()
                .filter(|message| {
                    message.to == state.config.agent
                        && message.kind == AgentMessageKind::Request
                        && known.insert(message.message_id.clone())
                })
                .collect::<Vec<_>>();
            if state.runtime.pending.len().saturating_add(accepted.len()) > MAX_PENDING {
                state.runtime.attention = Some(RunnerAttention::PendingOverflow);
                state.runtime.updated_at_ms = now_ms();
                admission.needs_attention = true;
                return Ok(());
            }

            accepted.sort_by(|left, right| {
                left.created_at_ms
                    .cmp(&right.created_at_ms)
                    .then_with(|| left.message_id.cmp(&right.message_id))
            });
            admission.admitted = accepted.len();
            admission.ignored = result.messages.len().saturating_sub(accepted.len());
            state.runtime.pending.extend(
                accepted
                    .into_iter()
                    .map(|message| message.message_id.clone()),
            );
            if state.runtime.pending.is_empty() && state.runtime.active.is_none() {
                admission.cursor_advanced = state.runtime.committed_cursor != result.cursor;
                state.runtime.committed_cursor.clone_from(&result.cursor);
                state.runtime.staged_cursor = None;
            } else {
                state.runtime.staged_cursor = Some(result.cursor.clone());
            }
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        Ok(admission)
    }

    /// Persists the launching checkpoint before a caller may spawn a child.
    fn begin_next_locked(
        &self,
        mode: RunnerExecutionMode,
        reply_cursor: &str,
    ) -> Result<RunnerLaunch> {
        validate_cursor(reply_cursor)?;
        let mut launch = None;
        self.update(|state| {
            validate_execution_mode(mode, state.runtime.enabled)?;
            ensure!(
                state.runtime.attention.is_none(),
                "runner needs attention and cannot begin another request"
            );
            ensure!(
                state.runtime.active.is_none(),
                "runner already has an active request"
            );
            let request = state
                .runtime
                .pending
                .first()
                .context("runner has no pending request")?;
            let session_id = new_session_id()?;
            let now = now_ms();
            state.runtime.active = Some(ActiveRequest {
                message_id: request.clone(),
                session_id: session_id.clone(),
                phase: ActivePhase::Launching,
                reply_cursor: reply_cursor.to_string(),
                started_at_ms: now,
                spawned_at_ms: None,
                pid: None,
                process_start_id: None,
            });
            state.runtime.updated_at_ms = now;
            launch = Some(RunnerLaunch {
                message_id: request.clone(),
                session_id,
                reply_cursor: reply_cursor.to_string(),
            });
            Ok(())
        })?;
        launch.context("runner launch checkpoint was not created")
    }

    fn mark_spawned_locked(
        &self,
        message_id: &str,
        pid: u32,
        process_start_id: &str,
    ) -> Result<RunnerStatus> {
        ensure!(
            is_valid_hash(message_id),
            "invalid runner request message id"
        );
        ensure!(pid > 0, "runner child pid must be non-zero");
        ensure!(
            !process_start_id.is_empty()
                && process_start_id.len() <= MAX_PROCESS_START_ID_BYTES
                && !process_start_id.chars().any(char::is_control),
            "runner process start identifier is invalid or too large"
        );
        self.update(|state| {
            let active = state
                .runtime
                .active
                .as_mut()
                .context("runner has no active request")?;
            ensure!(
                active.message_id == message_id,
                "active runner request changed"
            );
            ensure!(
                active.phase == ActivePhase::Launching,
                "runner request is not in the launching phase"
            );
            let now = now_ms();
            active.phase = ActivePhase::Running;
            active.pid = Some(pid);
            active.process_start_id = Some(process_start_id.to_string());
            active.spawned_at_ms = Some(now);
            state.runtime.updated_at_ms = now;
            Ok(())
        })?;
        self.status()
    }

    fn observe_terminals_locked(
        &self,
        request: &AgentMessage,
        messages: &[AgentMessage],
    ) -> Result<Option<RunnerStatus>> {
        validate_message(request)?;
        ensure!(
            messages.len() <= AGENT_INBOX_MAX_LIMIT,
            "runner terminal observation exceeds the inbox bound"
        );
        for message in messages {
            validate_message(message)?;
        }
        let mut completed = false;
        self.update(|state| {
            let active = state
                .runtime
                .active
                .as_ref()
                .context("runner has no active request to correlate a terminal")?;
            ensure!(
                request.message_id == active.message_id,
                "runner terminal observation request does not match the active request"
            );
            ensure!(
                request.to == state.config.agent && request.kind == AgentMessageKind::Request,
                "runner terminal observation requires the active direct request for the configured agent"
            );
            let Some(terminal) = messages.iter().find(|message| {
                message.from == state.config.agent
                    && message.to == request.from
                    && matches!(
                        message.kind,
                        AgentMessageKind::Result | AgentMessageKind::Blocked
                    )
                    && message.reply_to.as_deref() == Some(active.message_id.as_str())
            }) else {
                return Ok(());
            };
            let request_id = active.message_id.clone();
            state.runtime.last_terminal = Some(LastTerminal {
                kind: terminal.kind,
                message_id: terminal.message_id.clone(),
                observed_at_ms: now_ms(),
            });
            touch_completed(&mut state.runtime.completed_ids, &request_id);
            state
                .runtime
                .pending
                .retain(|request| request != &request_id);
            state.runtime.active = None;
            if state.runtime.pending.is_empty() {
                if let Some(cursor) = state.runtime.staged_cursor.take() {
                    state.runtime.committed_cursor = cursor;
                }
            }
            state.runtime.updated_at_ms = now_ms();
            completed = true;
            Ok(())
        })?;
        if completed {
            Ok(Some(self.status()?))
        } else {
            Ok(None)
        }
    }

    #[cfg(test)]
    fn record_inbox_failure(&self) -> Result<RunnerStatus> {
        self.record_inbox_failure_locked()
    }

    fn record_inbox_failure_locked(&self) -> Result<RunnerStatus> {
        self.update(|state| {
            state.runtime.inbox_failure_count = state
                .runtime
                .inbox_failure_count
                .saturating_add(1)
                .min(MAX_FAILURES);
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    #[cfg(test)]
    fn record_inbox_recovery(&self) -> Result<RunnerStatus> {
        self.record_inbox_recovery_locked()
    }

    fn record_inbox_recovery_locked(&self) -> Result<RunnerStatus> {
        self.update(|state| {
            state.runtime.inbox_failure_count = 0;
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    /// Stops before launching a child when refresh preparation cannot be
    /// completed locally. The queued IDs and inbox checkpoints remain intact
    /// for explicit operator inspection and discard.
    fn record_preparation_failed_locked(&self, mode: RunnerExecutionMode) -> Result<RunnerStatus> {
        self.update(|state| {
            validate_execution_mode(mode, state.runtime.enabled)?;
            ensure!(
                !state.runtime.pending.is_empty(),
                "runner has no pending request to preserve after refresh preparation failed"
            );
            ensure!(
                state.runtime.active.is_none(),
                "runner has an active request and cannot record a refresh preparation failure"
            );
            ensure!(
                state.runtime.attention.is_none(),
                "runner already needs attention"
            );
            state.runtime.attention = Some(RunnerAttention::PreparationFailed);
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    /// Fails closed after any restart that finds a possibly executed request.
    fn checkpoint_startup_locked(&self) -> Result<RunnerStatus> {
        self.update(|state| {
            if let Some(active) = &mut state.runtime.active {
                if matches!(active.phase, ActivePhase::Launching | ActivePhase::Running) {
                    active.phase = ActivePhase::Ambiguous;
                    state.runtime.attention = Some(RunnerAttention::AmbiguousExecution);
                    state.runtime.updated_at_ms = now_ms();
                }
            }
            Ok(())
        })?;
        self.status()
    }

    /// Marks a correlated launcher session ambiguous when its terminal signal
    /// could not be observed or published. The active request remains pinned
    /// in place until an explicit disabled reset discards it.
    fn record_delivery_unknown_locked(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<RunnerStatus> {
        ensure!(
            is_valid_hash(message_id),
            "invalid runner request message id"
        );
        validate_session_id(session_id)?;
        self.update(|state| {
            ensure!(
                matches!(
                    state.runtime.attention,
                    None | Some(RunnerAttention::AmbiguousExecution)
                        | Some(RunnerAttention::DeliveryUnknown)
                ),
                "runner already needs unrelated attention"
            );
            let active = state
                .runtime
                .active
                .as_mut()
                .context("runner has no active request")?;
            ensure!(
                active.message_id == message_id && active.session_id == session_id,
                "runner launcher session does not match the active request"
            );
            active.phase = ActivePhase::Ambiguous;
            state.runtime.attention = Some(RunnerAttention::DeliveryUnknown);
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    pub fn reset_to_current_cursor(
        &self,
        cursor: &str,
        discard_pending: bool,
    ) -> Result<RunnerStatus> {
        validate_cursor(cursor)?;
        let _control_lease = self.acquire_control_lease()?;
        self.update(|state| {
            ensure!(
                !state.runtime.enabled,
                "disable the runner before resetting it"
            );
            ensure!(
                discard_pending,
                "reset requires explicit discard_pending=true; ambiguous work cannot be retried"
            );
            state.runtime.committed_cursor = cursor.to_string();
            state.runtime.staged_cursor = None;
            state.runtime.pending.clear();
            state.runtime.active = None;
            state.runtime.attention = None;
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    fn acquire_control_lease(&self) -> Result<RunnerLifetimeLock> {
        RunnerLifetimeLock::try_acquire_exact_store(&self.identity.canonical_workspace, self)?
            .context("this runner store is not the configured agent for the workspace")
    }

    fn load(&self) -> Result<RunnerState> {
        let _lock = open_lock_shared(&self.lock_path).context("lock runner state for reading")?;
        self.load_unlocked()
    }

    fn load_unlocked(&self) -> Result<RunnerState> {
        let state = read_runner_state_unlocked(&self.state_path)?;
        ensure!(
            state.config.agent == self.identity.agent,
            "runner state agent '{}' does not match its directory identity '{}'",
            state.config.agent,
            self.identity.agent
        );
        ensure!(
            state.generation_id == self.identity.generation_id,
            "runner store handle is stale for configuration generation"
        );
        Ok(state)
    }

    fn update(&self, f: impl FnOnce(&mut RunnerState) -> Result<()>) -> Result<()> {
        let _lock = open_lock_exclusive(&self.lock_path).context("lock runner state for update")?;
        let mut state = self.load_unlocked()?;
        f(&mut state)?;
        validate_state(&state)?;
        self.write_unlocked(&state)
    }

    fn write_unlocked(&self, state: &RunnerState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state).context("serialize runner state")?;
        ensure!(
            bytes.len() <= MAX_STATE_BYTES as usize,
            "runner state exceeds its size bound"
        );
        atomic_overwrite(&self.state_path, &bytes).context("commit runner state")?;
        set_private_file(&self.state_path)
    }
}

impl RunnerExecutionSession<'_> {
    fn validate_lease(&self) -> Result<()> {
        self.lease.ensure_store(self.store)
    }

    pub(crate) fn ensure_matches(&self, base: &Path, agent: &str) -> Result<()> {
        self.validate_lease()?;
        self.lease.ensure_matches(base, agent)
    }

    /// Admits one inbox batch only when this exact execution mode remains
    /// enabled at the admission state update's linearization point.
    pub fn admit_inbox(&self, result: &AgentInboxResult) -> Result<RunnerAdmission> {
        self.validate_lease()?;
        self.store.admit_inbox_locked(self.mode, result)
    }

    /// Records a failed inbox poll without changing task lifecycle state.
    pub fn record_inbox_failure(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.record_inbox_failure_locked()
    }

    /// Clears inbox failure accounting without changing task lifecycle state.
    pub fn record_inbox_recovery(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.record_inbox_recovery_locked()
    }

    /// Records a body-free refresh preparation failure before a child launch.
    /// Only this generation-bound execution session may make the transition.
    pub fn record_preparation_failed(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.record_preparation_failed_locked(self.mode)
    }

    /// Persists the pre-spawn checkpoint while retaining the lifetime lease.
    pub fn begin_next(&self, reply_cursor: &str) -> Result<RunnerLaunch> {
        self.validate_lease()?;
        self.store.begin_next_locked(self.mode, reply_cursor)
    }

    /// Records the correlated child process after a successful spawn.
    pub fn mark_spawned(
        &self,
        message_id: &str,
        pid: u32,
        process_start_id: &str,
    ) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store
            .mark_spawned_locked(message_id, pid, process_start_id)
    }

    /// Completes the active direct request only after observing a terminal
    /// signal from the configured agent to its original sender.
    ///
    /// `request` is intentionally supplied in-memory rather than persisted so
    /// runner state remains body-free.
    pub fn observe_terminals(
        &self,
        request: &AgentMessage,
        messages: &[AgentMessage],
    ) -> Result<Option<RunnerStatus>> {
        self.validate_lease()?;
        self.store.observe_terminals_locked(request, messages)
    }

    /// Marks an interrupted active launcher lifecycle ambiguous.
    pub fn checkpoint_startup(&self) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store.checkpoint_startup_locked()
    }

    /// Pins a launcher session whose terminal delivery could not be established.
    pub fn record_delivery_unknown(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<RunnerStatus> {
        self.validate_lease()?;
        self.store
            .record_delivery_unknown_locked(message_id, session_id)
    }
}

/// Read-only workspace status lookup; `None` means no runner is configured.
pub fn runner_status(base: &Path) -> Result<Option<RunnerStatus>> {
    RunnerStore::status_if_configured(base)
}

/// Read-only process identity for exact local runner orphan cleanup.
pub fn runner_process_metadata(base: &Path) -> Result<Option<RunnerProcessMetadata>> {
    Ok(read_configured_state(base)?.and_then(|state| process_metadata_from_state(&state)))
}

/// Removes only the configured runner state after fail-closed lifecycle gates.
pub fn remove_configured(base: &Path, discard_pending: bool) -> Result<()> {
    let _lifecycle_guard = RunnerLifecycleLock::acquire(base)?;
    let agent = find_configured(base)?.context("no agent runner is configured")?;
    let store = RunnerStore::open_existing(base, &agent)?;
    let _lifetime_guard = RunnerLifetimeLock::try_acquire_internal(
        base,
        &agent,
        false,
        &store.identity.generation_id,
    )?;
    let dir = runner_dir_path(base, &agent)?;
    let _state_guard = open_lock_exclusive(&store.lock_path)
        .context("lock configured runner state for removal")?;
    let state = store.load_unlocked()?;
    ensure!(
        !state.runtime.enabled,
        "disable the configured runner before removing it"
    );
    if !discard_pending {
        ensure!(
            state.runtime.pending.is_empty()
                && state.runtime.active.is_none()
                && state.runtime.attention.is_none(),
            "runner has pending, active, or needs-attention work; removal requires explicit discard_pending=true"
        );
    }
    fs::remove_dir_all(&dir).context("remove configured runner state")?;
    if let Some(parent) = dir.parent() {
        if let Ok(parent_dir) = File::open(parent) {
            parent_dir
                .sync_all()
                .context("sync agent state directory after runner removal")?;
        }
    }
    Ok(())
}

impl RunnerState {
    fn phase(&self) -> RunnerPhase {
        if self.runtime.attention.is_some() {
            RunnerPhase::NeedsAttention
        } else {
            match self.runtime.active.as_ref().map(|active| active.phase) {
                Some(ActivePhase::Launching) => RunnerPhase::Launching,
                Some(ActivePhase::Running) => RunnerPhase::Running,
                Some(ActivePhase::Ambiguous) => RunnerPhase::NeedsAttention,
                None => RunnerPhase::Idle,
            }
        }
    }
}

fn status_from_state(state: &RunnerState) -> RunnerStatus {
    let active = state.runtime.active.as_ref();
    let terminal = state.runtime.last_terminal.as_ref();
    RunnerStatus {
        configured: true,
        enabled: state.runtime.enabled,
        agent: state.config.agent.clone(),
        phase: state.phase(),
        pending_count: state.runtime.pending.len(),
        active_message_id: active.map(|item| item.message_id.clone()),
        active_session_id: active.map(|item| item.session_id.clone()),
        active_started_at_ms: active.map(|item| item.started_at_ms),
        active_spawned_at_ms: active.and_then(|item| item.spawned_at_ms),
        last_terminal_kind: terminal.map(|item| item.kind),
        last_terminal_message_id: terminal.map(|item| item.message_id.clone()),
        attention: state.runtime.attention,
        updated_at_ms: state.runtime.updated_at_ms,
        inbox_failure_count: state.runtime.inbox_failure_count,
    }
}

fn process_metadata_from_state(state: &RunnerState) -> Option<RunnerProcessMetadata> {
    state.runtime.active.as_ref().and_then(|active| {
        Some(RunnerProcessMetadata {
            pid: active.pid?,
            process_start_id: active.process_start_id.clone()?,
        })
    })
}

/// Workspace-wide serialization for runner configuration and destructive
/// agent-root lifecycle operations.
#[derive(Debug)]
pub(crate) struct RunnerLifecycleLock {
    _file: File,
}

impl RunnerLifecycleLock {
    pub(crate) async fn acquire_async(base: &Path) -> Result<Self> {
        let base = base.to_path_buf();
        tokio::task::spawn_blocking(move || Self::acquire(&base))
            .await
            .context("join runner lifecycle lock acquisition")?
    }

    pub(crate) fn acquire(base: &Path) -> Result<Self> {
        let (file, _path) = Self::open(base)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if lock_is_contended(&error) => {
                #[cfg(test)]
                notify_lifecycle_lock_contention(&_path);
                fs2::FileExt::lock_exclusive(&file)
                    .context("acquire contended runner lifecycle lock")?;
            }
            Err(error) => return Err(error).context("acquire runner lifecycle lock"),
        }
        Ok(Self { _file: file })
    }

    fn try_acquire(base: &Path) -> Result<Option<Self>> {
        let (file, _path) = Self::open(base)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if lock_is_contended(&error) => Ok(None),
            Err(error) => Err(error).context("acquire runner lifecycle lock"),
        }
    }

    fn open(base: &Path) -> Result<(File, PathBuf)> {
        let agents = crate::workspace_layout::ensure_workspace_state(base)?.join("agents");
        ensure_private_dir(&agents)?;
        let path = agents.join(CONFIGURE_LOCK);
        ensure_regular_or_missing(&path, "runner lifecycle lock")?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).context("open runner lifecycle lock")?;
        set_private_file(&path)?;
        Ok((file, path))
    }
}

fn lock_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

#[cfg(test)]
type TestHookId = u64;

#[cfg(test)]
static NEXT_TEST_HOOK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
fn next_test_hook_id() -> TestHookId {
    let id = NEXT_TEST_HOOK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_ne!(id, 0, "runner test hook id space exhausted");
    id
}

#[cfg(test)]
struct LifecycleContentionHook {
    id: TestHookId,
    path: PathBuf,
    entered: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
static LIFECYCLE_CONTENTION_HOOKS: std::sync::Mutex<Vec<LifecycleContentionHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
struct LifecycleContentionProbe {
    id: TestHookId,
    entered: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl LifecycleContentionProbe {
    fn wait(&self, diagnostic: &str) {
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
fn install_lifecycle_contention_hook(base: &Path) -> Result<LifecycleContentionProbe> {
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
fn notify_lifecycle_lock_contention(path: &Path) {
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
struct TestPauseHook {
    id: TestHookId,
    canonical_base: PathBuf,
    agent: String,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static OPERATION_GUARD_PAUSE_HOOKS: std::sync::Mutex<Vec<TestPauseHook>> =
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
struct TestPause {
    id: TestHookId,
    registry: &'static std::sync::Mutex<Vec<TestPauseHook>>,
    label: &'static str,
    entered: std::sync::mpsc::Receiver<()>,
    release: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(test)]
impl TestPause {
    fn wait(&self, diagnostic: &str) {
        self.wait_with_timeout(diagnostic, TEST_HOOK_TIMEOUT);
    }

    fn wait_with_timeout(&self, diagnostic: &str, timeout: std::time::Duration) {
        self.entered
            .recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("{diagnostic} within {timeout:?}: {error:?}"));
    }

    fn release(&mut self) -> Result<()> {
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
fn install_test_pause(
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
fn take_test_pause(
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
fn wait_for_test_pause_release(hook: TestPauseHook, label: &str, timeout: std::time::Duration) {
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
fn install_operation_guard_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(&OPERATION_GUARD_PAUSE_HOOKS, "operation guard", base, agent)
}

#[cfg(test)]
fn pause_operation_guard_if_requested(base: &Path, agent: &str) {
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
fn install_inbox_admission_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(&INBOX_ADMISSION_PAUSE_HOOKS, "inbox admission", base, agent)
}

#[cfg(test)]
fn pause_inbox_admission_if_requested(identity: &RunnerIdentity) {
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
fn install_status_discovery_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(
        &STATUS_DISCOVERY_PAUSE_HOOKS,
        "status discovery",
        base,
        agent,
    )
}

#[cfg(test)]
fn pause_status_discovery_if_requested(base: &Path, agent: &str) {
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
fn install_status_snapshot_pause(base: &Path, agent: &str) -> Result<TestPause> {
    install_test_pause(&STATUS_SNAPSHOT_PAUSE_HOOKS, "status snapshot", base, agent)
}

#[cfg(test)]
fn pause_status_snapshot_if_requested(base: &Path, agent: &str) {
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

/// Canonical workspace plus agent-root identity carried by a lifetime lease.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunnerIdentity {
    canonical_workspace: PathBuf,
    canonical_agent_root: PathBuf,
    agent: String,
    generation_id: String,
}

impl RunnerIdentity {
    fn capture(base: &Path, agent: &str, generation_id: &str) -> Result<Self> {
        validate_agent_layout(base, agent)?;
        validate_generation_id(generation_id)?;
        Ok(Self {
            canonical_workspace: fs::canonicalize(base)
                .context("canonicalize runner workspace identity")?,
            canonical_agent_root: fs::canonicalize(runner_agent_root(base, agent)?)
                .context("canonicalize runner agent identity")?,
            agent: agent.to_string(),
            generation_id: generation_id.to_string(),
        })
    }
}

/// Nonblocking process-lifetime exclusivity for a configured agent runner.
#[derive(Debug)]
pub(crate) struct RunnerLifetimeLock {
    _file: File,
    identity: RunnerIdentity,
}

impl RunnerLifetimeLock {
    fn try_acquire_exact_store(base: &Path, store: &RunnerStore) -> Result<Option<Self>> {
        validate_name(&store.identity.agent)?;
        let _lifecycle_guard = RunnerLifecycleLock::try_acquire(base)?
            .context("another runner lifecycle operation is already in progress")?;
        if find_configured(base)?.as_deref() != Some(store.identity.agent.as_str()) {
            return Ok(None);
        }
        let lease = Self::try_acquire_internal(
            base,
            &store.identity.agent,
            false,
            &store.identity.generation_id,
        )?;
        ensure!(
            lease.identity == store.identity,
            "runner lifetime lock does not own this runner configuration"
        );
        Ok(Some(lease))
    }

    #[cfg(test)]
    fn try_acquire_configured(base: &Path, agent: &str) -> Result<Option<Self>> {
        validate_name(agent)?;
        let lifecycle_guard = RunnerLifecycleLock::acquire(base)?;
        Self::try_acquire_configured_locked(base, agent, &lifecycle_guard)
    }

    fn try_acquire_configured_locked(
        base: &Path,
        agent: &str,
        _lifecycle_guard: &RunnerLifecycleLock,
    ) -> Result<Option<Self>> {
        if find_configured(base)?.as_deref() != Some(agent) {
            return Ok(None);
        }
        let store = RunnerStore::open_existing(base, agent)?;
        Self::try_acquire_internal(base, agent, false, &store.identity.generation_id).map(Some)
    }

    fn try_acquire_internal(
        base: &Path,
        agent: &str,
        create: bool,
        generation_id: &str,
    ) -> Result<Self> {
        let identity = RunnerIdentity::capture(base, agent, generation_id)?;
        let dir = runner_dir_path(base, agent)?;
        if create {
            ensure_private_dir(&dir)?;
        } else {
            ensure_existing_real_dir(&dir, "runner state directory")?;
        }
        let path = dir.join(LIFETIME_LOCK);
        ensure_regular_or_missing(&path, "runner lifetime lock")?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).context("open runner lifetime lock")?;
        set_private_file(&path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self {
                _file: file,
                identity,
            }),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                bail!("another local runner is already active for agent '{agent}'")
            }
            Err(error) => Err(error).context("acquire runner lifetime lock"),
        }
    }

    pub(crate) fn ensure_matches(&self, base: &Path, agent: &str) -> Result<()> {
        ensure!(
            self.identity.agent == agent,
            "runner lifetime lock does not own agent '{agent}' in this workspace"
        );
        let configured = RunnerStore::open_configured(base)?;
        ensure!(
            configured.identity.agent == agent && self.identity == configured.identity,
            "runner lifetime lock does not own agent '{agent}' in this workspace"
        );
        Ok(())
    }

    fn ensure_store(&self, store: &RunnerStore) -> Result<()> {
        store.load()?;
        ensure!(
            self.identity == store.identity,
            "runner lifetime lock does not own this runner configuration"
        );
        Ok(())
    }
}

/// Full-operation runner exclusion for agent worktree mutations.
#[derive(Debug)]
pub(crate) enum RunnerOperationGuard {
    Configured { _lease: RunnerLifetimeLock },
    Unconfigured { _lifecycle: RunnerLifecycleLock },
}

impl RunnerOperationGuard {
    pub(crate) async fn acquire_async(base: &Path, agent: &str) -> Result<Self> {
        validate_name(agent)?;
        let lifecycle = RunnerLifecycleLock::acquire_async(base).await?;
        let guard = if let Some(lease) =
            RunnerLifetimeLock::try_acquire_configured_locked(base, agent, &lifecycle)?
        {
            drop(lifecycle);
            Self::Configured { _lease: lease }
        } else {
            Self::Unconfigured {
                _lifecycle: lifecycle,
            }
        };
        #[cfg(test)]
        pause_operation_guard_if_requested(base, agent);
        Ok(guard)
    }

    pub(crate) fn protects_configured_runner(&self) -> bool {
        matches!(self, Self::Configured { .. })
    }
}

fn validate_configured_baseline(base: &Path, agent: &str) -> Result<()> {
    ensure!(
        crate::workspace_layout::workspace_is_configured(base),
        "runner configuration requires an initialized FeanorFS workspace"
    );
    let config = crate::local::load_config(base)?;
    ensure!(
        config.format_version == 3,
        "agent runners require a format-v3 workspace"
    );
    validate_agent_layout(base, agent)?;
    let base_ref = runner_agent_root(base, agent)?.join("state/base-snapshot");
    let metadata = fs::symlink_metadata(&base_ref).context("agent base snapshot is missing")?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "agent base snapshot is not a regular file"
    );
    ensure!(
        metadata.len() <= 128,
        "agent base snapshot exceeds its size bound"
    );
    let snapshot = fs::read_to_string(&base_ref).context("read agent base snapshot")?;
    ensure!(
        is_valid_hash(snapshot.trim()),
        "agent base snapshot must contain one full snapshot id"
    );
    Ok(())
}

fn validate_agent_layout(base: &Path, agent: &str) -> Result<()> {
    validate_name(agent)?;
    let agents = crate::workspace_layout::workspace_state_path(base)?.join("agents");
    let root = runner_agent_root(base, agent)?;
    ensure_existing_real_dir(&agents, "agents directory")?;
    ensure_existing_real_dir(&root, "agent root")?;
    ensure_existing_real_dir(&root.join("worktree"), "agent worktree")?;
    ensure_existing_real_dir(&root.join("state"), "agent state directory")?;
    let canonical_agents = fs::canonicalize(&agents)?;
    let canonical_root = fs::canonicalize(&root)?;
    ensure!(
        canonical_root == canonical_agents.join(agent),
        "agent root is reached through a filesystem alias: {}",
        root.display()
    );
    for (path, label) in [
        (root.join("worktree"), "agent worktree"),
        (root.join("state"), "agent state directory"),
    ] {
        ensure!(
            fs::canonicalize(&path)?
                == canonical_root.join(path.file_name().context("agent path has no name")?),
            "{label} is reached through a filesystem alias: {}",
            path.display()
        );
    }
    Ok(())
}

fn runner_agent_root(base: &Path, agent: &str) -> Result<PathBuf> {
    validate_name(agent)?;
    Ok(crate::workspace_layout::workspace_state_path(base)?
        .join("agents")
        .join(agent))
}

fn runner_dir_path(base: &Path, agent: &str) -> Result<PathBuf> {
    Ok(runner_agent_root(base, agent)?.join("state/runner"))
}

fn read_configured_state(base: &Path) -> Result<Option<RunnerState>> {
    for attempt in 0..CONFIGURED_READ_ATTEMPTS {
        let agent = match find_configured(base) {
            Ok(Some(agent)) => agent,
            Ok(None) => return Ok(None),
            Err(error) if attempt + 1 < CONFIGURED_READ_ATTEMPTS && error_is_not_found(&error) => {
                std::thread::yield_now();
                continue;
            }
            Err(error) => return Err(error),
        };
        #[cfg(test)]
        pause_status_discovery_if_requested(base, &agent);
        match read_configured_state_for_agent(base, &agent) {
            Ok(state) => return Ok(Some(state)),
            Err(error) if attempt + 1 < CONFIGURED_READ_ATTEMPTS && error_is_not_found(&error) => {
                std::thread::yield_now();
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("configured runner read loop returns on its final attempt")
}

fn read_configured_state_for_agent(base: &Path, agent: &str) -> Result<RunnerState> {
    validate_agent_layout(base, agent)?;
    let dir = runner_dir_path(base, agent)?;
    ensure_existing_real_dir(&dir, "runner state directory")?;
    let state_path = dir.join(STATE_FILE);
    let lock_path = dir.join(STATE_LOCK);
    ensure_regular_or_missing(&state_path, "runner state")?;
    ensure_regular_or_missing(&lock_path, "runner state lock")?;
    ensure!(lock_path.is_file(), "runner state lock is missing");
    let lock = open_lock_shared(&lock_path).context("lock runner state for status snapshot")?;
    let state = read_runner_state_unlocked(&state_path)?;
    ensure!(
        state.config.agent == agent,
        "runner state agent '{}' does not match its directory identity '{}'",
        state.config.agent,
        agent
    );
    drop(lock);
    #[cfg(test)]
    pause_status_snapshot_if_requested(base, agent);
    Ok(state)
}

fn error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
    })
}

fn find_configured(base: &Path) -> Result<Option<String>> {
    let directory = crate::workspace_layout::workspace_state_path(base)?.join("agents");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("scan configured runners"),
    };
    let mut configured = Vec::new();
    for (index, entry) in entries.enumerate() {
        ensure!(
            index < MAX_AGENT_ENTRIES,
            "agent directory exceeds runner scan bound"
        );
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if validate_name(&name).is_err() {
            continue;
        }
        let state = entry.path().join("state").join("runner").join(STATE_FILE);
        match fs::symlink_metadata(&state) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                configured.push(name)
            }
            Ok(_) => bail!("runner state is not a regular file: {}", state.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect configured runner state"),
        }
    }
    ensure!(
        configured.len() <= 1,
        "multiple configured runners found; only one is allowed per workspace"
    );
    Ok(configured.pop())
}

fn read_runner_state_unlocked(path: &Path) -> Result<RunnerState> {
    let metadata = fs::symlink_metadata(path).context("runner state is missing")?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "runner state is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_STATE_BYTES,
        "runner state exceeds its size bound"
    );
    let bytes = fs::read(path).context("read runner state")?;
    let probe: SchemaProbe = serde_json::from_slice(&bytes).context("parse runner state schema")?;
    ensure!(
        probe.schema_version == RUNNER_STATE_SCHEMA_VERSION,
        "unsupported runner state schema {} (expected {RUNNER_STATE_SCHEMA_VERSION})",
        probe.schema_version
    );
    let state: RunnerState = serde_json::from_slice(&bytes).context("parse runner state")?;
    validate_state(&state)?;
    Ok(state)
}

fn validate_state(state: &RunnerState) -> Result<()> {
    ensure!(
        state.schema_version == RUNNER_STATE_SCHEMA_VERSION,
        "unsupported runner state schema {}",
        state.schema_version
    );
    validate_generation_id(&state.generation_id)?;
    validate_name(&state.config.agent)?;
    validate_program_shape(&state.config.program)?;
    validate_args(&state.config.fixed_args)?;
    ensure!(
        (60..=86_400).contains(&state.config.timeout_secs),
        "runner timeout is outside the supported bound"
    );
    validate_cursor(&state.runtime.committed_cursor)?;
    if let Some(cursor) = &state.runtime.staged_cursor {
        validate_cursor(cursor)?;
    }
    ensure!(
        state.runtime.pending.len() <= MAX_PENDING,
        "too many pending runner requests"
    );
    ensure!(
        state.runtime.completed_ids.len() <= MAX_COMPLETED,
        "too many completed runner request ids"
    );
    ensure!(
        state.runtime.inbox_failure_count <= MAX_FAILURES,
        "runner failure count exceeds its bound"
    );
    ensure!(
        state.runtime.pending.is_empty() == state.runtime.staged_cursor.is_none(),
        "runner pending queue and staged cursor are inconsistent"
    );
    let mut ids = HashSet::new();
    for pending_id in &state.runtime.pending {
        ensure!(is_valid_hash(pending_id), "invalid pending request id");
        ensure!(ids.insert(pending_id), "duplicate pending request id");
    }
    let mut completed = HashSet::new();
    for id in &state.runtime.completed_ids {
        ensure!(is_valid_hash(id), "invalid completed request id");
        ensure!(completed.insert(id), "duplicate completed request id");
        ensure!(
            !ids.contains(id),
            "completed request is still present in the pending queue"
        );
    }
    if let Some(active) = &state.runtime.active {
        ensure!(
            is_valid_hash(&active.message_id),
            "invalid active request id"
        );
        validate_session_id(&active.session_id)?;
        validate_cursor(&active.reply_cursor)?;
        ensure!(
            state
                .runtime
                .pending
                .iter()
                .any(|item| item == &active.message_id),
            "active request is not present in the pending queue"
        );
        ensure!(
            active.process_start_id.as_ref().is_none_or(|id| {
                !id.is_empty()
                    && id.len() <= MAX_PROCESS_START_ID_BYTES
                    && !id.chars().any(char::is_control)
            }),
            "invalid persisted process start identifier"
        );
        match active.phase {
            ActivePhase::Launching => ensure!(
                active.pid.is_none()
                    && active.process_start_id.is_none()
                    && active.spawned_at_ms.is_none(),
                "launching runner has spawned-process metadata"
            ),
            ActivePhase::Running => ensure!(
                active.pid.is_some_and(|pid| pid > 0)
                    && active.process_start_id.is_some()
                    && active.spawned_at_ms.is_some(),
                "running runner lacks spawned-process metadata"
            ),
            ActivePhase::Ambiguous => ensure!(
                matches!(
                    state.runtime.attention,
                    Some(RunnerAttention::AmbiguousExecution | RunnerAttention::DeliveryUnknown)
                ),
                "ambiguous runner request lacks matching attention state"
            ),
        }
    }
    if matches!(
        state.runtime.attention,
        Some(RunnerAttention::AmbiguousExecution | RunnerAttention::DeliveryUnknown)
    ) {
        ensure!(
            state
                .runtime
                .active
                .as_ref()
                .is_some_and(|active| active.phase == ActivePhase::Ambiguous),
            "ambiguous attention lacks active request metadata"
        );
    }
    if state.runtime.attention == Some(RunnerAttention::PreparationFailed) {
        ensure!(
            !state.runtime.pending.is_empty() && state.runtime.active.is_none(),
            "refresh preparation attention must retain an idle pending request"
        );
    }
    if let Some(terminal) = &state.runtime.last_terminal {
        ensure!(
            matches!(
                terminal.kind,
                AgentMessageKind::Result | AgentMessageKind::Blocked
            ),
            "invalid last terminal kind"
        );
        ensure!(
            is_valid_hash(&terminal.message_id),
            "invalid terminal message id"
        );
    }
    Ok(())
}

fn validate_program(program: &Path) -> Result<()> {
    validate_program_shape(program)?;
    let canonical = fs::canonicalize(program).context("canonicalize runner program")?;
    ensure!(
        canonical == program,
        "runner program must already be a canonical absolute path"
    );
    ensure!(
        fs::symlink_metadata(program)?.file_type().is_file(),
        "runner program must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            fs::metadata(program)?.permissions().mode() & 0o111 != 0,
            "runner program must be executable"
        );
    }
    Ok(())
}

fn validate_program_shape(program: &Path) -> Result<()> {
    ensure!(program.is_absolute(), "runner program must be absolute");
    let value = program
        .to_str()
        .context("runner program path must be valid UTF-8")?;
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_PROGRAM_BYTES
            && !value.chars().any(char::is_control),
        "runner program path is invalid or too large"
    );
    Ok(())
}

fn validate_args(args: &[String]) -> Result<()> {
    ensure!(
        args.len() <= MAX_ARGS,
        "runner has too many fixed arguments"
    );
    let mut total = 0usize;
    for arg in args {
        ensure!(
            arg.len() <= MAX_ARG_BYTES,
            "runner fixed argument is too large"
        );
        ensure!(!arg.contains('\0'), "runner fixed argument contains NUL");
        total = total.saturating_add(arg.len());
    }
    ensure!(
        total <= MAX_ARGV_BYTES,
        "runner fixed argv exceeds its size bound"
    );
    Ok(())
}

fn validate_inbox(result: &AgentInboxResult) -> Result<()> {
    validate_cursor(&result.cursor)?;
    ensure!(
        result.messages.len() <= AGENT_INBOX_MAX_LIMIT,
        "runner inbox exceeds the message count bound"
    );
    for message in &result.messages {
        validate_message(message)?;
    }
    Ok(())
}

fn validate_message(message: &AgentMessage) -> Result<()> {
    ensure!(
        is_valid_hash(&message.message_id),
        "invalid agent message id"
    );
    validate_name(&message.from)?;
    ensure!(
        message.to == "*" || feanorfs_common::is_valid_agent_name(&message.to),
        "invalid agent message recipient"
    );
    ensure!(
        message.body.len() <= AGENT_MESSAGE_MAX_BODY_BYTES,
        "agent message body exceeds its bound"
    );
    ensure!(
        is_valid_hash(&message.about_snapshot),
        "invalid message about snapshot"
    );
    if let Some(reply_to) = &message.reply_to {
        ensure!(is_valid_hash(reply_to), "invalid message reply id");
    }
    Ok(())
}

fn validate_cursor(cursor: &str) -> Result<()> {
    ensure!(
        cursor.is_empty() || is_valid_hash(cursor),
        "runner cursor must be empty or a full snapshot id"
    );
    Ok(())
}

fn validate_nonempty_cursor(cursor: &str) -> Result<()> {
    ensure!(
        is_valid_hash(cursor),
        "initial runner cursor must be a full snapshot id"
    );
    Ok(())
}

fn validate_execution_mode(mode: RunnerExecutionMode, enabled: bool) -> Result<()> {
    match mode {
        RunnerExecutionMode::Supervised => {
            ensure!(enabled, "supervised runner execution requires enabled=true");
        }
        RunnerExecutionMode::Foreground => {
            ensure!(
                !enabled,
                "foreground runner execution requires enabled=false"
            );
        }
    }
    Ok(())
}

fn validate_generation_id(generation_id: &str) -> Result<()> {
    ensure!(
        generation_id.len() == 32
            && generation_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid runner configuration generation"
    );
    Ok(())
}

fn validate_session_id(session_id: &str) -> Result<()> {
    ensure!(
        session_id.len() == 32
            && session_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid runner session id"
    );
    Ok(())
}

fn new_session_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate runner session id")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn new_generation_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate runner configuration generation")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn touch_completed(completed: &mut Vec<String>, request_id: &str) {
    completed.retain(|id| id != request_id);
    completed.push(request_id.to_string());
    if completed.len() > MAX_COMPLETED {
        completed.drain(..completed.len() - MAX_COMPLETED);
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).context("create private runner state directory")?;
    let metadata = fs::symlink_metadata(path).context("inspect runner state directory")?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "runner state path is not a regular directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_existing_real_dir(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "{label} is not a real directory: {}",
        path.display()
    );
    Ok(())
}

fn set_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn ensure_regular_or_missing(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => bail!("{label} is not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {label}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
            })
            .unwrap_or_else(|| "non-string panic payload".to_string())
    }

    fn prepare_baseline(base: &Path, agent: &str, format_version: u32) {
        crate::local::save_config(
            base,
            &crate::local::Config {
                server_url: "http://127.0.0.1:1".into(),
                workspace_id: "runner-test".into(),
                encryption_password: Some("e".repeat(64)),
                server_password: None,
                tls_ca_pem: None,
                format_version,
                hub_local: false,
                relay: None,
            },
        )
        .unwrap();
        let root = runner_agent_root(base, agent).unwrap();
        fs::create_dir_all(root.join("worktree")).unwrap();
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("state/base-snapshot"), id('f')).unwrap();
    }

    fn setup_disabled() -> (tempfile::TempDir, RunnerStore) {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let store = RunnerStore::configure(
            base.path(),
            "worker",
            &program,
            vec!["--fixed".into()],
            3600,
            &id('a'),
        )
        .unwrap();
        (base, store)
    }

    fn setup() -> (tempfile::TempDir, RunnerStore) {
        let (base, store) = setup_disabled();
        store.set_enabled(true).unwrap();
        (base, store)
    }

    fn message(
        message_id: char,
        from: &str,
        to: &str,
        kind: AgentMessageKind,
        reply_to: Option<char>,
    ) -> AgentMessage {
        AgentMessage {
            message_id: id(message_id),
            from: from.into(),
            to: to.into(),
            kind,
            body: "private task body".into(),
            about_snapshot: id('f'),
            reply_to: reply_to.map(id),
            created_at_ms: message_id as i64,
        }
    }

    fn inbox(cursor: char, messages: Vec<AgentMessage>) -> AgentInboxResult {
        AgentInboxResult {
            cursor: id(cursor),
            cursor_reset: false,
            messages,
        }
    }

    #[test]
    fn baseline_cursor_and_single_configuration_guards() {
        let (base, store) = setup_disabled();
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        assert!(!store.status().unwrap().enabled);
        assert!(RunnerStore::open_configured(base.path()).is_ok());
        assert_eq!(
            runner_status(base.path()).unwrap(),
            Some(store.status().unwrap())
        );
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert!(
            RunnerStore::configure(base.path(), "worker", &program, vec![], 60, &id('b'))
                .unwrap_err()
                .to_string()
                .contains("use reconfigure")
        );
        prepare_baseline(base.path(), "other", 3);
        assert!(
            RunnerStore::configure(base.path(), "other", &program, vec![], 60, &id('b'))
                .unwrap_err()
                .to_string()
                .contains("already configured")
        );

        let unconfigured = tempfile::tempdir().unwrap();
        assert_eq!(runner_status(unconfigured.path()).unwrap(), None);
    }

    #[test]
    fn configured_only_lease_does_not_create_state_for_unconfigured_agents() {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        prepare_baseline(base.path(), "other", 3);
        let worker_runner = runner_dir_path(base.path(), "worker").unwrap();
        let other_runner = runner_dir_path(base.path(), "other").unwrap();

        assert!(
            RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
                .unwrap()
                .is_none()
        );
        assert!(!worker_runner.exists());
        assert!(!other_runner.exists());

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let _store =
            RunnerStore::configure(base.path(), "worker", &program, Vec::new(), 3600, &id('a'))
                .unwrap();
        assert!(
            RunnerLifetimeLock::try_acquire_configured(base.path(), "other")
                .unwrap()
                .is_none()
        );
        assert!(!other_runner.exists());
        assert!(
            RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn execution_session_owns_exact_lease_and_reacquire_fails_closed() {
        let (base, store) = setup();
        let (other_base, _other_store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert!(RunnerLifetimeLock::try_acquire_configured(base.path(), "worker").is_err());
        assert!(store
            .execution_session(other_base.path(), RunnerExecutionMode::Supervised)
            .unwrap_err()
            .to_string()
            .contains("configuration"));

        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, 42, "session-owned-child")
            .unwrap();
        session
            .observe_terminals(
                &request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        store
            .admit_inbox(&inbox(
                'c',
                vec![message(
                    '3',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        session.begin_next(&id('c')).unwrap();
        drop(session);

        let resumed = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert_eq!(
            resumed.checkpoint_startup().unwrap().attention,
            Some(RunnerAttention::AmbiguousExecution)
        );
        assert!(resumed.begin_next(&id('d')).is_err());
    }

    #[test]
    fn disabled_live_session_blocks_reset_until_drop() {
        let (base, store) = setup();
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, 42, "live-reset-child")
            .unwrap();
        store.set_enabled(false).unwrap();
        let checkpoint = store.status().unwrap();

        let error = store.reset_to_current_cursor(&id('c'), true).unwrap_err();
        assert!(error.to_string().contains("already active"));
        assert_eq!(store.status().unwrap(), checkpoint);

        drop(session);
        let reset = store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(reset.phase, RunnerPhase::Idle);
        assert_eq!(reset.pending_count, 0);
        assert!(reset.active_message_id.is_none());
        assert!(reset.attention.is_none());
        assert_eq!(store.committed_cursor().unwrap(), id('c'));
    }

    #[test]
    fn disable_preserves_supervised_checkpoint_and_allows_terminal_observation() {
        let (base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox(
                'b',
                vec![
                    request.clone(),
                    message('2', "human", "worker", AgentMessageKind::Request, None),
                ],
            ))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();

        let disabled = store.set_enabled(false).unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.pending_count, 2);
        assert_eq!(
            disabled.active_message_id.as_deref(),
            Some(launch.message_id.as_str())
        );
        session
            .mark_spawned(&launch.message_id, 42, "disabled-session-child")
            .unwrap();
        assert_eq!(
            runner_process_metadata(base.path()).unwrap(),
            Some(RunnerProcessMetadata {
                pid: 42,
                process_start_id: "disabled-session-child".to_string(),
            })
        );
        let terminal = session
            .observe_terminals(
                &request,
                &[message(
                    '3',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap()
            .unwrap();
        assert!(!terminal.enabled);
        assert_eq!(terminal.pending_count, 1);
        assert!(terminal.active_message_id.is_none());
        assert_eq!(runner_process_metadata(base.path()).unwrap(), None);

        let error = session.begin_next(&id('c')).unwrap_err();
        assert!(error.to_string().contains("enabled=true"));
        assert_eq!(store.status().unwrap().pending_count, 1);
    }

    #[test]
    fn disable_linearizes_before_in_flight_supervised_admission() {
        let (base, store) = setup();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let batch = inbox(
            'b',
            vec![message(
                '1',
                "human",
                "worker",
                AgentMessageKind::Request,
                None,
            )],
        );
        let mut admission_pause = install_inbox_admission_pause(base.path(), "worker").unwrap();

        std::thread::scope(|scope| {
            let admission = scope.spawn(|| session.admit_inbox(&batch));
            admission_pause.wait("session admission did not reach its pre-update boundary");
            let disabled = store.set_enabled(false).unwrap();
            assert!(!disabled.enabled);
            let after_disable = store.status().unwrap();
            let cursor_after_disable = store.committed_cursor().unwrap();
            let state_after_disable = fs::read(store.path()).unwrap();

            admission_pause.release().unwrap();
            let error = admission.join().unwrap().unwrap_err();
            assert!(error.to_string().contains("enabled=true"));
            assert_eq!(store.status().unwrap(), after_disable);
            assert_eq!(store.committed_cursor().unwrap(), cursor_after_disable);
            assert_eq!(fs::read(store.path()).unwrap(), state_after_disable);
        });
    }

    #[test]
    fn production_inbox_admission_is_session_bound_and_mode_checked() {
        let (supervised_base, supervised_store) = setup();
        assert!(supervised_store
            .execution_session(supervised_base.path(), RunnerExecutionMode::Foreground)
            .is_err());
        let supervised = supervised_store
            .execution_session(supervised_base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert_eq!(
            supervised
                .admit_inbox(&inbox(
                    'b',
                    vec![message(
                        '1',
                        "human",
                        "worker",
                        AgentMessageKind::Request,
                        None,
                    )],
                ))
                .unwrap()
                .admitted,
            1
        );

        let (foreground_base, foreground_store) = setup_disabled();
        assert!(foreground_store
            .execution_session(foreground_base.path(), RunnerExecutionMode::Supervised)
            .is_err());
        let foreground = foreground_store
            .execution_session(foreground_base.path(), RunnerExecutionMode::Foreground)
            .unwrap();
        assert_eq!(
            foreground
                .admit_inbox(&inbox(
                    'c',
                    vec![message(
                        '2',
                        "human",
                        "worker",
                        AgentMessageKind::Request,
                        None,
                    )],
                ))
                .unwrap()
                .admitted,
            1
        );
    }

    #[test]
    fn foreground_execution_session_blocks_enable_until_drop() {
        let (base, store) = setup_disabled();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Foreground)
            .unwrap();

        let error = store.set_enabled(true).unwrap_err();
        assert!(error.to_string().contains("already active"));
        assert!(!store.status().unwrap().enabled);

        drop(session);
        assert!(store.set_enabled(true).unwrap().enabled);
    }

    #[test]
    fn stale_store_cannot_apply_control_mutations_to_a_recreated_configuration() {
        let (base, stale) = setup_disabled();
        let stale_generation = stale.identity.generation_id.clone();
        remove_configured(base.path(), false).unwrap();
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let fresh =
            RunnerStore::configure(base.path(), "worker", &program, Vec::new(), 3600, &id('b'))
                .unwrap();
        assert_ne!(fresh.identity.generation_id, stale_generation);
        for error in [
            stale.set_enabled(true).unwrap_err(),
            stale.set_enabled(false).unwrap_err(),
            stale.reset_to_current_cursor(&id('c'), true).unwrap_err(),
        ] {
            assert!(error.to_string().contains("stale"));
        }
        assert!(!fresh.status().unwrap().enabled);
    }

    #[tokio::test]
    async fn removal_requires_disabled_explicit_discard_and_preserves_agent_state() {
        let (base, store) = setup();
        let root = runner_agent_root(base.path(), "worker").unwrap();
        fs::write(root.join("worktree/keep.txt"), b"worktree").unwrap();
        fs::create_dir_all(root.join("state/runtime")).unwrap();
        fs::write(root.join("state/runtime/keep"), b"runtime").unwrap();

        let enabled_error = remove_configured(base.path(), true).unwrap_err();
        assert!(enabled_error.to_string().contains("disable"));
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        store.set_enabled(false).unwrap();
        let pending_error = remove_configured(base.path(), false).unwrap_err();
        assert!(pending_error.to_string().contains("discard_pending=true"));

        remove_configured(base.path(), true).unwrap();
        assert_eq!(runner_status(base.path()).unwrap(), None);
        assert!(!root.join("state/runner").exists());
        assert_eq!(
            fs::read(root.join("worktree/keep.txt")).unwrap(),
            b"worktree"
        );
        assert!(root.join("state/base-snapshot").is_file());
        assert_eq!(
            fs::read(root.join("state/runtime/keep")).unwrap(),
            b"runtime"
        );

        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        crate::agent::clean_agent(base.path(), &db, "worker")
            .await
            .unwrap();
        assert!(
            !root.exists(),
            "ordinary cleanup is unblocked after removal"
        );
    }

    #[test]
    fn status_and_process_metadata_tolerate_concurrent_runner_removal() {
        let (status_base, _status_store) = setup_disabled();
        let mut status_pause =
            install_status_discovery_pause(status_base.path(), "worker").unwrap();
        let status_base_path = status_base.path().to_path_buf();
        let status_reader = std::thread::spawn(move || runner_status(&status_base_path));
        status_pause.wait("runner status did not finish configuration discovery");
        remove_configured(status_base.path(), false).unwrap();
        status_pause.release().unwrap();
        assert_eq!(status_reader.join().unwrap().unwrap(), None);

        let (process_base, _process_store) = setup_disabled();
        let mut process_pause =
            install_status_discovery_pause(process_base.path(), "worker").unwrap();
        let process_base_path = process_base.path().to_path_buf();
        let process_reader =
            std::thread::spawn(move || runner_process_metadata(&process_base_path));
        process_pause.wait("runner process metadata did not finish configuration discovery");
        remove_configured(process_base.path(), false).unwrap();
        process_pause.release().unwrap();
        assert_eq!(process_reader.join().unwrap().unwrap(), None);
    }

    #[test]
    fn status_returns_one_atomic_snapshot_during_reconfigure() {
        let (base, store) = setup_disabled();
        let before = store.status().unwrap();
        let mut status_pause = install_status_snapshot_pause(base.path(), "worker").unwrap();
        let base_path = base.path().to_path_buf();
        let status_reader = std::thread::spawn(move || runner_status(&base_path));
        status_pause.wait("runner status did not capture its configuration snapshot");

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let reconfigured = RunnerStore::reconfigure(
            base.path(),
            "worker",
            &program,
            vec!["--replacement".into()],
            7200,
        )
        .unwrap();
        status_pause.release().unwrap();

        assert_eq!(status_reader.join().unwrap().unwrap(), Some(before));
        assert_eq!(
            runner_status(base.path()).unwrap(),
            Some(reconfigured.status().unwrap())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_clean_contention_keeps_the_executor_responsive() {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();

        let (acquired_sender, acquired_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let holder_base = base.path().to_path_buf();
        let holder = std::thread::spawn(move || {
            let _guard = RunnerLifecycleLock::acquire(&holder_base).unwrap();
            acquired_sender.send(()).unwrap();
            release_receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
        });
        acquired_receiver
            .recv_timeout(TEST_HOOK_TIMEOUT)
            .expect("lifecycle holder did not acquire the lock");

        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            release_sender.send(()).is_ok()
        });
        // The holder's one-second watchdog proves executor responsiveness.
        // This outer timeout only bounds cleanup after the lock is released,
        // so use the standard hook allowance for loaded CI filesystems.
        let cleaned = tokio::time::timeout(
            TEST_HOOK_TIMEOUT,
            crate::agent::clean_agent(base.path(), &db, "worker"),
        )
        .await;

        assert!(
            release.await.unwrap(),
            "executor did not run the lock releaser"
        );
        assert!(
            holder.join().unwrap(),
            "lifecycle holder reached its watchdog instead of the async release"
        );
        cleaned
            .expect("clean blocked the current-thread executor")
            .unwrap();
    }

    #[test]
    fn removal_refuses_an_active_lifetime_lease() {
        let (base, _store) = setup_disabled();
        let lease = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
            .unwrap()
            .unwrap();
        let error = remove_configured(base.path(), true).unwrap_err();
        assert!(error.to_string().contains("already active"));
        assert!(runner_dir_path(base.path(), "worker").unwrap().is_dir());
        drop(lease);
        remove_configured(base.path(), false).unwrap();
        assert_eq!(runner_status(base.path()).unwrap(), None);
    }

    #[test]
    fn same_path_lifecycle_probes_are_independent_and_share_contention() {
        let base = tempfile::tempdir().unwrap();
        let held = RunnerLifecycleLock::acquire(base.path()).unwrap();
        let discarded = install_lifecycle_contention_hook(base.path()).unwrap();
        let second = install_lifecycle_contention_hook(base.path()).unwrap();
        let third = install_lifecycle_contention_hook(base.path()).unwrap();
        drop(discarded);
        let base_path = base.path().to_path_buf();
        let contender = std::thread::spawn(move || RunnerLifecycleLock::acquire(&base_path));

        second.wait("second same-path lifecycle probe was not notified");
        third.wait("third same-path lifecycle probe was not notified");
        drop(held);
        drop(contender.join().unwrap().unwrap());
    }

    #[test]
    fn same_key_operation_pauses_are_consumed_one_at_a_time() {
        let base = tempfile::tempdir().unwrap();
        let mut first = install_operation_guard_pause(base.path(), "worker").unwrap();
        let mut second = install_operation_guard_pause(base.path(), "worker").unwrap();

        std::thread::scope(|scope| {
            let first_worker =
                scope.spawn(|| pause_operation_guard_if_requested(base.path(), "worker"));
            first.wait("first same-key operation pause was not entered");
            assert_eq!(
                second.entered.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty),
                "one worker must not consume both same-key pauses"
            );
            first.release().unwrap();
            first_worker.join().unwrap();

            let second_worker =
                scope.spawn(|| pause_operation_guard_if_requested(base.path(), "worker"));
            second.wait("second same-key operation pause was not entered");
            second.release().unwrap();
            second_worker.join().unwrap();
        });
    }

    #[test]
    fn same_key_inbox_pause_drop_removes_only_its_token() {
        let (base, store) = setup_disabled();
        let first = install_inbox_admission_pause(base.path(), "worker").unwrap();
        let mut second = install_inbox_admission_pause(base.path(), "worker").unwrap();
        drop(first);
        let identity = store.identity.clone();

        std::thread::scope(|scope| {
            let worker = scope.spawn(|| pause_inbox_admission_if_requested(&identity));
            second.wait("remaining same-key inbox pause was not entered");
            second.release().unwrap();
            worker.join().unwrap();
        });
    }

    #[test]
    fn pause_timeout_and_disconnect_fail_loudly_and_cleanup_owner() {
        let base = tempfile::tempdir().unwrap();
        let pause = install_operation_guard_pause(base.path(), "worker").unwrap();
        let pause_id = pause.id;
        let observer_timeout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            pause.wait_with_timeout(
                "simulated operation pause observer timeout",
                std::time::Duration::from_millis(10),
            );
        }))
        .unwrap_err();
        assert!(panic_text(observer_timeout).contains("simulated operation pause observer timeout"));
        assert!(
            !OPERATION_GUARD_PAUSE_HOOKS
                .lock()
                .unwrap()
                .iter()
                .any(|hook| hook.id == pause_id),
            "observer timeout must remove its exact unconsumed hook"
        );

        let pause = install_operation_guard_pause(base.path(), "worker").unwrap();
        let paused_id = pause.id;
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| pause_operation_guard_if_requested(base.path(), "worker"));
            pause.wait("worker did not enter before simulated observer panic");
            let observer_panic =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let _pause = pause;
                    panic!("simulated observer panic after worker entry");
                }));
            assert!(panic_text(observer_panic.unwrap_err()).contains("simulated observer panic"));
            worker.join().unwrap();
        });
        assert!(
            !OPERATION_GUARD_PAUSE_HOOKS
                .lock()
                .unwrap()
                .iter()
                .any(|hook| hook.id == paused_id),
            "observer unwind must release its worker and remove its exact hook"
        );

        let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        drop(release_sender);
        let disconnected = TestPauseHook {
            id: next_test_hook_id(),
            canonical_base: fs::canonicalize(base.path()).unwrap(),
            agent: "worker".to_string(),
            entered: entered_sender,
            release: release_receiver,
        };
        let worker_disconnect = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for_test_pause_release(
                disconnected,
                "disconnect proof",
                std::time::Duration::from_millis(10),
            );
        }))
        .unwrap_err();
        entered_receiver.recv().unwrap();
        let disconnect_text = panic_text(worker_disconnect);
        assert!(disconnect_text.contains("paused worker"));
        assert!(disconnect_text.contains("Disconnected"));

        let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
        let (_release_sender, release_receiver) = std::sync::mpsc::channel();
        let timed_out = TestPauseHook {
            id: next_test_hook_id(),
            canonical_base: fs::canonicalize(base.path()).unwrap(),
            agent: "worker".to_string(),
            entered: entered_sender,
            release: release_receiver,
        };
        let worker_timeout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for_test_pause_release(
                timed_out,
                "timeout proof",
                std::time::Duration::from_millis(10),
            );
        }))
        .unwrap_err();
        entered_receiver.recv().unwrap();
        let timeout_text = panic_text(worker_timeout);
        assert!(timeout_text.contains("paused worker"));
        assert!(timeout_text.contains("Timeout"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_lock_blocks_actual_clean_until_destructive_window_opens() {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let root = runner_agent_root(base.path(), "worker").unwrap();
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let held = RunnerLifecycleLock::acquire(base.path()).unwrap();
        let attempted = install_lifecycle_contention_hook(base.path()).unwrap();
        let base_path = base.path().to_path_buf();
        let clean =
            tokio::spawn(async move { crate::agent::clean_agent(&base_path, &db, "worker").await });

        attempted.wait("clean did not reach the contended lifecycle lock");
        assert!(
            !clean.is_finished(),
            "contended clean cannot pass the held lock"
        );
        assert!(
            root.is_dir(),
            "clean cannot mutate the agent root while blocked"
        );
        drop(held);
        clean.await.unwrap().unwrap();
        assert!(
            !root.exists(),
            "clean completes after the lifecycle window opens"
        );
    }

    #[test]
    fn configure_requires_format_three_real_agent_and_full_refs() {
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let unconfigured = tempfile::tempdir().unwrap();
        assert!(RunnerStore::configure(
            unconfigured.path(),
            "worker",
            &program,
            vec![],
            60,
            &id('a')
        )
        .is_err());

        let legacy = tempfile::tempdir().unwrap();
        prepare_baseline(legacy.path(), "worker", 2);
        assert!(
            RunnerStore::configure(legacy.path(), "worker", &program, vec![], 60, &id('a'))
                .unwrap_err()
                .to_string()
                .contains("format-v3")
        );

        let missing_ref = tempfile::tempdir().unwrap();
        prepare_baseline(missing_ref.path(), "worker", 3);
        fs::remove_file(
            runner_agent_root(missing_ref.path(), "worker")
                .unwrap()
                .join("state/base-snapshot"),
        )
        .unwrap();
        assert!(RunnerStore::configure(
            missing_ref.path(),
            "worker",
            &program,
            vec![],
            60,
            &id('a')
        )
        .is_err());

        let invalid_ref = tempfile::tempdir().unwrap();
        prepare_baseline(invalid_ref.path(), "worker", 3);
        fs::write(
            runner_agent_root(invalid_ref.path(), "worker")
                .unwrap()
                .join("state/base-snapshot"),
            "not-a-hash",
        )
        .unwrap();
        assert!(RunnerStore::configure(
            invalid_ref.path(),
            "worker",
            &program,
            vec![],
            60,
            &id('a')
        )
        .is_err());

        let empty_cursor = tempfile::tempdir().unwrap();
        prepare_baseline(empty_cursor.path(), "worker", 3);
        assert!(
            RunnerStore::configure(empty_cursor.path(), "worker", &program, vec![], 60, "")
                .is_err()
        );
    }

    #[test]
    fn direct_requests_only_and_empty_or_ignored_reads_advance() {
        let (_base, store) = setup();
        let result = inbox(
            'b',
            vec![
                message('1', "human", "worker", AgentMessageKind::Request, None),
                message('2', "human", "*", AgentMessageKind::Request, None),
                message('3', "human", "worker", AgentMessageKind::Status, None),
                message('4', "human", "other", AgentMessageKind::Request, None),
            ],
        );
        let admitted = store.admit_inbox(&result).unwrap();
        assert_eq!(admitted.admitted, 1);
        assert_eq!(store.status().unwrap().pending_count, 1);
        store.set_enabled(false).unwrap();
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        let ignored = store
            .admit_inbox(&inbox(
                'd',
                vec![message('5', "human", "*", AgentMessageKind::Status, None)],
            ))
            .unwrap();
        assert!(ignored.cursor_advanced);
        assert_eq!(store.committed_cursor().unwrap(), id('d'));
        let empty = store.admit_inbox(&inbox('e', vec![])).unwrap();
        assert!(empty.cursor_advanced);
        let unchanged = store.admit_inbox(&inbox('e', vec![])).unwrap();
        assert!(!unchanged.cursor_advanced);
    }

    #[test]
    fn execution_modes_require_matching_persisted_enablement() {
        let (_foreground_base, foreground) = setup_disabled();
        foreground
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let supervised_error = foreground
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap_err();
        assert!(supervised_error.to_string().contains("enabled=true"));
        assert!(foreground.status().unwrap().active_message_id.is_none());
        foreground
            .begin_next(RunnerExecutionMode::Foreground, &id('b'))
            .unwrap();

        let (_supervised_base, supervised) = setup();
        supervised
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '2',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let foreground_error = supervised
            .begin_next(RunnerExecutionMode::Foreground, &id('b'))
            .unwrap_err();
        assert!(foreground_error.to_string().contains("enabled=false"));
        assert!(supervised.status().unwrap().active_message_id.is_none());
        supervised
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
    }

    #[test]
    fn batch_cursor_advances_only_after_all_correlated_terminals() {
        let (_base, store) = setup();
        let second_request = message('2', "human", "worker", AgentMessageKind::Request, None);
        let first_request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox(
                'b',
                vec![second_request.clone(), first_request.clone()],
            ))
            .unwrap();
        let first = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .mark_spawned(&first.message_id, 42, "start-1")
            .unwrap();
        store
            .observe_terminals(
                &first_request,
                &[message(
                    '3',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let second = store
            .begin_next(RunnerExecutionMode::Supervised, &id('c'))
            .unwrap();
        assert_eq!(second.message_id, id('2'));
        store
            .observe_terminals(
                &second_request,
                &[message(
                    '4',
                    "worker",
                    "human",
                    AgentMessageKind::Blocked,
                    Some('2'),
                )],
            )
            .unwrap();
        assert_eq!(store.committed_cursor().unwrap(), id('b'));
        assert_eq!(
            store.status().unwrap().last_terminal_kind,
            Some(AgentMessageKind::Blocked)
        );
    }

    #[test]
    fn completed_ids_dedupe_replayed_requests() {
        let (_base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .observe_terminals(
                &request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        let replay = store.admit_inbox(&inbox('c', vec![request])).unwrap();
        assert_eq!(replay.admitted, 0);
        assert_eq!(store.status().unwrap().pending_count, 0);
        assert_eq!(store.committed_cursor().unwrap(), id('c'));
    }

    #[test]
    fn reconfigure_updates_only_configuration_and_disables() {
        let (base, store) = setup();
        let completed = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![completed.clone()]))
            .unwrap();
        store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .observe_terminals(
                &completed,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        store
            .admit_inbox(&inbox(
                'c',
                vec![message(
                    '3',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let reset = AgentInboxResult {
            cursor: id('d'),
            cursor_reset: true,
            messages: Vec::new(),
        };
        store.admit_inbox(&reset).unwrap();
        let before = store.status().unwrap();
        let cursor_before = store.committed_cursor().unwrap();

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let replaced = RunnerStore::reconfigure(
            base.path(),
            "worker",
            &program,
            vec!["--replacement".into()],
            7200,
        )
        .unwrap();
        let after = replaced.status().unwrap();
        assert!(!after.enabled);
        assert_eq!(after.pending_count, before.pending_count);
        assert_eq!(after.attention, before.attention);
        assert_eq!(after.last_terminal_kind, before.last_terminal_kind);
        assert_eq!(
            after.last_terminal_message_id,
            before.last_terminal_message_id
        );
        assert_eq!(replaced.committed_cursor().unwrap(), cursor_before);
        assert_eq!(replaced.config().unwrap().fixed_args, ["--replacement"]);
        assert_eq!(replaced.config().unwrap().timeout_secs, 7200);
        replaced.set_enabled(true).unwrap();
        replaced.set_enabled(false).unwrap();
        replaced.reset_to_current_cursor(&id('f'), true).unwrap();
        replaced.set_enabled(true).unwrap();
        assert_eq!(
            replaced
                .admit_inbox(&inbox('9', vec![completed]))
                .unwrap()
                .admitted,
            0,
            "reconfigure must preserve the completed-request ledger"
        );
    }

    #[test]
    fn reconfigure_refuses_launching_or_running_work() {
        for running in [false, true] {
            let (base, store) = setup();
            store
                .admit_inbox(&inbox(
                    'b',
                    vec![message(
                        '1',
                        "human",
                        "worker",
                        AgentMessageKind::Request,
                        None,
                    )],
                ))
                .unwrap();
            let launch = store
                .begin_next(RunnerExecutionMode::Supervised, &id('b'))
                .unwrap();
            if running {
                store
                    .mark_spawned(&launch.message_id, 42, "process-start")
                    .unwrap();
            }
            let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
            assert!(
                RunnerStore::reconfigure(base.path(), "worker", &program, vec![], 3600,)
                    .unwrap_err()
                    .to_string()
                    .contains("launching or running")
            );
        }
    }

    #[test]
    fn overflow_needs_explicit_disabled_reset() {
        let (_base, store) = setup();
        let messages = (0..=MAX_PENDING)
            .map(|index| {
                let ch = char::from_digit((index % 10) as u32, 10).unwrap();
                let mut item = message(ch, "human", "worker", AgentMessageKind::Request, None);
                item.message_id = format!("{index:064x}");
                item
            })
            .collect();
        let admitted = store.admit_inbox(&inbox('b', messages)).unwrap();
        assert!(admitted.needs_attention);
        assert_eq!(
            store.status().unwrap().attention,
            Some(RunnerAttention::PendingOverflow)
        );
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        assert!(store.reset_to_current_cursor(&id('c'), true).is_err());
        store.set_enabled(false).unwrap();
        assert!(store.reset_to_current_cursor(&id('c'), false).is_err());
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Idle);
    }

    #[test]
    fn cursor_reset_needs_attention_without_advancing_or_staging() {
        let (_base, store) = setup();
        let result = AgentInboxResult {
            cursor: id('b'),
            cursor_reset: true,
            messages: vec![message(
                '1',
                "human",
                "worker",
                AgentMessageKind::Request,
                None,
            )],
        };
        let admission = store.admit_inbox(&result).unwrap();
        assert!(admission.needs_attention);
        assert_eq!(admission.admitted, 0);
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let status = store.status().unwrap();
        assert_eq!(status.pending_count, 0);
        assert_eq!(status.attention, Some(RunnerAttention::CursorReset));
    }

    #[test]
    fn preparation_failure_requires_idle_pending_work_and_preserves_it_until_reset() {
        let (base, store) = setup();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert!(session
            .record_preparation_failed()
            .unwrap_err()
            .to_string()
            .contains("no pending request"));

        session
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let before =
            serde_json::from_slice::<serde_json::Value>(&fs::read(store.path()).unwrap()).unwrap();
        let status = session.record_preparation_failed().unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::PreparationFailed));
        assert_eq!(status.pending_count, 1);
        assert!(status.active_message_id.is_none());
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let after =
            serde_json::from_slice::<serde_json::Value>(&fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(after["runtime"]["pending"], before["runtime"]["pending"]);
        assert_eq!(
            after["runtime"]["staged_cursor"],
            before["runtime"]["staged_cursor"]
        );
        assert_eq!(after["runtime"]["attention"], "preparation_failed");
        assert!(session.begin_next(&id('b')).is_err());
        assert!(session.record_preparation_failed().is_err());

        store.set_enabled(false).unwrap();
        drop(session);
        let reset = store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(reset.phase, RunnerPhase::Idle);
        assert!(reset.attention.is_none());
        assert_eq!(reset.pending_count, 0);

        let (active_base, active_store) = setup();
        let active_session = active_store
            .execution_session(active_base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        active_session
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        active_session.begin_next(&id('b')).unwrap();
        assert!(active_session
            .record_preparation_failed()
            .unwrap_err()
            .to_string()
            .contains("active request"));
    }

    #[test]
    fn prelaunch_checkpoint_persists_without_body_or_output() {
        let (_base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let launch = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        let bytes = fs::read_to_string(store.path()).unwrap();
        assert!(bytes.contains(&launch.session_id));
        assert!(!bytes.contains("private task body"));
        assert!(!bytes.contains("output"));
        assert!(!bytes.contains("about_snapshot"));
        assert!(!bytes.contains("\"from\""));
        let state: serde_json::Value = serde_json::from_str(&bytes).unwrap();
        assert_eq!(
            state["runtime"]["pending"],
            serde_json::json!([id('1')]),
            "pending persistence must contain message IDs only"
        );
        let invocation = RunnerInvocation::new(&launch, "worker", request).unwrap();
        assert_eq!(invocation.schema_version, 1);
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Launching);
    }

    #[test]
    fn store_rejects_directory_identity_mismatch() {
        let (_base, store) = setup();
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        state["config"]["agent"] = serde_json::json!("other");
        fs::write(store.path(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(store
            .status()
            .unwrap_err()
            .to_string()
            .contains("directory identity"));
    }

    #[cfg(unix)]
    #[test]
    fn configure_rejects_symlinked_agent_layout_ancestors() {
        use std::os::unix::fs::symlink;

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        for component in ["root", "worktree", "state"] {
            let base = tempfile::tempdir().unwrap();
            prepare_baseline(base.path(), "worker", 3);
            let root = runner_agent_root(base.path(), "worker").unwrap();
            let (original, replacement) = match component {
                "root" => (root.clone(), root.with_file_name("real-worker")),
                name => (root.join(name), root.join(format!("real-{name}"))),
            };
            fs::rename(&original, &replacement).unwrap();
            symlink(&replacement, &original).unwrap();
            assert!(
                RunnerStore::configure(base.path(), "worker", &program, vec![], 60, &id('a'))
                    .unwrap_err()
                    .to_string()
                    .contains("not a real directory")
            );
        }
    }

    #[test]
    fn execution_session_requires_complete_terminal_correlation() {
        let (base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, 42, "correlation-test-child")
            .unwrap();

        let mut mismatched_request = request.clone();
        mismatched_request.message_id = id('9');
        assert!(session
            .observe_terminals(
                &mismatched_request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('9'),
                )],
            )
            .is_err());
        let mut indirect_request = request.clone();
        indirect_request.to = "other".into();
        assert!(session
            .observe_terminals(
                &indirect_request,
                &[message(
                    '3',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .is_err());

        let unrelated = [
            message('4', "worker", "other", AgentMessageKind::Result, Some('1')),
            message('5', "other", "human", AgentMessageKind::Result, Some('1')),
            message('6', "worker", "human", AgentMessageKind::Status, Some('1')),
            message('7', "worker", "human", AgentMessageKind::Result, Some('9')),
        ];
        for terminal in unrelated {
            assert!(session
                .observe_terminals(&request, &[terminal])
                .unwrap()
                .is_none());
        }
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Running);
        assert!(session
            .observe_terminals(
                &request,
                &[message(
                    '8',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1')
                )],
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn startup_marks_ambiguous_and_never_replays() {
        let (_base, store) = setup();
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let launch = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        let status = store.checkpoint_startup().unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::AmbiguousExecution));
        assert_eq!(
            status.active_message_id.as_deref(),
            Some(launch.message_id.as_str())
        );
        assert!(store
            .begin_next(RunnerExecutionMode::Supervised, &id('c'))
            .is_err());
        assert_eq!(store.status().unwrap().pending_count, 1);

        store.set_enabled(false).unwrap();
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        store.set_enabled(true).unwrap();
        store
            .admit_inbox(&inbox(
                'd',
                vec![message(
                    '2',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let running = store
            .begin_next(RunnerExecutionMode::Supervised, &id('d'))
            .unwrap();
        store
            .mark_spawned(&running.message_id, 42, "process-start")
            .unwrap();
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Running);
        assert_eq!(
            store.checkpoint_startup().unwrap().attention,
            Some(RunnerAttention::AmbiguousExecution)
        );
    }

    #[test]
    fn delivery_unknown_retains_active_queue_and_cursor_without_replay() {
        let (_base, store) = setup();
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let launch = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .mark_spawned(&launch.message_id, 42, "private-process-start-id")
            .unwrap();
        let before = store.status().unwrap();
        let cursor_before = store.committed_cursor().unwrap();

        let status = store
            .record_delivery_unknown(&launch.message_id, &launch.session_id)
            .unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::DeliveryUnknown));
        assert_eq!(status.pending_count, 1);
        assert_eq!(status.active_message_id, before.active_message_id);
        assert_eq!(status.active_session_id, before.active_session_id);
        assert_eq!(status.active_started_at_ms, before.active_started_at_ms);
        assert_eq!(status.active_spawned_at_ms, before.active_spawned_at_ms);
        assert_eq!(store.committed_cursor().unwrap(), cursor_before);
        assert!(store
            .begin_next(RunnerExecutionMode::Supervised, &id('c'))
            .is_err());
        assert!(store.admit_inbox(&inbox('c', Vec::new())).is_err());
        assert_eq!(store.status().unwrap().pending_count, 1);
        assert_eq!(store.committed_cursor().unwrap(), cursor_before);

        let redacted = serde_json::to_string(&status).unwrap();
        assert!(redacted.contains("delivery_unknown"));
        assert!(!redacted.contains("private-process-start-id"));
        assert!(!redacted.contains("private task body"));
        assert!(!redacted.contains("output"));
    }

    #[test]
    fn reset_retains_completed_dedupe_and_offline_recovery_preserves_tasks() {
        let (_base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .observe_terminals(
                &request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        store.record_inbox_failure().unwrap();
        assert_eq!(store.status().unwrap().inbox_failure_count, 1);
        assert_eq!(store.status().unwrap().pending_count, 0);
        store.record_inbox_recovery().unwrap();
        assert_eq!(store.status().unwrap().inbox_failure_count, 0);
        store.set_enabled(false).unwrap();
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        store.set_enabled(true).unwrap();
        assert_eq!(
            store
                .admit_inbox(&inbox('d', vec![request]))
                .unwrap()
                .admitted,
            0
        );
    }

    fn assert_offline_accounting_preserves_lifecycle(store: &RunnerStore) {
        let before = store.status().unwrap();
        let cursor = store.committed_cursor().unwrap();
        let failed = store.record_inbox_failure().unwrap();
        let mut expected_failed = before.clone();
        expected_failed.updated_at_ms = failed.updated_at_ms;
        expected_failed.inbox_failure_count = 1;
        assert_eq!(failed, expected_failed);
        assert_eq!(store.committed_cursor().unwrap(), cursor);

        let recovered = store.record_inbox_recovery().unwrap();
        let mut expected_recovered = before;
        expected_recovered.updated_at_ms = recovered.updated_at_ms;
        expected_recovered.inbox_failure_count = 0;
        assert_eq!(recovered, expected_recovered);
        assert_eq!(store.committed_cursor().unwrap(), cursor);
    }

    #[test]
    fn offline_failure_and_recovery_preserve_pending_launching_and_running_state() {
        let (_pending_base, pending) = setup();
        pending
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        assert_eq!(pending.status().unwrap().phase, RunnerPhase::Idle);
        assert_offline_accounting_preserves_lifecycle(&pending);

        let (_launching_base, launching) = setup();
        launching
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        launching
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        assert_eq!(launching.status().unwrap().phase, RunnerPhase::Launching);
        assert_offline_accounting_preserves_lifecycle(&launching);

        let (_running_base, running) = setup();
        running
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let launch = running
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        running
            .mark_spawned(&launch.message_id, 42, "process-start")
            .unwrap();
        assert_eq!(running.status().unwrap().phase, RunnerPhase::Running);
        assert_offline_accounting_preserves_lifecycle(&running);
    }

    #[tokio::test]
    async fn refresh_and_land_refuse_while_configured_runner_owns_the_lease() {
        let (base, _store) = setup_disabled();
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
        let lease = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
            .unwrap()
            .unwrap();

        let refresh_error = crate::agent::refresh_agent(
            base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
        )
        .await
        .unwrap_err();
        assert!(refresh_error.to_string().contains("already active"));

        let land_error = crate::agent::land_agent(
            base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(land_error.to_string().contains("already active"));
        drop(lease);
    }

    #[derive(Clone, Copy)]
    enum TestWorktreeMutation {
        Refresh,
        Land,
    }

    async fn assert_configure_waits_for_unconfigured_operation(operation: TestWorktreeMutation) {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let mut operation_pause = install_operation_guard_pause(base.path(), "worker").unwrap();
        let operation_base = base.path().to_path_buf();
        let operation_task = tokio::spawn(async move {
            let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
            let password = "e".repeat(64);
            match operation {
                TestWorktreeMutation::Refresh => crate::agent::refresh_agent(
                    &operation_base,
                    &db,
                    &api,
                    "runner-test",
                    "worker",
                    Some(&password),
                )
                .await
                .map(|_| ()),
                TestWorktreeMutation::Land => crate::agent::land_agent(
                    &operation_base,
                    &db,
                    &api,
                    "runner-test",
                    "worker",
                    Some(&password),
                    false,
                    false,
                )
                .await
                .map(|_| ()),
            }
        });
        operation_pause.wait("worktree mutation did not acquire its unconfigured runner guard");

        let contended = install_lifecycle_contention_hook(base.path()).unwrap();
        let configure_base = base.path().to_path_buf();
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let configure = tokio::task::spawn_blocking(move || {
            RunnerStore::configure(
                &configure_base,
                "worker",
                &program,
                Vec::new(),
                3600,
                &id('a'),
            )
        });
        contended.wait("runner configure did not contend on the held operation guard");
        assert!(!configure.is_finished());

        operation_pause.release().unwrap();
        assert!(operation_task.await.unwrap().is_err());
        configure.await.unwrap().unwrap();
        assert!(runner_status(base.path()).unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn configure_cannot_race_unconfigured_refresh_or_land() {
        assert_configure_waits_for_unconfigured_operation(TestWorktreeMutation::Refresh).await;
        assert_configure_waits_for_unconfigured_operation(TestWorktreeMutation::Land).await;
    }

    #[tokio::test]
    async fn guarded_refresh_requires_exact_identity_and_does_not_self_lock() {
        let (owned_base, owned_store) = setup_disabled();
        let (other_base, _other_store) = setup_disabled();
        let state = crate::workspace_layout::ensure_workspace_state(owned_base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
        let session = owned_store
            .execution_session(owned_base.path(), RunnerExecutionMode::Foreground)
            .unwrap();

        let mismatch = crate::agent::refresh_agent_guarded(
            other_base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
            &session,
        )
        .await
        .unwrap_err();
        assert!(mismatch.to_string().contains("does not own agent"));

        fs::remove_file(
            runner_agent_root(owned_base.path(), "worker")
                .unwrap()
                .join("state/base-snapshot"),
        )
        .unwrap();
        let validation = crate::agent::refresh_agent_guarded(
            owned_base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
            &session,
        )
        .await
        .unwrap_err();
        assert!(validation.to_string().contains("base snapshot"));
        assert!(!validation.to_string().contains("already active"));
    }

    #[test]
    fn bounds_corrupt_unknown_and_future_state_fail_closed() {
        let (base, store) = setup();
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert!(RunnerStore::reconfigure(base.path(), "worker", &program, vec![], 59,).is_err());
        assert!(RunnerStore::reconfigure(
            base.path(),
            "worker",
            &program,
            vec![String::new(); MAX_ARGS + 1],
            60,
        )
        .is_err());
        fs::write(store.path(), "not-json").unwrap();
        assert!(store.status().is_err());
        assert!(runner_status(base.path()).is_err());
        assert!(runner_process_metadata(base.path()).is_err());

        let (_base, store) = setup();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        value["unknown"] = serde_json::json!(true);
        fs::write(store.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(store
            .status()
            .unwrap_err()
            .to_string()
            .contains("parse runner state"));

        let (_base, store) = setup();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(99);
        fs::write(store.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(store
            .status()
            .unwrap_err()
            .to_string()
            .contains("unsupported runner state schema 99"));
    }

    #[cfg(unix)]
    #[test]
    fn configure_requires_executable_program_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let program_dir = tempfile::tempdir().unwrap();
        let program = program_dir.path().join("runner-program");
        fs::write(&program, b"not executable").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();
        let program = fs::canonicalize(program).unwrap();
        assert!(
            RunnerStore::configure(base.path(), "worker", &program, vec![], 60, &id('a'))
                .unwrap_err()
                .to_string()
                .contains("must be executable")
        );
    }

    #[test]
    fn second_lifetime_lock_refuses_without_waiting() {
        let (base, _store) = setup();
        let _first = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
            .unwrap()
            .unwrap();
        let error = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker").unwrap_err();
        assert!(error.to_string().contains("already active"));
    }
}
