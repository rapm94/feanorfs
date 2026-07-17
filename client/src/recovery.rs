//! Passphrase-encrypted, offline workspace recovery kits.
//!
//! The envelope deliberately exposes only versioned cryptographic metadata.
//! The complete workspace capability remains authenticated ciphertext until a
//! client decrypts it locally and hands it to the ordinary `start` path.

use anyhow::{bail, Context as _, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chacha20poly1305::{
    aead::{Aead as _, KeyInit as _, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use feanorfs_common::WorkspaceInvite;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write as _;
use std::path::Path;
use zeroize::{Zeroize as _, Zeroizing};

const FORMAT_VERSION: u32 = 1;
const KDF_NAME: &str = "argon2id-v19";
const CIPHER_NAME: &str = "xchacha20poly1305";
const KDF_MEMORY_KIB: u32 = 64 * 1024;
const KDF_ITERATIONS: u32 = 3;
const KDF_LANES: u32 = 1;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 24;
const MIN_PASSPHRASE_CHARS: usize = 12;
const MAX_PASSPHRASE_CHARS: usize = 1024;
const MAX_KIT_BYTES: usize = 256 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryEnvelope {
    format_version: u32,
    kdf: String,
    cipher: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize)]
struct AuthenticatedHeader<'a> {
    domain: &'static str,
    format_version: u32,
    kdf: &'a str,
    cipher: &'a str,
    salt: &'a str,
    nonce: &'a str,
}

/// Encrypt a portable workspace capability and write it atomically with
/// private file permissions.
pub fn export_recovery_kit(
    destination: &Path,
    invite: &WorkspaceInvite,
    passphrase: &str,
    replace_destination: bool,
) -> Result<()> {
    validate_passphrase(passphrase)?;
    validate_invite(invite)?;
    validate_destination(destination, replace_destination)?;

    let envelope = seal(invite, passphrase)?;
    let encoded = serde_json::to_vec_pretty(&envelope).context("encode recovery kit")?;
    if encoded.len() > MAX_KIT_BYTES {
        bail!("encrypted recovery kit exceeds {MAX_KIT_BYTES} bytes");
    }
    atomic_private_write(destination, &encoded)
        .with_context(|| format!("write recovery kit {}", destination.display()))
}

/// Decrypt and validate a workspace capability without writing workspace
/// configuration. Callers can therefore fail on a wrong passphrase or a
/// modified kit before the normal onboarding path creates any local state.
pub fn open_recovery_kit(source: &Path, passphrase: &str) -> Result<WorkspaceInvite> {
    validate_passphrase(passphrase)?;
    let encoded =
        fs::read(source).with_context(|| format!("read recovery kit {}", source.display()))?;
    if encoded.len() > MAX_KIT_BYTES {
        bail!("recovery kit exceeds {MAX_KIT_BYTES} bytes");
    }
    let envelope: RecoveryEnvelope =
        serde_json::from_slice(&encoded).context("parse recovery kit")?;
    let invite = open(&envelope, passphrase)?;
    validate_invite(&invite)?;
    Ok(invite)
}

fn seal(invite: &WorkspaceInvite, passphrase: &str) -> Result<RecoveryEnvelope> {
    let mut salt_bytes = [0_u8; SALT_BYTES];
    let mut nonce_bytes = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut salt_bytes)
        .map_err(|error| anyhow::anyhow!("generate recovery salt: {error}"))?;
    getrandom::fill(&mut nonce_bytes)
        .map_err(|error| anyhow::anyhow!("generate recovery nonce: {error}"))?;

    let salt = BASE64.encode(salt_bytes);
    let nonce = BASE64.encode(nonce_bytes);
    let aad = authenticated_header(&salt, &nonce)?;
    let key_bytes = derive_key(passphrase, &salt_bytes)?;
    let key: &Key = key_bytes.as_ref().try_into().expect("32-byte recovery key");
    let cipher = XChaCha20Poly1305::new(key);
    let plaintext = Zeroizing::new(serde_json::to_vec(invite)?);
    let xnonce: &XNonce = (&nonce_bytes).into();
    let ciphertext = cipher
        .encrypt(
            xnonce,
            Payload {
                msg: plaintext.as_ref(),
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("encrypt recovery kit"))?;
    salt_bytes.zeroize();
    nonce_bytes.zeroize();

    Ok(RecoveryEnvelope {
        format_version: FORMAT_VERSION,
        kdf: KDF_NAME.into(),
        cipher: CIPHER_NAME.into(),
        salt,
        nonce,
        ciphertext: BASE64.encode(ciphertext),
    })
}

fn open(envelope: &RecoveryEnvelope, passphrase: &str) -> Result<WorkspaceInvite> {
    validate_envelope(envelope)?;
    let salt = decode_exact::<SALT_BYTES>("salt", &envelope.salt)?;
    let nonce = decode_exact::<NONCE_BYTES>("nonce", &envelope.nonce)?;
    let ciphertext = BASE64
        .decode(&envelope.ciphertext)
        .context("decode recovery ciphertext")?;
    let aad = authenticated_header(&envelope.salt, &envelope.nonce)?;
    let key_bytes = derive_key(passphrase, &salt)?;
    let key: &Key = key_bytes.as_ref().try_into().expect("32-byte recovery key");
    let cipher = XChaCha20Poly1305::new(key);
    let xnonce: &XNonce = (&nonce).into();
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                xnonce,
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("recovery passphrase is incorrect or kit was modified"))?,
    );
    serde_json::from_slice(&plaintext).context("decode encrypted recovery capability")
}

