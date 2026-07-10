mod publication;
mod validation;

use tempfile::TempDir;

pub(super) use super::build_router;
use super::AppState;

pub(super) async fn app_state() -> AppState {
    let directory = TempDir::new().expect("create temp directory").keep();
    crate::init_app_state(directory, None)
        .await
        .expect("initialize app state")
}
