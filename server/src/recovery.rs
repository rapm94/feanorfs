use anyhow::{bail, Context as _, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chacha20poly1305::{
    aead::{Aead as _, KeyInit as _, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use rcgen::{Issuer, KeyPair};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::private_file::{
    atomic_private_write, create_private_dir, durable_remove_if_exists, open_private_lock,
};

const FORMAT_VERSION: u32 = 1;
const KDF_NAME: &str = "argon2id-v19";
const CIPHER_NAME: &str = "xchacha20poly1305";
const KDF_MEMORY_KIB: u32 = 64 * 1024;
const KDF_ITERATIONS: u32 = 3;
const KDF_LANES: u32 = 1;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 24;
const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
const MIN_PASSPHRASE_CHARS: usize = 12;
const RECOVERY_MARKER: &str = "recovery-import.json";
const RUNTIME_LOCK: &str = "hub-runtime.lock";

#[derive(Debug)]
pub struct HubRuntimeGuard {
    _lock: File,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryEnvelope {
    format_version: u32,
    kdf: String,
    cipher: String,
    salt: String,
    nonce: String,
    public_ca_fingerprint: String,
    ciphertext: String,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct RecoverySecrets {
    ca_cert_pem: String,
    ca_key_pem: String,
    auth_token: String,
}

#[derive(Serialize)]
struct AuthenticatedHeader<'a> {
    domain: &'static str,
    format_version: u32,
    kdf: &'a str,
    cipher: &'a str,
    salt: &'a str,
    nonce: &'a str,
    public_ca_fingerprint: &'a str,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryMarker {
    format_version: u32,
    bundle_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryExportResult {
    pub public_ca_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryImportResult {
    pub public_ca_fingerprint: String,
    pub resumed: bool,
    pub replaced_existing_identity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRotationResult {
    pub previous_public_ca_fingerprint: Option<String>,
    pub public_ca_fingerprint: String,
    pub recovery_bundle: PathBuf,
    pub resumed: bool,
}

pub fn acquire_hub_runtime(data_dir: &Path) -> Result<HubRuntimeGuard> {
    create_private_dir(data_dir)?;
    let lock = open_private_lock(&data_dir.join(RUNTIME_LOCK))?;
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| {
        anyhow::anyhow!(
            "hub data directory {} is in use; stop the hub before recovery or offline maintenance ({error})",
            data_dir.display()
        )
    })?;
    Ok(HubRuntimeGuard { _lock: lock })
}

pub fn ensure_recovery_complete(data_dir: &Path) -> Result<()> {
    let marker = data_dir.join(RECOVERY_MARKER);
    if marker.exists() {
        bail!(
            "hub identity maintenance is incomplete at {}; rerun the same `feanorfs serve recovery import` or `feanorfs serve recovery rotate` command before starting the hub",
            marker.display()
        );
    }
    Ok(())
}

pub fn export_recovery_bundle(
    data_dir: &Path,
    destination: &Path,
    passphrase: &str,
    replace_destination: bool,
) -> Result<RecoveryExportResult> {
    validate_passphrase(passphrase)?;
    let _guard = acquire_hub_runtime(data_dir)?;
    ensure_recovery_complete(data_dir)?;
    validate_recovery_destination(destination, replace_destination)?;

    let secrets = load_recovery_secrets(data_dir)?;
    validate_recovery_secrets(&secrets)?;
    let fingerprint = crate::tls::certificate_fingerprint(&secrets.ca_cert_pem);
    let envelope = seal(&secrets, passphrase, &fingerprint)?;
    let encoded = serde_json::to_vec_pretty(&envelope).context("encode recovery bundle")?;
    atomic_private_write(destination, &encoded)
        .with_context(|| format!("write recovery bundle {}", destination.display()))?;

    Ok(RecoveryExportResult {
        public_ca_fingerprint: fingerprint,
    })
}

pub fn import_recovery_bundle(
    data_dir: &Path,
    source: &Path,
    passphrase: &str,
    replace_existing_identity: bool,
) -> Result<RecoveryImportResult> {
    validate_passphrase(passphrase)?;
    let encoded =
        fs::read(source).with_context(|| format!("read recovery bundle {}", source.display()))?;
    if encoded.len() > MAX_BUNDLE_BYTES {
        bail!("recovery bundle exceeds {MAX_BUNDLE_BYTES} bytes");
    }
    let envelope: RecoveryEnvelope =
        serde_json::from_slice(&encoded).context("parse recovery bundle")?;
    let secrets = open(&envelope, passphrase)?;
    validate_recovery_secrets(&secrets)?;
    let fingerprint = crate::tls::certificate_fingerprint(&secrets.ca_cert_pem);
    if fingerprint != envelope.public_ca_fingerprint {
        bail!("recovery bundle CA fingerprint does not match its encrypted contents");
    }
    let bundle_hash = feanorfs_common::hash_bytes(&encoded);

    let _guard = acquire_hub_runtime(data_dir)?;
    let marker_path = data_dir.join(RECOVERY_MARKER);
    let marker = load_marker(&marker_path)?;
    let resumed = if let Some(marker) = &marker {
        if marker.format_version != FORMAT_VERSION || marker.bundle_hash != bundle_hash {
            bail!(
                "a different or unreadable recovery import is already pending at {}; resume with the original bundle",
                marker_path.display()
            );
        }
        true
    } else {
        false
    };

    let conflicts = identity_conflicts(data_dir, &secrets)?;
    if conflicts && !replace_existing_identity && !resumed {
        bail!(
            "the hub already has a different CA or token; rerun with --replace only if every existing client should keep using the identity from this bundle"
        );
    }

    if !resumed {
        let marker = RecoveryMarker {
            format_version: FORMAT_VERSION,
            bundle_hash,
        };
        let encoded_marker = serde_json::to_vec_pretty(&marker)?;
        atomic_private_write(&marker_path, &encoded_marker)
            .context("write durable recovery import fence")?;
    }

    install_recovery_secrets(data_dir, &secrets)?;
    durable_remove_if_exists(&marker_path).context("clear recovery import fence")?;

    Ok(RecoveryImportResult {
        public_ca_fingerprint: fingerprint,
        resumed,
        replaced_existing_identity: conflicts,
    })
}

pub fn rotate_hub_identity(
    data_dir: &Path,
    recovery_destination: &Path,
    passphrase: &str,
    replace_destination: bool,
) -> Result<IdentityRotationResult> {
    validate_passphrase(passphrase)?;

    if data_dir.join(RECOVERY_MARKER).exists() {
        let imported = import_recovery_bundle(data_dir, recovery_destination, passphrase, true)
            .context(
                "resume the pending hub identity rotation with its generated recovery bundle",
            )?;
        return Ok(IdentityRotationResult {
            previous_public_ca_fingerprint: None,
            public_ca_fingerprint: imported.public_ca_fingerprint,
            recovery_bundle: recovery_destination.to_path_buf(),
            resumed: true,
        });
    }

    let guard = acquire_hub_runtime(data_dir)?;
    ensure_recovery_complete(data_dir)?;
    validate_recovery_destination(recovery_destination, replace_destination)?;
    ensure_rotation_backup_is_external(data_dir, recovery_destination)?;

    let existing = load_recovery_secrets(data_dir)?;
    validate_recovery_secrets(&existing)?;
    let previous_fingerprint = crate::tls::certificate_fingerprint(&existing.ca_cert_pem);

    let (ca_cert_pem, ca_key_pem) = crate::tls::generate_private_ca()?;
    let rotated = RecoverySecrets {
        ca_cert_pem,
        ca_key_pem: ca_key_pem.to_string(),
        auth_token: feanorfs_common::generate_password()?,
    };
    validate_recovery_secrets(&rotated)?;
    let fingerprint = crate::tls::certificate_fingerprint(&rotated.ca_cert_pem);
    let envelope = seal(&rotated, passphrase, &fingerprint)?;
    let encoded = serde_json::to_vec_pretty(&envelope).context("encode rotated recovery bundle")?;
    atomic_private_write(recovery_destination, &encoded).with_context(|| {
        format!(
            "write rotated recovery bundle {}",
            recovery_destination.display()
        )
    })?;

    let marker = RecoveryMarker {
        format_version: FORMAT_VERSION,
        bundle_hash: feanorfs_common::hash_bytes(&encoded),
    };
    atomic_private_write(
        &data_dir.join(RECOVERY_MARKER),
        &serde_json::to_vec_pretty(&marker)?,
    )
    .context("write durable identity rotation fence")?;
    drop(guard);

    let imported = import_recovery_bundle(data_dir, recovery_destination, passphrase, true)
        .context(
        "install the rotated hub identity; rerun this command with the same bundle path to resume",
    )?;
    Ok(IdentityRotationResult {
        previous_public_ca_fingerprint: Some(previous_fingerprint),
        public_ca_fingerprint: imported.public_ca_fingerprint,
        recovery_bundle: recovery_destination.to_path_buf(),
        resumed: imported.resumed,
    })
}

fn load_recovery_secrets(data_dir: &Path) -> Result<RecoverySecrets> {
    let tls_dir = data_dir.join("tls");
    Ok(RecoverySecrets {
        ca_cert_pem: fs::read_to_string(tls_dir.join("ca-cert.pem"))
            .context("read hub CA certificate; start the hub once before exporting recovery")?,
        ca_key_pem: fs::read_to_string(tls_dir.join("ca-key.pem"))
            .context("read hub CA private key; restore the hub identity before exporting")?,
        auth_token: fs::read_to_string(data_dir.join("auth-token")).context(
            "read hub authentication token; start the hub once before exporting recovery",
        )?,
    })
}

fn validate_recovery_secrets(secrets: &RecoverySecrets) -> Result<()> {
    crate::serve::validate_auth_token(&secrets.auth_token)?;
    let key = KeyPair::from_pem(&secrets.ca_key_pem).context("parse recovery CA private key")?;
    Issuer::from_ca_cert_pem(&secrets.ca_cert_pem, key)
        .context("recovery CA certificate does not match its private key")?;
    Ok(())
}

fn seal(
    secrets: &RecoverySecrets,
    passphrase: &str,
    fingerprint: &str,
) -> Result<RecoveryEnvelope> {
    let mut salt_bytes = [0_u8; SALT_BYTES];
    let mut nonce_bytes = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut salt_bytes)
        .map_err(|error| anyhow::anyhow!("generate recovery salt: {error}"))?;
    getrandom::fill(&mut nonce_bytes)
        .map_err(|error| anyhow::anyhow!("generate recovery nonce: {error}"))?;
    let salt = BASE64.encode(salt_bytes);
    let nonce = BASE64.encode(nonce_bytes);
    let aad = authenticated_header(&salt, &nonce, fingerprint)?;
    let key_bytes = derive_key(passphrase, &salt_bytes)?;
    let key: &Key = key_bytes.as_ref().try_into().expect("32-byte recovery key");
    let cipher = XChaCha20Poly1305::new(key);
    let plaintext = Zeroizing::new(serde_json::to_vec(secrets)?);
    let xnonce: &XNonce = (&nonce_bytes).into();
    let ciphertext = cipher
        .encrypt(
            xnonce,
            Payload {
                msg: plaintext.as_ref(),
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("encrypt recovery bundle"))?;
    salt_bytes.zeroize();
    nonce_bytes.zeroize();

    Ok(RecoveryEnvelope {
        format_version: FORMAT_VERSION,
        kdf: KDF_NAME.into(),
        cipher: CIPHER_NAME.into(),
        salt,
        nonce,
        public_ca_fingerprint: fingerprint.into(),
        ciphertext: BASE64.encode(ciphertext),
    })
}

fn open(envelope: &RecoveryEnvelope, passphrase: &str) -> Result<RecoverySecrets> {
    validate_envelope(envelope)?;
    let salt = decode_exact::<SALT_BYTES>("salt", &envelope.salt)?;
    let nonce = decode_exact::<NONCE_BYTES>("nonce", &envelope.nonce)?;
    let ciphertext = BASE64
        .decode(&envelope.ciphertext)
        .context("decode recovery ciphertext")?;
    let aad = authenticated_header(
        &envelope.salt,
        &envelope.nonce,
        &envelope.public_ca_fingerprint,
    )?;
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
            .map_err(|_| {
                anyhow::anyhow!("recovery passphrase is incorrect or bundle was modified")
            })?,
    );
    serde_json::from_slice(&plaintext).context("decode encrypted recovery contents")
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

fn authenticated_header(salt: &str, nonce: &str, fingerprint: &str) -> Result<Vec<u8>> {
    serde_json::to_vec(&AuthenticatedHeader {
        domain: "feanorfs hub recovery bundle",
        format_version: FORMAT_VERSION,
        kdf: KDF_NAME,
        cipher: CIPHER_NAME,
        salt,
        nonce,
        public_ca_fingerprint: fingerprint,
    })
    .context("encode recovery authentication header")
}

fn validate_envelope(envelope: &RecoveryEnvelope) -> Result<()> {
    if envelope.format_version != FORMAT_VERSION {
        bail!(
            "unsupported recovery bundle format {}",
            envelope.format_version
        );
    }
    if envelope.kdf != KDF_NAME || envelope.cipher != CIPHER_NAME {
        bail!("unsupported recovery bundle cryptography");
    }
    Ok(())
}

fn validate_passphrase(passphrase: &str) -> Result<()> {
    if passphrase.chars().count() < MIN_PASSPHRASE_CHARS {
        bail!("recovery passphrase must contain at least {MIN_PASSPHRASE_CHARS} characters");
    }
    Ok(())
}

fn validate_recovery_destination(destination: &Path, replace_destination: bool) -> Result<()> {
    if destination.exists() && !replace_destination {
        bail!(
            "recovery bundle already exists at {}; pass --replace to overwrite it atomically",
            destination.display()
        );
    }
    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if !parent.is_dir() {
            bail!(
                "recovery bundle directory does not exist: {}",
                parent.display()
            );
        }
    }
    Ok(())
}

fn ensure_rotation_backup_is_external(data_dir: &Path, destination: &Path) -> Result<()> {
    let canonical_data_dir = fs::canonicalize(data_dir)
        .with_context(|| format!("resolve hub data directory {}", data_dir.display()))?;
    let destination_parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_destination_parent = fs::canonicalize(destination_parent).with_context(|| {
        format!(
            "resolve recovery bundle directory {}",
            destination_parent.display()
        )
    })?;
    if canonical_destination_parent.starts_with(&canonical_data_dir) {
        bail!(
            "the rotated recovery bundle must be stored outside the hub data directory so identity maintenance cannot overwrite hub state"
        );
    }
    Ok(())
}

fn decode_exact<const N: usize>(label: &str, encoded: &str) -> Result<[u8; N]> {
    let decoded = BASE64
        .decode(encoded)
        .with_context(|| format!("decode recovery {label}"))?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("recovery {label} has the wrong length"))
}

fn load_marker(path: &Path) -> Result<Option<RecoveryMarker>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("parse recovery import fence")
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn identity_conflicts(data_dir: &Path, secrets: &RecoverySecrets) -> Result<bool> {
    let expected = [
        (
            data_dir.join("tls/ca-cert.pem"),
            secrets.ca_cert_pem.as_bytes(),
        ),
        (
            data_dir.join("tls/ca-key.pem"),
            secrets.ca_key_pem.as_bytes(),
        ),
        (data_dir.join("auth-token"), secrets.auth_token.as_bytes()),
    ];
    for (path, contents) in expected {
        match fs::read(path) {
            Ok(existing) if existing != contents => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn install_recovery_secrets(data_dir: &Path, secrets: &RecoverySecrets) -> Result<()> {
    let tls_dir = data_dir.join("tls");
    create_private_dir(&tls_dir)?;
    atomic_private_write(&tls_dir.join("ca-key.pem"), secrets.ca_key_pem.as_bytes())?;
    atomic_private_write(&tls_dir.join("ca-cert.pem"), secrets.ca_cert_pem.as_bytes())?;
    atomic_private_write(&data_dir.join("auth-token"), secrets.auth_token.as_bytes())?;
    durable_remove_if_exists(&tls_dir.join("server-key.pem"))?;
    durable_remove_if_exists(&tls_dir.join("server-cert.pem"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &str = "correct horse battery staple";

    fn initialized_hub() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = crate::ServeOptions {
            data_dir: dir.path().to_path_buf(),
            ..crate::ServeOptions::default()
        };
        crate::prepare_tls(&mut opts).unwrap();
        crate::resolve_or_create_auth_token(dir.path(), None, false).unwrap();
        dir
    }

    #[test]
    fn encrypted_export_import_preserves_ca_and_token_and_refreshes_leaf() {
        let source = initialized_hub();
        let bundle_dir = tempfile::tempdir().unwrap();
        let bundle = bundle_dir.path().join("hub.fnr-recovery");
        let source_secrets = load_recovery_secrets(source.path()).unwrap();
        let exported = export_recovery_bundle(source.path(), &bundle, PASSPHRASE, false).unwrap();
        let encoded = fs::read_to_string(&bundle).unwrap();
        assert!(!encoded.contains(&source_secrets.auth_token));
        assert!(!encoded.contains("PRIVATE KEY"));

        let target = tempfile::tempdir().unwrap();
        fs::create_dir_all(target.path().join("tls")).unwrap();
        fs::write(target.path().join("tls/server-key.pem"), "old leaf").unwrap();
        let imported = import_recovery_bundle(target.path(), &bundle, PASSPHRASE, false).unwrap();
        assert_eq!(
            imported.public_ca_fingerprint,
            exported.public_ca_fingerprint
        );
        assert_eq!(
            load_recovery_secrets(target.path()).unwrap().auth_token,
            source_secrets.auth_token
        );
        let restored_secrets = load_recovery_secrets(target.path()).unwrap();
        assert_eq!(
            feanorfs_common::hub_mdns_hostname(&restored_secrets.ca_cert_pem),
            feanorfs_common::hub_mdns_hostname(&source_secrets.ca_cert_pem)
        );
        assert!(!target.path().join("tls/server-key.pem").exists());
        assert!(!target.path().join(RECOVERY_MARKER).exists());
    }

    #[test]
    fn wrong_passphrase_and_tampering_fail_closed() {
        let source = initialized_hub();
        let bundle_dir = tempfile::tempdir().unwrap();
        let bundle = bundle_dir.path().join("hub.fnr-recovery");
        export_recovery_bundle(source.path(), &bundle, PASSPHRASE, false).unwrap();
        let target = tempfile::tempdir().unwrap();
        assert!(
            import_recovery_bundle(target.path(), &bundle, "wrong password has length", false)
                .is_err()
        );
        assert!(!target.path().join("auth-token").exists());

        let mut envelope: RecoveryEnvelope =
            serde_json::from_slice(&fs::read(&bundle).unwrap()).unwrap();
        envelope.public_ca_fingerprint.push('0');
        fs::write(&bundle, serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert!(import_recovery_bundle(target.path(), &bundle, PASSPHRASE, false).is_err());
    }

    #[test]
    fn import_rejects_conflicts_without_replace_and_resumes_matching_fence() {
        let source = initialized_hub();
        let bundle_dir = tempfile::tempdir().unwrap();
        let bundle = bundle_dir.path().join("hub.fnr-recovery");
        export_recovery_bundle(source.path(), &bundle, PASSPHRASE, false).unwrap();
        let encoded = fs::read(&bundle).unwrap();
        let target = initialized_hub();
        assert!(import_recovery_bundle(target.path(), &bundle, PASSPHRASE, false).is_err());

        let marker = RecoveryMarker {
            format_version: FORMAT_VERSION,
            bundle_hash: feanorfs_common::hash_bytes(&encoded),
        };
        atomic_private_write(
            &target.path().join(RECOVERY_MARKER),
            &serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        assert!(ensure_recovery_complete(target.path()).is_err());
        let imported = import_recovery_bundle(target.path(), &bundle, PASSPHRASE, false).unwrap();
        assert!(imported.resumed);
        assert!(imported.replaced_existing_identity);
    }

    #[test]
    fn export_requires_offline_hub() {
        let source = initialized_hub();
        let _guard = acquire_hub_runtime(source.path()).unwrap();
        let bundle_dir = tempfile::tempdir().unwrap();
        assert!(export_recovery_bundle(
            source.path(),
            &bundle_dir.path().join("bundle"),
            PASSPHRASE,
            false
        )
        .is_err());
        assert!(rotate_hub_identity(
            source.path(),
            &bundle_dir.path().join("rotated"),
            PASSPHRASE,
            false
        )
        .is_err());
    }

    #[test]
    fn rotation_changes_identity_preserves_storage_and_writes_encrypted_backup() {
        let hub = initialized_hub();
        let old = load_recovery_secrets(hub.path()).unwrap();
        let old_fingerprint = crate::tls::certificate_fingerprint(&old.ca_cert_pem);
        let blobs = hub.path().join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join("opaque-object"), b"ciphertext").unwrap();
        fs::write(hub.path().join("db.sqlite"), b"opaque metadata").unwrap();

        let backup_dir = tempfile::tempdir().unwrap();
        let backup = backup_dir.path().join("rotated.recovery");
        let result = rotate_hub_identity(hub.path(), &backup, PASSPHRASE, false).unwrap();
        let rotated = load_recovery_secrets(hub.path()).unwrap();
        let encoded = fs::read_to_string(&backup).unwrap();

        assert_eq!(
            result.previous_public_ca_fingerprint.as_deref(),
            Some(old_fingerprint.as_str())
        );
        assert_eq!(
            result.public_ca_fingerprint,
            crate::tls::certificate_fingerprint(&rotated.ca_cert_pem)
        );
        assert_ne!(rotated.ca_cert_pem, old.ca_cert_pem);
        assert_ne!(rotated.ca_key_pem, old.ca_key_pem);
        assert_ne!(rotated.auth_token, old.auth_token);
        assert!(!encoded.contains(&rotated.auth_token));
        assert!(!encoded.contains("PRIVATE KEY"));
        assert_eq!(
            fs::read(blobs.join("opaque-object")).unwrap(),
            b"ciphertext"
        );
        assert_eq!(
            fs::read(hub.path().join("db.sqlite")).unwrap(),
            b"opaque metadata"
        );
        assert!(!hub.path().join("tls/server-key.pem").exists());
        assert!(!hub.path().join("tls/server-cert.pem").exists());
        assert!(!hub.path().join(RECOVERY_MARKER).exists());

        let restored = tempfile::tempdir().unwrap();
        import_recovery_bundle(restored.path(), &backup, PASSPHRASE, false).unwrap();
        let restored_identity = load_recovery_secrets(restored.path()).unwrap();
        assert_eq!(restored_identity.ca_cert_pem, rotated.ca_cert_pem);
        assert_eq!(restored_identity.auth_token, rotated.auth_token);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rotation_resumes_from_its_durable_bundle_fence() {
        let hub = initialized_hub();
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = backup_dir.path().join("rotated.recovery");
        let (ca_cert_pem, ca_key_pem) = crate::tls::generate_private_ca().unwrap();
        let staged = RecoverySecrets {
            ca_cert_pem,
            ca_key_pem: ca_key_pem.to_string(),
            auth_token: feanorfs_common::generate_password().unwrap(),
        };
        let fingerprint = crate::tls::certificate_fingerprint(&staged.ca_cert_pem);
        let envelope = seal(&staged, PASSPHRASE, &fingerprint).unwrap();
        let encoded = serde_json::to_vec_pretty(&envelope).unwrap();
        atomic_private_write(&backup, &encoded).unwrap();
        atomic_private_write(
            &hub.path().join(RECOVERY_MARKER),
            &serde_json::to_vec_pretty(&RecoveryMarker {
                format_version: FORMAT_VERSION,
                bundle_hash: feanorfs_common::hash_bytes(&encoded),
            })
            .unwrap(),
        )
        .unwrap();

        let result = rotate_hub_identity(hub.path(), &backup, PASSPHRASE, false).unwrap();
        assert!(result.resumed);
        assert_eq!(result.previous_public_ca_fingerprint, None);
        assert_eq!(result.public_ca_fingerprint, fingerprint);
        assert_eq!(
            load_recovery_secrets(hub.path()).unwrap().auth_token,
            staged.auth_token
        );
        assert!(!hub.path().join(RECOVERY_MARKER).exists());
    }

    #[test]
    fn rotation_backup_cannot_overwrite_hub_state() {
        let hub = initialized_hub();
        let old = load_recovery_secrets(hub.path()).unwrap();

        assert!(
            rotate_hub_identity(hub.path(), &hub.path().join("auth-token"), PASSPHRASE, true)
                .is_err()
        );
        let unchanged = load_recovery_secrets(hub.path()).unwrap();
        assert_eq!(unchanged.ca_cert_pem, old.ca_cert_pem);
        assert_eq!(unchanged.auth_token, old.auth_token);
        assert!(!hub.path().join(RECOVERY_MARKER).exists());
    }
}
