mod durable;

#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Write};

pub(crate) use durable::read_local_state_text;
pub use durable::{check_no_legacy_db, DurableState};

const CURRENT_SCHEMA_VERSION: u32 = 1;

pub const ACCESS_LOG_MAX_ENTRIES: usize = 10_000;
pub const ACCESS_LOG_MIN_WEIGHT: f64 = 0.001;
pub(crate) const MAX_LOCAL_STATE_BYTES: usize = 128 * 1024 * 1024;
const MAX_LOCAL_STATE_RECORDS: usize = feanorfs_common::MAX_TREE_OUTPUT_PATHS;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalStateV1 {
    pub schema_version: u32,
    #[serde(default)]
    pub local_files: BTreeMap<String, CacheEntryV1>,
    #[serde(default)]
    pub file_access_log: Vec<AccessEntryV1>,
    #[serde(default)]
    pub last_session: BTreeMap<String, String>,
    #[serde(default)]
    pub conflict_registry: BTreeMap<String, ConflictRecordV1>,
    #[serde(default)]
    pub conflict_resolutions: Vec<ConflictResolutionV1>,
}

impl Default for LocalStateV1 {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            local_files: BTreeMap::new(),
            file_access_log: Vec::new(),
            last_session: BTreeMap::new(),
            conflict_registry: BTreeMap::new(),
            conflict_resolutions: Vec::new(),
        }
    }
}

impl LocalStateV1 {
    pub fn to_json(&self) -> Result<String> {
        let mut bytes = Vec::new();
        self.write_json(&mut bytes)?;
        String::from_utf8(bytes).context("serialize local state")
    }

    /// Serialize the canonical state directly to a writer while enforcing the
    /// on-disk size limit. The maps stay borrowed and only the two vectors that
    /// require deterministic sorting allocate temporary references.
    pub(crate) fn write_json<W: Write>(&self, writer: W) -> Result<usize> {
        self.write_json_with_limit(writer, MAX_LOCAL_STATE_BYTES)
    }

    fn write_json_with_limit<W: Write>(&self, writer: W, max_bytes: usize) -> Result<usize> {
        let canonical = self.canonical_for_serialize()?;
        let mut writer = BoundedWriter::new(writer, max_bytes);
        let serialization = serde_json::to_writer_pretty(&mut writer, &canonical);
        if writer.exceeded() {
            bail!("local_state.json exceeds {max_bytes} byte limit");
        }
        serialization.context("serialize local state")?;
        writer.flush().context("flush local state")?;
        Ok(writer.written())
    }

    fn canonical_for_serialize(&self) -> Result<CanonicalLocalState<'_>> {
        self.validate_bounds()?;
        let mut file_access_log = self.file_access_log.iter().collect::<Vec<_>>();
        file_access_log.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.sibling_path.cmp(&right.sibling_path))
        });
        let mut conflict_resolutions = self.conflict_resolutions.iter().collect::<Vec<_>>();
        conflict_resolutions.sort_by(|left, right| {
            left.resolved_at
                .cmp(&right.resolved_at)
                .then_with(|| left.path.cmp(&right.path))
        });
        let canonical = CanonicalLocalState {
            schema_version: self.schema_version,
            local_files: &self.local_files,
            file_access_log,
            last_session: &self.last_session,
            conflict_registry: &self.conflict_registry,
            conflict_resolutions,
        };
        Ok(canonical)
    }

    pub fn from_json(json: &str) -> Result<Self> {
        if json.len() > MAX_LOCAL_STATE_BYTES {
            bail!("local_state.json exceeds {MAX_LOCAL_STATE_BYTES} byte limit");
        }
        #[derive(Deserialize)]
        struct VersionOnly {
            #[serde(default)]
            schema_version: u64,
        }
        // Read the version without first allocating a generic JSON value that
        // duplicates every attacker-controlled key and string in memory.
        let version = serde_json::from_str::<VersionOnly>(json)
            .context("parse local state JSON (deserialize local state schema version)")?
            .schema_version;
        if version == 0 {
            return Err(crate::agent::continuous::unsupported_schema_failure(
                "local_state.json has invalid schema version 0. \
                 Remove it and re-initialize, or upgrade feanorfs.",
            ));
        }
        if version > u64::from(CURRENT_SCHEMA_VERSION) {
            return Err(crate::agent::continuous::unsupported_schema_failure(
                format!(
                    "local_state.json schema version {version} is newer than supported \
                 (max {CURRENT_SCHEMA_VERSION}). Upgrade feanorfs to open this workspace."
                ),
            ));
        }
        let state: Self = serde_json::from_str(json).context("deserialize local state")?;
        state.validate_bounds()?;
        for entry in &state.file_access_log {
            if !entry.weight.is_finite() {
                bail!(
                    "non-finite weight {} for {}/{} in local_state.json",
                    entry.weight,
                    entry.path,
                    entry.sibling_path
                );
            }
        }
        Ok(state)
    }

    fn validate_bounds(&self) -> Result<()> {
        for (label, count) in [
            ("local files", self.local_files.len()),
            ("session entries", self.last_session.len()),
            ("conflicts", self.conflict_registry.len()),
            ("conflict resolutions", self.conflict_resolutions.len()),
        ] {
            if count > MAX_LOCAL_STATE_RECORDS {
                bail!("local_state.json contains too many {label}");
            }
        }
        if self.file_access_log.len() > ACCESS_LOG_MAX_ENTRIES {
            bail!("local_state.json contains too many access-log entries");
        }
        Ok(())
    }

    pub fn prune_access_log(&mut self) {
        self.file_access_log
            .retain(|entry| entry.weight.abs() >= ACCESS_LOG_MIN_WEIGHT);
        if self.file_access_log.len() <= ACCESS_LOG_MAX_ENTRIES {
            return;
        }
        self.file_access_log.sort_by(|left, right| {
            left.weight
                .partial_cmp(&right.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.updated_at.cmp(&right.updated_at))
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.sibling_path.cmp(&right.sibling_path))
        });
        let drop_count = self.file_access_log.len() - ACCESS_LOG_MAX_ENTRIES;
        self.file_access_log.drain(..drop_count);
    }
}

