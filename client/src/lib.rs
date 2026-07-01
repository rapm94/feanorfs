pub mod agent;
pub mod api;
pub mod commands;
pub mod conflict_artifacts;
pub mod conflicts;
pub mod fs_util;
pub mod local;
pub mod predictive;
pub mod summary;
pub mod watch;

pub use agent::{clean_agent, commit_agent, list_agents, spawn_agent};
pub use api::ApiClient;
pub use commands::{
    do_cat, do_hydrate, do_pull_only, do_push_only, do_status, do_sync, CatResult, HydrateResult,
    MirrorState, PullResult, PushResult, StatusResult, SyncResult,
};
pub use conflicts::{resolve_conflict, ResolveKeep};
pub use feanorfs_common::AgentCommitResult as CommitResult;
pub use feanorfs_common::{ConcurrentEdit, ConflictKind, ConflictRecord, FileState};
pub use local::{
    load_config, load_global_config, save_config, save_global_config, ClientDb, Config,
    GlobalConfig,
};

pub use commands::do_cat as cat;
pub use commands::do_hydrate as hydrate;
pub use commands::do_pull_only as pull;
pub use commands::do_push_only as push;
pub use commands::do_sync as sync;
