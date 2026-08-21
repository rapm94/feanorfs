//! Pure hub protocol validation and outcome types shared by the HTTP server
//! and the in-process LocalHub adapter.
//!
//! This module is dependency-neutral by construction: it performs no I/O,
//! holds no locks, and manages no transactions. Each adapter (async SQLite on
//! the server, the in-process JSON-lock store in the agent core) keeps its own
//! durability, concurrency, and error machinery in place and maps these pure
//! outcomes to its own responses at the route boundary.
//!
//! Identifier, hash, path, and manifest-canonicalization validation that both
//! adapters already share live at the crate root (`is_valid_hash`,
//! `is_safe_rel_path`, `hash_bytes`, `canonical_manifest_hashes`,
//! `canonical_manifest_hash_list`); this module adds the outcome types and the
//! format-value predicate that were previously duplicated.

use anyhow::ensure;
use std::fmt;

/// The only workspace format version the hub protocol accepts.
///
/// Older flat-file formats remain servable through the legacy sync endpoints;
/// only format 3 (the object/snapshot API) is accepted by the publication
/// endpoints on both adapters.
pub const SUPPORTED_FORMAT_VERSION: u32 = 3;

/// Whether a workspace format version is accepted by the hub protocol.
#[must_use]
pub const fn is_supported_format_version(version: u32) -> bool {
    version == SUPPORTED_FORMAT_VERSION
}

/// Outcome of persisting one reachability manifest for a snapshot id.
///
/// Both the server's async SQLite store and the LocalHub's in-process JSON
/// store produce this outcome; each route maps it to the same HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestWriteOutcome {
    /// The manifest was newly stored.
    Stored,
    /// An identical canonical manifest already existed.
    Unchanged,
    /// The snapshot id already has a manifest with different canonical hashes;
    /// manifests are immutable once stored.
    Conflict,
    /// Manifest storage capacity was exhausted.
    Capacity,
}

/// Outcome of one migration-fence acquisition attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationWriteOutcome {
    /// The caller's token owns the workspace migration fence (created now, or
    /// already held by the same token; also returned when the workspace is
    /// already at the target format and needs no fence).
    Acquired,
    /// A different migration token already owns the fence.
    LockedByOther,
}

/// Parses and validates a migration fence token from a request header value.
///
/// Returns `Ok(None)` for a missing header, `Ok(Some(token))` for a
/// well-formed token, and `Err` for a malformed token. The token is a full
/// snapshot-style hash.
pub fn parse_migration_token(value: Option<&str>) -> Result<Option<&str>, MigrationTokenError> {
    match value {
        None => Ok(None),
        Some(token) if crate::is_valid_hash(token) => Ok(Some(token)),
        Some(_) => Err(MigrationTokenError::Malformed),
    }
}

/// Failure to parse a migration fence token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationTokenError {
    /// The header value is not a full snapshot-style hash.
    Malformed,
}

impl fmt::Display for MigrationTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("missing or invalid migration token"),
        }
    }
}

impl std::error::Error for MigrationTokenError {}

/// Validates a canonical manifest hash list independently of any storage
/// adapter. Mirrors the server SQLite precondition checks so both adapters
/// reject the same malformed manifests before any side effect.
pub fn validate_manifest_hashes(snapshot_id: &str, hashes: &[String]) -> anyhow::Result<()> {
    ensure!(
        crate::is_valid_hash(snapshot_id),
        "invalid snapshot id for manifest"
    );
    ensure!(!hashes.is_empty(), "manifest must not be empty");
    ensure!(
        hashes.len() <= crate::MANIFEST_MAX_ENTRIES,
        "manifest exceeds {} object entries",
        crate::MANIFEST_MAX_ENTRIES
    );
    ensure!(
        hashes.iter().all(|hash| crate::is_valid_hash(hash))
            && hashes.windows(2).all(|pair| pair[0] < pair[1]),
        "manifest object ids must be canonical, sorted, and unique"
    );
    ensure!(
        hashes
            .binary_search_by(|hash| hash.as_str().cmp(snapshot_id))
            .is_ok(),
        "manifest does not contain its snapshot root"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_format_three_is_supported() {
        assert!(is_supported_format_version(3));
        assert!(!is_supported_format_version(2));
        assert!(!is_supported_format_version(4));
        assert!(!is_supported_format_version(0));
    }

    #[test]
    fn migration_token_parsing_accepts_only_full_hashes() {
        assert_eq!(parse_migration_token(None).unwrap(), None);
        let token = "a".repeat(64);
        assert_eq!(
            parse_migration_token(Some(&token)).unwrap(),
            Some(token.as_str())
        );
        assert_eq!(
            parse_migration_token(Some("short")),
            Err(MigrationTokenError::Malformed)
        );
        assert_eq!(
            parse_migration_token(Some(&"A".repeat(64))),
            Err(MigrationTokenError::Malformed)
        );
    }

    #[test]
    fn manifest_hashes_validation_rejects_non_canonical_lists() {
        let root = "a".repeat(64);
        let blob = "b".repeat(64);
        assert!(validate_manifest_hashes(&root, std::slice::from_ref(&root)).is_ok());
        assert!(validate_manifest_hashes(&root, &[root.clone(), blob.clone()]).is_ok());
        // Unsorted entries are rejected even when the snapshot root is present.
        assert!(validate_manifest_hashes(&root, &[blob.clone(), root.clone()]).is_err());
        // Missing snapshot root is rejected.
        assert!(validate_manifest_hashes(&root, &[blob]).is_err());
        assert!(validate_manifest_hashes("bad", std::slice::from_ref(&root)).is_err());
        assert!(validate_manifest_hashes(&root, &[]).is_err());
    }
}
