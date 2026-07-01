use crate::db::Db;
use axum::extract::DefaultBodyLimit;
use axum::{
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use constant_time_eq::constant_time_eq;
use feanorfs_common::{
    compute_sync_delta, hash_bytes, is_safe_rel_path, is_valid_hash, FileState, SyncRequest,
    SyncResponse,
};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};
use tokio::fs;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub storage_dir: PathBuf,
    pub auth_token: Option<String>,
}

#[derive(Deserialize)]
struct UploadParams {
    workspace_id: String,
    path: String,
    hash: String,
    size: u64,
    mtime: i64,
    #[serde(default)]
    deleted: bool,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/sync/peek", post(handle_sync_peek))
        .route("/api/sync/diff", post(handle_sync_peek))
        .route("/api/upload", post(handle_upload))
        .route("/api/download/:hash", get(handle_download))
        .route("/api/workspaces", get(handle_get_workspaces))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024))
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
    match &state.auth_token {
        None => Ok(next.run(request).await),
        Some(expected) => {
            let provided = request
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match provided {
                Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => {
                    Ok(next.run(request).await)
                }
                _ => Err(StatusCode::UNAUTHORIZED),
            }
        }
    }
}

async fn handle_sync_peek(
    State(state): State<AppState>,
    Json(payload): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, StatusCode> {
    let workspace_id = payload.workspace_id;
    let client_files = payload.files;

    let server_files = state
        .db
        .get_workspace_files(&workspace_id)
        .await
        .map_err(|e| {
            tracing::error!("Error fetching workspace files: {:?}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(compute_sync_delta(&client_files, &server_files)))
}

async fn handle_upload(
    State(state): State<AppState>,
    Query(params): Query<UploadParams>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_safe_rel_path(&params.path) {
        tracing::warn!("Rejected upload with unsafe path: {}", params.path);
        return Err(StatusCode::BAD_REQUEST);
    }

    if params.deleted {
        if !is_valid_hash(&params.hash) {
            return Err(StatusCode::BAD_REQUEST);
        }
        let file_state = FileState {
            path: params.path,
            hash: params.hash,
            size: 0,
            mtime: params.mtime,
            deleted: true,
        };
        state
            .db
            .upsert_file(&params.workspace_id, &file_state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(StatusCode::OK);
    }

    if !is_valid_hash(&params.hash) {
        tracing::warn!("Rejected upload with invalid hash: {}", params.hash);
        return Err(StatusCode::BAD_REQUEST);
    }

    let computed_hash = hash_bytes(&body);
    if computed_hash != params.hash {
        tracing::warn!(
            "Hash mismatch for {}: expected {}, computed {}",
            params.path,
            params.hash,
            computed_hash
        );
        return Err(StatusCode::BAD_REQUEST);
    }

    let blob_path = state.storage_dir.join("blobs").join(&params.hash);
    if let Err(e) = fs::write(&blob_path, &body).await {
        tracing::error!("Failed to write blob: {:?}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let file_state = FileState {
        path: params.path,
        hash: params.hash,
        size: params.size,
        mtime: params.mtime,
        deleted: false,
    };

    if let Err(e) = state
        .db
        .upsert_file(&params.workspace_id, &file_state)
        .await
    {
        tracing::error!("Failed to upsert file in db: {:?}", e);
        let _ = fs::remove_file(&blob_path).await;
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok(StatusCode::OK)
}

async fn handle_download(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_valid_hash(&hash) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let blob_path = state.storage_dir.join("blobs").join(&hash);

    let file_content = match fs::read(&blob_path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StatusCode::NOT_FOUND);
        }
        Err(e) => {
            tracing::error!("Failed to read blob file: {:?}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    Ok(file_content)
}

async fn handle_get_workspaces(
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, StatusCode> {
    let workspaces = state.db.get_workspaces().await.map_err(|e| {
        tracing::error!("Error fetching workspaces: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(workspaces))
}
