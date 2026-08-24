//! Passphrase-encrypted, offline workspace recovery kits.
//!
//! The envelope deliberately exposes only versioned cryptographic metadata.
//! The complete workspace capability remains authenticated ciphertext until a
//! client decrypts it locally and hands it to the ordinary `start` path.

use anyhow::{bail, Context as _, Result};
use feanorfs_common::sealed_envelope;
use feanorfs_common::WorkspaceInvite;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Workspace recovery-kit domain: tags the authenticated header and carries
/// the exact bounds for workspace kits.
const WORKSPACE_DOMAIN: sealed_envelope::EnvelopeDomain = sealed_envelope::EnvelopeDomain {
    domain: "feanorfs workspace recovery kit",
    format_version: sealed_envelope::FORMAT_VERSION,
    kdf: sealed_envelope::KDF_NAME,
    cipher: sealed_envelope::CIPHER_NAME,
    noun: "kit",
    min_passphrase_chars: 12,
    max_passphrase_chars: Some(1024),
    max_plaintext_bytes: 128 * 1024,
    max_envelope_bytes: 256 * 1024,
    extra_header: Vec::new(),
};

/// Encrypt a portable workspace capability and write it atomically with
/// private file permissions.
pub fn export_recovery_kit(
    destination: &Path,
    invite: &WorkspaceInvite,
    passphrase: &str,
    replace_destination: bool,
) -> Result<()> {
    sealed_envelope::validate_passphrase(&WORKSPACE_DOMAIN, passphrase)?;
    validate_invite(invite)?;
    let destination = resolved_destination(destination)?;
    validate_destination(&destination, replace_destination)?;

    let envelope = seal(invite, passphrase)?;
    let encoded = sealed_envelope::encode(&WORKSPACE_DOMAIN, &envelope)?;
    if replace_destination {
        atomic_private_write(&destination, &encoded)
    } else {
        atomic_private_create_new(&destination, &encoded)
    }
    .with_context(|| format!("write recovery kit {}", destination.display()))
}

/// Decrypt and validate a workspace capability without writing workspace
/// configuration. Callers can therefore fail on a wrong passphrase or a
/// modified kit before the normal onboarding path creates any local state.
pub fn open_recovery_kit(source: &Path, passphrase: &str) -> Result<WorkspaceInvite> {
    sealed_envelope::validate_passphrase(&WORKSPACE_DOMAIN, passphrase)?;
    let source = fs::canonicalize(source)
        .with_context(|| format!("resolve recovery kit {}", source.display()))?;
    let file =
        File::open(&source).with_context(|| format!("open recovery kit {}", source.display()))?;
    let length = file.metadata()?.len();
    if length > WORKSPACE_DOMAIN.max_envelope_bytes as u64 {
        bail!(
            "recovery kit exceeds {} bytes",
            WORKSPACE_DOMAIN.max_envelope_bytes
        );
    }
    let mut encoded =
        Vec::with_capacity(usize::try_from(length).unwrap_or(WORKSPACE_DOMAIN.max_envelope_bytes));
    file.take(WORKSPACE_DOMAIN.max_envelope_bytes as u64 + 1)
        .read_to_end(&mut encoded)
        .with_context(|| format!("read recovery kit {}", source.display()))?;
    if encoded.len() > WORKSPACE_DOMAIN.max_envelope_bytes {
        bail!(
            "recovery kit exceeds {} bytes",
            WORKSPACE_DOMAIN.max_envelope_bytes
        );
    }
    let envelope: sealed_envelope::SealedEnvelope =
        serde_json::from_slice(&encoded).context("parse recovery kit")?;
    let invite = open(&envelope, passphrase)?;
    validate_invite(&invite)?;
    Ok(invite)
}

fn seal(invite: &WorkspaceInvite, passphrase: &str) -> Result<sealed_envelope::SealedEnvelope> {
    let plaintext = Zeroizing::new(serde_json::to_vec(invite)?);
    Ok(sealed_envelope::seal(
        &WORKSPACE_DOMAIN,
        passphrase,
        &plaintext,
    )?)
}

