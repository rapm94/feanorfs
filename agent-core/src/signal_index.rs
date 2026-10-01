//! Durable cache of walked signal-snapshot records for inbox reads.
//!
//! `collect_signals` walks the snapshot DAG and loads every visited snapshot
//! object; uncached objects cost one hub round trip each, so cursor-reset
//! rebuilds and long-absence catch-ups re-pay the whole delta. Snapshot ids
//! are content hashes, so a record keyed by id is valid forever — this index
//! is pure derived cache, never authority. Unlike schema-versioned state
//! stores it therefore resets silently on corruption or a newer schema
//! instead of failing closed: a miss only costs a refetch.

use feanorfs_common::{
    is_valid_hash, Snapshot, MAX_SNAPSHOT_AUTHOR_BYTES, MAX_SNAPSHOT_MESSAGE_BYTES,
    MAX_SNAPSHOT_PARENTS,
};
use std::io::Read;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SIGNAL_INDEX_SCHEMA_VERSION: u32 = 1;
const SIGNAL_INDEX_FILE: &str = "signals-index.json";
const SIGNAL_INDEX_MAX_ENTRIES: usize = 8192;
const SIGNAL_INDEX_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// One walked snapshot's walk-relevant record (root is never needed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexedSignal {
    parents: Vec<String>,
    author: String,
    created_at_ms: i64,
    message: Option<String>,
}

impl IndexedSignal {
    fn structurally_valid(&self, id: &str) -> bool {
        is_valid_hash(id)
            && self.parents.len() <= MAX_SNAPSHOT_PARENTS
            && self.parents.iter().all(|parent| is_valid_hash(parent))
            && self.author.len() <= MAX_SNAPSHOT_AUTHOR_BYTES
            && self.message.as_ref().is_none_or(|message| message.len() <= MAX_SNAPSHOT_MESSAGE_BYTES)
    }
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
        let entries = (|| {
            let file = std::fs::File::open(state_dir.join(SIGNAL_INDEX_FILE)).ok()?;
            if file.metadata().ok()?.len() > SIGNAL_INDEX_MAX_BYTES {
                return None;
            }
            let mut bytes = Vec::new();
            file.take(SIGNAL_INDEX_MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() as u64 > SIGNAL_INDEX_MAX_BYTES {
                return None;
            }
            let mut file: SignalIndexFile = serde_json::from_slice(&bytes).ok()?;
            if file.schema_version != SIGNAL_INDEX_SCHEMA_VERSION {
                return None;
            }
            // put() receives fetched, verified snapshots. On load, structural
            // validation and the byte cap are the integrity boundary: full
            // re-authentication requires refetching, deliberately avoided for
            // this disposable cache. It is never snapshot authority.
            file.entries.retain(|id, entry| entry.structurally_valid(id));
            while file.entries.len() > SIGNAL_INDEX_MAX_ENTRIES {
                file.entries.pop_first();
            }
            Some(file.entries)
        })().unwrap_or_default();
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
        let entry = IndexedSignal::from(snapshot);
        if self.entries.contains_key(id) || !entry.structurally_valid(id) {
            return;
        }
        while self.entries.len() >= SIGNAL_INDEX_MAX_ENTRIES {
            let Some(smallest) = self.entries.keys().next().cloned() else {
                break;
            };
            self.entries.remove(&smallest);
        }
        self.entries.insert(id.to_string(), entry);
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
            parents: vec![hex_id(b'b')],
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
        assert_eq!(restored.parents, vec![hex_id(b'b')]);
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

    #[test]
    fn oversized_index_resets_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIGNAL_INDEX_FILE);
        let file = std::fs::File::create(path).unwrap();
        file.set_len(SIGNAL_INDEX_MAX_BYTES + 1).unwrap();
        assert!(SignalIndexSession::load(dir.path()).entries.is_empty());
    }

    #[test]
    fn loaded_invalid_entries_are_dropped_without_losing_valid_entries() {
        let dir = tempfile::tempdir().unwrap();
        let good = IndexedSignal::from(&snapshot(None));
        let mut entries = BTreeMap::from([(hex_id(b'a'), good.clone())]);
        let mut bad = good.clone();
        bad.parents = vec![hex_id(b'b'); MAX_SNAPSHOT_PARENTS + 1];
        entries.insert(hex_id(b'c'), bad);
        let mut bad = good.clone();
        bad.author = "a".repeat(MAX_SNAPSHOT_AUTHOR_BYTES + 1);
        entries.insert(hex_id(b'd'), bad);
        let mut bad = good.clone();
        bad.message = Some("x".repeat(MAX_SNAPSHOT_MESSAGE_BYTES + 1));
        entries.insert(hex_id(b'e'), bad);
        let mut bad = good.clone();
        bad.parents = vec!["invalid".into()];
        entries.insert(hex_id(b'f'), bad);
        entries.insert("invalid-id".into(), good);
        let file = SignalIndexFile { schema_version: SIGNAL_INDEX_SCHEMA_VERSION, entries };
        std::fs::write(dir.path().join(SIGNAL_INDEX_FILE), serde_json::to_vec(&file).unwrap()).unwrap();
        let loaded = SignalIndexSession::load(dir.path());
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.get(&hex_id(b'a')).is_some());
    }

    #[test]
    fn loaded_cardinality_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let entry = IndexedSignal::from(&snapshot(None));
        let file = SignalIndexFile {
            schema_version: SIGNAL_INDEX_SCHEMA_VERSION,
            entries: (0..SIGNAL_INDEX_MAX_ENTRIES + 1)
                .map(|index| (format!("{index:064x}"), entry.clone())).collect(),
        };
        std::fs::write(dir.path().join(SIGNAL_INDEX_FILE), serde_json::to_vec(&file).unwrap()).unwrap();
        let loaded = SignalIndexSession::load(dir.path());
        assert_eq!(loaded.entries.len(), SIGNAL_INDEX_MAX_ENTRIES);
        assert!(loaded.get(&format!("{:064x}", 0)).is_none());
    }

    #[tokio::test]
    async fn clean_session_flush_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SignalIndexSession::load(dir.path());
        session.flush().await;
        assert!(!dir.path().join(SIGNAL_INDEX_FILE).exists());
    }
}
