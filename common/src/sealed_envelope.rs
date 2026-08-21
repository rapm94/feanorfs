//! Pure sealed-envelope cryptography shared by the client workspace recovery
//! kits and the server hub recovery bundles.
//!
//! This module is deliberately free of filesystem I/O and side effects:
//! [`seal`] and [`open`] operate on in-memory bytes, and every caller keeps
//! its own payload schema, file handling, destination policy, crash fences,
//! and bounds policy. What is shared is the frozen cryptographic envelope:
//! an Argon2id v19 KDF (64 MiB, t=3, p=1) and an XChaCha20-Poly1305 AEAD
//! whose additional authenticated data binds a domain-tagged header. The
//! header carries the domain string, format version, KDF/cipher names,
//! salt, nonce, and any caller-supplied extra fields (in order), so an
//! envelope sealed for one domain cannot be opened with another.
//!
//! Format compatibility: the envelope JSON and the authenticated header
//! reproduce the pre-existing recovery kit / recovery bundle format exactly
//! (same field names, order, and values), so artifacts sealed before this
//! module existed still open, and new artifacts open with the old code.
//!
//! Failures are typed. Note that a wrong passphrase and a modified
//! ciphertext/header are cryptographically indistinguishable — both fail the
//! AEAD authentication — and both map to [`SealError::Authentication`].
//! Structural corruption (invalid base64, wrong exact lengths) maps to
//! [`SealError::Truncated`], bound violations to [`SealError::Oversized`],
//! and envelope-metadata mismatches to [`SealError::UnsupportedFormat`].

