use super::guards::{ensure_client_format, ensure_migration_access};
use super::{AppState, HeadQuery};
use crate::db::HeadSwap;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use feanorfs_common::{is_valid_hash, HeadResponse, SwapHeadRequest};

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
    if let Some(after) = query.after.as_deref() {
        if !is_valid_hash(after) {
            return Err(StatusCode::BAD_REQUEST);
        }
        return handle_get_head_with_wait(&state, &query.workspace_id, after, query.wait_ms).await;
    }
    let snapshot_id = state
        .db
        .get_head(&query.workspace_id)
        .await
        .map_err(|error| {
            tracing::error!(?error, "failed to fetch workspace head");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(HeadResponse {
        snapshot_id,
        wait_supported: false,
    }))
}

/// Waits (bounded) for the opaque head to leave `after`.
///
/// Responses produced here set `wait_supported`, which clients use to
/// distinguish a waiting hub from an older one that ignores the query fields.
async fn handle_get_head_with_wait(
    state: &AppState,
    workspace_id: &str,
    after: &str,
    wait_ms: Option<u64>,
) -> Result<Json<HeadResponse>, StatusCode> {
    let current = read_head_id(state, workspace_id).await?;
    if current.as_deref() != Some(after) {
        return Ok(wait_response(current));
    }
    let Some(requested_ms) = wait_ms else {
        // A caller that passes `after` without a wait window wants the
        // immediate head (plus support advertisement), not a long poll.
        return Ok(wait_response(current));
    };
    let wait_ms = requested_ms.min(super::head_wait::MAX_HEAD_WAIT_MS);
    if wait_ms == 0 {
        return Ok(wait_response(current));
    }

    let Some(mut waiter) = state.head_waiters.register(workspace_id) else {
        // An immediate unchanged success is indistinguishable from a real
        // timeout and would let callers retry in a tight loop. A retryable
        // status makes the shared observer apply its bounded backoff.
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    // Re-check after registration: a swap between the initial read and the
    // registration must never lose the wakeup.
    let registered_head = read_head_id(state, workspace_id).await?;
    if registered_head.as_deref() != Some(after) {
        return Ok(wait_response(registered_head));
    }

    tokio::select! {
        _ = waiter.receiver() => {
            let refreshed = read_head_id(state, workspace_id).await?;
            Ok(wait_response(refreshed))
        }
        _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {
            // Timeout returns the unchanged head without an error.
            Ok(wait_response(current))
        }
    }
}

async fn read_head_id(state: &AppState, workspace_id: &str) -> Result<Option<String>, StatusCode> {
    state.db.get_head(workspace_id).await.map_err(|error| {
        tracing::error!(?error, "failed to fetch workspace head");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

fn wait_response(snapshot_id: Option<String>) -> Json<HeadResponse> {
    Json(HeadResponse {
        snapshot_id,
        wait_supported: true,
    })
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
        HeadSwap::Swapped => {
            // Notify only after the swap is durably accepted; a rejected CAS
            // must never wake a waiter.
            state.head_waiters.notify(&request.workspace_id);
            Ok((
                StatusCode::OK,
                Json(HeadResponse {
                    snapshot_id: Some(request.new),
                    wait_supported: false,
                }),
            ))
        }
        HeadSwap::Conflict(snapshot_id) => Ok((
            StatusCode::CONFLICT,
            Json(HeadResponse {
                snapshot_id,
                wait_supported: false,
            }),
        )),
        HeadSwap::MissingManifest => Err(StatusCode::PRECONDITION_FAILED),
    }
}
