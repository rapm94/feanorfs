mod migration;
mod store;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::durable::DurableJson;

const CURRENT_HUB_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubStateV1 {
    pub schema_version: u32,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceMetaV1>,
}

impl Default for HubStateV1 {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_HUB_SCHEMA,
            workspaces: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMetaV1 {
    #[serde(default)]
    pub format_version: u32,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub manifests: BTreeMap<String, ManifestV1>,
    #[serde(default)]
    pub files: BTreeMap<String, LegacyFileV1>,
    #[serde(default)]
    pub migration_fence: Option<String>,
    #[serde(default)]
    pub fence_locked_at: i64,
}

impl Default for WorkspaceMetaV1 {
    fn default() -> Self {
        Self {
            format_version: 2,
            head: None,
            manifests: BTreeMap::new(),
            files: BTreeMap::new(),
            migration_fence: None,
            fence_locked_at: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatWrite {
    /// The format transition was applied.
    Applied,
    /// A v3 transition requires a manifested snapshot head.
    MissingManifestHead,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestV1 {
    pub hashes: Vec<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyFileV1 {
    pub hash: String,
    pub size: i64,
    pub mtime: i64,
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub deleted: bool,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationHubState {
    pub workspaces: BTreeMap<String, MigrationHubWorkspace>,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationHubWorkspace {
    pub format_version: u32,
    pub head: Option<String>,
    pub manifests: BTreeMap<String, MigrationHubManifest>,
    pub files: BTreeMap<String, MigrationHubFile>,
    pub migration_fence: Option<MigrationHubFence>,
}

impl Default for MigrationHubWorkspace {
    fn default() -> Self {
        Self {
            format_version: 2,
            head: None,
            manifests: BTreeMap::new(),
            files: BTreeMap::new(),
            migration_fence: None,
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationHubFile {
    pub hash: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub deleted: bool,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationHubManifest {
    pub hashes: Vec<String>,
    pub created_at_ms: i64,
}

#[doc(hidden)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationHubFence {
    pub token: String,
    pub locked_at: i64,
}

impl std::fmt::Debug for MigrationHubFence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MigrationHubFence")
            .field("token", &"<redacted>")
            .field("locked_at", &self.locked_at)
            .finish()
    }
}

pub struct HubDb {
    state: DurableJson<HubStateV1>,
    storage_dir: PathBuf,
}

impl std::fmt::Debug for HubDb {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HubDb")
            .field("storage_dir", &self.storage_dir)
            .finish_non_exhaustive()
    }
}
