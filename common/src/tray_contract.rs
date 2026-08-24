//! JSON contract for the menu-bar tray (`feanorfs tray status --json`).

use serde::{Deserialize, Serialize};

/// Marks a tray launched by the macOS/Linux supervisor or Windows Task
/// Scheduler. A managed child that loses the singleton race exits retryably
/// instead of being mistaken for an explicit user Quit.
pub const MANAGED_TRAY_ARG: &str = "--managed";

/// Aggregate dashboard for the tray app — one subprocess call instead of three.
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct TrayStatusResult {
    /// `idle` | `out_of_sync` | `offline` | `conflict` | `error` | `syncing`
    pub mirror_state: String,
    pub paused: bool,
    pub watching: bool,
    pub workspace_path: String,
    pub workspace_id: String,
    pub workspace_label: String,
    /// Total pending conflicts, including entries omitted from the bounded list.
    pub pending_conflict_count: u32,
    pub pending_conflicts: Vec<TrayConflictEntry>,
    pub agents: TrayAgentsSummary,
}

/// One bounded desktop refresh: status plus the global folder registry.
///
/// Keeping this as a separate additive contract preserves the shipped
/// `TrayStatusResult` JSON shape while allowing the tray to use one CLI
/// process per refresh instead of separate status and recent-list processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayOverviewResult {
    pub status: TrayStatusResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent: Option<RecentWorkspacesResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct TrayConflictEntry {
    pub path: String,
    /// `edit_edit` | `edit_delete` | `delete_edit`
    pub kind: String,
    /// Plain-language one-liner for humans (tray menu header).
    pub label: String,
    pub choices: Vec<String>,
}

/// Bounded, secret-free status snapshot published by the managed sync worker
/// after each sync. Routine tray refreshes read this file instead of scanning
/// the project or taking the sync lock, so tray polling cannot delay
/// file-change synchronization even in very large workspaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatusSnapshot {
    /// `idle` | `out_of_sync` | `offline` | `conflict` | `error` | `syncing`
    pub mirror_state: String,
    /// Total pending conflicts, including entries omitted from the bounded list.
    pub pending_conflict_count: u32,
    pub pending_conflicts: Vec<TrayConflictEntry>,
    pub published_at_ms: i64,
    pub version: String,
    /// Bounded live-reconciliation health; additive, secret-free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuous: Option<ContinuousHealth>,
    /// Bounded resolution counts/status projection (ids/state/counts only,
    /// never paths or bodies); additive, secret-free. Mutation stays in the
    /// CLI (`feanorfs agent resolution …`); the tray never resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionHealth>,
    /// Last observed mesh path class for this workspace:
    /// `lan` | `direct` | `direct_mapped` | `punched` | `unreachable`.
    /// Additive, secret-free; derived from local attempt state only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_reachability: Option<String>,
}

/// Bounded, secret-free resolution counts/status projection for the tray:
/// constant-cost job counts by lifecycle state, never paths or bodies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolutionHealth {
    /// Active jobs (prepared, no submitted result).
    pub active: u32,
    /// Submitted-but-not-applied jobs (including `requires_human`).
    pub submitted: u32,
    /// Completed (published) jobs.
    pub completed: u32,
    /// Revoked or superseded jobs.
    pub revoked: u32,
    /// Submitted `requires_human` jobs awaiting an explicit human decision.
    pub requires_human: u32,
}

