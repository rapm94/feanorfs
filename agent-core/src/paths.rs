use anyhow::{bail, Result};
use feanorfs_common::LegacyPolicy;
use std::path::{Path, PathBuf};

use crate::local::Config;

pub fn agents_dir(base: &Path) -> Result<PathBuf> {
    Ok(crate::workspace_layout::ensure_workspace_state(base)?.join("agents"))
}

pub fn agent_root(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agents_dir(base)?.join(name))
}

pub fn agent_dir(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agent_root(base, name)?.join("worktree"))
}

pub fn agent_base_ref(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(agent_root(base, name)?.join("state").join("base-snapshot"))
}

pub fn conflicts_dir(base: &Path) -> Result<PathBuf> {
    Ok(crate::workspace_layout::ensure_workspace_state(base)?.join("conflicts"))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Agent name must not be empty");
    }
    if name.chars().any(|c| c.is_control()) {
        bail!("Agent name must not contain control characters: '{name}'");
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        bail!("Agent name must be a single path segment: '{name}'");
    }
    Ok(())
}

pub fn legacy_policy_for_config(config: &Config) -> LegacyPolicy {
    LegacyPolicy::from_format_version(config.format_version)
}
