use axum::http::{HeaderMap, StatusCode};
use constant_time_eq::constant_time_eq;

use super::AppState;

pub(super) async fn ensure_client_format(
    state: &AppState,
    workspace_id: &str,
    headers: &HeaderMap,
) -> Result<(), StatusCode> {
    let format = state
        .db
        .workspace_format(workspace_id)
        .await
        .map_err(|error| {
            tracing::error!(?error, "failed to read workspace format");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if format >= 3 && client_format(headers) < 3 {
        Err(StatusCode::UPGRADE_REQUIRED)
    } else {
        Ok(())
    }
}

pub(super) async fn ensure_migration_access(
    state: &AppState,
    workspace_id: &str,
    headers: &HeaderMap,
) -> Result<(), StatusCode> {
    let Some(expected) = state
        .db
        .migration_token(workspace_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Ok(());
    };
    let provided = headers
        .get("x-feanorfs-migration")
        .and_then(|value| value.to_str().ok());
    if provided.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes())) {
        Ok(())
    } else {
        Err(StatusCode::LOCKED)
    }
}

pub(super) fn client_format(headers: &HeaderMap) -> u32 {
    headers
        .get("x-feanorfs-format")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(2)
}
