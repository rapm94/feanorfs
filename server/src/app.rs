mod guards;
mod routes_legacy;
mod routes_objects;
mod routes_publication;

#[cfg(test)]
mod tests;

use axum::extract::DefaultBodyLimit;
use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
    routing::{get, post},
    Router,
};
use constant_time_eq::constant_time_eq;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::db::Db;
use routes_legacy::{handle_sync_peek, handle_upload};
use routes_objects::{handle_download, handle_get_head, handle_get_workspaces, handle_swap_head};
use routes_publication::{
    handle_begin_migration, handle_get_format, handle_manifest, handle_set_format,
};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub storage_dir: PathBuf,
    pub auth_token: Option<String>,
    pub publication_lock: Arc<RwLock<()>>,
}

#[derive(Deserialize)]
struct UploadParams {
    workspace_id: String,
    path: String,
    hash: String,
    size: u64,
    mtime: i64,
    #[serde(default)]
    mode: u32,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    object: bool,
}

#[derive(Deserialize)]
struct HeadQuery {
    workspace_id: String,
}

#[derive(Deserialize)]
struct ManifestQuery {
    workspace_id: String,
    snapshot_id: String,
}

#[derive(Deserialize)]
struct FormatQuery {
    workspace_id: String,
    format_version: u32,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/sync/peek", post(handle_sync_peek))
        .route("/api/sync/diff", post(handle_sync_peek))
        .route("/api/upload", post(handle_upload))
        .route("/api/head", get(handle_get_head).put(handle_swap_head))
        .route("/api/manifest", post(handle_manifest))
        .route("/api/workspace/migration", post(handle_begin_migration))
        .route(
            "/api/workspace/format",
            get(handle_get_format).post(handle_set_format),
        )
        .route("/api/download/:hash", get(handle_download))
        .route("/api/workspaces", get(handle_get_workspaces))
        .layer(DefaultBodyLimit::max(crate::MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = &state.auth_token else {
        return Ok(next.run(request).await);
    };
    let provided = request
        .headers()
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes())) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}
