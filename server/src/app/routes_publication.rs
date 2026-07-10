use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use feanorfs_common::is_valid_hash;

use super::guards::{ensure_client_format, ensure_migration_access};
use super::{AppState, FormatQuery, HeadQuery, ManifestQuery};

const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

pub(super) async fn handle_manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ManifestQuery>,
    body: axum::body::Bytes,
) -> Result<StatusCode, (StatusCode, String)> {
    let _publication_guard = state.publication_lock.read().await;
    ensure_client_format(&state, &query.workspace_id, &headers)
        .await
        .map_err(|status| (status, String::new()))?;
    ensure_migration_access(&state, &query.workspace_id, &headers)
        .await
        .map_err(|status| (status, String::new()))?;
    if body.len() > MAX_MANIFEST_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, String::new()));
    }
    let manifest = std::str::from_utf8(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "manifest is not UTF-8".to_string()))?;
    for hash in manifest.lines() {
        if !is_valid_hash(hash)
            || !tokio::fs::try_exists(state.storage_dir.join("blobs").join(hash))
                .await
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, String::new()))?
        {
            return Err((
                StatusCode::PRECONDITION_FAILED,
                format!("manifest references missing blob {hash}"),
            ));
        }
    }
    state
        .db
        .upsert_manifest(&query.workspace_id, &query.snapshot_id, &body)
        .await
        .map_err(|error| {
            tracing::warn!(?error, "rejected snapshot reachability manifest");
            (StatusCode::BAD_REQUEST, "invalid manifest".to_string())
        })?;
    Ok(StatusCode::OK)
}

pub(super) async fn handle_set_format(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FormatQuery>,
) -> Result<StatusCode, StatusCode> {
    let _publication_guard = state.publication_lock.write().await;
    ensure_client_format(&state, &query.workspace_id, &headers).await?;
    ensure_migration_access(&state, &query.workspace_id, &headers).await?;
    if query.format_version != 3 {
        return Err(StatusCode::BAD_REQUEST);
    }
    state
        .db
        .set_workspace_format(&query.workspace_id, query.format_version)
        .await
        .map_err(|error| {
            tracing::error!(?error, "failed to set workspace format");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(StatusCode::OK)
}

pub(super) async fn handle_begin_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HeadQuery>,
) -> Result<StatusCode, StatusCode> {
    let _publication_guard = state.publication_lock.write().await;
    ensure_client_format(&state, &query.workspace_id, &headers).await?;
    let token = headers
        .get("x-feanorfs-migration")
        .and_then(|value| value.to_str().ok())
        .filter(|token| is_valid_hash(token))
        .ok_or(StatusCode::BAD_REQUEST)?;
    state
        .db
        .begin_migration(&query.workspace_id, token)
        .await
        .map_err(|_| StatusCode::LOCKED)?;
    Ok(StatusCode::OK)
}

pub(super) async fn handle_get_format(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HeadQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    ensure_client_format(&state, &query.workspace_id, &headers).await?;
    let format_version = state
        .db
        .workspace_format(&query.workspace_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::json!({ "format_version": format_version }),
    ))
}
