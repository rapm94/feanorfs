use axum::body::Body;
use http::StatusCode;
use std::collections::HashMap;

use super::http::{
    check_fence, check_legacy_compat, get_param, json_body, parse_required_param, response,
    status_err, RouteResult,
};
use super::LocalHub;

impl LocalHub {
    pub(super) fn route_list_workspaces(&self) -> anyhow::Result<http::Response<Body>> {
        let workspaces = self.db.list_workspaces()?;
        Ok(json_body(StatusCode::OK, &workspaces))
    }

    pub(super) fn route_sync_peek(&self, body: &[u8]) -> RouteResult {
        let request: feanorfs_common::SyncRequest = serde_json::from_slice(body)
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid sync request".into()))?;
        check_legacy_compat(&self.db, &request.workspace_id)?;
        let server_files = self
            .db
            .get_files(&request.workspace_id)
            .map_err(status_err)?
            .into_iter()
            .map(|(path, file)| feanorfs_common::FileState {
                path,
                hash: file.hash,
                size: feanorfs_common::file_size_from_db(file.size),
                mtime: file.mtime,
                deleted: file.deleted,
                mode: file.mode,
            })
            .collect::<Vec<_>>();
        let delta = feanorfs_common::compute_sync_delta(&request.files, &server_files);
        Ok(json_body(StatusCode::OK, &delta))
    }

    pub(super) fn route_upload(
        &self,
        body: &[u8],
        params: &HashMap<String, String>,
        migration_header: Option<&str>,
    ) -> RouteResult {
        let workspace_id = get_param(params, "workspace_id")?;
        check_fence(&self.db, workspace_id, migration_header)?;
        let hash = get_param(params, "hash")?;
        if !feanorfs_common::is_valid_hash(hash) {
            return Err((StatusCode::BAD_REQUEST, "invalid hash".into()));
        }
        let is_object = params.get("object").is_some_and(|value| value == "true");
        let path = get_param(params, "path")?;
        if !feanorfs_common::is_safe_rel_path(path) {
            return Err((StatusCode::BAD_REQUEST, "unsafe path".into()));
        }
        let deleted = params.get("deleted").is_some_and(|value| value == "true");
        if deleted {
            if is_object {
                return Err((StatusCode::BAD_REQUEST, "deleted+object invalid".into()));
            }
            check_legacy_compat(&self.db, workspace_id)?;
            let mtime = parse_required_param::<i64>(params, "mtime")?;
            self.db
                .upsert_file(workspace_id, path, hash, 0, mtime, 0, true)
                .map_err(status_err)?;
            return Ok(response(StatusCode::OK, Body::empty()));
        }
        let computed = feanorfs_common::hash_bytes(body);
        if computed != hash {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("hash mismatch: expected {hash}, computed {computed}"),
            ));
        }
        let legacy_metadata = if is_object {
            None
        } else {
            check_legacy_compat(&self.db, workspace_id)?;
            Some((
                parse_required_param::<i64>(params, "size")?,
                parse_required_param::<i64>(params, "mtime")?,
                parse_required_param::<u32>(params, "mode")?,
            ))
        };
        let blob_path = self.db.blob_path(hash);
        let is_new = !self.db.blob_exists(hash);
        if is_new {
            self.db.store_blob(hash, body).map_err(status_err)?;
        }
        if is_object {
            return Ok(response(StatusCode::OK, Body::empty()));
        }
        let (size, mtime, mode) = legacy_metadata.ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "missing legacy metadata".into(),
            )
        })?;
        match self
            .db
            .upsert_file(workspace_id, path, hash, size, mtime, mode, false)
        {
            Ok(()) => Ok(response(StatusCode::OK, Body::empty())),
            Err(error) => {
                if is_new {
                    let message = error.to_string();
                    let is_referenced = message.contains("committed-but-durability-uncertain")
                        && self
                            .db
                            .get_files(workspace_id)
                            .map_err(status_err)?
                            .iter()
                            .any(|(_, file)| file.hash == hash);
                    if !is_referenced {
                        let _ = std::fs::remove_file(blob_path);
                    }
                }
                Err(status_err(error))
            }
        }
    }
}
