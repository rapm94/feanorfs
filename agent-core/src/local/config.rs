use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_format_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    pub workspace_id: String,
    pub encryption_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_password: Option<String>,
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    #[serde(default)]
    pub hub_local: bool,
}

pub const LOCAL_HUB_URL: &str = "feanorfs+local://hub";

impl Config {
    #[must_use]
    pub fn is_local_hub(&self) -> bool {
        self.hub_local || self.server_url == LOCAL_HUB_URL
    }

    #[must_use]
    pub fn hub_data_dir(&self, workspace: &Path) -> PathBuf {
        workspace.join(".feanorfs").join("hub-data")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_password: Option<String>,
}

pub fn validate_e2ee_key(key: &str, format_version: u32) -> Result<()> {
    if format_version < 2 {
        return Ok(());
    }
    if key.len() != 64 || !key.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        bail!(
            "Encryption key must be exactly 64 lowercase hex characters (generated keys only). \
             Human passphrases are not accepted on format v2 workspaces."
        );
    }
    Ok(())
}

pub fn load_config(base_path: &Path) -> Result<Config> {
    let config_path = base_path.join(".feanorfs").join("config.json");
    let content = fs::read_to_string(&config_path)
        .context("Could not read config file. Make sure you have initialized the client.")?;
    serde_json::from_str(&content).context("parse workspace config")
}

pub fn save_config(base_path: &Path, config: &Config) -> Result<()> {
    let fs_dir = base_path.join(".feanorfs");
    fs::create_dir_all(&fs_dir)?;
    let content = serde_json::to_string_pretty(config)?;
    fs::write(fs_dir.join("config.json"), content)?;
    Ok(())
}

fn global_config_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME or USERPROFILE environment variable not set")?;
    Ok(PathBuf::from(home).join(".feanorfs"))
}

pub fn load_global_config() -> Result<GlobalConfig> {
    let path = global_config_dir()?.join("global.json");
    let content = fs::read_to_string(&path).context(
        "No server connection found. Run 'feanorfs connect <URL>' first, or pass the URL directly to 'init'.",
    )?;
    serde_json::from_str(&content).context("parse global config")
}

pub fn save_global_config(config: &GlobalConfig) -> Result<()> {
    let dir = global_config_dir()?;
    fs::create_dir_all(&dir)?;
    let content = serde_json::to_string_pretty(config)?;
    fs::write(dir.join("global.json"), content)?;
    Ok(())
}
