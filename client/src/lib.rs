pub mod agent;
pub mod api;
pub mod commands;
pub mod conflict_artifacts;
pub mod conflicts;
pub mod ctx;
pub mod fs_util;
pub mod hub;
pub mod local;
pub mod lock;
pub mod migrate;
pub mod migrate_sqlite;
pub mod predictive;
pub mod recent;
pub mod summary;
pub mod tray;
pub mod tray_state;
pub mod watch;

pub use api::ApiClient;
pub use commands::{
    do_cat, do_hydrate, do_pull_only, do_push_only, do_status, do_sync, prune_ignored, CatResult,
    HydrateResult, MirrorState, PruneIgnoredResult, PullResult, PushResult, StatusResult,
    SyncResult,
};
pub use conflict_artifacts::{resolve_artifact, ArtifactRole};
pub use conflicts::{resolve_conflict, ResolveKeep};
pub use ctx::SyncCtx;
pub use feanorfs_agent_core::{
    check_agent, clean_agent, commit_agent, land_agent, list_agents, refresh_agent,
    refresh_agent_with_options, spawn_agent, LandOptions, RefreshOptions, Runtime, SpawnOptions,
    Workspace,
};
pub use feanorfs_common::{
    decode_invite, encode_invite, looks_like_invite, WorkspaceInvite, INVITE_PREFIX,
};
pub use feanorfs_common::{
    AgentCheckResult, AgentCommitResult, AgentLandResult, AgentRefreshResult, ConcurrentEdit,
    ConflictKind, ConflictRecord, ConflictResolution, FileState, LegacyPolicy, LogEntry, LogResult,
    UndoResult,
};
pub use feanorfs_common::{
    AgentCleanResult, AgentListEntry, AgentListOfflineResult, AgentListResult, SpawnResult,
};
pub use feanorfs_common::{
    ConflictKeepResult, ConflictShowResult, RecentWorkspaceEntry, RecentWorkspacesResult,
    TrayAgentEntry, TrayAgentsSummary, TrayConflictEntry, TrayStatusResult,
};
pub use local::{
    load_config, load_global_config, save_config, save_global_config, validate_e2ee_key, ClientDb,
    Config, GlobalConfig, LOCAL_HUB_URL,
};
pub use migrate::{legacy_policy_for_config, migrate_workspace};
pub use recent::{list_recent_workspaces, register_workspace, set_active_workspace};
pub use tray::{build_conflict_show, do_tray_status, invalidate_agent_cache};
pub use tray_state::{
    clear_watch_pid, is_paused, is_syncing, is_watching, set_paused, write_watch_pid,
};

pub use commands::do_cat as cat;
pub use commands::do_hydrate as hydrate;
pub use commands::do_pull_only as pull;
pub use commands::do_push_only as push;
pub use commands::do_sync as sync;

use anyhow::Result;
use std::path::Path;

pub async fn open_client_db(workspace_root: &Path) -> Result<feanorfs_agent_core::ClientDb> {
    crate::migrate_sqlite::migrate_workspace_stores(workspace_root).await?;
    feanorfs_agent_core::ClientDb::new(workspace_root.join(".feanorfs")).await
}

pub async fn open_api_client(
    workspace_root: &Path,
    config: &crate::local::Config,
) -> Result<feanorfs_agent_core::ApiClient> {
    crate::migrate_sqlite::migrate_workspace_stores(workspace_root).await?;
    feanorfs_agent_core::ApiClient::from_config(workspace_root, config).await
}

// Back-compat type alias
pub use feanorfs_common::AgentCommitResult as CommitResult;
