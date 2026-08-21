//! Runner lifecycle, operation, and lifetime locks.

use crate::agent::continuous::ContinuousOwnerLock;
use crate::paths::validate_name;
use anyhow::{bail, ensure, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use super::session::RunnerExecutionSession;
use super::store::{
    ensure_existing_real_dir, ensure_private_dir, ensure_regular_or_missing, find_configured,
    runner_agent_root, runner_dir_path, set_private_file, validate_agent_layout,
    validate_generation_id, RunnerStore,
};
#[cfg(test)]
use super::test_hooks::{notify_lifecycle_lock_contention, pause_operation_guard_if_requested};

const LIFETIME_LOCK: &str = "runner-lifetime.lock";
pub(super) const CONFIGURE_LOCK: &str = "runner-configure.lock";
pub(super) fn ensure_no_interactive_owner(
    base: &Path,
    agent: &str,
    lifecycle: &RunnerLifecycleLock,
) -> Result<()> {
    let available = ContinuousOwnerLock::try_acquire_locked(base, agent, lifecycle)?;
    ensure!(
        available.is_some(),
        "an active `agent run` owns continuous reconciliation for agent '{agent}'"
    );
    Ok(())
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
/// Canonical workspace plus agent-root identity carried by a lifetime lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunnerIdentity {
    pub(super) canonical_workspace: PathBuf,
    pub(super) canonical_agent_root: PathBuf,
    pub(super) agent: String,
    pub(super) generation_id: String,
}

impl RunnerIdentity {
    pub(super) fn capture(base: &Path, agent: &str, generation_id: &str) -> Result<Self> {
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
    pub(super) fn try_acquire_exact_store(
        base: &Path,
        store: &RunnerStore,
    ) -> Result<Option<Self>> {
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
    pub(super) fn try_acquire_configured(base: &Path, agent: &str) -> Result<Option<Self>> {
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

    pub(super) fn try_acquire_internal(
        base: &Path,
        agent: &str,
        create: bool,
        generation_id: &str,
    ) -> Result<Self> {
        Self::try_acquire_internal_optional(base, agent, create, generation_id)?
            .with_context(|| format!("another local runner is already active for agent '{agent}'"))
    }

    /// Nonblocking lifetime-lock probe used by read-only liveness checks.
    /// Mutating callers keep the historical error-on-contention contract via
    /// [`Self::try_acquire_internal`].
    fn try_acquire_internal_optional(
        base: &Path,
        agent: &str,
        create: bool,
        generation_id: &str,
    ) -> Result<Option<Self>> {
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
            Ok(()) => Ok(Some(Self {
                _file: file,
                identity,
            })),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                Ok(None)
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

    pub(super) fn ensure_store(&self, store: &RunnerStore) -> Result<()> {
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
    Configured {
        _lease: RunnerLifetimeLock,
    },
    Unconfigured {
        _lifecycle: RunnerLifecycleLock,
    },
    /// Caller holds the exact runner execution session; used by the
    /// continuous controller inside the runner worker.
    RunnerOwned,
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
            // Manual land/refresh must not race an active `agent run`
            // continuous owner on an unconfigured agent.
            match ContinuousOwnerLock::try_acquire_locked(base, agent, &lifecycle)? {
                Some(_released) => {}
                None => bail!(
                    "an active `agent run` owns continuous reconciliation for agent '{agent}'; \
                     exit it before manual land/refresh"
                ),
            }
            Self::Unconfigured {
                _lifecycle: lifecycle,
            }
        };
        #[cfg(test)]
        pause_operation_guard_if_requested(base, agent);
        Ok(guard)
    }

    /// Guard for a caller that revalidated a [`RunnerOwnership`] token.
    pub(crate) fn for_runner_owned() -> Self {
        Self::RunnerOwned
    }

    pub(crate) fn protects_configured_runner(&self) -> bool {
        matches!(self, Self::Configured { .. } | Self::RunnerOwned)
    }

    /// Guard for the continuous controller: the caller already holds the
    /// interactive owner lock, so no continuous-owner rejection applies.
    pub(crate) async fn acquire_continuous_async(base: &Path, agent: &str) -> Result<Self> {
        validate_name(agent)?;
        let lifecycle = RunnerLifecycleLock::acquire_async(base).await?;
        if RunnerLifetimeLock::try_acquire_configured_locked(base, agent, &lifecycle)?.is_some() {
            bail!("agent '{agent}' has a configured runner; use the runner-owned path");
        }
        Ok(Self::Unconfigured {
            _lifecycle: lifecycle,
        })
    }

    /// Guard proven by the exact runner execution session.
    pub(crate) fn for_runner_session(
        session: &RunnerExecutionSession<'_>,
        base: &Path,
        agent: &str,
    ) -> Result<Self> {
        session.ensure_matches(base, agent)?;
        Ok(Self::RunnerOwned)
    }
}
/// Whether a runner is configured for exactly this agent.
pub(crate) fn configured_runner_is(base: &Path, agent: &str) -> Result<bool> {
    Ok(find_configured(base)?.as_deref() == Some(agent))
}

/// Whether any live process currently holds the runner lifetime lease for the
/// configured agent. A `None` result from a nonblocking acquisition attempt
pub(crate) fn runner_lifetime_held(base: &Path, agent: &str) -> Result<bool> {
    validate_name(agent)?;
    let lifecycle_guard = RunnerLifecycleLock::acquire(base)?;
    if find_configured(base)?.as_deref() != Some(agent) {
        return Ok(false);
    }
    let store = RunnerStore::open_existing(base, agent)?;
    let held = RunnerLifetimeLock::try_acquire_internal_optional(
        base,
        agent,
        false,
        &store.identity.generation_id,
    )?
    .is_none();
    drop(lifecycle_guard);
    Ok(held)
}
