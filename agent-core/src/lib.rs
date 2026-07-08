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
pub mod fs_util;
pub mod hub;
pub mod local;
pub mod lock;
pub mod paths;
pub mod sync_pass;

pub use agent::{
    check_agent, clean_agent, commit_agent, land_agent, list_agents, refresh_agent, spawn_agent,
};
pub use api::ApiClient;
pub use conflict_artifacts::{resolve_artifact, ArtifactRole};
pub use conflicts::{resolve_conflict, ResolveKeep};
pub use ctx::SyncCtx;
pub use feanorfs_common::{
    decode_invite, encode_invite, looks_like_invite, AgentCheckResult, AgentCleanResult,
    AgentCommitResult, AgentLandResult, AgentListEntry, AgentListOfflineResult, AgentListResult,
    AgentRefreshResult, ConcurrentEdit, ConflictKind, ConflictRecord, FileState, SpawnResult,
    WorkspaceInvite, INVITE_PREFIX,
};
pub use local::{
    load_config, load_global_config, save_config, save_global_config, validate_e2ee_key, ClientDb,
    Config, GlobalConfig, LOCAL_HUB_URL,
};
pub use paths::legacy_policy_for_config;
pub use paths::{agent_dir, agents_dir, conflicts_dir, validate_name};

use anyhow::{Context, Result};
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

    /// Open a workspace rooted at `path` (must contain `.feanorfs/config.json`).
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
        let db = rt.block_on(ClientDb::new(root.join(".feanorfs")))?;
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

    /// List agent workspace names under `.feanorfs/agents/`.
    pub fn list(&self) -> Result<Vec<String>> {
        self.rt.block_on(list_agents(&self.root, &self.db))
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
}