/// Borrowed serialization view that keeps deterministic vector ordering
/// without cloning the potentially large file/session/conflict maps.
#[derive(Serialize)]
struct CanonicalLocalState<'a> {
    schema_version: u32,
    local_files: &'a BTreeMap<String, CacheEntryV1>,
    file_access_log: Vec<&'a AccessEntryV1>,
    last_session: &'a BTreeMap<String, String>,
    conflict_registry: &'a BTreeMap<String, ConflictRecordV1>,
    conflict_resolutions: Vec<&'a ConflictResolutionV1>,
}

struct BoundedWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl<W> BoundedWriter<W> {
    fn new(inner: W, limit: usize) -> Self {
        Self {
            inner,
            written: 0,
            limit,
            exceeded: false,
        }
    }

    const fn written(&self) -> usize {
        self.written
    }

    const fn exceeded(&self) -> bool {
        self.exceeded
    }
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.written);
        if bytes.len() > remaining {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "local state size limit exceeded",
            ));
        }

        let written = self.inner.write(bytes)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntryV1 {
    pub plaintext_hash: String,
    pub encrypted_hash: String,
    pub size: i64,
    pub mtime: i64,
    pub server_mtime: i64,
    pub mode: i32,
    pub hydrated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessEntryV1 {
    pub path: String,
    pub sibling_path: String,
    pub weight: f64,
    pub updated_at: i64,
}

/// Stable serialized status of one local conflict record (serde
/// snake_case on the wire; unknown statuses from newer clients map to
/// [`Self::Unknown`] so local state never fails to load).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictRecordStatus {
    /// Ordinary pending conflict. When an identity sidecar exists beside its
    /// artifacts the record is fingerprinted and eligible for automatic
    /// resolution; without one it is a legacy path-only record.
    Pending,
    /// Legacy path-only record migrated from an older client: visible and
    /// manually resolvable, but never eligible for automatic prepare/apply.
    LegacyUnfingerprinted,
    /// Unknown/unsupported status from a newer client. Excluded from pending
    /// and automatic listings; never resolved automatically.
    #[serde(other)]
    Unknown,
}

impl ConflictRecordStatus {
    /// Parses one persisted status string (unknown values stay `Unknown`).
    #[must_use]
    pub fn from_db_str(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "legacy_unfingerprinted" => Self::LegacyUnfingerprinted,
            _ => Self::Unknown,
        }
    }

    /// Stable wire string for this status.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::LegacyUnfingerprinted => "legacy_unfingerprinted",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the record is pending (visible and manually resolvable),
    /// including migrated legacy unfingerprinted records.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending | Self::LegacyUnfingerprinted)
    }
}

/// Stable serialized resolution method for history records (serde
/// snake_case; unknown methods from newer clients map to [`Self::Other`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionMethod {
    Local,
    Cloud,
    Both,
    File,
    /// Automatic guarded candidate publication.
    Candidate,
    /// Unknown/unsupported method from a newer client.
    #[serde(other)]
    Other,
}

impl ResolutionMethod {
    /// Parses one persisted method string (unknown values stay `Other`).
    #[must_use]
    pub fn from_db_str(value: &str) -> Self {
        match value {
            "local" => Self::Local,
            "cloud" => Self::Cloud,
            "both" => Self::Both,
            "file" => Self::File,
            "candidate" => Self::Candidate,
            _ => Self::Other,
        }
    }

    /// Stable wire string for this method.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cloud => "cloud",
            Self::Both => "both",
            Self::File => "file",
            Self::Candidate => "candidate",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictRecordV1 {
    pub path: String,
    pub kind: feanorfs_common::ConflictKind,
    pub conflict_dir: String,
    pub opened_at: i64,
    pub status: ConflictRecordStatus,
    /// Fingerprint of the exact conflict identity bound to this record.
    /// `None` for legacy path-only records. The fingerprint keys the
    /// identity sidecar (`identity-<first-32-chars>.json`) beside the
    /// conflict artifacts; a record whose sidecar is missing or mismatched
    /// is treated as legacy manual-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictResolutionV1 {
    pub path: String,
    pub method: ResolutionMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file_hash: Option<String>,
    pub resolved_at: i64,
    pub resolver: String,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationLocalState {
    pub local_files: BTreeMap<String, MigrationCacheEntry>,
    pub file_access_log: Vec<MigrationAccessEntry>,
    pub last_session: BTreeMap<String, String>,
    pub conflict_registry: BTreeMap<String, MigrationConflictRecord>,
    pub conflict_resolutions: Vec<MigrationConflictResolution>,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationCacheEntry {
    pub plaintext_hash: String,
    pub encrypted_hash: String,
    pub size: u64,
    pub mtime: i64,
    pub server_mtime: i64,
    pub mode: u32,
    pub hydrated: bool,
    pub deleted_at: Option<i64>,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationAccessEntry {
    pub path: String,
    pub sibling_path: String,
    pub weight: f64,
    pub updated_at: i64,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationConflictRecord {
    pub path: String,
    pub kind: feanorfs_common::ConflictKind,
    pub conflict_dir: String,
    pub opened_at: i64,
    pub status: String,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationConflictResolution {
    pub path: String,
    pub method: String,
    pub source_file_hash: Option<String>,
    pub resolved_at: i64,
    pub resolver: String,
}
