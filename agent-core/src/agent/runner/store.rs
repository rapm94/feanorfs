//! Durable runner-store schema validation and state transitions.

use crate::agent::continuous::{conflict_failure, unsafe_path_failure, unsupported_schema_failure};
use crate::agent::scope::{validate_accepted_work, AcceptedWorkDescriptor};
use crate::durable::{
    atomic_overwrite, create_lock_acquire_exclusive, open_lock_exclusive, open_lock_shared,
};
use crate::paths::validate_name;
use crate::workspace_layout::{workspace_is_configured, workspace_state_path};
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{
    is_valid_hash, AgentInboxResult, AgentMessage, AgentMessageKind, AGENT_INBOX_MAX_LIMIT,
    AGENT_MESSAGE_MAX_BODY_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use super::contract::{
    RunnerAttention, RunnerConfig, RunnerExecutionMode, RunnerLaunch, RunnerPhase,
    RunnerProcessMetadata, RunnerScopeMode, RunnerStatus, RunnerWorkWait, RunnerWorkWaitKind,
    ScopeChangePublishState,
};
use super::ownership::{
    ensure_no_interactive_owner, RunnerIdentity, RunnerLifecycleLock, RunnerLifetimeLock,
};
#[cfg(test)]
use super::test_hooks::{pause_status_discovery_if_requested, pause_status_snapshot_if_requested};

const RUNNER_STATE_SCHEMA_VERSION: u32 = 3;
/// Pre-enforcement runner state (no scope mode); migrated to
/// [`RunnerScopeMode::LegacyUnenforced`] on load without rewriting state.
const RUNNER_STATE_LEGACY_SCHEMA_VERSION: u32 = 2;
const STATE_FILE: &str = "runner-state.json";
const STATE_LOCK: &str = "runner-state.lock";
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PROGRAM_BYTES: usize = 16 * 1024;
pub(super) const MAX_ARGS: usize = 128;
const MAX_ARG_BYTES: usize = 8 * 1024;
const MAX_ARGV_BYTES: usize = 64 * 1024;
pub(super) const MAX_PENDING: usize = 32;
const MAX_COMPLETED: usize = 10_000;
pub(super) const MAX_FAILURES: u32 = 1_000_000;
pub(super) const MAX_PROCESS_START_ID_BYTES: usize = 512;
const MAX_AGENT_ENTRIES: usize = 10_000;
const CONFIGURED_READ_ATTEMPTS: usize = 4;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ActivePhase {
    Launching,
    Running,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActiveRequest {
    pub(super) message_id: String,
    pub(super) session_id: String,
    pub(super) phase: ActivePhase,
    pub(super) reply_cursor: String,
    pub(super) started_at_ms: i64,
    pub(super) spawned_at_ms: Option<i64>,
    pub(super) pid: Option<u32>,
    pub(super) process_start_id: Option<String>,
    /// Accepted work bound at the pre-spawn checkpoint; `None` for
    /// legacy/advisory launches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) accepted_work: Option<AcceptedWorkDescriptor>,
}

/// Persisted record of the one deduplicated scope-change request published
/// for a waiting request (bounded; full paths stay out of runner state).
///
/// The record is persisted **before** the request message is sent
/// (crash-durable): `message_id` is `None` while publish-pending and filled
/// in by [`RunnerStore::complete_scope_change_request_locked`] after a
/// successful send. A restart that finds a publish-pending record flips it
/// to awaiting confirmation and never republishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScopeChangeRequestState {
    /// Message id of the sent request; `None` while publish-pending or
    /// awaiting confirmation after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub task_id: String,
    pub intent_message_id: String,
    /// Bounded fingerprint of the out-of-scope scope/operation set already
    /// reported; republishing is skipped while it is unchanged.
    pub paths_fingerprint: String,
    /// Canonical scope component (paths + concerns) at publish time. The
    /// admission gate releases the scope-amendment wait only when this
    /// changes (an amendment); empty for legacy records.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope_fingerprint: String,
    /// Publish lifecycle; see [`ScopeChangePublishState`]. Defaults to
    /// confirmed for records persisted before this field existed.
    #[serde(default)]
    pub publish_state: ScopeChangePublishState,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LastTerminal {
    pub(super) kind: AgentMessageKind,
    pub(super) message_id: String,
    pub(super) observed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunnerRuntime {
    pub(super) enabled: bool,
    pub(super) committed_cursor: String,
    pub(super) staged_cursor: Option<String>,
    pub(super) pending: Vec<String>,
    pub(super) active: Option<ActiveRequest>,
    pub(super) completed_ids: Vec<String>,
    pub(super) last_terminal: Option<LastTerminal>,
    pub(super) attention: Option<RunnerAttention>,
    /// Bounded typed wait for the next pending request (admission or scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) work_wait: Option<RunnerWorkWait>,
    /// One deduplicated scope-change request published while waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) scope_change_request: Option<ScopeChangeRequestState>,
    pub(super) updated_at_ms: i64,
    pub(super) inbox_failure_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunnerState {
    schema_version: u32,
    generation_id: String,
    pub(super) config: RunnerConfig,
    pub(super) runtime: RunnerRuntime,
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
    pub(super) identity: RunnerIdentity,
}
impl RunnerStore {
    /// Creates the workspace's only runner at the supplied current head.
    /// Legacy default: admission is not gated and land stays all-path. Use
    /// [`Self::configure_scoped`] to opt into advisory/enforced coordination.
    pub fn configure(
        base: &Path,
        agent: &str,
        canonical_absolute_program: &Path,
        fixed_args: Vec<String>,
        timeout_secs: u64,
        initial_head_cursor: &str,
    ) -> Result<Self> {
        Self::configure_scoped(
            base,
            agent,
            canonical_absolute_program,
            fixed_args,
            timeout_secs,
            initial_head_cursor,
            RunnerScopeMode::LegacyUnenforced,
        )
    }

    /// Creates the workspace's only runner with an explicit scope mode.
    #[allow(clippy::too_many_arguments)]
    pub fn configure_scoped(
        base: &Path,
        agent: &str,
        canonical_absolute_program: &Path,
        fixed_args: Vec<String>,
        timeout_secs: u64,
        initial_head_cursor: &str,
        scope_mode: RunnerScopeMode,
    ) -> Result<Self> {
        validate_name(agent)?;
        validate_nonempty_cursor(initial_head_cursor)?;
        validate_program(canonical_absolute_program)?;
        validate_args(&fixed_args)?;
        ensure!(
            (60..=86_400).contains(&timeout_secs),
            "runner timeout must be between 60 and 86400 seconds"
        );
        let lifecycle_guard = RunnerLifecycleLock::acquire(base)?;
        validate_configured_baseline(base, agent)?;
        ensure!(
            find_configured(base)?.is_none(),
            "an agent runner is already configured; use reconfigure to update it"
        );
        ensure_no_interactive_owner(base, agent, &lifecycle_guard)?;

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
                scope_mode,
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
                work_wait: None,
                scope_change_request: None,
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
        let lifecycle_guard = RunnerLifecycleLock::acquire(base)?;
        validate_configured_baseline(base, agent)?;
        let configured = find_configured(base)?.context("no agent runner is configured")?;
        ensure!(
            configured == agent,
            "runner '{configured}' is configured; it cannot be replaced by runner '{agent}'"
        );
        ensure_no_interactive_owner(base, agent, &lifecycle_guard)?;

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
        let scope_mode = state.config.scope_mode;
        state.config = RunnerConfig {
            agent: agent.to_string(),
            program: canonical_absolute_program.to_path_buf(),
            fixed_args,
            timeout_secs,
            configured_at_ms: now,
            scope_mode,
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

    pub(super) fn open_existing(base: &Path, agent: &str) -> Result<Self> {
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

    // State-machine unit tests exercise persistence transitions directly;
    // production callers can reach them only through RunnerExecutionSession.
    #[cfg(test)]
    pub(super) fn begin_next(
        &self,
        mode: RunnerExecutionMode,
        reply_cursor: &str,
    ) -> Result<RunnerLaunch> {
        self.begin_next_locked(mode, reply_cursor, None)
    }

    pub fn committed_cursor(&self) -> Result<String> {
        Ok(self.load()?.runtime.committed_cursor)
    }

    /// Read-only id of the next queued request (the one admission gates).
    pub fn next_pending_message_id(&self) -> Result<Option<String>> {
        Ok(self.load()?.runtime.pending.first().cloned())
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

    /// Persists the launching checkpoint before a caller may spawn a child.
    pub(super) fn begin_next_locked(
        &self,
        mode: RunnerExecutionMode,
        reply_cursor: &str,
        accepted_work: Option<AcceptedWorkDescriptor>,
    ) -> Result<RunnerLaunch> {
        validate_cursor(reply_cursor)?;
        let mut launch = None;
        self.update(|state| {
            validate_execution_mode(mode, state.runtime.enabled)?;
            if state.runtime.attention.is_some() {
                return Err(conflict_failure(
                    "runner needs attention and cannot begin another request",
                ));
            }
            ensure!(
                state.runtime.active.is_none(),
                "runner already has an active request"
            );
            let request = state
                .runtime
                .pending
                .first()
                .context("runner has no pending request")?;
            if state.config.scope_mode.is_enforced() {
                let descriptor = accepted_work
                    .as_ref()
                    .context("enforced runner launch requires an accepted-work descriptor")?;
                validate_accepted_work(descriptor, &state.config.agent, request)?;
            } else {
                ensure!(
                    accepted_work.is_none(),
                    "non-enforced runner launch cannot claim accepted work"
                );
            }
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
                accepted_work: accepted_work.clone(),
            });
            state.runtime.work_wait = None;
            state.runtime.scope_change_request = None;
            state.runtime.updated_at_ms = now;
            launch = Some(RunnerLaunch {
                message_id: request.clone(),
                session_id,
                reply_cursor: reply_cursor.to_string(),
                accepted_work,
            });
            Ok(())
        })?;
        launch.context("runner launch checkpoint was not created")
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
            state.runtime.work_wait = None;
            state.runtime.scope_change_request = None;
            state.runtime.updated_at_ms = now_ms();
            Ok(())
        })?;
        self.status()
    }

    fn acquire_control_lease(&self) -> Result<RunnerLifetimeLock> {
        RunnerLifetimeLock::try_acquire_exact_store(&self.identity.canonical_workspace, self)?
            .context("this runner store is not the configured agent for the workspace")
    }

    pub(super) fn load(&self) -> Result<RunnerState> {
        let _lock = open_lock_shared(&self.lock_path).context("lock runner state for reading")?;
        self.load_unlocked()
    }

    pub(super) fn load_unlocked(&self) -> Result<RunnerState> {
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

    pub(super) fn update(&self, f: impl FnOnce(&mut RunnerState) -> Result<()>) -> Result<()> {
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
        scope_mode: state.config.scope_mode,
        work_wait: state.runtime.work_wait.clone(),
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
fn validate_configured_baseline(base: &Path, agent: &str) -> Result<()> {
    ensure!(
        workspace_is_configured(base),
        "runner configuration requires an initialized FeanorFS workspace"
    );
    let config = crate::local::load_config(base)?;
    if config.format_version != 3 {
        return Err(unsupported_schema_failure(
            "agent runners require a format-v3 workspace",
        ));
    }
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
pub(super) fn validate_agent_layout(base: &Path, agent: &str) -> Result<()> {
    validate_name(agent)?;
    let agents = workspace_state_path(base)?.join("agents");
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
pub(super) fn runner_agent_root(base: &Path, agent: &str) -> Result<PathBuf> {
    validate_name(agent)?;
    Ok(workspace_state_path(base)?.join("agents").join(agent))
}
pub(super) fn runner_dir_path(base: &Path, agent: &str) -> Result<PathBuf> {
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
pub(super) fn find_configured(base: &Path) -> Result<Option<String>> {
    let directory = workspace_state_path(base)?.join("agents");
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
    if probe.schema_version != RUNNER_STATE_SCHEMA_VERSION
        && probe.schema_version != RUNNER_STATE_LEGACY_SCHEMA_VERSION
    {
        return Err(unsupported_schema_failure(format!(
            "unsupported runner state schema {} (expected {RUNNER_STATE_SCHEMA_VERSION})",
            probe.schema_version
        )));
    }
    let mut state: RunnerState = serde_json::from_slice(&bytes).context("parse runner state")?;
    if probe.schema_version == RUNNER_STATE_LEGACY_SCHEMA_VERSION {
        // Pre-enforcement state is explicitly legacy_unenforced: it never
        // claims accepted scope it never verified. The in-memory upgrade is
        // persisted by the next state update.
        state.schema_version = RUNNER_STATE_SCHEMA_VERSION;
        state.config.scope_mode = RunnerScopeMode::LegacyUnenforced;
    }
    validate_state(&state)?;
    Ok(state)
}
pub(super) fn validate_state(state: &RunnerState) -> Result<()> {
    if state.schema_version != RUNNER_STATE_SCHEMA_VERSION {
        return Err(unsupported_schema_failure(format!(
            "unsupported runner state schema {}",
            state.schema_version
        )));
    }
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
    if let Some(wait) = &state.runtime.work_wait {
        validate_message_id(&wait.message_id)?;
        ensure!(
            state.runtime.attention.is_none(),
            "runner cannot wait on work while needing attention"
        );
        ensure!(
            state.runtime.active.is_none(),
            "runner cannot wait on work while a request is active"
        );
        ensure!(
            state
                .runtime
                .pending
                .first()
                .is_some_and(|pending| pending == &wait.message_id),
            "runner work wait does not match the next pending request"
        );
        ensure!(
            match wait.kind {
                RunnerWorkWaitKind::ScopeAmendmentRequested => wait.reason.is_none(),
                _ => wait.reason.is_some(),
            },
            "runner work wait kind and typed reason are inconsistent"
        );
        ensure!(
            wait.out_of_scope_count <= feanorfs_common::WORK_MAX_PATHS as u32,
            "runner work wait out-of-scope count exceeds its bound"
        );
    }
    if let Some(record) = &state.runtime.scope_change_request {
        match &record.message_id {
            Some(message_id) => {
                validate_message_id(message_id)?;
                ensure!(
                    record.publish_state == ScopeChangePublishState::Confirmed,
                    "runner scope-change request must be confirmed once it carries a message id"
                );
            }
            None => {
                ensure!(
                    matches!(
                        record.publish_state,
                        ScopeChangePublishState::PublishPending
                            | ScopeChangePublishState::AwaitingConfirmation
                    ),
                    "runner scope-change request without a message id must be pending or awaiting confirmation"
                );
            }
        }
        ensure!(
            feanorfs_common::is_valid_task_id(&record.task_id),
            "runner scope-change request has an invalid task id"
        );
        validate_message_id(&record.intent_message_id)?;
        ensure!(
            !record.paths_fingerprint.is_empty()
                && record.paths_fingerprint.len() <= 128
                && record
                    .paths_fingerprint
                    .chars()
                    .all(|c| c.is_ascii_hexdigit()),
            "runner scope-change request fingerprint is invalid"
        );
        ensure!(
            record.scope_fingerprint.is_empty()
                || (record.scope_fingerprint.len() <= 128
                    && record
                        .scope_fingerprint
                        .chars()
                        .all(|c| c.is_ascii_hexdigit())),
            "runner scope-change scope fingerprint is invalid"
        );
        ensure!(
            state.runtime.work_wait.as_ref().is_some_and(|wait| {
                wait.kind == RunnerWorkWaitKind::ScopeAmendmentRequested
                    && wait.message_id == record.intent_message_id
            }) || state.runtime.active.as_ref().is_some_and(|active| {
                active.message_id == record.intent_message_id
            }),
            "runner scope-change request requires a matching scope-amendment wait or active request"
        );
    }
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
        if let Some(accepted_work) = &active.accepted_work {
            validate_accepted_work(accepted_work, &state.config.agent, &active.message_id)?;
            ensure!(
                state.config.scope_mode.is_enforced(),
                "active request carries accepted work on a non-enforced runner"
            );
        } else {
            ensure!(
                !state.config.scope_mode.is_enforced(),
                "enforced runner active request lacks accepted work"
            );
        }
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
pub(super) fn validate_inbox(result: &AgentInboxResult) -> Result<()> {
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
pub(super) fn validate_message(message: &AgentMessage) -> Result<()> {
    validate_message_id(&message.message_id)?;
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
pub(super) fn validate_message_id(message_id: &str) -> Result<()> {
    ensure!(
        is_valid_hash(message_id),
        "invalid runner request message id"
    );
    Ok(())
}
pub(super) fn validate_cursor(cursor: &str) -> Result<()> {
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
pub(super) fn validate_execution_mode(mode: RunnerExecutionMode, enabled: bool) -> Result<()> {
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
pub(super) fn validate_generation_id(generation_id: &str) -> Result<()> {
    ensure!(
        generation_id.len() == 32
            && generation_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid runner configuration generation"
    );
    Ok(())
}
pub(super) fn validate_session_id(session_id: &str) -> Result<()> {
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
pub(super) fn touch_completed(completed: &mut Vec<String>, request_id: &str) {
    completed.retain(|id| id != request_id);
    completed.push(request_id.to_string());
    if completed.len() > MAX_COMPLETED {
        completed.drain(..completed.len() - MAX_COMPLETED);
    }
}
pub(super) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub(super) fn ensure_private_dir(path: &Path) -> Result<()> {
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
pub(super) fn ensure_existing_real_dir(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(unsafe_path_failure(format!(
            "{label} is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}
pub(super) fn set_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
pub(super) fn ensure_regular_or_missing(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => bail!("{label} is not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {label}")),
    }
}