fn derive_key(passphrase: &str, salt: &[u8; SALT_BYTES]) -> Result<Zeroizing<[u8; 32]>> {
    let params = Params::new(KDF_MEMORY_KIB, KDF_ITERATIONS, KDF_LANES, Some(32))
        .map_err(|error| anyhow::anyhow!("configure recovery KDF: {error}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; 32]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|error| anyhow::anyhow!("derive recovery key: {error}"))?;
    Ok(key)
}

fn authenticated_header(salt: &str, nonce: &str) -> Result<Vec<u8>> {
    serde_json::to_vec(&AuthenticatedHeader {
        domain: "feanorfs workspace recovery kit",
        format_version: FORMAT_VERSION,
        kdf: KDF_NAME,
        cipher: CIPHER_NAME,
        salt,
        nonce,
    })
    .context("encode recovery authentication header")
}

fn validate_envelope(envelope: &RecoveryEnvelope) -> Result<()> {
    if envelope.format_version != FORMAT_VERSION {
        bail!(
            "unsupported recovery kit format {}",
            envelope.format_version
        );
    }
    if envelope.kdf != KDF_NAME || envelope.cipher != CIPHER_NAME {
        bail!("unsupported recovery kit cryptography");
    }
    Ok(())
}

fn validate_passphrase(passphrase: &str) -> Result<()> {
    let chars = passphrase.chars().count();
    if chars < MIN_PASSPHRASE_CHARS {
        bail!("recovery passphrase must contain at least {MIN_PASSPHRASE_CHARS} characters");
    }
    if chars > MAX_PASSPHRASE_CHARS {
        bail!("recovery passphrase exceeds {MAX_PASSPHRASE_CHARS} characters");
    }
    Ok(())
}

fn validate_invite(invite: &WorkspaceInvite) -> Result<()> {
    if invite.hub_local {
        bail!(
            "embedded local-hub workspaces are not portable; run `feanorfs start --host` in a new folder before creating a recovery kit"
        );
    }
    if invite.workspace_id.trim().is_empty() {
        bail!("recovery capability has an empty workspace ID");
    }
    crate::validate_e2ee_key(&invite.encryption_key, 3)?;
    let url = reqwest::Url::parse(&invite.server_url)
        .context("recovery capability has an invalid server URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("recovery capability server URL must use HTTP or HTTPS and include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("recovery capability must not place credentials in the server URL");
    }
    if invite.tls_ca_pem.is_some() && url.scheme() != "https" {
        bail!("recovery capability with a private CA must use HTTPS");
    }
    Ok(())
}

fn decode_exact<const N: usize>(name: &str, encoded: &str) -> Result<[u8; N]> {
    let decoded = BASE64
        .decode(encoded)
        .with_context(|| format!("decode recovery {name}"))?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("recovery {name} must contain exactly {N} bytes"))
}

fn validate_destination(destination: &Path, replace_destination: bool) -> Result<()> {
    if destination.exists() && !replace_destination {
        bail!(
            "recovery kit already exists at {}; pass --replace to overwrite it atomically",
            destination.display()
        );
    }
    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if !parent.is_dir() {
            bail!(
                "recovery kit directory does not exist: {}",
                parent.display()
            );
        }
    }
    Ok(())
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(bytes)?;
    file.commit()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::RelayConfig;

    const PASSPHRASE: &str = "correct horse battery staple";

    fn invite() -> WorkspaceInvite {
        WorkspaceInvite {
            server_url: "https://feanorfs-private.local:3030".into(),
            workspace_id: "fsw1-0123456789abcdef0123456789abcdef".into(),
            server_token: Some("server-secret-token".into()),
            encryption_key: "a".repeat(64),
            tls_ca_pem: Some("public-ca-certificate".into()),
            hub_local: false,
            relay: Some(RelayConfig {
                url: "https://relay.example".into(),
                route: "opaque-secret-route".into(),
            }),
        }
    }

    #[test]
    fn round_trip_hides_complete_capability_and_uses_private_permissions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workspace.fnrk");
        let invite = invite();
        export_recovery_kit(&path, &invite, PASSPHRASE, false).unwrap();

        let encoded = fs::read(&path).unwrap();
        for secret in [
            invite.server_url.as_str(),
            invite.workspace_id.as_str(),
            invite.server_token.as_deref().unwrap(),
            invite.encryption_key.as_str(),
            invite.tls_ca_pem.as_deref().unwrap(),
            invite.relay.as_ref().unwrap().route.as_str(),
        ] {
            assert!(!encoded
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()));
        }
        assert_eq!(open_recovery_kit(&path, PASSPHRASE).unwrap(), invite);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn wrong_passphrase_tamper_and_overwrite_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workspace.fnrk");
        export_recovery_kit(&path, &invite(), PASSPHRASE, false).unwrap();
        let original = fs::read(&path).unwrap();

        let wrong = open_recovery_kit(&path, "another valid passphrase").unwrap_err();
        assert!(wrong.to_string().contains("incorrect or kit was modified"));
        assert!(export_recovery_kit(&path, &invite(), PASSPHRASE, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);

        let mut envelope: RecoveryEnvelope = serde_json::from_slice(&original).unwrap();
        envelope.nonce.replace_range(..1, "A");
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let tampered = open_recovery_kit(&path, PASSPHRASE).unwrap_err();
        assert!(tampered
            .to_string()
            .contains("incorrect or kit was modified"));
    }

    #[test]
    fn rejects_nonportable_or_weak_capabilities_before_write() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workspace.fnrk");

        let mut bad = invite();
        bad.hub_local = true;
        assert!(export_recovery_kit(&path, &bad, PASSPHRASE, false).is_err());
        assert!(!path.exists());

        bad = invite();
        bad.encryption_key = "human-passphrase".into();
        assert!(export_recovery_kit(&path, &bad, PASSPHRASE, false).is_err());
        assert!(!path.exists());
    }
}
