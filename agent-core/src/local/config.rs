use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::credentials::{self, Secrets};
use super::private_file::{create_private_dir, write_private_json};

pub use super::credentials::CredentialProtection;

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
    /// Optional public CA certificate used to authenticate a private native-TLS hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    #[serde(default)]
    pub hub_local: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<feanorfs_common::RelayConfig>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<feanorfs_common::RelayConfig>,
}

pub fn validate_e2ee_key(key: &str, format_version: u32) -> Result<()> {
    if format_version < 2 {
        return Ok(());
    }
    if key.len() != 64 || !key.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        bail!(
            "Encryption key must be exactly 64 lowercase hex characters. \
             Use the generated recovery key; human passphrases are rejected for format-v2/v3 \
             workspaces because they can be brute-forced offline."
        );
    }
    Ok(())
}

pub fn load_config(base_path: &Path) -> Result<Config> {
    let config_path = base_path.join(".feanorfs").join("config.json");
    let content = std::fs::read_to_string(&config_path)
        .context("Could not read config file. Make sure you have initialized the client.")?;
    let mut config: Config = serde_json::from_str(&content).context("parse workspace config")?;
    if let Some(secrets) = credentials::load(&content)? {
        config.encryption_password = secrets.encryption_password;
        config.server_password = secrets.server_password;
    }
    Ok(config)
}

pub fn save_config(base_path: &Path, config: &Config) -> Result<()> {
    let fs_dir = base_path.join(".feanorfs");
    create_private_dir(&fs_dir)?;
    let path = fs_dir.join("config.json");
    if config_uses_os_store(&path)? {
        credentials::save(
            &path,
            serde_json::to_value(config)?,
            Secrets::new(
                config.encryption_password.clone(),
                config.server_password.clone(),
            ),
            true,
        )?;
        return Ok(());
    }
    let content = serde_json::to_string_pretty(config)?;
    write_private_json(&path, &content)
}

pub fn save_config_secure(base_path: &Path, config: &Config) -> Result<CredentialProtection> {
    let fs_dir = base_path.join(".feanorfs");
    create_private_dir(&fs_dir)?;
    let path = fs_dir.join("config.json");
    let require_existing = config_uses_os_store(&path)?;
    credentials::save(
        &path,
        serde_json::to_value(config)?,
        Secrets::new(
            config.encryption_password.clone(),
            config.server_password.clone(),
        ),
        require_existing,
    )
}

fn global_config_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME or USERPROFILE environment variable not set")?;
    Ok(PathBuf::from(home).join(".feanorfs"))
}

pub fn load_global_config() -> Result<GlobalConfig> {
    let path = global_config_dir()?.join("global.json");
    let content = std::fs::read_to_string(&path).context(
        "No server connection found. Run 'feanorfs connect <URL>' first, or pass the URL directly to 'init'.",
    )?;
    let mut config: GlobalConfig = serde_json::from_str(&content).context("parse global config")?;
    if let Some(secrets) = credentials::load(&content)? {
        config.server_password = secrets.server_password;
    }
    Ok(config)
}

pub fn save_global_config(config: &GlobalConfig) -> Result<()> {
    let dir = global_config_dir()?;
    create_private_dir(&dir)?;
    let path = dir.join("global.json");
    if config_uses_os_store(&path)? {
        credentials::save(
            &path,
            serde_json::to_value(config)?,
            Secrets::new(None, config.server_password.clone()),
            true,
        )?;
        return Ok(());
    }
    let content = serde_json::to_string_pretty(config)?;
    write_private_json(&path, &content)
}

pub fn save_global_config_secure(config: &GlobalConfig) -> Result<CredentialProtection> {
    let dir = global_config_dir()?;
    create_private_dir(&dir)?;
    let path = dir.join("global.json");
    let require_existing = config_uses_os_store(&path)?;
    credentials::save(
        &path,
        serde_json::to_value(config)?,
        Secrets::new(None, config.server_password.clone()),
        require_existing,
    )
}

fn config_uses_os_store(path: &Path) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(content) => credentials::has_marker(&content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("read existing FeanorFS config"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn workspace_credentials_are_private() {
        let workspace = tempfile::tempdir().unwrap();
        let config = Config {
            server_url: "https://example.test".into(),
            workspace_id: "private".into(),
            encryption_password: Some("a".repeat(64)),
            server_password: Some("server-token".into()),
            tls_ca_pem: Some("public-ca".into()),
            format_version: 3,
            hub_local: false,
            relay: None,
        };

        save_config(workspace.path(), &config).unwrap();

        let fs_dir = workspace.path().join(".feanorfs");
        assert_eq!(
            fs::metadata(&fs_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(fs_dir.join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        fs::set_permissions(
            fs_dir.join("config.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        save_config(workspace.path(), &config).unwrap();
        assert_eq!(
            fs::metadata(fs_dir.join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn legacy_configs_without_tls_ca_still_decode() {
        let workspace: Config = serde_json::from_str(
            r#"{"server_url":"http://127.0.0.1:3030","workspace_id":"legacy","encryption_password":null}"#,
        )
        .unwrap();
        assert_eq!(workspace.tls_ca_pem, None);
        assert_eq!(workspace.format_version, 1);
        assert!(!workspace.hub_local);
        assert_eq!(workspace.relay, None);

        let global: GlobalConfig = serde_json::from_str(
            r#"{"server_url":"https://hub.example","server_password":"token"}"#,
        )
        .unwrap();
        assert_eq!(global.tls_ca_pem, None);
        assert_eq!(global.relay, None);
    }
}
