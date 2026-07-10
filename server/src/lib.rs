pub mod app;
pub mod db;
pub mod gc;
pub mod serve;

pub use app::{build_router, AppState};
pub use serve::{resolve_auth_token, run_gc, run_http_server, ServeOptions};

/// Maximum request/response body size for upload/download (100 MiB).
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

use std::path::PathBuf;
use std::sync::Arc;

/// Initialize server state with an ephemeral or persistent data directory.
pub async fn init_app_state(
    data_dir: PathBuf,
    auth_token: Option<String>,
) -> anyhow::Result<AppState> {
    let blobs_dir = data_dir.join("blobs");
    tokio::fs::create_dir_all(&blobs_dir).await?;
    let db = db::Db::new(data_dir.join("db.sqlite")).await?;
    Ok(AppState {
        db: Arc::new(db),
        storage_dir: data_dir,
        auth_token,
        publication_lock: Arc::new(tokio::sync::RwLock::new(())),
    })
}
