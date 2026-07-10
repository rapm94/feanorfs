mod durable;

#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use durable::{check_no_legacy_db, DurableState};

const CURRENT_SCHEMA_VERSION: u32 = 1;

pub const ACCESS_LOG_MAX_ENTRIES: usize = 10_000;
pub const ACCESS_LOG_MIN_WEIGHT: f64 = 0.001;

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
    fn sort_for_serialize(&mut self) {
        self.file_access_log.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.sibling_path.cmp(&right.sibling_path))
        });
        self.conflict_resolutions.sort_by(|left, right| {
            left.resolved_at
                .cmp(&right.resolved_at)
                .then_with(|| left.path.cmp(&right.path))
        });
    }

    pub fn to_json(&self) -> Result<String> {
        let mut sorted = self.clone();
        sorted.sort_for_serialize();
        serde_json::to_string_pretty(&sorted).context("serialize local state")
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let raw: serde_json::Value =
            serde_json::from_str(json).context("parse local state JSON")?;
        let version = raw
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if version == 0 {
            bail!(
                "local_state.json has invalid schema version 0. \
                 Remove it and re-initialize, or upgrade feanorfs."
            );
        }
        if version > u64::from(CURRENT_SCHEMA_VERSION) {
            bail!(
                "local_state.json schema version {version} is newer than supported \
                 (max {CURRENT_SCHEMA_VERSION}). Upgrade feanorfs to open this workspace."
            );
        }
        let state: Self = serde_json::from_value(raw).context("deserialize local state")?;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictRecordV1 {
    pub path: String,
    pub kind: feanorfs_common::ConflictKind,
    pub conflict_dir: String,
    pub opened_at: i64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictResolutionV1 {
    pub path: String,
    pub method: String,
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
