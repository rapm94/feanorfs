use anyhow::{bail, Context as _, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::local::ClientDb;
use crate::paths::{agent_dir, agent_runtime_dir};

const MIGRATION_LOCK: &str = "runtime-migration.lock";

pub(super) struct AgentRuntime {
    pub(super) state_dir: PathBuf,
    pub(super) db: ClientDb,
}

pub(super) async fn open_agent_runtime(base: &Path, name: &str) -> Result<AgentRuntime> {
    let state_dir = prepare_agent_runtime_state(base, name)?;
    let db = ClientDb::new(&state_dir).await?;
    Ok(AgentRuntime { state_dir, db })
}

/// Prepare agent-owned cache state without registering the worktree as a
/// top-level workspace. A verified legacy cache is copied, never removed.
pub(super) fn prepare_agent_runtime_state(base: &Path, name: &str) -> Result<PathBuf> {
    let state_dir = agent_runtime_dir(base, name)?;
    let owner_state = state_dir
        .parent()
        .context("agent runtime state has no owning state directory")?;
    ensure_private_dir(owner_state)?;

    let lock_path = owner_state.join(MIGRATION_LOCK);
    reject_non_file(&lock_path, "agent runtime migration lock")?;
    let _migration_guard = crate::durable::create_lock_acquire_exclusive(&lock_path)?;
    ensure_private_dir(&state_dir)?;

    let destination = state_dir.join("local_state.json");
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            return Ok(state_dir);
        }
        Ok(_) => bail!(
            "agent runtime local state is not a regular file: {}",
            destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect agent runtime local state"),
    }

    migrate_legacy_local_state(base, name, &state_dir)?;
    Ok(state_dir)
}

fn migrate_legacy_local_state(base: &Path, name: &str, destination: &Path) -> Result<()> {
    let worktree = agent_dir(base, name)?;
    let legacy = crate::workspace_layout::legacy_workspace_state_path(&worktree)?;
    match fs::symlink_metadata(&legacy) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!(
            "legacy agent runtime state is not a regular directory: {}",
            legacy.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect legacy agent runtime state"),
    }

    if !legacy_identity_matches(&worktree, &legacy)? {
        tracing::warn!(
            "Preserving unverified legacy agent cache at {}; rebuilding under its owner",
            legacy.display()
        );
        return Ok(());
    }
    crate::state::check_no_legacy_db(&legacy)?;

    let source = legacy.join("local_state.json");
    match fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!(
            "legacy agent local state is not a regular file: {}",
            source.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect legacy agent local state"),
    }

    let source_lock = legacy.join("local_state.lock");
    reject_non_file(&source_lock, "legacy agent local-state lock")?;
    let _source_guard = crate::durable::open_lock_shared(&source_lock)
        .context("lock legacy agent local state for migration")?;
    let content =
        crate::state::read_local_state_text(&source).context("read legacy agent local state")?;
    crate::state::LocalStateV1::from_json(&content).context("validate legacy agent local state")?;
    crate::durable::atomic_overwrite(&destination.join("local_state.json"), content.as_bytes())
        .context("copy legacy agent local state into owned runtime")?;
    Ok(())
}

fn legacy_identity_matches(worktree: &Path, legacy: &Path) -> Result<bool> {
    let identity = legacy.join("identity");
    match fs::symlink_metadata(&identity) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!(
            "legacy agent identity is not a regular file: {}",
            identity.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspect legacy agent identity"),
    }
    let stored = fs::read_to_string(identity).context("read legacy agent identity")?;
    crate::workspace_layout::workspace_identity_matches(worktree, stored.trim())
}

fn reject_non_file(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => bail!("{label} is not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {label}")),
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "agent runtime state is not a regular directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
