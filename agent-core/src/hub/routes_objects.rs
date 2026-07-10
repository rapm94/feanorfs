use axum::body::Body;
use http::StatusCode;
use std::collections::HashMap;

use super::http::{check_fence, get_param, json_body, response, status_err, RouteResult};
use super::{LocalHub, MAX_MANIFEST_BYTES};

impl LocalHub {
    pub(super) fn route_download(&self, hash: &str) -> RouteResult {
        if hash.contains("..") || hash.contains('/') {
            return Err((StatusCode::NOT_FOUND, String::new()));
        }
        if !feanorfs_common::is_valid_hash(hash) {
            return Err((StatusCode::BAD_REQUEST, String::new()));
        }
        match std::fs::read(self.db.blob_path(hash)) {
            Ok(data) => Ok(response(StatusCode::OK, Body::from(data))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err((StatusCode::NOT_FOUND, String::new()))
            }
            Err(error) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read error: {error}"),
            )),
        }
    }

    pub(super) fn route_get_head(&self, params: &HashMap<String, String>) -> RouteResult {
        let workspace_id = get_param(params, "workspace_id")?;
        let snapshot_id = self.db.get_head(workspace_id).map_err(status_err)?;
        Ok(json_body(
            StatusCode::OK,
            &feanorfs_common::HeadResponse { snapshot_id },
        ))
    }

    pub(super) fn route_swap_head(
        &self,
        body: &[u8],
        migration_header: Option<&str>,
    ) -> RouteResult {
        let request: feanorfs_common::SwapHeadRequest = serde_json::from_slice(body)
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid request".into()))?;
        check_fence(&self.db, &request.workspace_id, migration_header)?;
        if !feanorfs_common::is_valid_hash(&request.new)
            || request
                .expected
                .as_deref()
                .is_some_and(|id| !feanorfs_common::is_valid_hash(id))
        {
            return Err((StatusCode::BAD_REQUEST, "invalid snapshot id".into()));
        }
        if !self
            .db
            .manifest_exists(&request.workspace_id, &request.new)
            .map_err(status_err)?
        {
            return Err((StatusCode::PRECONDITION_FAILED, "manifest required".into()));
        }
        let previous = self
            .db
            .swap_head(
                &request.workspace_id,
                request.expected.as_deref(),
                &request.new,
            )
            .map_err(status_err)?;
        if previous == request.expected {
            Ok(json_body(
                StatusCode::OK,
                &feanorfs_common::HeadResponse {
                    snapshot_id: Some(request.new),
                },
            ))
        } else {
            Ok(json_body(
                StatusCode::CONFLICT,
                &feanorfs_common::HeadResponse {
                    snapshot_id: previous,
                },
            ))
        }
    }

    pub(super) fn route_manifest(
        &self,
        body: &[u8],
        params: &HashMap<String, String>,
        migration_header: Option<&str>,
    ) -> RouteResult {
        let workspace_id = get_param(params, "workspace_id")?;
        check_fence(&self.db, workspace_id, migration_header)?;
        let snapshot_id = get_param(params, "snapshot_id")?;
        if body.len() > MAX_MANIFEST_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "manifest too large".into()));
        }
        let manifest = std::str::from_utf8(body)
            .map_err(|_| (StatusCode::BAD_REQUEST, "manifest not UTF-8".into()))?;
        let mut hashes = Vec::new();
        for hash in manifest
            .lines()
            .map(str::trim)
            .filter(|hash| !hash.is_empty())
        {
            if !feanorfs_common::is_valid_hash(hash) || !self.db.blob_exists(hash) {
                return Err((
                    StatusCode::PRECONDITION_FAILED,
                    format!("manifest references missing blob {hash}"),
                ));
            }
            hashes.push(hash.to_string());
        }
        if !feanorfs_common::is_valid_hash(snapshot_id) {
            return Err((StatusCode::BAD_REQUEST, "invalid manifest".into()));
        }
        self.db
            .store_manifest(workspace_id, snapshot_id, hashes)
            .map_err(status_err)?;
        Ok(response(StatusCode::OK, Body::empty()))
    }
}