fn open(envelope: &sealed_envelope::SealedEnvelope, passphrase: &str) -> Result<WorkspaceInvite> {
    let plaintext = sealed_envelope::open(&WORKSPACE_DOMAIN, passphrase, envelope)?;
    serde_json::from_slice(&plaintext).context("decode encrypted recovery capability")
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
    if let Some(policy) = invite.ignore_policy.as_deref() {
        crate::join_preflight::normalize_ignore_policy(policy)
            .context("recovery capability has an invalid mirror ignore policy")?;
    }
    Ok(())
}

fn resolved_destination(destination: &Path) -> Result<PathBuf> {
    if destination.exists() {
        return fs::canonicalize(destination).context("resolve existing recovery kit");
    }
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .context("recovery kit path must name a file")?;
    Ok(fs::canonicalize(parent)
        .context("resolve recovery kit directory")?
        .join(name))
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

/// Private create-new: writes `bytes` to a random 0o600
/// temp file, syncs it, and publishes via a hard link so the destination is
/// never replaced. Exactly one concurrent writer wins; the rest fail. Parent
/// directory is synced on Unix so a published kit survives power loss. On any
/// error the temp file is removed.
fn atomic_private_create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("recovery kit has no parent")?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate recovery temp name: {error}"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = parent.join(format!(".feanorfs-recovery-{suffix}.tmp"));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("create recovery temp file")?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path).with_context(|| {
            format!("publish recovery kit without replacing {}", path.display())
        })?;
        fs::remove_file(&temporary)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Private durable replacement: mode 0o600 temp file,
/// atomic rename, post-commit mode fix, and parent-directory sync on Unix so
/// an exported recovery kit survives power loss (used for `--replace`).
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
    #[cfg(unix)]
    {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::File::open(parent)?.sync_all()?;
        }
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
            mesh: None,
            ignore_policy: Some("target/\n".into()),
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

        let mut envelope: sealed_envelope::SealedEnvelope =
            serde_json::from_slice(&original).unwrap();
        let replacement = if envelope.nonce.starts_with('A') {
            "B"
        } else {
            "A"
        };
        envelope.nonce.replace_range(..1, replacement);
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

    #[test]
    fn non_replace_recovery_publication_has_one_atomic_winner() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("workspace.fnrk");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for bytes in [b"first".as_slice(), b"second".as_slice()] {
                let destination = destination.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    atomic_private_create_new(&destination, bytes)
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
    fn oversized_recovery_kit_is_rejected_from_metadata_before_parsing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oversized.fnrk");
        let file = File::create(&path).unwrap();
        file.set_len(WORKSPACE_DOMAIN.max_envelope_bytes as u64 + 1)
            .unwrap();
        let error = open_recovery_kit(&path, PASSPHRASE).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn old_kit_fixture_still_opens() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../common/tests/fixtures/workspace-kit-v1.json"
        );
        let recovered = open_recovery_kit(Path::new(fixture), PASSPHRASE).unwrap();
        assert_eq!(recovered, invite());
        // A wrong passphrase against the old fixture fails before any state
        // can change; opening never writes.
        let error = open_recovery_kit(Path::new(fixture), "another valid passphrase").unwrap_err();
        assert!(error.to_string().contains("incorrect or kit was modified"));
    }

    #[test]
    fn hub_bundle_fixture_is_rejected_by_workspace_kit_reader() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../common/tests/fixtures/hub-bundle-v1.json"
        );
        // Cross-domain substitution fails at parse: the workspace reader
        // rejects the bundle's authenticated `public_ca_fingerprint` field.
        let error = open_recovery_kit(Path::new(fixture), PASSPHRASE).unwrap_err();
        assert!(format!("{error:#}").contains("public_ca_fingerprint"));
    }

    #[test]
    fn truncated_kit_fails_before_any_write() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("truncated.fnrk");
        let encoded: Vec<u8>;
        {
            let full = root.path().join("full.fnrk");
            export_recovery_kit(&full, &invite(), PASSPHRASE, false).unwrap();
            encoded = fs::read(&full).unwrap();
        }
        fs::write(&path, &encoded[..encoded.len() / 2]).unwrap();
        assert!(open_recovery_kit(&path, PASSPHRASE).is_err());
    }
}
