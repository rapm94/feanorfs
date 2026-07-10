use anyhow::{bail, Result};
use feanorfs_common::LegacyPolicy;
use std::path::{Path, PathBuf};

use crate::local::Config;

#[must_use]
pub fn agents_dir(base: &Path) -> PathBuf {
    base.join(".feanorfs").join("agents")
}

#[must_use]
pub fn agent_dir(base: &Path, name: &str) -> PathBuf {
    agents_dir(base).join(name)
}

#[must_use]
pub fn agent_base_ref(base: &Path, name: &str) -> PathBuf {
    agent_dir(base, name)
        .join(".feanorfs")
        .join("base-snapshot")
}

#[must_use]
pub fn conflicts_dir(base: &Path) -> PathBuf {
    base.join(".feanorfs").join("conflicts")
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Agent name must not be empty");
    }
    if name.chars().any(|c| c.is_control()) {
        bail!("Agent name must not contain control characters: '{}'", name);
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        bail!("Agent name must be a single path segment: '{}'", name);
    }
    Ok(())
}

pub fn legacy_policy_for_config(config: &Config) -> LegacyPolicy {
    LegacyPolicy::from_format_version(config.format_version)
}
