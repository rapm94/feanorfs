//! Journal and state types for materialization.

use feanorfs_common::FileState;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct PreserveMaterializationStage(anyhow::Error);

impl std::fmt::Display for PreserveMaterializationStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for PreserveMaterializationStage {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

pub(crate) fn preserve_materialization_stage(error: anyhow::Error) -> anyhow::Error {
    PreserveMaterializationStage(error).into()
}

pub(crate) fn must_preserve_materialization_stage(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<PreserveMaterializationStage>()
        .is_some()
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct JournalDownload {
    pub(crate) file: FileState,
    pub(crate) plaintext_hash: String,
    pub(crate) hydrated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct MaterializationDirectoryIdentity {
    pub(crate) volume_serial: u32,
    pub(crate) file_index: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct MaterializationDirectoryProof {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) identity: Option<MaterializationDirectoryIdentity>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct MaterializationJournal {
    pub(crate) phase: String,
    /// Explicitly distinguishes journals written by the current activation
    /// protocol from older journals that predate per-publication progress.
    #[serde(default)]
    pub(crate) publication_progress_recorded: bool,
    pub(crate) original_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) original_readonly: std::collections::BTreeMap<String, bool>,
    pub(crate) downloads: Vec<JournalDownload>,
    pub(crate) delete_paths: Vec<String>,
    /// Paths whose publication hard link was verified and durably recorded.
    /// Missing in older journals, which retain the conservative legacy
    /// recovery heuristics below.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) published_paths: Vec<String>,
    /// Path whose publication is between the pre-link journal write and the
    /// post-link progress write. Missing in older journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) publishing_path: Option<String>,
    /// Exact identities of destination directories created by this
    /// activation. Missing in old journals, so recovery removes none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) created_directories: Vec<MaterializationDirectoryProof>,
}

pub(crate) struct MaterializationAnchors {
    #[cfg(unix)]
    pub(crate) base: std::fs::File,
    #[cfg(unix)]
    pub(crate) stage: std::fs::File,
}

pub(crate) struct MaterializationBackup {
    pub(crate) path: String,
    #[cfg(not(unix))]
    pub(crate) original: PathBuf,
    #[cfg(not(unix))]
    pub(crate) backup: PathBuf,
    #[cfg(not(unix))]
    pub(crate) readonly: bool,
    #[cfg(unix)]
    pub(crate) original_parent: std::fs::File,
    #[cfg(unix)]
    pub(crate) backup_parent: std::fs::File,
}

#[cfg(unix)]
pub(crate) struct CreatedMaterializationDirectory {
    pub(crate) parent: std::fs::File,
    pub(crate) directory: std::fs::File,
    pub(crate) name: std::ffi::CString,
    pub(crate) path: String,
}

pub(crate) struct PublishedDownload {
    pub(crate) destination: PathBuf,
    pub(crate) expected: JournalDownload,
    pub(crate) mode_applied: bool,
    #[cfg(unix)]
    pub(crate) file: std::fs::File,
    #[cfg(windows)]
    /// Delete-capable handle for the exact inode published by this
    /// transaction.  Windows rollback must use this handle rather than
    /// reopening the destination path, which may have been replaced by a
    /// user while activation was in flight.
    pub(crate) file: std::fs::File,
    #[cfg(unix)]
    pub(crate) directory_chain: Vec<std::fs::File>,
    #[cfg(unix)]
    pub(crate) created_directories: Vec<CreatedMaterializationDirectory>,
    #[cfg(not(unix))]
    pub(crate) created_directories: Vec<MaterializationDirectoryProof>,
}
