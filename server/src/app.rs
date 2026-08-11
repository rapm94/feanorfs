mod guards;
mod routes_legacy;
mod routes_objects;
pub(crate) mod routes_pair_relay;
mod routes_publication;
pub(crate) mod routes_tunnel_relay;

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
use futures_util::StreamExt as _;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

use crate::db::Db;
use routes_legacy::{handle_sync_peek, handle_upload};
use routes_objects::{handle_download, handle_get_head, handle_get_workspaces, handle_swap_head};
use routes_pair_relay::{handle_pair_relay, PairRelayState};
use routes_publication::{
    handle_begin_migration, handle_get_format, handle_manifest, handle_set_format,
};
use routes_tunnel_relay::{handle_tunnel_relay, TunnelRelayState};

pub(crate) const MAX_PROTECTED_REQUESTS: usize = 64;
pub(crate) const MAX_UPLOAD_REQUESTS: usize = 4;
pub(crate) const MAX_MANIFEST_REQUESTS: usize = 2;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub storage_dir: PathBuf,
    pub auth_token: Option<String>,
    pub publication_lock: Arc<RwLock<()>>,
    pub(crate) protected_requests: Arc<Semaphore>,
    pub(crate) upload_requests: Arc<Semaphore>,
    pub(crate) manifest_requests: Arc<Semaphore>,
    pub(crate) pair_relay: PairRelayState,
    pub(crate) tunnel_relay: TunnelRelayState,
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
    build_router_inner(state, false)
}

pub(crate) fn build_router_with_relay(state: AppState) -> Router {
    build_router_inner(state, true)
}

fn build_router_inner(state: AppState, relay: bool) -> Router {
    let protected = Router::new()
        .route("/api/sync/peek", post(handle_sync_peek))
        .route("/api/sync/diff", post(handle_sync_peek))
        .route(
            "/api/upload",
            post(handle_upload).layer(axum::middleware::from_fn_with_state(
                state.clone(),
                upload_admission_middleware,
            )),
        )
        .route("/api/head", get(handle_get_head).put(handle_swap_head))
        .route(
            "/api/manifest",
            post(handle_manifest)
                .layer(DefaultBodyLimit::max(crate::MAX_MANIFEST_BYTES))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    manifest_admission_middleware,
                )),
        )
        .route("/api/workspace/migration", post(handle_begin_migration))
        .route(
            "/api/workspace/format",
            get(handle_get_format).post(handle_set_format),
        )
        .route("/api/download/{hash}", get(handle_download))
        .route("/api/workspaces", get(handle_get_workspaces))
        .route("/api/version", get(handle_version))
        .layer(DefaultBodyLimit::max(crate::MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    let mut router = Router::new().merge(protected);
    if relay {
        router = router
            .route("/api/pair-relay/{session}/{role}", get(handle_pair_relay))
            .route("/api/tunnel-relay/{route}/{role}", get(handle_tunnel_relay));
    }
    router
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(
                |request: &Request<axum::body::Body>| {
                    tracing::info_span!("http_request", method = %request.method())
                },
            ),
        )
        .with_state(state)
}

async fn handle_version() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }))
}

async fn upload_admission_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    limited_body_middleware(state.upload_requests, request, next).await
}

async fn manifest_admission_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > crate::MAX_MANIFEST_BYTES as u64)
    {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    limited_body_middleware(state.manifest_requests, request, next).await
}

async fn limited_body_middleware(
    semaphore: Arc<Semaphore>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let permit = semaphore
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let response = next.run(request).await;
    Ok(response.map(|body| body_holding_permit(body, permit)))
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(expected) = &state.auth_token {
        let provided = request
            .headers()
            .get("Authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if !provided.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes())) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let permit = state
        .protected_requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let response = next.run(request).await;
    Ok(response.map(|body| body_holding_permit(body, permit)))
}

fn body_holding_permit(body: axum::body::Body, permit: OwnedSemaphorePermit) -> axum::body::Body {
    let stream = futures_util::stream::unfold(
        (body.into_data_stream(), permit),
        |(mut body, permit)| async move { body.next().await.map(|item| (item, (body, permit))) },
    );
    axum::body::Body::from_stream(stream)
}
