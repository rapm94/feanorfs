use anyhow::{bail, Context as _, Result};
use rcgen::Issuer;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::private_file::{
    atomic_private_create_new, atomic_private_write, create_private_dir, durable_remove_if_exists,
    open_private_lock,
};
use feanorfs_common::sealed_envelope;

const RECOVERY_MARKER: &str = "recovery-import.json";
const RUNTIME_LOCK: &str = "hub-runtime.lock";

/// Hub recovery-bundle domain without the dynamic authenticated header
/// fields. `hub_domain(fingerprint)` clones this and binds the fingerprint
/// into the authenticated header.
const HUB_DOMAIN_BASE: sealed_envelope::EnvelopeDomain = sealed_envelope::EnvelopeDomain {
    domain: "feanorfs hub recovery bundle",
    format_version: sealed_envelope::FORMAT_VERSION,
    kdf: sealed_envelope::KDF_NAME,
    cipher: sealed_envelope::CIPHER_NAME,
    noun: "bundle",
    min_passphrase_chars: 12,
    max_passphrase_chars: None,
    max_plaintext_bytes: 1024 * 1024,
    max_envelope_bytes: 2 * 1024 * 1024,
    extra_header: Vec::new(),
};

fn hub_domain(fingerprint: &str) -> sealed_envelope::EnvelopeDomain {
    let mut domain = HUB_DOMAIN_BASE.clone();
    domain.extra_header = vec![("public_ca_fingerprint", fingerprint.to_string())];
    domain
}

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

