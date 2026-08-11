use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Portable execute-bit marker stored in canonical tree entries.
pub const EXECUTABLE_MODE: u32 = 1;
/// Maximum plaintext bytes in one canonical tree or snapshot object.
pub const MAX_CANONICAL_OBJECT_BYTES: usize = 16 * 1024 * 1024;
/// Exact AEAD framing overhead: prefix, nonce, and authentication tag.
pub const MAX_ENCRYPTED_OBJECT_BYTES: usize = MAX_CANONICAL_OBJECT_BYTES + 29;
/// Maximum possible minimum-sized entries in one bounded tree object.
pub const MAX_TREE_ENTRIES: usize = (MAX_CANONICAL_OBJECT_BYTES - 12) / 94;
/// Maximum parents supported by append/undo snapshot semantics.
pub const MAX_SNAPSHOT_PARENTS: usize = 2;
/// Maximum UTF-8 bytes in one snapshot author label.
pub const MAX_SNAPSHOT_AUTHOR_BYTES: usize = 512;
/// Maximum UTF-8 bytes in one snapshot message or encrypted signal envelope.
pub const MAX_SNAPSHOT_MESSAGE_BYTES: usize = crate::AGENT_MESSAGE_MAX_ENCODED_BYTES;
/// Maximum nested directory levels in one canonical tree graph.
pub const MAX_TREE_DEPTH: usize = 256;
/// Maximum distinct tree objects expanded by one operation.
pub const MAX_TREE_OBJECTS: usize = crate::MANIFEST_MAX_ENTRIES;
/// Maximum flat files/conflicts/changes emitted by one tree operation.
pub const MAX_TREE_OUTPUT_PATHS: usize = crate::MANIFEST_MAX_ENTRIES;
/// Maximum structural work across decoded entries and path components.
pub const MAX_TREE_WORK_ITEMS: usize = 8 * crate::MANIFEST_MAX_ENTRIES;
/// Maximum aggregate UTF-8 bytes retained for emitted canonical paths.
pub const MAX_TREE_PATH_BYTES_TOTAL: usize = 64 * 1024 * 1024;

/// Portable execute-bit intent for every live leg of a first-class conflict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictModes {
    #[serde(default, skip_serializing_if = "is_zero_mode")]
    pub base: u32,
    #[serde(default, skip_serializing_if = "is_zero_mode")]
    pub ours: u32,
    #[serde(default, skip_serializing_if = "is_zero_mode")]
    pub theirs: u32,
}

impl ConflictModes {
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.base == 0 && self.ours == 0 && self.theirs == 0
    }
}

const fn is_zero_mode(mode: &u32) -> bool {
    *mode == 0
}

/// Semantic kind of one canonical tree entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeEntryKind {
    /// Regular file blob.
    File,
    /// Child tree object.
    Dir,
    /// Unresolved three-way conflict. Missing legs represent add/delete shapes.
    Conflict {
        base: Option<String>,
        ours: Option<String>,
        theirs: Option<String>,
        #[serde(default, skip_serializing_if = "ConflictModes::is_zero")]
        modes: ConflictModes,
    },
}

/// One named child in a canonical tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: TreeEntryKind,
    /// File/dir object id, or the conflict leg visible in the working copy.
    pub hash: String,
    pub size: u64,
    pub mode: u32,
}

impl TreeEntry {
    /// Returns whether this entry references a child tree.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        matches!(self.kind, TreeEntryKind::Dir)
    }
}

/// Canonically ordered directory object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// Encodes this tree in platform-independent canonical bytes.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        crate::tree_codec::encode_tree(self)
    }

    /// Decodes and validates canonical tree bytes.
    ///
    /// # Errors
    /// Returns an error for malformed, non-canonical, or unsupported bytes.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
        crate::tree_codec::decode_tree(bytes)
    }

    /// Validates semantic and portable canonical-tree invariants.
    ///
    /// # Errors
    /// Returns an error for invalid entries or portable sibling collisions.
    pub fn validate(&self) -> Result<()> {
        crate::tree_codec::validate_tree(self)
    }

    /// Returns the Blake3 id of this tree's canonical bytes.
    #[must_use]
    pub fn id(&self) -> String {
        crate::hash_bytes(&self.to_canonical_bytes())
    }
}

/// Immutable workspace snapshot pointing to a root tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub root: String,
    pub parents: Vec<String>,
    pub author: String,
    pub created_at_ms: i64,
    pub message: Option<String>,
}

impl Snapshot {
    /// Encodes this snapshot in platform-independent canonical bytes.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        crate::tree_codec::encode_snapshot(self)
    }

    /// Decodes and validates canonical snapshot bytes.
    ///
    /// # Errors
    /// Returns an error for malformed, non-canonical, or unsupported bytes.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
        crate::tree_codec::decode_snapshot(bytes)
    }

    /// Validates object ids and bounded append-history shape.
    ///
    /// # Errors
    /// Returns an error for invalid, duplicate, or excessive parents.
    pub fn validate(&self) -> Result<()> {
        crate::tree_codec::validate_snapshot(self)
    }

    /// Returns the Blake3 id of this snapshot's canonical bytes.
    #[must_use]
    pub fn id(&self) -> String {
        crate::hash_bytes(&self.to_canonical_bytes())
    }
}

/// Bottom-up result of converting a flat file map into immutable trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeBundle {
    pub root: String,
    pub trees: HashMap<String, Tree>,
}

/// Classification of one path-level tree difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One path-level difference between two tree roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeChange {
    pub path: String,
    pub kind: TreeChangeKind,
    pub before: Option<TreeEntry>,
    pub after: Option<TreeEntry>,
}
