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
