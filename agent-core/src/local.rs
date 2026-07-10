mod access;
mod cache;
mod config;
mod conflicts;
mod scan;
mod walker;

#[cfg(test)]
mod tests;

use crate::state::DurableState;

pub use config::{
    load_config, load_global_config, save_config, save_global_config, validate_e2ee_key, Config,
    GlobalConfig, LOCAL_HUB_URL,
};
pub use scan::{scan_local_directory, scan_local_directory_with_opts};
pub use walker::{
    build_workspace_walker, collect_symlink_warnings, normalize_path_nfc, DEFAULT_IGNORES,
};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: String,
    pub plaintext_hash: String,
    pub encrypted_hash: String,
    pub size: u64,
    pub mtime: i64,
    pub server_mtime: i64,
    pub mode: u32,
    pub hydrated: bool,
    pub deleted_at: Option<i64>,
}

#[derive(Debug)]
pub struct ClientDb {
    state: DurableState,
}