/// Fixed, secret-free live-reconciliation health for the tray and `doctor`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContinuousHealth {
    pub agents_live: u32,
    pub agents_attention: u32,
    pub agents_offline: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct TrayAgentsSummary {
    pub working: u32,
    pub need_attention: u32,
    pub entries: Vec<TrayAgentEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct TrayAgentEntry {
    pub name: String,
    /// `clean` | `changes` | `conflicts` | `offline`
    pub state: String,
    pub change_count: u32,
    pub conflict_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct RecentWorkspacesResult {
    pub active: Option<String>,
    pub workspaces: Vec<RecentWorkspaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
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

/// Typed outcome of one CLI setup/start run (`feanorfs --json start` and the
/// `tray join` final event).
///
/// The tray classifies partial setup failures from `stage`, `committed`, and
/// `recovery` — never from the human-readable `detail` text. `detail` exists
/// only for presentation ("Details: …" in dialogs); changing CLI wording can
/// never change tray state classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupResult {
    /// Furthest setup stage completed before the outcome.
    pub stage: SetupStage,
    /// Durable effects committed before the outcome.
    pub committed: SetupCommitted,
    /// True when rerunning setup/start resumes without redoing completed stages.
    pub retryable: bool,
    /// Safe recovery action; absent only when every stage completed.
    pub recovery: Option<SetupRecovery>,
    /// Stable human presentation detail; never used for classification.
    pub detail: Option<String>,
}

/// Setup stages in completion order.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetupStage {
    /// No setup stage completed; the workspace was not configured.
    None,
    /// Workspace identity established (paired, linked, or freshly created); the
    /// initial sync did not complete.
    Paired,
    /// The initial sync completed; the automatic background service is not installed.
    InitialSync,
    /// The background service is installed; the system tray is not registered.
    ServiceInstalled,
    /// Every setup stage completed.
    TrayRegistered,
}

/// Durable effects committed by a setup run.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupCommitted {
    pub workspace_configured: bool,
    pub initial_sync_completed: bool,
    pub background_service_installed: bool,
    pub tray_registered: bool,
}

/// Safe recovery action for a partial setup outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetupRecovery {
    /// Nothing durable was committed; rerun the whole setup flow.
    RetrySetup,
    /// Resume the interrupted setup by rerunning `feanorfs start`; completed
    /// stages (identity, sync, service) are preserved.
    RetryStart,
    /// Reinstall the automatic background service.
    ReinstallService,
    /// Re-register the system tray.
    RetryTray,
}

impl SetupResult {
    /// Outcome when every setup stage completed.
    pub fn completed() -> Self {
        Self {
            stage: SetupStage::TrayRegistered,
            committed: SetupCommitted {
                workspace_configured: true,
                initial_sync_completed: true,
                background_service_installed: true,
                tray_registered: true,
            },
            retryable: false,
            recovery: None,
            detail: None,
        }
    }

    /// Outcome when the flow failed before any stage committed.
    pub fn generic(detail: &str) -> Self {
        Self {
            stage: SetupStage::None,
            committed: SetupCommitted::default(),
            retryable: true,
            recovery: Some(SetupRecovery::RetrySetup),
            detail: Some(detail.to_string()),
        }
    }

    /// Outcome for a staged failure: `stage` is the furthest stage completed,
    /// `committed` lists the durable effects, and `detail` is the stable human
    /// presentation of the error (never classified).
    pub fn staged(
        stage: SetupStage,
        committed: SetupCommitted,
        recovery: SetupRecovery,
        detail: &str,
    ) -> Self {
        Self {
            stage,
            committed,
            retryable: true,
            recovery: Some(recovery),
            detail: Some(detail.to_string()),
        }
    }
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
            pending_conflict_count: 1,
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

    pub fn tray_overview_result() -> TrayOverviewResult {
        TrayOverviewResult {
            status: tray_status_result(),
            recent: Some(recent_workspaces_result()),
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

    pub fn tray_overview_json() -> String {
        serde_json::to_string(&tray_overview_result()).unwrap()
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

    pub fn worker_status() -> WorkerStatusSnapshot {
        WorkerStatusSnapshot {
            mirror_state: "idle".into(),
            pending_conflict_count: 1,
            pending_conflicts: vec![TrayConflictEntry {
                path: "notes.txt".into(),
                kind: "edit_edit".into(),
                label: "Both sides changed notes.txt".into(),
                choices: vec!["local".into(), "cloud".into(), "both".into()],
            }],
            published_at_ms: 1_719_500_000_000,
            version: env!("CARGO_PKG_VERSION").into(),
            continuous: None,
            resolution: None,
            mesh_reachability: Some("lan".into()),
        }
    }

    pub fn worker_status_json() -> String {
        serde_json::to_string(&worker_status()).unwrap()
    }
}
