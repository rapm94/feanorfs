//! Embeddable agent workspace isolation — blocking API over async internals.
//!
//! Rust consumers call [`Runtime`] / [`Workspace`] directly. FFI and Node bindings
//! serialize the same JSON shapes documented in `docs/agent-api.md`.

pub mod agent;
pub mod api;
pub mod conflict_artifacts;
pub mod conflicts;
pub mod crypto;
pub mod ctx;
mod durable;
pub mod fs_util;
mod head;
pub mod history;
pub mod hub;
mod hub_state;
pub mod large_file;
pub mod local;
pub mod lock;
mod object_gc;
pub mod objects;
pub mod paths;
mod prepared_tree;
pub mod snapshot;
mod snapshot_diff;
mod state;
pub mod sync_pass;
mod tree_reconcile;
pub mod tunnel;
pub mod workspace_layout;

pub use agent::{
    check_agent, clean_agent, commit_agent, land_agent, list_agents, refresh_agent,
    refresh_agent_with_options, spawn_agent, RefreshOptions,
};
pub use api::ApiClient;
pub use conflict_artifacts::{resolve_artifact, ArtifactRole};
pub use conflicts::{resolve_conflict, ResolveKeep};
pub use ctx::SyncCtx;
pub use feanorfs_common::{
    decode_invite, encode_invite, looks_like_invite, AgentCheckResult, AgentCleanResult,
    AgentCommitResult, AgentLandResult, AgentListEntry, AgentListOfflineResult, AgentListResult,
    AgentRefreshResult, ConcurrentEdit, ConflictKind, ConflictRecord, FileState, RelayConfig,
    SpawnResult, WorkspaceInvite, INVITE_PREFIX,
};
pub use head::SwapHeadResult;
pub use history::{log, undo};
pub use hub::LocalHub;
pub use local::{
    load_config, load_global_config, save_config, save_config_secure, save_global_config,
    save_global_config_secure, validate_e2ee_key, ClientDb, Config, CredentialProtection,
    GlobalConfig, LOCAL_HUB_URL,
};
pub use objects::ObjectStore;
pub use paths::legacy_policy_for_config;
pub use paths::{agent_dir, agents_dir, conflicts_dir, validate_name};
pub use snapshot::SnapshotEngine;
pub use snapshot_diff::TreeDiffStats;
pub use workspace_layout::{
    ensure_workspace_state, global_state_root, maintain_workspace_state, workspace_is_configured,
    workspace_state_id, workspace_state_path,
};

use anyhow::{Context, Result};
#[doc(hidden)]
pub use hub_state::{
    MigrationHubFence, MigrationHubFile, MigrationHubManifest, MigrationHubState,
    MigrationHubWorkspace,
};
#[doc(hidden)]
pub use local::ClientDb as _ClientDb;
#[doc(hidden)]
pub use state::{
    MigrationAccessEntry, MigrationCacheEntry, MigrationConflictRecord,
    MigrationConflictResolution, MigrationLocalState,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Shared Tokio runtime for blocking SDK callers.
pub struct Runtime {
    inner: tokio::runtime::Runtime,
}

impl Runtime {
    /// Build a multi-thread Tokio runtime for agent operations.
    pub fn new() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build Tokio runtime")?,
        }))
    }

    /// Run an async future to completion on this runtime.
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.inner.block_on(fut)
    }

    /// Open a workspace rooted at `path` (state is resolved under `~/.feanorfs`).
    pub fn open_workspace(self: &Arc<Self>, path: impl AsRef<Path>) -> Result<Workspace> {
        Workspace::open(self, path.as_ref())
    }
}

/// Options for [`Workspace::spawn`].
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub no_sync: bool,
    pub replace: bool,
}

/// Options for [`Workspace::land`].
#[derive(Debug, Clone, Default)]
pub struct LandOptions {
    pub clean: bool,
    pub propose: bool,
}

/// A configured FeanorFS workspace with agent operations.
pub struct Workspace {
    root: PathBuf,
    rt: Arc<Runtime>,
    config: Config,
    db: ClientDb,
    api: ApiClient,
}

impl Workspace {
    /// Load config, cache DB, and transport for a workspace directory.
    pub fn open(rt: &Arc<Runtime>, root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        let config = load_config(&root)?;
        let state = ensure_workspace_state(&root)?;
        let db = rt.block_on(ClientDb::new(state))?;
        let api = rt.block_on(ApiClient::from_config(&root, &config))?;
        Ok(Self {
            root,
            rt: Arc::clone(rt),
            config,
            db,
            api,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    fn password(&self) -> Option<&str> {
        self.config.encryption_password.as_deref()
    }

    fn workspace_id(&self) -> &str {
        &self.config.workspace_id
    }

    /// List agent workspace names from global workspace state.
    pub fn list(&self) -> Result<Vec<String>> {
        self.rt.block_on(list_agents(&self.root, &self.db))
    }

    /// Return the absolute worktree path for an existing agent.
    pub fn agent_path(&self, name: &str) -> Result<PathBuf> {
        let path = agent_dir(&self.root, name)?;
        if !path.is_dir() {
            anyhow::bail!("agent workspace '{name}' not found");
        }
        Ok(path)
    }

    /// Spawn an isolated agent workspace.
    pub fn spawn(&self, name: &str, opts: SpawnOptions) -> Result<SpawnResult> {
        let files_copied = self.rt.block_on(spawn_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
            opts.no_sync,
            opts.replace,
        ))?;
        Ok(SpawnResult {
            agent: name.to_string(),
            files_copied,
        })
    }

    /// Read-only preview of one agent's changes and conflicts.
    pub fn status(&self, name: &str) -> Result<AgentCheckResult> {
        self.rt.block_on(check_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
        ))
    }

    /// Pull cloud changes into the agent for paths the agent has not edited.
    pub fn refresh(&self, name: &str) -> Result<AgentRefreshResult> {
        self.rt.block_on(refresh_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
        ))
    }

    /// Integrate agent work into the main workspace.
    pub fn land(&self, name: &str, opts: LandOptions) -> Result<AgentLandResult> {
        self.rt.block_on(land_agent(
            &self.root,
            &self.db,
            &self.api,
            self.workspace_id(),
            name,
            self.password(),
            opts.clean,
            opts.propose,
        ))
    }

    /// Remove an agent workspace and its snapshot rows.
    pub fn clean(&self, name: &str) -> Result<AgentCleanResult> {
        self.rt.block_on(clean_agent(&self.root, &self.db, name))?;
        Ok(AgentCleanResult {
            cleaned: name.to_string(),
        })
    }

    /// Resolve a pending workspace conflict after reconciliation.
    pub fn resolve(&self, path: &str, keep: ResolveKeep, file_source: Option<&Path>) -> Result<()> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt
            .block_on(resolve_conflict(&ctx, path, keep, file_source))
    }

    /// Lists reachable workspace snapshots, newest first.
    pub fn log(&self, limit: usize) -> Result<feanorfs_common::LogResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(history::log(&ctx, limit))
    }

    /// Restores a reachable snapshot as a new snapshot on current head.
    pub fn undo(&self, snapshot_id: &str) -> Result<feanorfs_common::UndoResult> {
        let ctx = SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config)?;
        self.rt.block_on(history::undo(&ctx, snapshot_id))
    }
}
