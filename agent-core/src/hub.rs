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

use self::http::json_body;
use crate::hub_state::HubDb;

const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

type CacheKey = (PathBuf, Option<String>);

static HUB_CACHE: OnceLock<Mutex<HashMap<CacheKey, Arc<LocalHub>>>> = OnceLock::new();

fn hub_cache() -> &'static Mutex<HashMap<CacheKey, Arc<LocalHub>>> {
    HUB_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct LocalHub {
    db: HubDb,
    auth_token: Option<String>,
    publication_lock: RwLock<()>,
    head_notify: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
}

struct LocalHeadNotifier<'a> {
    hub: &'a LocalHub,
    workspace_id: String,
    notify: Arc<tokio::sync::Notify>,
}

impl Drop for LocalHeadNotifier<'_> {
    fn drop(&mut self) {
        let mut map = self
            .hub
            .head_notify
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = map.get(&self.workspace_id).is_some_and(|current| {
            Arc::ptr_eq(current, &self.notify) && Arc::strong_count(current) == 2
        });
        if remove {
            map.remove(&self.workspace_id);
        }
    }
}

#[derive(Clone, Copy)]
struct RoutedRequest<'a> {
    method: &'a Method,
    path: &'a str,
    body: &'a [u8],
    params: &'a HashMap<String, String>,
    migration_header: Option<&'a str>,
}

impl std::fmt::Debug for LocalHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalHub")
            .field("auth_required", &self.auth_token.is_some())
            .finish_non_exhaustive()
    }
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
            head_notify: Mutex::new(HashMap::new()),
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
            head_notify: Mutex::new(HashMap::new()),
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
        if method == Method::GET && path == "/api/head" {
            if let Some(after) = params.get("after").cloned() {
                return self.request_head_with_wait(&params, &after).await;
            }
        }
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

    /// In-process bounded head-change waiting, mirroring the HTTP route's
    /// semantics: respond immediately when the head already differs from
    /// `after`, otherwise wait for the next durable swap or the timeout.
    async fn request_head_with_wait(
        &self,
        params: &HashMap<String, String>,
        after: &str,
    ) -> anyhow::Result<Response<Body>> {
        if !feanorfs_common::is_valid_hash(after) {
            return Ok(response(StatusCode::BAD_REQUEST, Body::empty()));
        }
        let Some(workspace_id) = params
            .get("workspace_id")
            .map(String::as_str)
            .filter(|workspace_id| !workspace_id.is_empty())
        else {
            return Ok(response(StatusCode::BAD_REQUEST, Body::empty()));
        };
        let requested_wait_ms = match params.get("wait_ms") {
            None => None,
            Some(value) => match value.parse::<u64>() {
                Ok(wait_ms) => Some(wait_ms),
                Err(_) => return Ok(response(StatusCode::BAD_REQUEST, Body::empty())),
            },
        };
        let respond = |snapshot_id: Option<String>| {
            json_body(
                StatusCode::OK,
                &feanorfs_common::HeadResponse {
                    snapshot_id,
                    wait_supported: true,
                },
            )
        };
        let current = match self.db.get_head(workspace_id) {
            Ok(current) => current,
            Err(error) => {
                return Ok(response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Body::from(error.to_string()),
                ))
            }
        };
        if current.as_deref() != Some(after) {
            return Ok(respond(current));
        }
        let Some(wait_ms) = requested_wait_ms else {
            return Ok(respond(current));
        };
        let wait_ms = wait_ms.min(crate::head::MAX_HEAD_WAIT_MS);
        if wait_ms == 0 {
            return Ok(respond(current));
        }
        let notify = {
            let mut map = self
                .head_notify
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(map.entry(workspace_id.to_string()).or_default())
        };
        let registration = LocalHeadNotifier {
            hub: self,
            workspace_id: workspace_id.to_string(),
            notify,
        };
        // Register the notification future before the second head read. A
        // `notify_waiters` between that read and `select!` must not be lost.
        let notified = registration.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        // Re-check after registration: a swap between the initial read and
        // the registration must never lose the wakeup.
        match self.db.get_head(workspace_id) {
            Ok(registered_head) => {
                if registered_head.as_deref() != Some(after) {
                    return Ok(respond(registered_head));
                }
            }
            Err(error) => {
                return Ok(response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Body::from(error.to_string()),
                ));
            }
        }
        tokio::select! {
            _ = &mut notified => {
                let refreshed = match self.db.get_head(workspace_id) {
                    Ok(refreshed) => refreshed,
                    Err(error) => {
                        return Ok(response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Body::from(error.to_string()),
                        ))
                    }
                };
                Ok(respond(refreshed))
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {
                Ok(respond(current))
            }
        }
    }

    fn notify_head_waiters(&self, workspace_id: &str) {
        if let Some(notify) = self
            .head_notify
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(workspace_id)
        {
            notify.notify_waiters();
        }
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

    pub async fn read_body_bounded(
        response: Response<Body>,
        max_bytes: usize,
    ) -> anyhow::Result<(StatusCode, Vec<u8>)> {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), max_bytes)
            .await
            .context("read bounded response body")?;
        Ok((status, bytes.to_vec()))
    }
}