use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chacha20poly1305::{
    aead::{Aead as _, KeyInit as _, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;
use zeroize::{Zeroize as _, Zeroizing};

/// Frozen envelope format version.
pub const FORMAT_VERSION: u32 = 1;
/// Frozen KDF name carried in the authenticated header.
pub const KDF_NAME: &str = "argon2id-v19";
/// Frozen cipher name carried in the authenticated header.
pub const CIPHER_NAME: &str = "xchacha20poly1305";
/// Argon2id memory cost in KiB (64 MiB).
pub const KDF_MEMORY_KIB: u32 = 64 * 1024;
/// Argon2id iteration cost.
pub const KDF_ITERATIONS: u32 = 3;
/// Argon2id lane count.
pub const KDF_LANES: u32 = 1;
/// Salt length in bytes.
pub const SALT_BYTES: usize = 16;
/// XChaCha20 nonce length in bytes.
pub const NONCE_BYTES: usize = 24;
/// Derived key length in bytes.
pub const KEY_BYTES: usize = 32;
/// Poly1305 authentication tag length in bytes.
const TAG_BYTES: usize = 16;

/// Domain configuration that tags the authenticated header and carries the
/// exact bounds for one artifact class (workspace kit vs hub bundle).
///
/// The same bytes must be sealed and opened with the same domain; changing
/// any field — including an extra header value — yields an envelope that
/// the other domain cannot open.
#[derive(Debug, Clone)]
pub struct EnvelopeDomain {
    /// Authenticated domain tag, e.g. `"feanorfs workspace recovery kit"`.
    pub domain: &'static str,
    /// Envelope format version carried in the authenticated header.
    pub format_version: u32,
    /// KDF name carried in the authenticated header.
    pub kdf: &'static str,
    /// Cipher name carried in the authenticated header.
    pub cipher: &'static str,
    /// Artifact noun used in user-facing error messages (`"kit"`, `"bundle"`).
    pub noun: &'static str,
    /// Minimum passphrase length in characters (Unicode scalar values).
    pub min_passphrase_chars: usize,
    /// Maximum passphrase length in characters; `None` means unbounded.
    pub max_passphrase_chars: Option<usize>,
    /// Exact maximum plaintext size in bytes, enforced at seal and open.
    pub max_plaintext_bytes: usize,
    /// Exact maximum encoded-envelope size in bytes, enforced at encode and
    /// used by callers to bound file reads.
    pub max_envelope_bytes: usize,
    /// Extra authenticated header fields appended after `nonce`, in order.
    pub extra_header: Vec<(&'static str, String)>,
}

/// One sealed envelope in the frozen recovery format. The fields exactly
/// match the pre-existing recovery kit / recovery bundle JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedEnvelope {
    pub format_version: u32,
    pub kdf: String,
    pub cipher: String,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
}

/// Typed failure from sealing, opening, encoding, or validating an envelope.
#[derive(Debug)]
#[non_exhaustive]
pub enum SealError {
    /// The passphrase is wrong or the envelope was modified after sealing.
    /// AEAD authentication failed; the two cases are cryptographically
    /// indistinguishable.
    Authentication { noun: &'static str },
    /// An authenticated field is invalid base64 or has the wrong byte
    /// length, i.e. the envelope is truncated or corrupt.
    Truncated {
        noun: &'static str,
        field: &'static str,
    },
    /// A decoded value exceeds its exact bound.
    Oversized {
        noun: &'static str,
        what: &'static str,
        limit: usize,
    },
    /// Envelope metadata (format version / KDF / cipher) does not match the
    /// domain it is being opened with.
    UnsupportedFormat {
        noun: &'static str,
        field: &'static str,
        found: String,
    },
    /// The passphrase length is outside the domain's limits.
    InvalidPassphrase {
        chars: usize,
        min: usize,
        max: Option<usize>,
    },
    /// Header or envelope JSON could not be serialized.
    Json(serde_json::Error),
    /// An internal randomness, KDF configuration, or cipher failure that
    /// should not occur; carries a rendered message.
    Internal(String),
}

impl fmt::Display for SealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SealError::Authentication { noun } => {
                write!(f, "recovery passphrase is incorrect or {noun} was modified")
            }
            SealError::Truncated { field, .. } => write!(f, "decode recovery {field}"),
            SealError::Oversized { noun, what, limit } => {
                write!(f, "recovery {noun} {what} exceeds {limit} bytes")
            }
            SealError::UnsupportedFormat { noun, field, found } => match *field {
                "format_version" => write!(f, "unsupported recovery {noun} format {found}"),
                _ => write!(f, "unsupported recovery {noun} cryptography"),
            },
            SealError::InvalidPassphrase { chars, min, max } => match max {
                Some(max) if *chars > *max => {
                    write!(f, "recovery passphrase exceeds {max} characters")
                }
                _ => write!(
                    f,
                    "recovery passphrase must contain at least {min} characters"
                ),
            },
            SealError::Json(error) => write!(f, "{error}"),
            SealError::Internal(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SealError {}

/// Validate that a passphrase meets the domain's length limits.
///
/// Callers run this before any filesystem write so a weak passphrase fails
/// before state can change; [`seal`] and [`open`] also enforce it.
pub fn validate_passphrase(domain: &EnvelopeDomain, passphrase: &str) -> Result<(), SealError> {
    let chars = passphrase.chars().count();
    if chars < domain.min_passphrase_chars {
        return Err(SealError::InvalidPassphrase {
            chars,
            min: domain.min_passphrase_chars,
            max: domain.max_passphrase_chars,
        });
    }
    if let Some(max) = domain.max_passphrase_chars {
        if chars > max {
            return Err(SealError::InvalidPassphrase {
                chars,
                min: domain.min_passphrase_chars,
                max: domain.max_passphrase_chars,
            });
        }
    }
    Ok(())
}

/// Seal `plaintext` into a frozen-format envelope for `domain`.
///
/// Pure: no filesystem access, no side effects. The envelope is
/// non-deterministic only in the freshly generated salt and nonce; every
/// other byte is a deterministic function of the inputs, matching the
/// pre-existing format exactly.
pub fn seal(
    domain: &EnvelopeDomain,
    passphrase: &str,
    plaintext: &[u8],
) -> Result<SealedEnvelope, SealError> {
    validate_passphrase(domain, passphrase)?;
    if plaintext.len() > domain.max_plaintext_bytes {
        return Err(SealError::Oversized {
            noun: domain.noun,
            what: "plaintext",
            limit: domain.max_plaintext_bytes,
        });
    }
    let mut salt_bytes = [0_u8; SALT_BYTES];
    let mut nonce_bytes = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut salt_bytes)
        .map_err(|error| SealError::Internal(format!("generate recovery salt: {error}")))?;
    getrandom::fill(&mut nonce_bytes)
        .map_err(|error| SealError::Internal(format!("generate recovery nonce: {error}")))?;

    let salt = BASE64.encode(salt_bytes);
    let nonce = BASE64.encode(nonce_bytes);
    let aad = authenticated_header(domain, &salt, &nonce)?;
    let key_bytes = derive_key(passphrase, &salt_bytes)?;
    let key: &Key = key_bytes
        .as_ref()
        .try_into()
        .expect("derived recovery key has the exact cipher key size");
    let cipher = XChaCha20Poly1305::new(key);
    let xnonce: &XNonce = (&nonce_bytes).into();
    let ciphertext = cipher
        .encrypt(
            xnonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| SealError::Internal(format!("encrypt recovery {}", domain.noun)))?;
    salt_bytes.zeroize();
    nonce_bytes.zeroize();

    Ok(SealedEnvelope {
        format_version: domain.format_version,
        kdf: domain.kdf.into(),
        cipher: domain.cipher.into(),
        salt,
        nonce,
        ciphertext: BASE64.encode(ciphertext),
    })
}

/// Open an envelope sealed for `domain`, returning the authenticated
/// plaintext bytes. The caller parses its own payload schema from the bytes.
pub fn open(
    domain: &EnvelopeDomain,
    passphrase: &str,
    envelope: &SealedEnvelope,
) -> Result<Zeroizing<Vec<u8>>, SealError> {
    validate_passphrase(domain, passphrase)?;
    if envelope.format_version != domain.format_version {
        return Err(SealError::UnsupportedFormat {
            noun: domain.noun,
            field: "format_version",
            found: envelope.format_version.to_string(),
        });
    }
    if envelope.kdf != domain.kdf || envelope.cipher != domain.cipher {
        return Err(SealError::UnsupportedFormat {
            noun: domain.noun,
            field: "cryptography",
            found: envelope.cipher.clone(),
        });
    }
    let salt = decode_exact::<SALT_BYTES>(domain, "salt", &envelope.salt)?;
    let nonce = decode_exact::<NONCE_BYTES>(domain, "nonce", &envelope.nonce)?;
    let ciphertext = BASE64
        .decode(&envelope.ciphertext)
        .map_err(|_| SealError::Truncated {
            noun: domain.noun,
            field: "ciphertext",
        })?;
    let ciphertext_limit = domain.max_plaintext_bytes + TAG_BYTES;
    if ciphertext.len() > ciphertext_limit {
        return Err(SealError::Oversized {
            noun: domain.noun,
            what: "ciphertext",
            limit: ciphertext_limit,
        });
    }
    let aad = authenticated_header(domain, &envelope.salt, &envelope.nonce)?;
    let key_bytes = derive_key(passphrase, &salt)?;
    let key: &Key = key_bytes
        .as_ref()
        .try_into()
        .expect("derived recovery key has the exact cipher key size");
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
            .map_err(|_| SealError::Authentication { noun: domain.noun })?,
    );
    if plaintext.len() > domain.max_plaintext_bytes {
        return Err(SealError::Oversized {
            noun: domain.noun,
            what: "plaintext",
            limit: domain.max_plaintext_bytes,
        });
    }
    Ok(plaintext)
}

/// Deterministic pretty-JSON encoding of an envelope with the domain's exact
/// encoded-size bound enforced. The bytes reproduce the pre-existing
/// `serde_json::to_vec_pretty` output for the same fields.
pub fn encode(domain: &EnvelopeDomain, envelope: &SealedEnvelope) -> Result<Vec<u8>, SealError> {
    let encoded = serde_json::to_vec_pretty(envelope).map_err(SealError::Json)?;
    if encoded.len() > domain.max_envelope_bytes {
        return Err(SealError::Oversized {
            noun: domain.noun,
            what: "encoded envelope",
            limit: domain.max_envelope_bytes,
        });
    }
    Ok(encoded)
}

fn derive_key(
    passphrase: &str,
    salt: &[u8; SALT_BYTES],
) -> Result<Zeroizing<[u8; KEY_BYTES]>, SealError> {
    let params = Params::new(KDF_MEMORY_KIB, KDF_ITERATIONS, KDF_LANES, Some(KEY_BYTES))
        .map_err(|error| SealError::Internal(format!("configure recovery KDF: {error}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; KEY_BYTES]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|error| SealError::Internal(format!("derive recovery key: {error}")))?;
    Ok(key)
}

/// The domain-tagged authenticated header. Field order is preserved (the
/// serializer emits map entries in feed order) so the bytes exactly match
/// the pre-existing header format.
struct AuthenticatedHeader<'a> {
    domain: &'a str,
    format_version: u32,
    kdf: &'a str,
    cipher: &'a str,
    salt: &'a str,
    nonce: &'a str,
    extras: &'a [(&'static str, String)],
}

impl Serialize for AuthenticatedHeader<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("domain", &self.domain)?;
        map.serialize_entry("format_version", &self.format_version)?;
        map.serialize_entry("kdf", &self.kdf)?;
        map.serialize_entry("cipher", &self.cipher)?;
        map.serialize_entry("salt", &self.salt)?;
        map.serialize_entry("nonce", &self.nonce)?;
        for (name, value) in self.extras {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

fn authenticated_header(
    domain: &EnvelopeDomain,
    salt: &str,
    nonce: &str,
) -> Result<Vec<u8>, SealError> {
    serde_json::to_vec(&AuthenticatedHeader {
        domain: domain.domain,
        format_version: domain.format_version,
        kdf: domain.kdf,
        cipher: domain.cipher,
        salt,
        nonce,
        extras: &domain.extra_header,
    })
    .map_err(SealError::Json)
}

fn decode_exact<const N: usize>(
    domain: &EnvelopeDomain,
    field: &'static str,
    encoded: &str,
) -> Result<[u8; N], SealError> {
    let decoded = BASE64.decode(encoded).map_err(|_| SealError::Truncated {
        noun: domain.noun,
        field,
    })?;
    decoded.try_into().map_err(|_| SealError::Truncated {
        noun: domain.noun,
        field,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const PASSPHRASE: &str = "correct horse battery staple";
    const WORKSPACE_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/workspace-kit-v1.json"
    );
    const HUB_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hub-bundle-v1.json"
    );

    fn workspace_domain() -> EnvelopeDomain {
        EnvelopeDomain {
            domain: "feanorfs workspace recovery kit",
            format_version: FORMAT_VERSION,
            kdf: KDF_NAME,
            cipher: CIPHER_NAME,
            noun: "kit",
            min_passphrase_chars: 12,
            max_passphrase_chars: Some(1024),
            max_plaintext_bytes: 128 * 1024,
            max_envelope_bytes: 256 * 1024,
            extra_header: Vec::new(),
        }
    }

    fn hub_domain(fingerprint: &str) -> EnvelopeDomain {
        EnvelopeDomain {
            domain: "feanorfs hub recovery bundle",
            format_version: FORMAT_VERSION,
            kdf: KDF_NAME,
            cipher: CIPHER_NAME,
            noun: "bundle",
            min_passphrase_chars: 12,
            max_passphrase_chars: None,
            max_plaintext_bytes: 1024 * 1024,
            max_envelope_bytes: 2 * 1024 * 1024,
            extra_header: vec![("public_ca_fingerprint", fingerprint.to_string())],
        }
    }

    #[test]
    fn sealed_round_trip_reproduces_frozen_format() {
        let domain = workspace_domain();
        let envelope = seal(&domain, PASSPHRASE, b"opaque capability payload").unwrap();
        assert_eq!(envelope.format_version, FORMAT_VERSION);
        assert_eq!(envelope.kdf, KDF_NAME);
        assert_eq!(envelope.cipher, CIPHER_NAME);
        assert_eq!(
            decode_exact::<SALT_BYTES>(&domain, "salt", &envelope.salt)
                .unwrap()
                .len(),
            SALT_BYTES
        );
        assert_eq!(
            decode_exact::<NONCE_BYTES>(&domain, "nonce", &envelope.nonce)
                .unwrap()
                .len(),
            NONCE_BYTES
        );

        let plaintext = open(&domain, PASSPHRASE, &envelope).unwrap();
        assert_eq!(&*plaintext, b"opaque capability payload");

        // The deterministic bounded encode round-trips through JSON.
        let encoded = encode(&domain, &envelope).unwrap();
        assert!(encoded.len() <= domain.max_envelope_bytes);
        let reparsed: SealedEnvelope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(reparsed, envelope);
    }

    #[test]
    fn sealed_cross_domain_substitution_fails() {
        let workspace = workspace_domain();
        let hub = hub_domain("fingerprint-a");
        let envelope = seal(&workspace, PASSPHRASE, b"workspace capability").unwrap();
        let error = open(&hub, PASSPHRASE, &envelope)
            .expect_err("hub domain must not open a workspace kit");
        assert!(matches!(error, SealError::Authentication { .. }));

        let envelope = seal(&hub, PASSPHRASE, b"hub secrets").unwrap();
        let error = open(&workspace, PASSPHRASE, &envelope)
            .expect_err("workspace domain must not open a hub bundle");
        assert!(matches!(error, SealError::Authentication { .. }));
    }

    #[test]
    fn sealed_extra_header_mismatch_fails() {
        let sealed_with = hub_domain("fingerprint-a");
        let opened_with = hub_domain("fingerprint-b");
        let envelope = seal(&sealed_with, PASSPHRASE, b"hub secrets").unwrap();
        let error = open(&opened_with, PASSPHRASE, &envelope)
            .expect_err("a different extra header value must fail authentication");
        assert!(matches!(error, SealError::Authentication { .. }));
    }

    #[test]
    fn sealed_wrong_passphrase_is_authentication() {
        let domain = workspace_domain();
        let envelope = seal(&domain, PASSPHRASE, b"capability").unwrap();
        let error = open(&domain, "a different valid passphrase", &envelope).unwrap_err();
        assert!(matches!(error, SealError::Authentication { .. }));
        assert!(error.to_string().contains("incorrect or kit was modified"));
    }

    #[test]
    fn sealed_truncated_fields_are_distinct() {
        let domain = workspace_domain();
        let envelope = seal(&domain, PASSPHRASE, b"capability").unwrap();

        let mut truncated = envelope.clone();
        truncated.salt = BASE64.encode([0_u8; 8]);
        let error = open(&domain, PASSPHRASE, &truncated).unwrap_err();
        assert!(matches!(error, SealError::Truncated { field: "salt", .. }));

        let mut truncated = envelope.clone();
        truncated.nonce = "not base64!".into();
        let error = open(&domain, PASSPHRASE, &truncated).unwrap_err();
        assert!(matches!(error, SealError::Truncated { field: "nonce", .. }));

        let mut truncated = envelope.clone();
        truncated.ciphertext = "!!!!".into();
        let error = open(&domain, PASSPHRASE, &truncated).unwrap_err();
        assert!(matches!(
            error,
            SealError::Truncated {
                field: "ciphertext",
                ..
            }
        ));
    }

    #[test]
    fn sealed_oversized_inputs_are_rejected() {
        let domain = workspace_domain();

        // Plaintext above the domain bound fails before any random/KDF work.
        let huge = vec![0_u8; domain.max_plaintext_bytes + 1];
        let error = seal(&domain, PASSPHRASE, &huge).unwrap_err();
        assert!(matches!(
            error,
            SealError::Oversized {
                what: "plaintext",
                ..
            }
        ));

        // Ciphertext whose decoded size exceeds the exact bound fails at open.
        let envelope = SealedEnvelope {
            format_version: FORMAT_VERSION,
            kdf: KDF_NAME.into(),
            cipher: CIPHER_NAME.into(),
            salt: BASE64.encode([0_u8; SALT_BYTES]),
            nonce: BASE64.encode([0_u8; NONCE_BYTES]),
            ciphertext: BASE64.encode(vec![0_u8; domain.max_plaintext_bytes + 1 + TAG_BYTES]),
        };
        let error = open(&domain, PASSPHRASE, &envelope).unwrap_err();
        assert!(matches!(
            error,
            SealError::Oversized {
                what: "ciphertext",
                ..
            }
        ));

        // An envelope that encodes beyond the domain bound fails at encode.
        let envelope = SealedEnvelope {
            format_version: FORMAT_VERSION,
            kdf: KDF_NAME.into(),
            cipher: CIPHER_NAME.into(),
            salt: BASE64.encode([0_u8; SALT_BYTES]),
            nonce: BASE64.encode([0_u8; NONCE_BYTES]),
            ciphertext: BASE64.encode(vec![0_u8; domain.max_envelope_bytes * 3 / 4]),
        };
        let error = encode(&domain, &envelope).unwrap_err();
        assert!(matches!(
            error,
            SealError::Oversized {
                what: "encoded envelope",
                ..
            }
        ));
    }

    #[test]
    fn sealed_unsupported_metadata_is_distinct() {
        let domain = workspace_domain();
        let envelope = seal(&domain, PASSPHRASE, b"capability").unwrap();

        let mut modified = envelope.clone();
        modified.format_version = 99;
        let error = open(&domain, PASSPHRASE, &modified).unwrap_err();
        assert!(matches!(
            error,
            SealError::UnsupportedFormat {
                field: "format_version",
                ..
            }
        ));
        assert!(error
            .to_string()
            .contains("unsupported recovery kit format 99"));

        let mut modified = envelope.clone();
        modified.kdf = "pbkdf2".into();
        let error = open(&domain, PASSPHRASE, &modified).unwrap_err();
        assert!(matches!(
            error,
            SealError::UnsupportedFormat {
                field: "cryptography",
                ..
            }
        ));
        assert!(error
            .to_string()
            .contains("unsupported recovery kit cryptography"));
    }

    #[test]
    fn sealed_passphrase_limits_are_enforced() {
        let domain = workspace_domain();
        let error = validate_passphrase(&domain, "short").unwrap_err();
        assert!(matches!(error, SealError::InvalidPassphrase { .. }));
        assert!(error.to_string().contains("at least 12"));

        let long = "x".repeat(domain.max_passphrase_chars.unwrap() + 1);
        let error = validate_passphrase(&domain, &long).unwrap_err();
        assert!(matches!(error, SealError::InvalidPassphrase { .. }));
        assert!(error.to_string().contains("exceeds 1024"));
    }

    #[test]
    fn sealed_old_workspace_kit_fixture_opens() {
        let encoded = fs::read(WORKSPACE_FIXTURE).expect("workspace kit fixture exists");
        let envelope: SealedEnvelope = serde_json::from_slice(&encoded).unwrap();
        let plaintext = open(&workspace_domain(), PASSPHRASE, &envelope).unwrap();
        let invite: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(
            invite["workspace_id"],
            "fsw1-0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn sealed_old_hub_bundle_fixture_opens() {
        let encoded = fs::read(HUB_FIXTURE).expect("hub bundle fixture exists");
        let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        let fingerprint = value["public_ca_fingerprint"].as_str().unwrap();
        let base = SealedEnvelope {
            format_version: value["format_version"].as_u64().unwrap() as u32,
            kdf: value["kdf"].as_str().unwrap().to_string(),
            cipher: value["cipher"].as_str().unwrap().to_string(),
            salt: value["salt"].as_str().unwrap().to_string(),
            nonce: value["nonce"].as_str().unwrap().to_string(),
            ciphertext: value["ciphertext"].as_str().unwrap().to_string(),
        };
        let plaintext = open(&hub_domain(fingerprint), PASSPHRASE, &base).unwrap();
        let secrets: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
        assert!(secrets["ca_cert_pem"]
            .as_str()
            .unwrap()
            .contains("BEGIN CERTIFICATE"));
        assert!(secrets["ca_key_pem"]
            .as_str()
            .unwrap()
            .contains("PRIVATE KEY"));
        assert_eq!(secrets["auth_token"].as_str().unwrap().len(), 64);
    }
}
