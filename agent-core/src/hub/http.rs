use axum::body::Body;
use constant_time_eq::constant_time_eq;
use http::{header, Response, StatusCode};
use std::collections::HashMap;

use crate::hub_state::HubDb;

pub(super) type RouteResult = Result<Response<Body>, (StatusCode, String)>;

pub(super) fn status_err(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub(super) fn check_fence(
    db: &HubDb,
    workspace_id: &str,
    migration_header: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    let fence = db.get_migration_fence(workspace_id).map_err(status_err)?;
    let Some(expected) = fence else {
        return Ok(());
    };
    let provided = migration_header.unwrap_or("");
    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err((StatusCode::LOCKED, String::new()))
    }
}

pub(super) fn check_legacy_compat(
    db: &HubDb,
    workspace_id: &str,
) -> Result<(), (StatusCode, String)> {
    if db.get_format(workspace_id).map_err(status_err)? >= 3 {
        Err((
            StatusCode::UPGRADE_REQUIRED,
            "format v3 requires object API".to_string(),
        ))
    } else {
        Ok(())
    }
}

pub(super) fn get_param<'a>(
    params: &'a HashMap<String, String>,
    key: &str,
) -> Result<&'a str, (StatusCode, String)> {
    params.get(key).map(String::as_str).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("missing query parameter: {key}"),
        )
    })
}

pub(super) fn parse_required_param<T: std::str::FromStr>(
    params: &HashMap<String, String>,
    key: &str,
) -> Result<T, (StatusCode, String)> {
    let raw = get_param(params, key)?;
    raw.parse::<T>()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid {key}: {raw}")))
}

pub(super) fn response(status: StatusCode, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
}

pub(super) fn json_body<T: serde::Serialize>(status: StatusCode, value: &T) -> Response<Body> {
    let mut response = response(
        status,
        Body::from(serde_json::to_vec(value).unwrap_or_default()),
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}
