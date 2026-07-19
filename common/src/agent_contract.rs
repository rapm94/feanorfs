//! Frozen JSON contract fixtures for the agent SDK (SDK-1).
//! Snapshot tests compare serialized output against these strings.

use crate::{
    AgentCheckResult, AgentLandResult, AgentRefreshResult, ConcurrentEdit, ConflictKind, FileState,
    LandedPath,
};
use serde::{Deserialize, Serialize};

/// `feanorfs --json agent spawn` result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnResult {
    pub agent: String,
    pub files_copied: usize,
}

/// One row in `feanorfs --json agent status` (list mode, online).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentListEntry {
    pub name: String,
    pub state: String,
}

/// `feanorfs --json agent status` (list mode, online).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentListResult {
    pub agents: Vec<AgentListEntry>,
}

/// `feanorfs --json agent status` (list mode, offline).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentListOfflineResult {
    pub agents: Vec<String>,
}

/// `feanorfs --json agent clean` result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCleanResult {
    pub cleaned: String,
}

/// One immutable workspace snapshot exposed through history APIs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntry {
    pub snapshot_id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub created_at_ms: i64,
    pub message: Option<String>,
    pub changed_paths: Vec<String>,
}

/// Structured workspace history result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogResult {
    pub entries: Vec<LogEntry>,
}

/// Result of restoring a historical snapshot as a new commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoResult {
    pub snapshot_id: String,
    pub restored_snapshot_id: String,
    pub changed_paths: Vec<String>,
}

fn sample_file_state(path: &str) -> FileState {
    FileState {
        path: path.to_string(),
        hash: "a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456".to_string(),
        size: 42,
        mtime: 1_719_500_000_000,
        deleted: false,
        mode: 0,
    }
}

fn sample_concurrent_edit() -> ConcurrentEdit {
    ConcurrentEdit {
        path: "src/main.rs".to_string(),
        base: Some(sample_file_state("src/main.rs")),
        ours: Some(sample_file_state("src/main.rs")),
        theirs: Some(sample_file_state("src/main.rs")),
        original_file: Some(
            "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/src/main.rs.original"
                .to_string(),
        ),
        local_file: Some(
            "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/src/main.rs.local".to_string(),
        ),
        cloud_file: Some(
            "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/src/main.rs.cloud".to_string(),
        ),
        kind: Some(ConflictKind::EditEdit),
        local_available: true,
        cloud_available: true,
        is_binary: false,
        hint: Some("both sides edited since spawn".to_string()),
        proposed_file: None,
        proposal_clean: None,
    }
}

/// Canonical JSON fixtures — update only with a semver-major contract bump.
pub mod fixtures {
    use super::*;

    pub fn spawn_result() -> SpawnResult {
        SpawnResult {
            agent: "ci1".to_string(),
            files_copied: 12,
        }
    }

    pub fn agent_list_result() -> AgentListResult {
        AgentListResult {
            agents: vec![
                AgentListEntry {
                    name: "ci1".to_string(),
                    state: "2 change(s)".to_string(),
                },
                AgentListEntry {
                    name: "ci2".to_string(),
                    state: "clean".to_string(),
                },
            ],
        }
    }

    pub fn agent_list_offline_result() -> AgentListOfflineResult {
        AgentListOfflineResult {
            agents: vec!["ci1".to_string(), "ci2".to_string()],
        }
    }

    pub fn agent_check_result() -> AgentCheckResult {
        AgentCheckResult {
            agent_name: "ci1".to_string(),
            our_changes: vec![sample_file_state("doc.txt")],
            their_changes: vec![],
            conflicts: vec![],
            conflict_risk: vec!["notes.md".to_string()],
        }
    }

    pub fn agent_land_result() -> AgentLandResult {
        AgentLandResult {
            agent_name: "ci1".to_string(),
            our_changes: vec![sample_file_state("doc.txt")],
            their_changes: vec![],
            conflicts: vec![sample_concurrent_edit()],
            landed: vec![LandedPath {
                path: "doc.txt".to_string(),
                action: "applied".to_string(),
            }],
            message: "Landed 1 path; 1 needs attention.".to_string(),
            snapshot_id: None,
        }
    }

    pub fn agent_refresh_result() -> AgentRefreshResult {
        AgentRefreshResult {
            agent_name: "ci1".to_string(),
            refreshed: vec!["README.md".to_string()],
            deferred: vec!["doc.txt".to_string()],
        }
    }

    pub fn agent_clean_result() -> AgentCleanResult {
        AgentCleanResult {
            cleaned: "ci1".to_string(),
        }
    }

    pub fn log_result() -> LogResult {
        LogResult {
            entries: vec![LogEntry {
                snapshot_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(),
                parents: vec![
                    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".to_string(),
                ],
                author: "ci1".to_string(),
                created_at_ms: 1_719_500_000_000,
                message: Some("land".to_string()),
                changed_paths: vec!["src/main.rs".to_string()],
            }],
        }
    }

    pub fn undo_result() -> UndoResult {
        UndoResult {
            snapshot_id: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
            restored_snapshot_id:
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            changed_paths: vec!["src/main.rs".to_string()],
        }
    }

    pub fn spawn_json() -> String {
        serde_json::to_string(&spawn_result()).unwrap()
    }

    pub fn agent_list_json() -> String {
        serde_json::to_string(&agent_list_result()).unwrap()
    }

    pub fn agent_list_offline_json() -> String {
        serde_json::to_string(&agent_list_offline_result()).unwrap()
    }

    pub fn agent_check_json() -> String {
        serde_json::to_string(&agent_check_result()).unwrap()
    }

    pub fn agent_land_json() -> String {
        serde_json::to_string(&agent_land_result()).unwrap()
    }

    pub fn agent_refresh_json() -> String {
        serde_json::to_string(&agent_refresh_result()).unwrap()
    }

    pub fn agent_clean_json() -> String {
        serde_json::to_string(&agent_clean_result()).unwrap()
    }

    pub fn log_json() -> String {
        serde_json::to_string(&log_result()).unwrap()
    }

    pub fn undo_json() -> String {
        serde_json::to_string(&undo_result()).unwrap()
    }
}
