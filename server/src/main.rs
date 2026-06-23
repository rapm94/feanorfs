mod db;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use db::Db;
use feanorfs_common::{FileState, SyncRequest, SyncResponse};
use serde::Deserialize;
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::fs;

#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    storage_dir: PathBuf,
}

#[derive(Deserialize)]
struct UploadParams {
    workspace_id: String,
    path: String,
    hash: String,
    size: u64,
    mtime: i64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing subscriber
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "feanorfs_server=info,tower_http=info".into()),
        )
        .init();

    // Setup directories
    let base_dir = PathBuf::from("server-data");
    let db_path = base_dir.join("db.sqlite");
    let blobs_dir = base_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).await?;

    // Initialize DB
    let db = Db::new(&db_path).await?;
    let state = AppState {
        db: Arc::new(db),
        storage_dir: base_dir,
    };

    // Build router with TraceLayer
    let app = Router::new()
        .route("/api/sync/diff", post(handle_sync_diff))
        .route("/api/upload", post(handle_upload))
        .route("/api/download/:hash", get(handle_download))
        .route("/api/workspaces", get(handle_get_workspaces))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3030));
    tracing::info!("FeanorFS Sync Server starting on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_sync_diff(
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

    let mut server_map: HashMap<String, FileState> = server_files
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();

    let mut upload_required = Vec::new();
    let mut download_required = Vec::new();
    let mut delete_local = Vec::new();

    let client_map: HashMap<String, FileState> = client_files
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();

    // 1. Process client files compared with server
    for (path, client_file) in &client_map {
        if let Some(server_file) = server_map.get(path) {
            // File exists on both
            if client_file.mtime > server_file.mtime {
                // Client has a newer version
                if client_file.deleted {
                    // Update server state to deleted
                    state
                        .db
                        .upsert_file(&workspace_id, client_file)
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    server_map.insert(path.clone(), client_file.clone());
                } else {
                    upload_required.push(path.clone());
                }
            } else if server_file.mtime > client_file.mtime {
                // Server has a newer version
                if server_file.deleted {
                    delete_local.push(path.clone());
                } else {
                    download_required.push(server_file.clone());
                }
            } else {
                // Mtimes are equal, verify hash consistency
                if client_file.hash != server_file.hash && !client_file.deleted {
                    upload_required.push(path.clone());
                }
            }
        } else {
            // Client has it, server doesn't
            if !client_file.deleted {
                upload_required.push(path.clone());
            } else {
                // Client deleted it before server saw it, mark as deleted on server anyway
                state
                    .db
                    .upsert_file(&workspace_id, client_file)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            }
        }
    }

    // 2. Process server files that client does not know about
    for (path, server_file) in &server_map {
        if !client_map.contains_key(path) && !server_file.deleted {
            download_required.push(server_file.clone());
        }
    }

    Ok(Json(SyncResponse {
        upload_required,
        download_required,
        delete_local,
    }))
}

async fn handle_upload(
    State(state): State<AppState>,
    Query(params): Query<UploadParams>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, StatusCode> {
    // Verify hash matches uploaded body
    let computed_hash = feanorfs_common::hash_bytes(&body);
    if computed_hash != params.hash {
        tracing::warn!(
            "Hash mismatch for {}: expected {}, computed {}",
            params.path, params.hash, computed_hash
        );
        return Err(StatusCode::BAD_REQUEST);
    }

    // Write file content to blobs directory
    let blob_path = state.storage_dir.join("blobs").join(&params.hash);
    if let Err(e) = fs::write(&blob_path, &body).await {
        tracing::error!("Failed to write blob: {:?}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Update database metadata
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
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok(StatusCode::OK)
}

async fn handle_download(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let blob_path = state.storage_dir.join("blobs").join(&hash);
    if !blob_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let file_content = fs::read(&blob_path).await.map_err(|e| {
        tracing::error!("Failed to read blob file: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

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

