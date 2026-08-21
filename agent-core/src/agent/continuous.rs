//! Continuous reconciliation ownership, status, and guarded entry points.
//!
//! This module defines the table-driven contract shared by interactive
//! `agent run` owners, configured runner workers, CLI status, events, and
//! tests. It owns process-lifetime exclusivity for one `(workspace,
//! agent-name)` pair and exposes guarded land/refresh entry points that the
//! continuous controller (client-side) drives. It grants no merge authority:
//! every mutation flows through the existing land/refresh/conflict machinery.
//!
//! Activation is explicit: only `agent run` or an enabled configured runner
//! acquire a lease. Spawning, starting FeanorFS, or upgrading never activates
//! a dormant agent.

use crate::api::{is_retryable_transport_error, ApiClient};
use crate::ctx::SyncCtx;
use crate::durable;
use crate::local::ClientDb;
use crate::paths::agent_state_dir;
use crate::snapshot::SnapshotEngine;
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{
    is_valid_agent_name, is_valid_hash, AgentLandResult, AgentRefreshResult, ContinuousAgentStatus,
    ContinuousAttention, ContinuousPhase, CONTINUOUS_STATUS_SCHEMA_VERSION,
};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub use super::runner::RunnerExecutionSession;

const STATUS_FILE: &str = "continuous-status.json";
const OWNER_LOCK: &str = "continuous-owner.lock";
/// Bound for the persisted status projection; never grows with history.
const MAX_STATUS_BYTES: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// Process-lifetime ownership
// ---------------------------------------------------------------------------

/// Nonblocking process-lifetime exclusivity for a continuous controller that
/// owns an agent without a configured runner (`agent run`).
///
/// The OS releases the lock when the owning process exits, so a crash or an
/// upgrade can never leave a stale owner that blocks future activation.
#[derive(Debug)]
pub struct ContinuousOwnerLock {
    _file: File,
    canonical_workspace: PathBuf,
    agent: String,
}

impl ContinuousOwnerLock {
    /// Tries to claim exclusive continuous ownership for `(base, agent)`.
    ///
    /// Returns `Ok(None)` when another live process holds the lease.
    pub fn try_acquire(base: &Path, agent: &str) -> Result<Option<Self>> {
        let lifecycle = super::runner::RunnerLifecycleLock::acquire(base)?;
        Self::try_acquire_locked(base, agent, &lifecycle)
    }

