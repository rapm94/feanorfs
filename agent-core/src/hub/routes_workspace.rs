use axum::body::Body;
use feanorfs_common::hub_contract::MigrationWriteOutcome;
use http::StatusCode;
use std::collections::HashMap;

use super::http::{check_fence, get_param, json_body, response, status_err, RouteResult};
use super::LocalHub;
use crate::hub_state::FormatWrite;

impl LocalHub {
    pub(super) fn route_get_format(&self, params: &HashMap<String, String>) -> RouteResult {
        let workspace_id = get_param(params, "workspace_id")?;
        let format_version = self.db.get_format(workspace_id).map_err(status_err)?;
        Ok(json_body(
            StatusCode::OK,
            &serde_json::json!({ "format_version": format_version }),
        ))
    }

    pub(super) fn route_set_format(
        &self,
        params: &HashMap<String, String>,
        migration_header: Option<&str>,
    ) -> RouteResult {
        let workspace_id = get_param(params, "workspace_id")?;
        check_fence(&self.db, workspace_id, migration_header)?;
        let version = get_param(params, "format_version")?
            .parse::<u32>()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid format_version".into()))?;
        if !feanorfs_common::hub_contract::is_supported_format_version(version) {
            return Err((
                StatusCode::BAD_REQUEST,
                "only format version 3 is accepted".into(),
            ));
        }
        match self.db.set_format(workspace_id, version) {
            Ok(FormatWrite::Applied) => Ok(response(StatusCode::OK, Body::empty())),
            // A v3 transition without a manifested head is an internal
            // precondition failure, matching the server route's 500.
            Ok(FormatWrite::MissingManifestHead) => {
                Err((StatusCode::INTERNAL_SERVER_ERROR, String::new()))
            }
            Err(error) => Err(status_err(error)),
        }
    }

    pub(super) fn route_begin_migration(
        &self,
        params: &HashMap<String, String>,
        migration_header: Option<&str>,
    ) -> RouteResult {
        let workspace_id = get_param(params, "workspace_id")?;
        let token = migration_header
            .filter(|token| feanorfs_common::is_valid_hash(token))
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "missing or invalid migration token".into(),
                )
            })?;
        if self.db.get_format(workspace_id).map_err(status_err)? >= 3 {
            return Ok(response(StatusCode::OK, Body::empty()));
        }
        match self.db.begin_migration(workspace_id, token) {
            Ok(MigrationWriteOutcome::Acquired) => Ok(response(StatusCode::OK, Body::empty())),
            Ok(MigrationWriteOutcome::LockedByOther) => Err((
                StatusCode::LOCKED,
                "workspace migration is already locked".into(),
            )),
            Err(error) => Err(status_err(error)),
        }
    }
}
