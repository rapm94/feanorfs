use anyhow::{Context as _, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use zeroize::Zeroizing;

use super::credentials::{load, save, CredentialProtection, Secrets};
use super::private_file::write_private_json;

pub(crate) fn load_node_signing_key(path: &Path) -> Result<Option<Zeroizing<String>>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read machine identity"),
    };
    if let Some(secrets) = load(&content)? {
        return Ok(secrets.node_signing_key.map(Zeroizing::new));
    }
    let secrets: Secrets = serde_json::from_str(&content).context("parse machine identity")?;
    if !matches!(secrets.version(), 1 | 2) {
        anyhow::bail!("unsupported machine identity version {}", secrets.version());
    }
    Ok(secrets.node_signing_key.map(Zeroizing::new))
}

pub(crate) fn save_node_signing_key(
    path: &Path,
    node_signing_key: &str,
) -> Result<CredentialProtection> {
    let config = serde_json::json!({
        "version": 2,
        "node_signing_key": node_signing_key,
    });
    save(
        path,
        config,
        Secrets::machine(node_signing_key.to_string()),
        false,
    )
}

pub(crate) fn save_node_signing_key_private(path: &Path, node_signing_key: &str) -> Result<()> {
    let content = Zeroizing::new(serde_json::to_string_pretty(&Secrets::machine(
        node_signing_key.to_string(),
    ))?);
    write_private_json(path, &content)
}
