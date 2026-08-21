//! Durable cache of walked signal-snapshot records for inbox reads.
//!
//! `collect_signals` walks the snapshot DAG and loads every visited snapshot
//! object; uncached objects cost one hub round trip each, so cursor-reset
//! rebuilds and long-absence catch-ups re-pay the whole delta. Snapshot ids
//! are content hashes, so a record keyed by id is valid forever — this index
//! is pure derived cache, never authority. Unlike schema-versioned state
//! stores it therefore resets silently on corruption or a newer schema
//! instead of failing closed: a miss only costs a refetch.

use feanorfs_common::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SIGNAL_INDEX_SCHEMA_VERSION: u32 = 1;
const SIGNAL_INDEX_FILE: &str = "signals-index.json";
const SIGNAL_INDEX_MAX_ENTRIES: usize = 8192;

/// One walked snapshot's walk-relevant record (root is never needed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexedSignal {
    parents: Vec<String>,
    author: String,
    created_at_ms: i64,
    message: Option<String>,
}

impl From<&Snapshot> for IndexedSignal {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            parents: snapshot.parents.clone(),
            author: snapshot.author.clone(),
            created_at_ms: snapshot.created_at_ms,
            message: snapshot.message.clone(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SignalIndexFile {
    schema_version: u32,
    entries: BTreeMap<String, IndexedSignal>,
}

/// In-memory session over the durable signal index. Load once per inbox
/// read, consult before hub loads, flush once at the end (best-effort).
pub(crate) struct SignalIndexSession {
    path: PathBuf,
    entries: BTreeMap<String, IndexedSignal>,
    dirty: bool,
}

impl SignalIndexSession {
    /// A session that never loads or persists anything (unresolvable state
    /// directory).
    pub(crate) fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            entries: BTreeMap::new(),
            dirty: false,
        }
    }

    /// Loads the index, resetting silently on absence, corruption, or an
    /// unsupported schema version.
    pub(crate) fn load(state_dir: &Path) -> Self {
        let entries = std::fs::read_to_string(state_dir.join(SIGNAL_INDEX_FILE))
            .ok()
            .and_then(|text| {
                let file: SignalIndexFile = serde_json::from_str(&text).ok()?;
                (file.schema_version == SIGNAL_INDEX_SCHEMA_VERSION).then_some(file.entries)
            })
            .unwrap_or_default();
        Self {
            path: state_dir.to_path_buf(),
            entries,
            dirty: false,
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<Snapshot> {
        self.entries.get(id).map(|entry| Snapshot {
            root: String::new(),
            parents: entry.parents.clone(),
            author: entry.author.clone(),
            created_at_ms: entry.created_at_ms,
            message: entry.message.clone(),
        })
    }

    pub(crate) fn put(&mut self, id: &str, snapshot: &Snapshot) {
        if self.entries.contains_key(id) {
            return;
        }
        while self.entries.len() >= SIGNAL_INDEX_MAX_ENTRIES {
            let Some(smallest) = self.entries.keys().next().cloned() else {
                break;
            };
            self.entries.remove(&smallest);
        }
        self.entries
            .insert(id.to_string(), IndexedSignal::from(snapshot));
        self.dirty = true;
    }

    /// Persists accumulated entries; failures are non-fatal because the
    /// index is a pure cache.
    pub(crate) async fn flush(&mut self) {
        if !self.dirty || self.path.as_os_str().is_empty() {
            return;
        }
        let file = SignalIndexFile {
            schema_version: SIGNAL_INDEX_SCHEMA_VERSION,
            entries: std::mem::take(&mut self.entries),
        };
        match serde_json::to_vec(&file) {
            Ok(bytes) => {
                if let Err(error) =
                    crate::fs_util::atomic_write_visible(&self.path, SIGNAL_INDEX_FILE, &bytes)
                        .await
                {
                    tracing::debug!("signal index flush failed (cache-only): {error}");
                } else {
                    self.dirty = false;
                }
            }
            Err(error) => tracing::debug!("signal index serialization failed: {error}"),
        }
        self.entries = file.entries;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(message: Option<&str>) -> Snapshot {
        Snapshot {
            root: "r".to_string(),
            parents: vec!["p".to_string()],
            author: "worker".to_string(),
            created_at_ms: 42,
            message: message.map(str::to_string),
        }
    }

    fn hex_id(byte: u8) -> String {
        std::iter::repeat_n(byte as char, 64).collect()
    }

    #[tokio::test]
    async fn roundtrip_and_absent_file_start_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SignalIndexSession::load(dir.path());
        assert!(session.get(&hex_id(b'a')).is_none());

        session.put(&hex_id(b'a'), &snapshot(Some("ffmsg1:x")));
        session.flush().await;

        let reloaded = SignalIndexSession::load(dir.path());
        let restored = reloaded.get(&hex_id(b'a')).expect("entry survives reload");
        assert_eq!(restored.parents, vec!["p".to_string()]);
        assert_eq!(restored.author, "worker");
        assert_eq!(restored.created_at_ms, 42);
        assert_eq!(restored.message.as_deref(), Some("ffmsg1:x"));
    }

    #[tokio::test]
    async fn corrupt_or_newer_schema_resets_silently() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SignalIndexSession::load(dir.path());
        session.put(&hex_id(b'a'), &snapshot(None));
        session.flush().await;

        std::fs::write(dir.path().join(SIGNAL_INDEX_FILE), "not json").unwrap();
        assert!(SignalIndexSession::load(dir.path())
            .get(&hex_id(b'a'))
            .is_none());

        let newer = SignalIndexFile {
            schema_version: SIGNAL_INDEX_SCHEMA_VERSION + 1,
            entries: BTreeMap::new(),
        };
        std::fs::write(
            dir.path().join(SIGNAL_INDEX_FILE),
            serde_json::to_string(&newer).unwrap(),
        )
        .unwrap();
        assert!(SignalIndexSession::load(dir.path())
            .get(&hex_id(b'a'))
            .is_none());
    }

    #[tokio::test]
    async fn bound_evicts_smallest_ids_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SignalIndexSession::load(dir.path());
        for i in 0..(SIGNAL_INDEX_MAX_ENTRIES + 10) {
            session.put(&format!("{i:064x}"), &snapshot(None));
        }
        assert_eq!(session.entries.len(), SIGNAL_INDEX_MAX_ENTRIES);
        // The ten smallest ids were evicted; the newest survive.
        for i in 0..10 {
            assert!(session.get(&format!("{i:064x}")).is_none());
        }
        assert!(session
            .get(&format!("{:064x}", SIGNAL_INDEX_MAX_ENTRIES + 9))
            .is_some());

        session.flush().await;
        let reloaded = SignalIndexSession::load(dir.path());
        assert_eq!(reloaded.entries.len(), SIGNAL_INDEX_MAX_ENTRIES);
    }

    #[tokio::test]
    async fn clean_session_flush_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SignalIndexSession::load(dir.path());
        session.flush().await;
        assert!(!dir.path().join(SIGNAL_INDEX_FILE).exists());
    }
}
