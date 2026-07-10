mod http;
mod routes_legacy;
mod routes_objects;
mod routes_workspace;

#[cfg(test)]
mod tests;

use ::http::{Method, Response, StatusCode};
use anyhow::Context;
use axum::body::Body;
use constant_time_eq::constant_time_eq;
use http::response;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::RwLock;

use crate::hub_state::HubDb;

const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

type CacheKey = (PathBuf, Option<String>);

static HUB_CACHE: OnceLock<Mutex<HashMap<CacheKey, Arc<LocalHub>>>> = OnceLock::new();

fn hub_cache() -> &'static Mutex<HashMap<CacheKey, Arc<LocalHub>>> {
    HUB_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug)]
pub struct LocalHub {
    db: HubDb,
    auth_token: Option<String>,
    publication_lock: RwLock<()>,
}

#[derive(Clone, Copy)]
struct RoutedRequest<'a> {
    method: &'a Method,
    path: &'a str,
    body: &'a [u8],
    params: &'a HashMap<String, String>,
    migration_header: Option<&'a str>,
}

impl LocalHub {
    pub async fn open(data_dir: PathBuf, auth_token: Option<String>) -> anyhow::Result<Arc<Self>> {
        let legacy_database = data_dir.join("db.sqlite");
        if legacy_database.exists() {
            anyhow::bail!(
                "Legacy SQLite database found at {}. \
                 Run 'feanorfs migrate' from the workspace root to convert it.",
                legacy_database.display()
            );
        }
        let canonical_dir = data_dir.canonicalize().or_else(|_| {
            std::fs::create_dir_all(&data_dir).and_then(|_| data_dir.canonicalize())
        })?;
        let cache_key = (canonical_dir, auth_token.clone());
        if let Some(hub) = hub_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&cache_key)
            .cloned()
        {
            return Ok(hub);
        }
        let hub = Arc::new(Self {
            db: HubDb::open(&data_dir)?,
            auth_token,
            publication_lock: RwLock::new(()),
        });
        hub_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(cache_key, Arc::clone(&hub));
        Ok(hub)
    }

    #[doc(hidden)]
    pub async fn open_for_migration(data_dir: PathBuf) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            db: HubDb::open_for_migration(&data_dir)?,
            auth_token: None,
            publication_lock: RwLock::new(()),
        }))
    }

    #[doc(hidden)]
    pub fn migration_db(&self) -> &HubDb {
        &self.db
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: Vec<u8>,
        credentials: (Option<&str>, Option<&str>),
        _content_type: Option<&str>,
    ) -> anyhow::Result<Response<Body>> {
        if let Some(expected) = &self.auth_token {
            let provided = credentials.0.unwrap_or("");
            if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                return Ok(response(StatusCode::UNAUTHORIZED, Body::empty()));
            }
        }
        if body.len() > MAX_BODY_BYTES {
            return Ok(response(
                StatusCode::PAYLOAD_TOO_LARGE,
                Body::from("Failed to buffer the request body: length limit exceeded"),
            ));
        }
        let params = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        let request = RoutedRequest {
            method: &method,
            path,
            body: &body,
            params: &params,
            migration_header: credentials.1,
        };
        if matches!(
            (method.as_str(), path),
            ("POST", "/api/workspace/format") | ("POST", "/api/workspace/migration")
        ) {
            let _publication_guard = self.publication_lock.write().await;
            return Ok(self.dispatch_request(request));
        }
        if matches!(
            (method.as_str(), path),
            ("POST", "/api/upload") | ("PUT", "/api/head") | ("POST", "/api/manifest")
        ) {
            let _publication_guard = self.publication_lock.read().await;
            return Ok(self.dispatch_request(request));
        }
        Ok(self.dispatch_request(request))
    }

    fn dispatch_request(&self, request: RoutedRequest<'_>) -> Response<Body> {
        let result = match (request.method.as_str(), request.path) {
            ("GET", "/api/workspaces") => self
                .route_list_workspaces()
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())),
            ("POST", "/api/sync/peek") | ("POST", "/api/sync/diff") => {
                self.route_sync_peek(request.body)
            }
            ("POST", "/api/upload") => {
                self.route_upload(request.body, request.params, request.migration_header)
            }
            ("GET", route) if route.starts_with("/api/download/") => {
                self.route_download(&route["/api/download/".len()..])
            }
            ("GET", "/api/head") => self.route_get_head(request.params),
            ("PUT", "/api/head") => self.route_swap_head(request.body, request.migration_header),
            ("POST", "/api/manifest") => {
                self.route_manifest(request.body, request.params, request.migration_header)
            }
            ("GET", "/api/workspace/format") => self.route_get_format(request.params),
            ("POST", "/api/workspace/format") => {
                self.route_set_format(request.params, request.migration_header)
            }
            ("POST", "/api/workspace/migration") => {
                self.route_begin_migration(request.params, request.migration_header)
            }
            _ => Err((StatusCode::NOT_FOUND, "not found".to_string())),
        };
        match result {
            Ok(route_response) => route_response,
            Err((status, message)) => response(status, Body::from(message)),
        }
    }

    pub async fn read_body(response: Response<Body>) -> anyhow::Result<(StatusCode, Vec<u8>)> {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .context("read response body")?;
        Ok((status, bytes.to_vec()))
    }
}
