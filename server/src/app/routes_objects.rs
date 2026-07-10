use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use feanorfs_common::{is_valid_hash, HeadResponse, SwapHeadRequest};

use super::guards::{client_format, ensure_client_format, ensure_migration_access};
use super::{AppState, HeadQuery};
use crate::db::HeadSwap;

pub(super) async fn handle_download(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_valid_hash(&hash) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let file = match tokio::fs::File::open(state.storage_dir.join("blobs").join(hash)).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StatusCode::NOT_FOUND);
        }
        Err(error) => {
            tracing::error!(?error, "failed to open blob file");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    Ok(axum::body::Body::from_stream(
        tokio_util::io::ReaderStream::new(file),
    ))
}

pub(super) async fn handle_get_workspaces(
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, StatusCode> {
    let workspaces = state.db.get_workspaces().await.map_err(|error| {
        tracing::error!(?error, "failed to fetch workspaces");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(workspaces))
}

pub(super) async fn handle_get_head(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HeadQuery>,
) -> Result<Json<HeadResponse>, StatusCode> {
    ensure_client_format(&state, &query.workspace_id, &headers).await?;
    let snapshot_id = state
        .db
        .get_head(&query.workspace_id)
        .await
        .map_err(|error| {
            tracing::error!(?error, "failed to fetch workspace head");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(HeadResponse { snapshot_id }))
}

pub(super) async fn handle_swap_head(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SwapHeadRequest>,
) -> Result<(StatusCode, Json<HeadResponse>), StatusCode> {
    let _publication_guard = state.publication_lock.read().await;
    ensure_client_format(&state, &request.workspace_id, &headers).await?;
    ensure_migration_access(&state, &request.workspace_id, &headers).await?;
    if !is_valid_hash(&request.new)
        || request
            .expected
            .as_deref()
            .is_some_and(|id| !is_valid_hash(id))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if client_format(&headers) >= 3
        && !state
            .db
            .manifest_exists(&request.workspace_id, &request.new)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        return Err(StatusCode::PRECONDITION_FAILED);
    }
    match state
        .db
        .swap_head(
            &request.workspace_id,
            request.expected.as_deref(),
            &request.new,
        )
        .await
        .map_err(|error| {
            tracing::error!(?error, "failed to swap workspace head");
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
        HeadSwap::Swapped => Ok((
            StatusCode::OK,
            Json(HeadResponse {
                snapshot_id: Some(request.new),
            }),
        )),
        HeadSwap::Conflict(snapshot_id) => {
            Ok((StatusCode::CONFLICT, Json(HeadResponse { snapshot_id })))
        }
    }
}