    pub(crate) fn try_acquire_locked(
        base: &Path,
        agent: &str,
        _lifecycle: &super::runner::RunnerLifecycleLock,
    ) -> Result<Option<Self>> {
        crate::paths::validate_name(agent)?;
        let state_dir = agent_state_dir(base, agent)?;
        let metadata = std::fs::symlink_metadata(&state_dir)
            .with_context(|| format!("read agent state directory {}", state_dir.display()))?;
        ensure_real_dir(&metadata, &state_dir, "agent state directory")?;
        let path = state_dir.join(OWNER_LOCK);
        let lock_metadata = std::fs::symlink_metadata(&path);
        if let Ok(metadata) = &lock_metadata {
            ensure_regular(metadata, &path, "continuous owner lock")?;
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).context("open continuous owner lock")?;
        set_private_file(&path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self {
                _file: file,
                canonical_workspace: std::fs::canonicalize(base)
                    .context("canonicalize continuous owner workspace")?,
                agent: agent.to_string(),
            })),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                Ok(None)
            }
            Err(error) => Err(error).context("acquire continuous owner lock"),
        }
    }

    fn ensure_matches(&self, base: &Path, agent: &str) -> Result<()> {
        let canonical_workspace =
            std::fs::canonicalize(base).context("canonicalize continuous owner workspace")?;
        if self.canonical_workspace != canonical_workspace || self.agent != agent {
            bail!("continuous owner lock does not own agent '{agent}' in this workspace");
        }
        Ok(())
    }

    /// Claims interactive ownership; fails when another owner exists or a
    /// configured runner already owns this agent.
    pub fn acquire_interactive(base: &Path, agent: &str) -> Result<Self> {
        let lifecycle_guard = super::runner::RunnerLifecycleLock::acquire(base)?;
        if super::runner::configured_runner_is(base, agent)? {
            bail!(
                "agent '{agent}' has a configured runner; run it through `feanorfs agent runner` \
                 instead of `agent run`"
            );
        }
        match ContinuousOwnerLock::try_acquire_locked(base, agent, &lifecycle_guard)? {
            Some(lock) => Ok(lock),
            None => {
                bail!("another process already owns continuous reconciliation for agent '{agent}'")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worktree identity safety
// ---------------------------------------------------------------------------

/// Fails closed when the agent worktree is missing, replaced by a symlink or
/// other non-directory, or otherwise not a stable ordinary directory.
///
/// Resolves the workspace state slot directly instead of going through
/// `agent_dir`, which triggers legacy layout migration and would silently
/// recreate a removed worktree as an empty directory.
pub fn verify_agent_worktree(base: &Path, agent: &str) -> Result<()> {
    crate::paths::validate_name(agent)?;
    let state = crate::workspace_layout::workspace_state_path(base)?;
    let path = state.join("agents").join(agent).join("worktree");
    let metadata = std::fs::symlink_metadata(&path).with_context(|| {
        format!(
            "agent worktree '{}' is missing or unreadable",
            path.display()
        )
    })?;
    ensure_real_dir(&metadata, &path, "agent worktree")
}

fn ensure_real_dir(metadata: &std::fs::Metadata, path: &Path, label: &str) -> Result<()> {
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(unsafe_path_failure(format!(
            "{label} '{}' is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_regular(metadata: &std::fs::Metadata, path: &Path, label: &str) -> Result<()> {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(unsafe_path_failure(format!(
            "{label} '{}' is not a regular file",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set private permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Bounded status persistence
// ---------------------------------------------------------------------------

/// Reads the persisted status projection, or `None` when absent or written by
/// a newer schema this build cannot interpret.
pub fn read_continuous_status(base: &Path, agent: &str) -> Result<Option<ContinuousAgentStatus>> {
    let state_dir = agent_state_dir(base, agent)?;
    let path = state_dir.join(STATUS_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if bytes.len() as u64 > MAX_STATUS_BYTES {
        bail!("continuous status for agent '{agent}' exceeds its size bound");
    }
    #[derive(serde::Deserialize)]
    struct SchemaProbe {
        schema_version: u32,
    }
    let schema: SchemaProbe = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse continuous status schema for agent '{agent}'"))?;
    if schema.schema_version > CONTINUOUS_STATUS_SCHEMA_VERSION {
        tracing::warn!(
            agent = agent,
            schema_version = schema.schema_version,
            "continuous status written by a newer schema; ignoring"
        );
        return Ok(None);
    }
    let status: ContinuousAgentStatus = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse continuous status for agent '{agent}'"))?;
    validate_status(agent, &status)?;
    Ok(Some(status))
}

/// Returns the status only when its recorded owner still holds the lease;
/// a stale projection from a dead owner is treated as absent.
pub fn live_continuous_status(base: &Path, agent: &str) -> Result<Option<ContinuousAgentStatus>> {
    let Some(status) = read_continuous_status(base, agent)? else {
        return Ok(None);
    };
    if !status.active {
        return Ok(Some(status));
    }
    let owner_alive = if super::runner::configured_runner_is(base, agent)? {
        super::runner::runner_lifetime_held(base, agent)?
    } else {
        ContinuousOwnerLock::try_acquire(base, agent)?.is_none()
    };
    Ok(owner_alive.then_some(status))
}

/// Atomically persists the bounded status projection.
pub fn write_continuous_status(
    base: &Path,
    agent: &str,
    status: &ContinuousAgentStatus,
) -> Result<()> {
    validate_status(agent, status)?;
    let bytes = serde_json::to_vec(status).context("serialize continuous status")?;
    if bytes.len() as u64 > MAX_STATUS_BYTES {
        bail!("continuous status for agent '{agent}' exceeds its size bound");
    }
    let state_dir = agent_state_dir(base, agent)?;
    durable::atomic_overwrite(&state_dir.join(STATUS_FILE), &bytes)
}

fn validate_status(agent: &str, status: &ContinuousAgentStatus) -> Result<()> {
    if status.schema_version != CONTINUOUS_STATUS_SCHEMA_VERSION {
        return Err(unsupported_schema_failure(format!(
            "unsupported continuous status schema {} for agent '{agent}'",
            status.schema_version
        )));
    }
    ensure!(
        is_valid_agent_name(&status.agent) && status.agent == agent,
        "continuous status identity does not match agent '{agent}'"
    );
    for (label, value) in [
        ("observed head", status.observed_head.as_deref()),
        ("observed tree", status.observed_tree.as_deref()),
        ("settled snapshot", status.settled_snapshot.as_deref()),
    ] {
        ensure!(
            value.is_none_or(is_valid_hash),
            "continuous status {label} is not a full snapshot id"
        );
    }
    if let Some(attention) = &status.attention {
        ensure!(
            !attention.reason.is_empty() && attention.reason.len() <= 128,
            "continuous status attention reason exceeds its bound"
        );
        ensure!(
            attention.detail.len() <= 16 * 1024,
            "continuous status attention detail exceeds its bound"
        );
    }
    ensure!(
        status
            .owner_start_id
            .as_ref()
            .is_none_or(|identity| identity.len() <= 1024),
        "continuous status owner identity exceeds its bound"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Table-driven error classification
// ---------------------------------------------------------------------------

/// Typed failure categories produced at canonical boundaries and consumed by
/// [`classify_continuous_error`]. Rendered text never drives classification:
/// producers wrap their errors with one of the `*_failure` constructors below
/// and the classifier downcasts the typed marker from the error chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuousFailureKind {
    /// Real pending conflicts block automatic mutation until a human
    /// resolves them.
    PendingConflicts,
    /// The workspace/agent layout is not a plain portable directory tree.
    UnsafePath,
    /// A file or the head changed while it was being read or CAS'd; retrying
    /// after a fresh read is safe.
    RetryableVolatility,
    /// The workspace uses an unsupported snapshot format or state schema.
    UnsupportedSchema,
    /// A materialized result failed verification and must not be published.
    VerificationFailed,
}

impl ContinuousFailureKind {
    /// Bounded attention reason projected into status files when the kind
    /// does not retry.
    fn attention_reason(self) -> &'static str {
        match self {
            ContinuousFailureKind::PendingConflicts => "pending_conflicts",
            ContinuousFailureKind::UnsafePath => "unsafe_path",
            ContinuousFailureKind::RetryableVolatility => "retryable_volatility",
            ContinuousFailureKind::UnsupportedSchema => "unsupported_schema",
            ContinuousFailureKind::VerificationFailed => "verification_failed",
        }
    }
}

/// Typed marker carried in the error chain. Its `Display` and `source` mirror
/// the wrapped error exactly, so user-facing text and the underlying cause
/// chain are preserved while classification becomes type-driven.
#[derive(Debug)]
pub(crate) struct ContinuousFailure {
    kind: ContinuousFailureKind,
    source: anyhow::Error,
}

impl std::fmt::Display for ContinuousFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, formatter)
    }
}

impl std::error::Error for ContinuousFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

fn tagged_failure(kind: ContinuousFailureKind, error: impl std::fmt::Display) -> anyhow::Error {
    ContinuousFailure {
        kind,
        source: anyhow::anyhow!("{error}"),
    }
    .into()
}

/// Tags an error as a real pending-conflict failure (attention, no retry).
pub(crate) fn conflict_failure(error: impl std::fmt::Display) -> anyhow::Error {
    tagged_failure(ContinuousFailureKind::PendingConflicts, error)
}

/// Tags an error as a filesystem-safety failure (symlink, non-directory, or
/// otherwise unsafe layout; attention, no retry).
pub(crate) fn unsafe_path_failure(error: impl std::fmt::Display) -> anyhow::Error {
    tagged_failure(ContinuousFailureKind::UnsafePath, error)
}

/// Tags an error as retryable volatility (file or head changed during a read
/// or CAS; safe to retry after a fresh observation).
pub(crate) fn retryable_volatility_failure(error: impl std::fmt::Display) -> anyhow::Error {
    tagged_failure(ContinuousFailureKind::RetryableVolatility, error)
}

/// Tags an error as an unsupported snapshot format or state schema failure
/// (attention, no retry).
pub(crate) fn unsupported_schema_failure(error: impl std::fmt::Display) -> anyhow::Error {
    tagged_failure(ContinuousFailureKind::UnsupportedSchema, error)
}

/// Tags an error as a materialization verification failure (attention, no
/// retry; the destination must be inspected before any automatic recovery).
pub(crate) fn verification_failure(error: impl std::fmt::Display) -> anyhow::Error {
    tagged_failure(ContinuousFailureKind::VerificationFailed, error)
}

fn continuous_failure_kind(error: &anyhow::Error) -> Option<ContinuousFailureKind> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<ContinuousFailure>()
            .map(|marker| marker.kind)
    })
}

/// How the controller must react to one failed operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuousErrorClass {
    /// Transient: retry after debounce or backoff with one rerun bit.
    Retryable,
    /// Fail-closed: pause automatic mutation until explicit action.
    Attention(ContinuousAttention),
}

/// Classifies one failed land/refresh/sync error against the shared
/// contract. Retryable transport and file volatility retry; conflicts,
/// unsafe layouts, corrupt state, and unsupported schemas enter attention.
/// Unknown failures fail closed to attention and are never retried.
pub fn classify_continuous_error(error: &anyhow::Error) -> ContinuousErrorClass {
    if is_retryable_transport_error(error) {
        return ContinuousErrorClass::Retryable;
    }
    if crate::lock::is_lock_contention(error) {
        return ContinuousErrorClass::Retryable;
    }
    if durable::commit_durability_is_uncertain(error) {
        return ContinuousErrorClass::Retryable;
    }
    if let Some(kind) = continuous_failure_kind(error) {
        return match kind {
            ContinuousFailureKind::RetryableVolatility => ContinuousErrorClass::Retryable,
            kind => ContinuousErrorClass::Attention(ContinuousAttention {
                reason: kind.attention_reason().to_string(),
                detail: bounded_detail(error),
            }),
        };
    }
    ContinuousErrorClass::Attention(ContinuousAttention {
        reason: "corrupt_state".to_string(),
        detail: bounded_detail(error),
    })
}

/// Bounded, control-character-safe error detail for status output.
fn bounded_detail(error: &anyhow::Error) -> String {
    let mut out = String::new();
    for character in format!("{error:#}").chars().take(1024) {
        if character.is_control() {
            out.extend(character.escape_default());
        } else {
            out.push(character);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Guarded entry points (controller-facing, no merge authority)
// ---------------------------------------------------------------------------

/// Lands an agent whose exact runner execution session is held by the caller.
/// Uses `clean=false` and `propose=false` — automatic outbound reconciliation
/// never removes the agent and never writes `.proposed` merge artifacts.
pub async fn land_agent_guarded(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    session: &RunnerExecutionSession<'_>,
) -> Result<AgentLandResult> {
    land_agent_guarded_scoped(base, db, api, _workspace_id, name, _password, session, None).await
}

/// Scope-guarded variant of [`land_agent_guarded`]: only paths covered by the
/// accepted scope are published; everything else stays local and unlanded.
#[allow(clippy::too_many_arguments)]
pub async fn land_agent_guarded_scoped(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    session: &RunnerExecutionSession<'_>,
    scope: Option<&feanorfs_common::WorkScope>,
) -> Result<AgentLandResult> {
    session.ensure_matches(base, name)?;
    let guard = super::runner::RunnerOperationGuard::for_runner_session(session, base, name)?;
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    super::land::land_agent_with_ctx(&ctx, name, false, false, &guard, scope).await
}

/// Lands an agent whose interactive continuous ownership is held by the
/// caller (the `ContinuousOwnerLock` proves exclusivity).
pub async fn land_agent_continuous(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    owner: &ContinuousOwnerLock,
) -> Result<AgentLandResult> {
    owner.ensure_matches(base, name)?;
    let guard = super::runner::RunnerOperationGuard::acquire_continuous_async(base, name).await?;
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    super::land::land_agent_with_ctx(&ctx, name, false, false, &guard, None).await
}

/// Scope-guarded variant of [`land_agent_continuous`] for the interactive
/// controller: only paths covered by the accepted scope are published.
#[allow(clippy::too_many_arguments)]
pub async fn land_agent_continuous_scoped(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    owner: &ContinuousOwnerLock,
    scope: &feanorfs_common::WorkScope,
) -> Result<AgentLandResult> {
    owner.ensure_matches(base, name)?;
    let guard = super::runner::RunnerOperationGuard::acquire_continuous_async(base, name).await?;
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    super::land::land_agent_with_ctx(&ctx, name, false, false, &guard, Some(scope)).await
}

/// Refreshes an agent whose interactive continuous ownership is held by the
/// caller. Never uses `--replace`.
pub async fn refresh_agent_continuous(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    owner: &ContinuousOwnerLock,
) -> Result<AgentRefreshResult> {
    owner.ensure_matches(base, name)?;
    let _guard = super::runner::RunnerOperationGuard::acquire_continuous_async(base, name).await?;
    super::refresh::refresh_agent_impl(base, db, api, name, super::RefreshOptions::default()).await
}

/// Startup reconciliation probe: compares the workspace head, the agent base,
/// and the agent worktree without mutating anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuousProbe {
    pub current_head: Option<String>,
    /// Tree root of `current_head`, if any.
    pub head_tree: Option<String>,
    /// Agent base snapshot id.
    pub agent_base: Option<String>,
    /// Number of agent-local paths that differ from the base.
    pub local_changes: usize,
    /// True when the head tree equals the agent base tree.
    pub base_is_current: bool,
    /// Number of pre-existing three-way conflicts.
    pub conflicts: usize,
}

/// Reads current files/head instead of trusting stale controller status, so a
/// restarted controller reconciles from authoritative state.
pub async fn probe_agent_state(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    name: &str,
) -> Result<ContinuousProbe> {
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    let snapshots = SnapshotEngine::new(&ctx);
    let current_head = api.get_head(ctx.workspace_id()).await?;
    let head_tree = match current_head.as_deref() {
        Some(id) => Some(snapshots.load_snapshot(id).await?.root),
        None => None,
    };
    let agent_base = snapshots.read_agent_base(name).await.ok();
    let base_tree = match agent_base.as_deref() {
        Some(id) => Some(snapshots.load_snapshot(id).await?.root),
        None => None,
    };
    let diff = super::diff::compute_agent_diff(&ctx, name).await?;
    Ok(ContinuousProbe {
        base_is_current: base_tree.is_some() && base_tree == head_tree,
        current_head,
        head_tree,
        agent_base,
        local_changes: diff.our_changes.len(),
        conflicts: diff.conflicts.len(),
    })
}
/// Bounded, secret-free aggregation of live controller status files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveReconciliationHealth {
    pub agents_live: u32,
    pub agents_attention: u32,
    pub agents_offline: u32,
}

/// Reads only the bounded per-agent status projections (never worktree
/// contents) so routine tray/doctor refresh stays constant-cost.
pub fn live_reconciliation_health(base: &Path) -> Result<LiveReconciliationHealth> {
    let mut health = LiveReconciliationHealth::default();
    let Ok(entries) = std::fs::read_dir(crate::paths::agents_dir(base)?) else {
        return Ok(health);
    };
    for entry in entries.flatten().take(1000) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(Some(status)) = live_continuous_status(base, &name) else {
            continue;
        };
        if !status.active {
            continue;
        }
        health.agents_live = health.agents_live.saturating_add(1);
        if status.attention.is_some() {
            health.agents_attention = health.agents_attention.saturating_add(1);
        } else if status.phase == ContinuousPhase::Offline {
            health.agents_offline = health.agents_offline.saturating_add(1);
        }
    }
    Ok(health)
}

/// Lands an agent whose exact configured-runner ownership is carried by the
/// caller's revalidated identity token. Uses `clean=false`, `propose=false`.
pub async fn land_agent_runner_owned(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    ownership: &super::runner::RunnerOwnership,
) -> Result<AgentLandResult> {
    land_agent_runner_owned_scoped(
        base,
        db,
        api,
        _workspace_id,
        name,
        _password,
        ownership,
        None,
    )
    .await
}

/// Scope-guarded variant of [`land_agent_runner_owned`]: only paths covered
/// by the accepted scope are published; everything else stays local and
/// unlanded.
#[allow(clippy::too_many_arguments)]
pub async fn land_agent_runner_owned_scoped(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    ownership: &super::runner::RunnerOwnership,
    scope: Option<&feanorfs_common::WorkScope>,
) -> Result<AgentLandResult> {
    ownership.verify(base, name)?;
    let guard = super::runner::RunnerOperationGuard::for_runner_owned();
    let config = crate::local::load_config(base)?;
    let ctx = SyncCtx::from_config(api, db, base, &config)?;
    super::land::land_agent_with_ctx(&ctx, name, false, false, &guard, scope).await
}

/// Refreshes an agent whose exact configured-runner ownership is carried by
/// the caller's revalidated identity token. Never uses `--replace`.
pub async fn refresh_agent_runner_owned(
    base: &Path,
    db: &ClientDb,
    api: &ApiClient,
    _workspace_id: &str,
    name: &str,
    _password: Option<&str>,
    ownership: &super::runner::RunnerOwnership,
) -> Result<AgentRefreshResult> {
    ownership.verify(base, name)?;
    let _guard = super::runner::RunnerOperationGuard::for_runner_owned();
    super::refresh::refresh_agent_impl(base, db, api, name, super::RefreshOptions::default()).await
}

/// Maps a settled phase + result into the bounded status projection.
#[allow(clippy::too_many_arguments)]
pub fn build_status(
    agent: &str,
    active: bool,
    phase: ContinuousPhase,
    observed_head: Option<String>,
    observed_tree: Option<String>,
    settled_snapshot: Option<String>,
    pending_local: bool,
    deferred_count: u32,
    attention: Option<ContinuousAttention>,
    owner_pid: Option<u32>,
    owner_start_id: Option<String>,
) -> ContinuousAgentStatus {
    ContinuousAgentStatus {
        schema_version: CONTINUOUS_STATUS_SCHEMA_VERSION,
        agent: agent.to_string(),
        active,
        phase,
        observed_head,
        observed_tree,
        settled_snapshot,
        pending_local,
        deferred_count,
        attention,
        owner_pid,
        owner_start_id,
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        (dir, base)
    }

    #[tokio::test]
    async fn status_roundtrip_is_bounded_and_atomic() {
        let (_dir, base) = temp_base();
        let agent = "worker";
        // Spawn needs real setup; instead create the state dir manually.
        let state = agent_state_dir(&base, agent).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let status = build_status(
            agent,
            true,
            ContinuousPhase::Idle,
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into()),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
            false,
            0,
            None,
            Some(42),
            None,
        );
        write_continuous_status(&base, agent, &status).unwrap();
        let loaded = read_continuous_status(&base, agent).unwrap().unwrap();
        assert_eq!(loaded, status);
    }

    #[test]
    fn status_validation_rejects_wrong_identity_and_malformed_ids() {
        let (_dir, base) = temp_base();
        let agent = "worker";
        let state = agent_state_dir(&base, agent).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let mut status = build_status(
            agent,
            true,
            ContinuousPhase::Idle,
            Some("a".repeat(64)),
            Some("b".repeat(64)),
            Some("a".repeat(64)),
            false,
            0,
            None,
            Some(42),
            None,
        );
        status.agent = "other".to_string();
        assert!(write_continuous_status(&base, agent, &status).is_err());

        status.agent = agent.to_string();
        status.observed_head = Some("not-a-snapshot".to_string());
        std::fs::write(
            state.join(STATUS_FILE),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
        assert!(read_continuous_status(&base, agent).is_err());

        std::fs::write(
            state.join(STATUS_FILE),
            br#"{"schema_version":2,"phase":"future_phase"}"#,
        )
        .unwrap();
        assert!(read_continuous_status(&base, agent).unwrap().is_none());
    }

    #[test]
    fn health_counts_only_the_offline_phase_as_offline() {
        let (_dir, base) = temp_base();
        for agent in ["editing", "offline"] {
            std::fs::create_dir_all(agent_state_dir(&base, agent).unwrap()).unwrap();
        }
        let _editing_owner = ContinuousOwnerLock::try_acquire(&base, "editing")
            .unwrap()
            .expect("editing owner");
        let _offline_owner = ContinuousOwnerLock::try_acquire(&base, "offline")
            .unwrap()
            .expect("offline owner");
        write_continuous_status(
            &base,
            "editing",
            &build_status(
                "editing",
                true,
                ContinuousPhase::LocalDirty,
                Some("a".repeat(64)),
                Some("b".repeat(64)),
                None,
                true,
                0,
                None,
                Some(1),
                None,
            ),
        )
        .unwrap();
        write_continuous_status(
            &base,
            "offline",
            &build_status(
                "offline",
                true,
                ContinuousPhase::Offline,
                Some("a".repeat(64)),
                Some("b".repeat(64)),
                None,
                false,
                0,
                None,
                Some(2),
                None,
            ),
        )
        .unwrap();

        let health = live_reconciliation_health(&base).unwrap();
        assert_eq!(health.agents_live, 2);
        assert_eq!(health.agents_attention, 0);
        assert_eq!(health.agents_offline, 1);
    }

    #[test]
    fn classification_matrix_is_typed() {
        use crate::api::request_status_error;
        use crate::api::{api_failure_kind, ApiFailureKind};
        use http::StatusCode;

        // Retryable transport: every retryable server status class.
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            let error = request_status_error("GET", "/api/head", status, &[]);
            assert_eq!(
                classify_continuous_error(&error),
                ContinuousErrorClass::Retryable,
                "status {status} must retry"
            );
        }
        // Non-retryable statuses are not transport; without a typed marker
        // they fail closed to corrupt-state attention.
        let not_retryable =
            request_status_error("GET", "/api/head", StatusCode::NOT_IMPLEMENTED, &[]);
        match classify_continuous_error(&not_retryable) {
            ContinuousErrorClass::Retryable => panic!("501 must not retry"),
            ContinuousErrorClass::Attention(attention) => {
                assert_eq!(attention.reason, "corrupt_state")
            }
        }

        // Manifest missing blob is a typed endpoint/status kind, never text.
        let manifest_rejection = request_status_error(
            "POST",
            "/api/manifest",
            StatusCode::PRECONDITION_FAILED,
            b"412 precondition body",
        );
        assert_eq!(
            api_failure_kind(&manifest_rejection),
            Some(ApiFailureKind::ManifestReferencesMissingBlob)
        );
        // An unrelated 412 on another endpoint is not a manifest rejection.
        let unrelated_412 = request_status_error(
            "PUT",
            "/api/head",
            StatusCode::PRECONDITION_FAILED,
            b"412 precondition body",
        );
        assert_eq!(
            api_failure_kind(&unrelated_412),
            Some(ApiFailureKind::Other)
        );

        // Post-commit durability uncertainty retries; pre-commit failure does
        // not (it is not the typed marker and must fail closed).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.json");
        crate::durable::set_atomic_faults(crate::durable::AtomicFaults {
            fail_before_commit: false,
            fail_after_commit: true,
        });
        let uncertain = crate::durable::atomic_overwrite(&path, b"x").unwrap_err();
        crate::durable::set_atomic_faults(crate::durable::AtomicFaults::default());
        assert!(crate::durable::commit_durability_is_uncertain(&uncertain));
        assert_eq!(
            classify_continuous_error(&uncertain),
            ContinuousErrorClass::Retryable
        );

        crate::durable::set_atomic_faults(crate::durable::AtomicFaults {
            fail_before_commit: true,
            fail_after_commit: false,
        });
        let pre_commit = crate::durable::atomic_overwrite(&path, b"y").unwrap_err();
        crate::durable::set_atomic_faults(crate::durable::AtomicFaults::default());
        assert!(!crate::durable::commit_durability_is_uncertain(&pre_commit));
        match classify_continuous_error(&pre_commit) {
            ContinuousErrorClass::Retryable => panic!("pre-commit failure must not retry"),
            ContinuousErrorClass::Attention(attention) => {
                assert_eq!(attention.reason, "corrupt_state")
            }
        }

        // Typed producer markers classify regardless of human wording.
        let cases = [
            (
                conflict_failure(
                    "Your folder needs attention before landing agent work. Conflicts: a, b",
                ),
                "pending_conflicts",
            ),
            (
                conflict_failure("no pending conflict for a.txt"),
                "pending_conflicts",
            ),
            (
                unsafe_path_failure("agent worktree '/x' is not a real directory"),
                "unsafe_path",
            ),
            (
                unsafe_path_failure("workspace path a/b contains a symlink at a/b"),
                "unsafe_path",
            ),
            (
                unsafe_path_failure("server requested an unsafe download path: ../x"),
                "unsafe_path",
            ),
            (
                unsupported_schema_failure("agent runners require a format-v3 workspace"),
                "unsupported_schema",
            ),
            (
                unsupported_schema_failure(
                    "hub_state.json schema version 9 is newer than supported (max 1)",
                ),
                "unsupported_schema",
            ),
            (
                verification_failure(
                    "interrupted materialization x changed; refusing automatic recovery",
                ),
                "verification_failed",
            ),
        ];
        for (error, expected_reason) in cases {
            match classify_continuous_error(&error) {
                ContinuousErrorClass::Retryable => {
                    panic!("{expected_reason} must not retry")
                }
                ContinuousErrorClass::Attention(attention) => {
                    assert_eq!(attention.reason, expected_reason);
                }
            }
        }
        // Retryable volatility classifies as Retryable for every wording.
        for message in [
            "local path x changed while downloads were staged",
            "workspace head changed too many times while publishing snapshot",
            "local conflict version x changed during capture",
            "worktree changed during undo",
            "completely unrelated wording with no hint",
        ] {
            let error = retryable_volatility_failure(message);
            assert_eq!(
                classify_continuous_error(&error),
                ContinuousErrorClass::Retryable,
                "volatility marker must retry for: {message}"
            );
        }

        // Error-chain wrapping with multiple context layers preserves the
        // typed classification.
        let wrapped = conflict_failure("base conflict")
            .context("outer layer")
            .context("inner layer");
        match classify_continuous_error(&wrapped) {
            ContinuousErrorClass::Retryable => panic!("conflict must not retry"),
            ContinuousErrorClass::Attention(attention) => {
                assert_eq!(attention.reason, "pending_conflicts");
            }
        }
        let wrapped = retryable_volatility_failure("changed while read").context("outer layer");
        assert_eq!(
            classify_continuous_error(&wrapped),
            ContinuousErrorClass::Retryable
        );
    }

    #[test]
    fn misleading_text_alone_never_classifies() {
        // Every needle that used to drive the text table is inert when no
        // typed marker exists: plain text fails closed to corrupt-state
        // attention, never retry.
        for message in [
            "Your folder needs attention before landing agent work. Conflicts: a, b",
            "no pending conflict for a.txt",
            "workspace head changed too many times during conflict resolution",
            "local conflict version a changed during capture",
            "agent runners require a format-v3 workspace",
            "hub_state.json schema version 9 is newer than supported (max 1)",
            "workspace path x contains a symlink at y",
            "server requested an unsafe download path: ../x",
            "workspace file x changed while it was being read",
            "interrupted materialization x changed; refusing automatic recovery",
            "local upload source x changed after it was scanned; retry sync",
        ] {
            let error = anyhow::anyhow!("{message}");
            match classify_continuous_error(&error) {
                ContinuousErrorClass::Retryable => {
                    panic!("plain text must never retry: {message}")
                }
                ContinuousErrorClass::Attention(attention) => {
                    assert_eq!(
                        attention.reason, "corrupt_state",
                        "plain text must fail closed for: {message}"
                    );
                }
            }
        }
        // A genuine transport failure still retries even without a marker.
        let retryable = crate::api::request_status_error(
            "GET",
            "/api/head",
            http::StatusCode::SERVICE_UNAVAILABLE,
            b"temporary",
        );
        assert_eq!(
            classify_continuous_error(&retryable),
            ContinuousErrorClass::Retryable
        );
    }

    #[tokio::test]
    async fn owner_lock_is_exclusive() {
        let (_dir, base) = temp_base();
        let agent = "worker";
        let state = agent_state_dir(&base, agent).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let first = ContinuousOwnerLock::try_acquire(&base, agent)
            .unwrap()
            .expect("first owner");
        assert!(
            ContinuousOwnerLock::try_acquire(&base, agent)
                .unwrap()
                .is_none(),
            "second owner rejected"
        );
        drop(first);
        assert!(
            ContinuousOwnerLock::try_acquire(&base, agent)
                .unwrap()
                .is_some(),
            "owner released after drop"
        );
    }

    #[test]
    fn owner_lock_is_bound_to_its_workspace_and_agent() {
        let (_dir, base) = temp_base();
        for agent in ["first", "second"] {
            let state = agent_state_dir(&base, agent).unwrap();
            std::fs::create_dir_all(&state).unwrap();
        }
        let owner = ContinuousOwnerLock::try_acquire(&base, "first")
            .unwrap()
            .expect("first owner");
        assert!(owner.ensure_matches(&base, "first").is_ok());
        assert!(owner.ensure_matches(&base, "second").is_err());

        let other = tempfile::tempdir().unwrap();
        let state = agent_state_dir(other.path(), "first").unwrap();
        std::fs::create_dir_all(state).unwrap();
        assert!(owner.ensure_matches(other.path(), "first").is_err());
    }

    #[test]
    fn worktree_identity_fails_closed() {
        let (_dir, base) = temp_base();
        let agent = "worker";
        let state = agent_state_dir(&base, agent).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        // No worktree yet.
        assert!(verify_agent_worktree(&base, agent).is_err());
        let worktree = crate::paths::agent_dir(&base, agent).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        assert!(verify_agent_worktree(&base, agent).is_ok());
        // Replace with a symlink: must fail closed.
        std::fs::remove_dir(&worktree).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", &worktree).unwrap();
            assert!(verify_agent_worktree(&base, agent).is_err());
        }
    }
}
