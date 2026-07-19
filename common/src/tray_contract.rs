//! JSON contract for the menu-bar tray (`feanorfs tray status --json`).

use serde::{Deserialize, Serialize};

/// Aggregate dashboard for the tray app — one subprocess call instead of three.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayStatusResult {
    /// `idle` | `out_of_sync` | `offline` | `conflict` | `error` | `syncing`
    pub mirror_state: String,
    pub paused: bool,
    pub watching: bool,
    pub workspace_path: String,
    pub workspace_id: String,
    pub workspace_label: String,
    pub pending_conflicts: Vec<TrayConflictEntry>,
    pub agents: TrayAgentsSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayConflictEntry {
    pub path: String,
    /// `edit_edit` | `edit_delete` | `delete_edit`
    pub kind: String,
    /// Plain-language one-liner for humans (tray menu header).
    pub label: String,
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayAgentsSummary {
    pub working: u32,
    pub need_attention: u32,
    pub entries: Vec<TrayAgentEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayAgentEntry {
    pub name: String,
    /// `clean` | `changes` | `conflicts` | `offline`
    pub state: String,
    pub change_count: u32,
    pub conflict_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentWorkspacesResult {
    pub active: Option<String>,
    pub workspaces: Vec<RecentWorkspaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentWorkspaceEntry {
    pub path: String,
    pub workspace_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayPauseResult {
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictKeepResult {
    pub resolved: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictShowResult {
    pub path: String,
    pub kind: String,
    pub local_path: String,
    pub cloud_path: String,
    pub original_path: String,
    pub is_binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// Canonical JSON fixtures — update only with a semver-major contract bump.
pub mod fixtures {
    use super::*;

    pub fn tray_status_result() -> TrayStatusResult {
        TrayStatusResult {
            mirror_state: "idle".into(),
            paused: false,
            watching: true,
            workspace_path: "/Users/dev/project".into(),
            workspace_id: "my-workspace".into(),
            workspace_label: "my-workspace".into(),
            pending_conflicts: vec![TrayConflictEntry {
                path: "notes.txt".into(),
                kind: "edit_edit".into(),
                label: "Both sides changed notes.txt".into(),
                choices: vec!["local".into(), "cloud".into(), "both".into()],
            }],
            agents: TrayAgentsSummary {
                working: 1,
                need_attention: 0,
                entries: vec![TrayAgentEntry {
                    name: "ci1".into(),
                    state: "changes".into(),
                    change_count: 2,
                    conflict_count: 0,
                }],
            },
        }
    }

    pub fn recent_workspaces_result() -> RecentWorkspacesResult {
        RecentWorkspacesResult {
            active: Some("/Users/dev/project".into()),
            workspaces: vec![RecentWorkspaceEntry {
                path: "/Users/dev/project".into(),
                workspace_id: "my-workspace".into(),
                label: "my-workspace".into(),
            }],
        }
    }

    pub fn tray_pause_result() -> TrayPauseResult {
        TrayPauseResult { paused: true }
    }

    pub fn conflict_keep_result() -> ConflictKeepResult {
        ConflictKeepResult {
            resolved: "notes.txt".into(),
        }
    }

    pub fn conflict_show_result() -> ConflictShowResult {
        ConflictShowResult {
            path: "notes.txt".into(),
            kind: "edit_edit".into(),
            local_path: "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/notes.txt.local"
                .into(),
            cloud_path: "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/notes.txt.cloud"
                .into(),
            original_path:
                "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/notes.txt.original".into(),
            is_binary: false,
            diff: Some("--- notes.txt\n+++ notes.txt\n@@\n-local\n+cloud\n".into()),
        }
    }

    pub fn tray_status_json() -> String {
        serde_json::to_string(&tray_status_result()).unwrap()
    }

    pub fn recent_workspaces_json() -> String {
        serde_json::to_string(&recent_workspaces_result()).unwrap()
    }

    pub fn tray_pause_json() -> String {
        serde_json::to_string(&tray_pause_result()).unwrap()
    }

    pub fn conflict_keep_json() -> String {
        serde_json::to_string(&conflict_keep_result()).unwrap()
    }

    pub fn conflict_show_json() -> String {
        serde_json::to_string(&conflict_show_result()).unwrap()
    }
}
