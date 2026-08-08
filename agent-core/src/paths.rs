use anyhow::{bail, Result};
use feanorfs_common::LegacyPolicy;
use std::path::{Path, PathBuf};

use crate::local::Config;

pub fn agents_dir(base: &Path) -> Result<PathBuf> {
    Ok(crate::workspace_layout::ensure_workspace_state(base)?.join("agents"))
}

pub fn agent_root(base: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(agents_dir(base)?.join(name))
}

pub fn agent_dir(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agent_root(base, name)?.join("worktree"))
}

pub(crate) fn agent_state_dir(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agent_root(base, name)?.join("state"))
}

pub(crate) fn agent_runtime_dir(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agent_state_dir(base, name)?.join("runtime"))
}

pub fn agent_runner_dir(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agent_state_dir(base, name)?.join("runner"))
}

pub fn agent_base_ref(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agent_state_dir(base, name)?.join("base-snapshot"))
}

pub fn conflicts_dir(base: &Path) -> Result<PathBuf> {
    Ok(crate::workspace_layout::ensure_workspace_state(base)?.join("conflicts"))
}

pub fn validate_name(name: &str) -> Result<()> {
    if !feanorfs_common::is_valid_agent_name(name) {
        bail!(
            "Agent name must be a non-empty portable path segment of at most {} UTF-8 bytes: '{name}'",
            feanorfs_common::AGENT_NAME_MAX_BYTES
        );
    }
    Ok(())
}

pub fn legacy_policy_for_config(config: &Config) -> LegacyPolicy {
    LegacyPolicy::from_format_version(config.format_version)
}
