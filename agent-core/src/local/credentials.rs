use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use zeroize::Zeroizing;

use super::credential_platform::os_store_allowed;
use super::private_file::write_private_json;

const SERVICE: &str = "com.feanorfs.credentials";
const STORE_OS: &str = "os";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProtection {
    OsCredentialStore,
    PrivateFile,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Secrets {
    version: u8,
    pub encryption_password: Option<String>,
    pub server_password: Option<String>,
}

impl Secrets {
    pub(super) fn new(
        encryption_password: Option<String>,
        server_password: Option<String>,
    ) -> Self {
        Self {
            version: 1,
            encryption_password,
            server_password,
        }
    }

    fn is_empty(&self) -> bool {
        self.encryption_password.is_none() && self.server_password.is_none()
    }
}

#[derive(Deserialize)]
struct Marker {
    #[serde(default)]
    credential_store: Option<String>,
    #[serde(default)]
    credential_id: Option<String>,
}

pub(super) fn has_marker(content: &str) -> Result<bool> {
    Ok(marker(content)?.is_some())
}

pub(super) fn load(content: &str) -> Result<Option<Secrets>> {
    let Some(id) = marker(content)? else {
        return Ok(None);
    };
    ensure_os_store_allowed()?;
    let entry = keyring::Entry::new(SERVICE, &id).context("open OS credential entry")?;
    let encoded = Zeroizing::new(
        entry
            .get_password()
            .context("read FeanorFS credentials from the OS credential store")?,
    );
    let secrets: Secrets = serde_json::from_str(&encoded)
        .context("decode FeanorFS credentials from the OS credential store")?;
    if secrets.version != 1 {
        anyhow::bail!(
            "unsupported FeanorFS credential version {}",
            secrets.version
        );
    }
    Ok(Some(secrets))
}

pub(super) fn save(
    path: &Path,
    mut config: Value,
    secrets: Secrets,
    require_existing_store: bool,
) -> Result<CredentialProtection> {
    let existing_id = match fs::read_to_string(path) {
        Ok(content) => marker(&content)?,
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read existing credential reference"),
    };
    let os_store_allowed = os_store_allowed();
    if secrets.is_empty() {
        if let Some(id) = existing_id {
            clear(path, &config, &id)?;
        } else {
            write_value(path, &config)?;
        }
        return Ok(CredentialProtection::PrivateFile);
    }
    if !os_store_allowed && existing_id.is_none() {
        write_value(path, &config)?;
        return Ok(CredentialProtection::PrivateFile);
    }
    if !os_store_allowed {
        ensure_os_store_allowed()?;
    }

    let id = match existing_id {
        Some(id) => id,
        None => generate_id()?,
    };
    let entry = match keyring::Entry::new(SERVICE, &id) {
        Ok(entry) => entry,
        Err(_error) if !require_existing_store => {
            write_value(path, &config)?;
            return Ok(CredentialProtection::PrivateFile);
        }
        Err(error) => return Err(error).context("open OS credential entry"),
    };
    let previous = if require_existing_store {
        Some(Zeroizing::new(entry.get_password().context(
            "read existing FeanorFS credential before update",
        )?))
    } else {
        None
    };
    let encoded = Zeroizing::new(serde_json::to_string(&secrets)?);
    if let Err(error) = entry.set_password(&encoded) {
        if require_existing_store {
            return Err(error).context("update FeanorFS credentials in the OS credential store");
        }
        write_value(path, &config)?;
        return Ok(CredentialProtection::PrivateFile);
    }

    redact_and_mark(&mut config, &id)?;
    if let Err(error) = write_value(path, &config) {
        match previous {
            Some(previous) => {
                let _ = entry.set_password(&previous);
            }
            None => {
                let _ = entry.delete_credential();
            }
        }
        return Err(error).context("persist OS credential reference");
    }
    Ok(CredentialProtection::OsCredentialStore)
}

fn ensure_os_store_allowed() -> Result<()> {
    if os_store_allowed() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    anyhow::bail!(
        "this config uses macOS Keychain, but the current FeanorFS binary is unsigned; \
         install the signed macOS release or re-link this workspace"
    );
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!(
        "this config uses the OS credential store; unset FEANORFS_CREDENTIAL_STORE to update it"
    );
}

fn clear(path: &Path, config: &Value, id: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, id).context("open OS credential entry")?;
    let previous = Zeroizing::new(
        entry
            .get_password()
            .context("read FeanorFS credential before removal")?,
    );
    entry
        .delete_credential()
        .context("remove FeanorFS credential from the OS credential store")?;
    if let Err(error) = write_value(path, config) {
        let _ = entry.set_password(&previous);
        return Err(error).context("persist removal of OS credential reference");
    }
    Ok(())
}

fn marker(content: &str) -> Result<Option<String>> {
    let marker: Marker = serde_json::from_str(content).context("parse credential reference")?;
    match (marker.credential_store.as_deref(), marker.credential_id) {
        (None, None) => Ok(None),
        (Some(STORE_OS), Some(id)) if !id.is_empty() => Ok(Some(id)),
        (Some(store), _) => anyhow::bail!("unsupported credential store `{store}`"),
        _ => anyhow::bail!("incomplete OS credential reference"),
    }
}

fn generate_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate OS credential identifier")?;
    let mut id = String::with_capacity(37);
    id.push_str("fsc1-");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(id, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(id)
}

fn redact_and_mark(value: &mut Value, id: &str) -> Result<()> {
    let object = value
        .as_object_mut()
        .context("FeanorFS config must be a JSON object")?;
    object.remove("encryption_password");
    object.remove("server_password");
    object.insert("credential_store".into(), Value::String(STORE_OS.into()));
    object.insert("credential_id".into(), Value::String(id.into()));
    Ok(())
}

fn write_value(path: &Path, value: &Value) -> Result<()> {
    let content = Zeroizing::new(serde_json::to_string_pretty(value)?);
    write_private_json(path, &content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_config_contains_only_a_non_secret_reference() {
        let mut value = serde_json::json!({
            "server_url": "https://hub.example",
            "encryption_password": "secret-key",
            "server_password": "secret-token"
        });
        redact_and_mark(&mut value, "fsc1-public-id").unwrap();
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("encryption_password"));
        assert!(!object.contains_key("server_password"));
        assert_eq!(object["credential_store"], "os");
        assert_eq!(object["credential_id"], "fsc1-public-id");
    }

    #[test]
    fn malformed_markers_fail_closed() {
        assert!(marker(r#"{"credential_store":"os"}"#).is_err());
        assert!(marker(r#"{"credential_store":"future","credential_id":"x"}"#).is_err());
    }
}
