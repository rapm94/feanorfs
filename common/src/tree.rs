use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Portable execute-bit marker stored in canonical tree entries.
pub const EXECUTABLE_MODE: u32 = 1;

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
