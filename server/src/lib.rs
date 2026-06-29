pub mod app;
pub mod db;

pub use app::{build_router, AppState};

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
    })
}