impl RecoveryEnvelope {
    /// Move the shared envelope fields into the primitive's envelope type
    /// without copying; the server-specific `public_ca_fingerprint` field
    /// stays in the caller (it is bound into the authenticated header via
    /// the domain's extra header fields).
    fn into_sealed_envelope(self) -> sealed_envelope::SealedEnvelope {
        sealed_envelope::SealedEnvelope {
            format_version: self.format_version,
            kdf: self.kdf,
            cipher: self.cipher,
            salt: self.salt,
            nonce: self.nonce,
            ciphertext: self.ciphertext,
        }
    }
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct RecoverySecrets {
    ca_cert_pem: String,
    ca_key_pem: String,
    auth_token: String,
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
    sealed_envelope::validate_passphrase(&HUB_DOMAIN_BASE, passphrase)?;
    let _guard = acquire_hub_runtime(data_dir)?;
    ensure_recovery_complete(data_dir)?;
    let destination = ensure_recovery_bundle_is_external(data_dir, destination)?;
    validate_recovery_destination(&destination, replace_destination)?;

    let secrets = load_recovery_secrets(data_dir)?;
    validate_recovery_secrets(&secrets)?;
    let fingerprint = crate::tls::certificate_fingerprint(&secrets.ca_cert_pem);
    let envelope = seal(&secrets, passphrase, &fingerprint)?;
    let encoded = serde_json::to_vec_pretty(&envelope).context("encode recovery bundle")?;
    if encoded.len() > HUB_DOMAIN_BASE.max_envelope_bytes {
        bail!(
            "recovery bundle exceeds {} bytes",
            HUB_DOMAIN_BASE.max_envelope_bytes
        );
    }
    write_recovery_bundle(&destination, &encoded, replace_destination)
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
    sealed_envelope::validate_passphrase(&HUB_DOMAIN_BASE, passphrase)?;
    let source = ensure_recovery_bundle_is_external(data_dir, source)?;
    let encoded = read_bounded_recovery_bundle(&source)?;
    let envelope: RecoveryEnvelope =
        serde_json::from_slice(&encoded).context("parse recovery bundle")?;
    let (secrets, header_fingerprint) = open(envelope, passphrase)?;
    validate_recovery_secrets(&secrets)?;
    let fingerprint = crate::tls::certificate_fingerprint(&secrets.ca_cert_pem);
    if fingerprint != header_fingerprint {
        bail!("recovery bundle CA fingerprint does not match its encrypted contents");
    }
    let bundle_hash = feanorfs_common::hash_bytes(&encoded);

    let _guard = acquire_hub_runtime(data_dir)?;
    let marker_path = data_dir.join(RECOVERY_MARKER);
    let marker = load_marker(&marker_path)?;
    let resumed = if let Some(marker) = &marker {
        if marker.format_version != sealed_envelope::FORMAT_VERSION
            || marker.bundle_hash != bundle_hash
        {
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
            format_version: sealed_envelope::FORMAT_VERSION,
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
    sealed_envelope::validate_passphrase(&HUB_DOMAIN_BASE, passphrase)?;

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
    let resolved_recovery_destination =
        ensure_recovery_bundle_is_external(data_dir, recovery_destination)?;
    validate_recovery_destination(&resolved_recovery_destination, replace_destination)?;

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
    if encoded.len() > HUB_DOMAIN_BASE.max_envelope_bytes {
        bail!(
            "recovery bundle exceeds {} bytes",
            HUB_DOMAIN_BASE.max_envelope_bytes
        );
    }
    write_recovery_bundle(
        &resolved_recovery_destination,
        &encoded,
        replace_destination,
    )
    .with_context(|| {
        format!(
            "write rotated recovery bundle {}",
            recovery_destination.display()
        )
    })?;

    let marker = RecoveryMarker {
        format_version: sealed_envelope::FORMAT_VERSION,
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
    let key = crate::tls::validate_private_ca(&secrets.ca_cert_pem, &secrets.ca_key_pem)
        .map_err(|error| anyhow::anyhow!("validate recovery CA identity: {error:#}"))?;
    Issuer::from_ca_cert_pem(&secrets.ca_cert_pem, key).context("load recovery CA issuer")?;
    Ok(())
}

fn seal(
    secrets: &RecoverySecrets,
    passphrase: &str,
    fingerprint: &str,
) -> Result<RecoveryEnvelope> {
    let domain = hub_domain(fingerprint);
    let plaintext = Zeroizing::new(serde_json::to_vec(secrets)?);
    let base = sealed_envelope::seal(&domain, passphrase, &plaintext)?;
    Ok(RecoveryEnvelope {
        format_version: base.format_version,
        kdf: base.kdf,
        cipher: base.cipher,
        salt: base.salt,
        nonce: base.nonce,
        public_ca_fingerprint: fingerprint.into(),
        ciphertext: base.ciphertext,
    })
}

fn open(envelope: RecoveryEnvelope, passphrase: &str) -> Result<(RecoverySecrets, String)> {
    let mut envelope = envelope;
    let header_fingerprint = std::mem::take(&mut envelope.public_ca_fingerprint);
    let domain = hub_domain(&header_fingerprint);
    let base = envelope.into_sealed_envelope();
    let plaintext = sealed_envelope::open(&domain, passphrase, &base)?;
    let secrets: RecoverySecrets =
        serde_json::from_slice(&plaintext).context("decode encrypted recovery contents")?;
    Ok((secrets, header_fingerprint))
}

fn write_recovery_bundle(destination: &Path, encoded: &[u8], replace: bool) -> Result<()> {
    if replace {
        atomic_private_write(destination, encoded)
    } else {
        atomic_private_create_new(destination, encoded)
    }
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

fn ensure_recovery_bundle_is_external(data_dir: &Path, bundle: &Path) -> Result<PathBuf> {
    let resolved_data_dir = resolve_existing_or_parent(data_dir)
        .with_context(|| format!("resolve hub data directory {}", data_dir.display()))?;
    let resolved_bundle = resolve_existing_or_parent(bundle)
        .with_context(|| format!("resolve recovery bundle path {}", bundle.display()))?;
    if resolved_bundle.starts_with(&resolved_data_dir) {
        bail!(
            "recovery bundles must be stored outside the hub data directory so identity maintenance cannot overwrite hub state"
        );
    }
    Ok(resolved_bundle)
}

fn resolve_existing_or_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).context("canonicalize existing path");
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .context("path must name a file or directory")?;
    Ok(fs::canonicalize(parent)?.join(name))
}

fn read_bounded_recovery_bundle(source: &Path) -> Result<Vec<u8>> {
    let file =
        File::open(source).with_context(|| format!("open recovery bundle {}", source.display()))?;
    let length = file.metadata()?.len();
    if length > HUB_DOMAIN_BASE.max_envelope_bytes as u64 {
        bail!(
            "recovery bundle exceeds {} bytes",
            HUB_DOMAIN_BASE.max_envelope_bytes
        );
    }
    let mut encoded =
        Vec::with_capacity(usize::try_from(length).unwrap_or(HUB_DOMAIN_BASE.max_envelope_bytes));
    file.take(HUB_DOMAIN_BASE.max_envelope_bytes as u64 + 1)
        .read_to_end(&mut encoded)
        .with_context(|| format!("read recovery bundle {}", source.display()))?;
    if encoded.len() > HUB_DOMAIN_BASE.max_envelope_bytes {
        bail!(
            "recovery bundle exceeds {} bytes",
            HUB_DOMAIN_BASE.max_envelope_bytes
        );
    }
    Ok(encoded)
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
    fn recovery_validation_rejects_mismatched_ca_and_private_key() {
        let first = initialized_hub();
        let second = initialized_hub();
        let mut secrets = load_recovery_secrets(first.path()).unwrap();
        secrets.ca_key_pem = load_recovery_secrets(second.path())
            .unwrap()
            .ca_key_pem
            .clone();
        let error = validate_recovery_secrets(&secrets).expect_err("mismatched key must fail");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn non_replace_bundle_publication_has_an_atomic_single_winner() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("hub.fnr-recovery");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for content in [b"first".as_slice(), b"second".as_slice()] {
                let barrier = std::sync::Arc::clone(&barrier);
                let destination = destination.clone();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    write_recovery_bundle(&destination, content, false)
                }));
            }
            barrier.wait();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let bytes = fs::read(destination).unwrap();
        assert!(bytes == b"first" || bytes == b"second");
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
    fn recovery_bundle_paths_cannot_alias_live_hub_state() {
        let hub = initialized_hub();
        let token_path = hub.path().join("auth-token");
        let original_token = fs::read(&token_path).unwrap();
        let error = export_recovery_bundle(hub.path(), &token_path, PASSPHRASE, true)
            .expect_err("export must not overwrite live identity");
        assert!(error.to_string().contains("outside the hub data directory"));
        assert_eq!(fs::read(&token_path).unwrap(), original_token);

        let external = tempfile::tempdir().unwrap();
        let bundle = external.path().join("hub.fnr-recovery");
        export_recovery_bundle(hub.path(), &bundle, PASSPHRASE, false).unwrap();
        let internal = hub.path().join("internal-recovery-bundle");
        fs::copy(&bundle, &internal).unwrap();
        let error = import_recovery_bundle(hub.path(), &internal, PASSPHRASE, true)
            .expect_err("import source must remain available outside live state");
        assert!(error.to_string().contains("outside the hub data directory"));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_bundle_rejects_symlinked_parent_into_hub_state() {
        use std::os::unix::fs::symlink;

        let hub = initialized_hub();
        let external = tempfile::tempdir().unwrap();
        let alias = external.path().join("hub-alias");
        symlink(hub.path(), &alias).unwrap();
        let destination = alias.join("bundle.fnr-recovery");
        let error = export_recovery_bundle(hub.path(), &destination, PASSPHRASE, false)
            .expect_err("symlink alias must not bypass external-path check");
        assert!(error.to_string().contains("outside the hub data directory"));
    }

    #[test]
    fn oversized_recovery_bundle_is_rejected_before_parsing() {
        let target_parent = tempfile::tempdir().unwrap();
        let target = target_parent.path().join("hub");
        fs::create_dir(&target).unwrap();
        let bundle_dir = tempfile::tempdir().unwrap();
        let bundle = bundle_dir.path().join("oversized.fnr-recovery");
        let file = File::create(&bundle).unwrap();
        file.set_len(HUB_DOMAIN_BASE.max_envelope_bytes as u64 + 1)
            .unwrap();
        let error = import_recovery_bundle(&target, &bundle, PASSPHRASE, false)
            .expect_err("oversized bundle must fail");
        assert!(error.to_string().contains("exceeds"));
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
            format_version: sealed_envelope::FORMAT_VERSION,
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
                format_version: sealed_envelope::FORMAT_VERSION,
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

    #[test]
    fn old_bundle_fixture_still_imports() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../common/tests/fixtures/hub-bundle-v1.json"
        );
        let encoded = fs::read(fixture).expect("hub bundle fixture exists");
        let envelope: RecoveryEnvelope = serde_json::from_slice(&encoded).unwrap();
        let target = tempfile::tempdir().unwrap();
        let imported =
            import_recovery_bundle(target.path(), Path::new(fixture), PASSPHRASE, false).unwrap();
        assert_eq!(
            imported.public_ca_fingerprint,
            envelope.public_ca_fingerprint
        );
        assert!(!target.path().join(RECOVERY_MARKER).exists());
        let secrets = load_recovery_secrets(target.path()).unwrap();
        assert_eq!(
            crate::tls::certificate_fingerprint(&secrets.ca_cert_pem),
            envelope.public_ca_fingerprint
        );
    }

    #[test]
    fn truncated_bundle_fails_before_any_write() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../common/tests/fixtures/hub-bundle-v1.json"
        );
        let encoded = fs::read(fixture).expect("hub bundle fixture exists");
        let target = tempfile::tempdir().unwrap();
        let truncated = target.path().join("truncated.fnr-recovery");
        fs::write(&truncated, &encoded[..encoded.len() / 2]).unwrap();
        assert!(import_recovery_bundle(target.path(), &truncated, PASSPHRASE, false).is_err());
        assert!(!target.path().join("auth-token").exists());
        assert!(!target.path().join(RECOVERY_MARKER).exists());
    }
}
