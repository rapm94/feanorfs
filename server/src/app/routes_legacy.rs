use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use feanorfs_common::{
    compute_sync_delta, hash_bytes, is_safe_rel_path, is_valid_hash, FileState, SyncRequest,
    SyncResponse,
};

use super::guards::{ensure_client_format, ensure_migration_access};
use super::{AppState, UploadParams};

pub(super) async fn handle_sync_peek(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, StatusCode> {
    let workspace_id = payload.workspace_id;
    ensure_client_format(&state, &workspace_id, &headers).await?;
    if state
        .db
        .workspace_format(&workspace_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        >= 3
    {
        return Err(StatusCode::UPGRADE_REQUIRED);
    }
    let server_files = state
        .db
        .get_workspace_files(&workspace_id)
        .await
        .map_err(|error| {
            tracing::error!(?error, "failed to fetch workspace files");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(compute_sync_delta(&payload.files, &server_files)))
}

pub(super) async fn handle_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<UploadParams>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, StatusCode> {
    let _publication_guard = state.publication_lock.read().await;
    ensure_client_format(&state, &params.workspace_id, &headers).await?;
    ensure_migration_access(&state, &params.workspace_id, &headers).await?;
    if !params.object
        && state
            .db
            .workspace_format(&params.workspace_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            >= 3
    {
        return Err(StatusCode::UPGRADE_REQUIRED);
    }
    if !is_safe_rel_path(&params.path) {
        tracing::warn!(path = %params.path, "rejected upload with unsafe path");
        return Err(StatusCode::BAD_REQUEST);
    }
    if params.deleted {
        if params.object || !is_valid_hash(&params.hash) {
            return Err(StatusCode::BAD_REQUEST);
        }
        state
            .db
            .upsert_file(
                &params.workspace_id,
                &FileState {
                    path: params.path,
                    hash: params.hash,
                    size: 0,
                    mtime: params.mtime,
                    deleted: true,
                    mode: 0,
                },
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(StatusCode::OK);
    }
    if !is_valid_hash(&params.hash) {
        tracing::warn!(hash = %params.hash, "rejected upload with invalid hash");
        return Err(StatusCode::BAD_REQUEST);
    }
    let computed_hash = hash_bytes(&body);
    if computed_hash != params.hash {
        tracing::warn!(
            path = %params.path,
            expected = %params.hash,
            computed = %computed_hash,
            "upload hash mismatch"
        );
        return Err(StatusCode::BAD_REQUEST);
    }
    let blob_path = state.storage_dir.join("blobs").join(&params.hash);
    if let Err(error) = tokio::fs::write(&blob_path, &body).await {
        tracing::error!(?error, "failed to write blob");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    if params.object {
        return Ok(StatusCode::OK);
    }
    let file_state = FileState {
        path: params.path,
        hash: params.hash,
        size: params.size,
        mtime: params.mtime,
        deleted: false,
        mode: params.mode,
    };
    if let Err(error) = state
        .db
        .upsert_file(&params.workspace_id, &file_state)
        .await
    {
        tracing::error!(?error, "failed to upsert file");
        let _ = tokio::fs::remove_file(&blob_path).await;
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(StatusCode::OK)
}
