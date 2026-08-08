use anyhow::{bail, Context as _, Result};
#[cfg(test)]
use rcgen::date_time_ymd;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use std::fs;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use crate::private_file::{atomic_private_write, create_private_dir, open_private_lock};
use crate::serve::ServeOptions;

const TLS_DIR: &str = "tls";
const CA_CERT: &str = "ca-cert.pem";
const CA_KEY: &str = "ca-key.pem";
const SERVER_CERT: &str = "server-cert.pem";
const SERVER_KEY: &str = "server-key.pem";

#[derive(Debug, Clone)]
pub struct TlsIdentity {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// Public CA certificate embedded in secure hub/workspace invites when needed.
    pub public_ca_pem: Option<String>,
    pub fingerprint: Option<String>,
    /// Stable CA-bound hostname included in automatically generated leaf SANs.
    /// Custom certificate deployments own their DNS names and leave this unset.
    pub mdns_hostname: Option<String>,
}

pub fn prepare_tls(opts: &mut ServeOptions) -> Result<Option<TlsIdentity>> {
    crate::ensure_recovery_complete(&opts.data_dir)?;
    if opts.allow_http {
        if opts.tls_cert.is_some() || opts.tls_key.is_some() || opts.tls_ca.is_some() {
            bail!("--allow-http conflicts with TLS certificate options");
        }
        return Ok(None);
    }

    match (&opts.tls_cert, &opts.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let public_ca_pem = opts
                .tls_ca
                .as_ref()
                .map(|path| {
                    fs::read_to_string(path)
                        .with_context(|| format!("read TLS CA certificate {}", path.display()))
                })
                .transpose()?;
            let fingerprint = public_ca_pem.as_deref().map(certificate_fingerprint);
            Ok(Some(TlsIdentity {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                public_ca_pem,
                fingerprint,
                mdns_hostname: None,
            }))
        }
        (None, None) if opts.tls_ca.is_none() => {
            let identity = prepare_auto_tls(&opts.data_dir)?;
            opts.tls_cert = Some(identity.cert_path.clone());
            opts.tls_key = Some(identity.key_path.clone());
            opts.tls_ca = Some(opts.data_dir.join(TLS_DIR).join(CA_CERT));
            Ok(Some(identity))
        }
        _ => bail!("--tls-cert and --tls-key must be provided together; --tls-ca is optional"),
    }
}

fn prepare_auto_tls(data_dir: &Path) -> Result<TlsIdentity> {
    let tls_dir = data_dir.join(TLS_DIR);
    create_private_dir(&tls_dir)?;
    let lock_path = tls_dir.join("material.lock");
    let lock = open_private_lock(&lock_path)?;
    fs2::FileExt::lock_exclusive(&lock).context("lock TLS material")?;

    let ca_key_path = tls_dir.join(CA_KEY);
    let ca_cert_path = tls_dir.join(CA_CERT);
    if ca_cert_path.exists() && !ca_key_path.exists() {
        bail!(
            "TLS CA private key is missing at {}; restore it from backup or create a new hub data directory",
            ca_key_path.display()
        );
    }

    let ca_key = Zeroizing::new(if ca_key_path.exists() {
        fs::read_to_string(&ca_key_path)
            .with_context(|| format!("read TLS CA key {}", ca_key_path.display()))?
    } else {
        let key = KeyPair::generate()
            .context("generate TLS CA key")?
            .serialize_pem();
        atomic_private_write(&ca_key_path, key.as_bytes())?;
        key
    });
    let ca_cert = if ca_cert_path.exists() {
        fs::read_to_string(&ca_cert_path)
            .with_context(|| format!("read TLS CA certificate {}", ca_cert_path.display()))?
    } else {
        let generated_key = KeyPair::from_pem(&ca_key).context("parse generated TLS CA key")?;
        let cert = ca_params()
            .self_signed(&generated_key)
            .context("generate TLS CA certificate")?
            .pem();
        atomic_private_write(&ca_cert_path, cert.as_bytes())?;
        cert
    };
    let ca_key_pair = validate_private_ca(&ca_cert, &ca_key)?;
    let issuer =
        Issuer::from_ca_cert_pem(&ca_cert, ca_key_pair).context("parse TLS CA certificate")?;
    let mdns_hostname = feanorfs_common::hub_mdns_hostname(&ca_cert);

    let leaf_key = KeyPair::generate().context("generate TLS server key")?;
    let mut leaf_params = CertificateParams::new(server_names(Some(&mdns_hostname))?)
        .context("build TLS server certificate parameters")?;
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, "FeanorFS Hub");
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    leaf_params.use_authority_key_identifier_extension = true;
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &issuer)
        .context("sign TLS server certificate")?;

    let cert_path = tls_dir.join(SERVER_CERT);
    let key_path = tls_dir.join(SERVER_KEY);
    let certificate_chain = format!("{}{}", leaf_cert.pem(), ca_cert);
    atomic_private_write(&cert_path, certificate_chain.as_bytes())?;
    let leaf_key_pem = Zeroizing::new(leaf_key.serialize_pem());
    atomic_private_write(&key_path, leaf_key_pem.as_bytes())?;

    Ok(TlsIdentity {
        cert_path,
        key_path,
        public_ca_pem: Some(ca_cert.clone()),
        fingerprint: Some(certificate_fingerprint(&ca_cert)),
        mdns_hostname: Some(mdns_hostname),
    })
}

