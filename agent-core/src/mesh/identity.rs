use anyhow::{ensure, Context as _, Result};
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use feanorfs_common::NodeId;
use std::path::Path;
use zeroize::{Zeroize as _, Zeroizing};

const MACHINE_IDENTITY_FILE: &str = "machine.json";
const MACHINE_IDENTITY_LOCK: &str = "machine.lock";

#[derive(Clone)]
pub struct MachineIdentity {
    signing_key: SigningKey,
}

impl std::fmt::Debug for MachineIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MachineIdentity")
            .field("node_id", &self.node_id())
            .finish()
    }
}

impl MachineIdentity {
    pub fn load_or_create() -> Result<Self> {
        let root = crate::workspace_layout::global_state_root()?;
        crate::local::create_private_dir(&root)?;
        Self::load_or_create_at(&root.join(MACHINE_IDENTITY_FILE), false)
    }

    fn load_or_create_at(path: &Path, private_only: bool) -> Result<Self> {
        let parent = path
            .parent()
            .context("machine identity has no parent directory")?;
        crate::local::create_private_dir(parent)?;
        let lock_path = parent.join(MACHINE_IDENTITY_LOCK);
        let _lock = crate::durable::create_lock_acquire_exclusive(&lock_path)?;
        set_private_file(&lock_path)?;

        if let Some(encoded) = crate::local::load_node_signing_key(path)? {
            return Ok(Self {
                signing_key: SigningKey::from_bytes(&decode_signing_key(&encoded)?),
            });
        }

        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret).context("generate mesh machine identity")?;
        let encoded = Zeroizing::new(encode_signing_key(&secret));
        if private_only {
            crate::local::save_node_signing_key_private(path, &encoded)?;
        } else {
            crate::local::save_node_signing_key(path, &encoded)?;
        }
        let signing_key = SigningKey::from_bytes(&secret);
        secret.zeroize();
        Ok(Self { signing_key })
    }

    #[must_use]
    pub fn node_id(&self) -> NodeId {
        NodeId::from_public_key(self.signing_key.verifying_key().to_bytes())
    }

    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing_key.sign(message).to_bytes()
    }

    #[must_use]
    pub fn verify(node_id: NodeId, message: &[u8], signature: &[u8; 64]) -> bool {
        VerifyingKey::from_bytes(node_id.as_bytes()).is_ok_and(|key| {
            key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(signature))
                .is_ok()
        })
    }

    #[cfg(test)]
    pub(crate) fn load_or_create_private(path: &Path) -> Result<Self> {
        Self::load_or_create_at(path, true)
    }
}

fn encode_signing_key(key: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in key {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_signing_key(encoded: &str) -> Result<[u8; 32]> {
    ensure!(
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "mesh machine identity must be 256-bit lowercase hex"
    );
    let mut key = [0_u8; 32];
    for (slot, pair) in key.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
        let high = hex_nibble(pair[0]).context("invalid mesh identity hex")?;
        let low = hex_nibble(pair[1]).context("invalid mesh identity hex")?;
        *slot = (high << 4) | low;
    }
    Ok(key)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn set_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_identity_is_stable_private_and_redacted() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("machine.json");

        let first = MachineIdentity::load_or_create_private(&path).unwrap();
        let second = MachineIdentity::load_or_create_private(&path).unwrap();

        assert_eq!(first.node_id(), second.node_id());
        let signature = first.sign(b"mesh handshake");
        assert!(MachineIdentity::verify(
            first.node_id(),
            b"mesh handshake",
            &signature
        ));
        assert!(!MachineIdentity::verify(
            first.node_id(),
            b"tampered handshake",
            &signature
        ));
        let persisted = std::fs::read_to_string(&path).unwrap();
        let secret = serde_json::from_str::<serde_json::Value>(&persisted).unwrap()
            ["node_signing_key"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!format!("{first:?}").contains(&secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