pub(crate) fn validate_private_ca(cert_pem: &str, key_pem: &str) -> Result<KeyPair> {
    let key = KeyPair::from_pem(key_pem).context("parse TLS CA private key")?;
    let (pem_remainder, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|_| anyhow::anyhow!("parse TLS CA certificate"))?;
    anyhow::ensure!(
        pem_remainder.iter().all(u8::is_ascii_whitespace) && pem.label == "CERTIFICATE",
        "TLS CA contains unexpected PEM data"
    );
    let (der_remainder, certificate) = x509_parser::parse_x509_certificate(&pem.contents)
        .map_err(|_| anyhow::anyhow!("parse TLS CA certificate"))?;
    anyhow::ensure!(
        der_remainder.is_empty(),
        "TLS CA contains trailing DER data"
    );
    anyhow::ensure!(certificate.is_ca(), "TLS certificate is not a CA");
    let key_usage = certificate
        .key_usage()
        .context("parse TLS CA key usage")?
        .context("TLS CA is missing key usage")?;
    anyhow::ensure!(
        key_usage.value.key_cert_sign(),
        "TLS CA cannot sign certificates"
    );
    anyhow::ensure!(
        certificate.tbs_certificate.subject == certificate.tbs_certificate.issuer,
        "TLS CA is not self-issued"
    );
    certificate
        .verify_signature(None)
        .context("TLS CA self-signature is invalid")?;
    anyhow::ensure!(
        certificate.validity().is_valid(),
        "TLS CA is not currently valid"
    );
    anyhow::ensure!(
        certificate.public_key().subject_public_key.data.as_ref() == key.public_key_raw(),
        "TLS CA certificate does not match its private key"
    );
    Ok(key)
}

pub(crate) fn generate_private_ca() -> Result<(String, Zeroizing<String>)> {
    let key = KeyPair::generate().context("generate TLS CA key")?;
    let certificate = ca_params()
        .self_signed(&key)
        .context("generate TLS CA certificate")?
        .pem();
    Ok((certificate, Zeroizing::new(key.serialize_pem())))
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "FeanorFS Hub CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    params
}

fn server_names(mdns_hostname: Option<&str>) -> Result<Vec<String>> {
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    for interface in if_addrs::get_if_addrs()? {
        names.push(interface.ip().to_string());
    }
    if let Some(hostname) = mdns_hostname {
        names.push(hostname.to_string());
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub(crate) fn certificate_fingerprint(pem: &str) -> String {
    feanorfs_common::hub_ca_fingerprint(pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_tls_reuses_ca_and_refreshes_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let first = prepare_auto_tls(dir.path()).unwrap();
        let first_ca = first.public_ca_pem.clone().unwrap();
        let first_leaf = fs::read_to_string(&first.cert_path).unwrap();
        let second = prepare_auto_tls(dir.path()).unwrap();
        assert_eq!(second.public_ca_pem.as_deref(), Some(first_ca.as_str()));
        assert_ne!(fs::read_to_string(&second.cert_path).unwrap(), first_leaf);
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.mdns_hostname, second.mdns_hostname);
        assert_eq!(
            first.mdns_hostname.as_deref(),
            Some(feanorfs_common::hub_mdns_hostname(&first_ca).as_str())
        );
        assert!(server_names(first.mdns_hostname.as_deref())
            .unwrap()
            .contains(first.mdns_hostname.as_ref().unwrap()));
    }

    #[test]
    fn auto_tls_rejects_a_mismatched_durable_ca_key_before_replacing_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let identity = prepare_auto_tls(dir.path()).unwrap();
        let original_leaf = fs::read(&identity.cert_path).unwrap();
        let wrong_key = KeyPair::generate().unwrap().serialize_pem();
        atomic_private_write(&dir.path().join(TLS_DIR).join(CA_KEY), wrong_key.as_bytes()).unwrap();

        let error = prepare_auto_tls(dir.path()).unwrap_err();
        assert!(error.to_string().contains("does not match its private key"));
        assert_eq!(fs::read(&identity.cert_path).unwrap(), original_leaf);
    }

    #[test]
    fn auto_tls_rejects_non_ca_and_expired_durable_identities() {
        for expired in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            prepare_auto_tls(dir.path()).unwrap();
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params
                .distinguished_name
                .push(DnType::CommonName, "Invalid CA");
            if expired {
                params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
                params.not_before = date_time_ymd(2000, 1, 1);
                params.not_after = date_time_ymd(2001, 1, 1);
            } else {
                params.is_ca = IsCa::NoCa;
                params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            }
            let cert = params.self_signed(&key).unwrap().pem();
            let tls_dir = dir.path().join(TLS_DIR);
            atomic_private_write(&tls_dir.join(CA_CERT), cert.as_bytes()).unwrap();
            atomic_private_write(&tls_dir.join(CA_KEY), key.serialize_pem().as_bytes()).unwrap();

            let error = prepare_auto_tls(dir.path()).unwrap_err().to_string();
            if expired {
                assert!(error.contains("not currently valid"), "{error}");
            } else {
                assert!(error.contains("not a CA"), "{error}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn auto_tls_private_material_has_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let identity = prepare_auto_tls(dir.path()).unwrap();
        let tls_dir = dir.path().join(TLS_DIR);
        assert_eq!(
            fs::metadata(&tls_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for path in [
            identity.cert_path,
            identity.key_path,
            tls_dir.join(CA_CERT),
            tls_dir.join(CA_KEY),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
