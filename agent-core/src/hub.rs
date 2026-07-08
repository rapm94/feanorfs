use anyhow::{Context, Result};
use axum::body::Body;
use axum::Router;
use feanorfs_server::{build_router, init_app_state, AppState};
use http::{Method, Request, Response, StatusCode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tower::ServiceExt;

static HUB_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<LocalHub>>>> = OnceLock::new();

fn hub_cache() -> &'static Mutex<HashMap<PathBuf, Arc<LocalHub>>> {
    HUB_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// In-process hub (CONN-2): Axum router served via `tower::ServiceExt::oneshot`.
pub struct LocalHub {
    router: Router,
    _state: Arc<AppState>,
}

impl LocalHub {
    pub async fn open(data_dir: PathBuf, auth_token: Option<String>) -> Result<Arc<Self>> {
        let key = data_dir.canonicalize().or_else(|_| {
            std::fs::create_dir_all(&data_dir)
                .and_then(|_| data_dir.canonicalize())
        })?;
        if let Some(hub) = hub_cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned() {
            return Ok(hub);
        }
        let state = init_app_state(data_dir, auth_token).await?;
        let state = Arc::new(state);
        let router = build_router((*state).clone());
        let hub = Arc::new(Self {
            router,
            _state: state,
        });
        hub_cache().lock().unwrap_or_else(|e| e.into_inner()).insert(key, hub.clone());
        Ok(hub)
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: Vec<u8>,
        bearer: Option<&str>,
        content_type: Option<&str>,
    ) -> Result<Response<Body>> {
        let uri = if query.is_empty() {
            format!("http://feanorfs.local{path}")
        } else {
            format!("http://feanorfs.local{path}?{query}")
        };
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = bearer {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(ct) = content_type {
            builder = builder.header("Content-Type", ct);
        }
        let req = builder
            .body(Body::from(body))
            .context("build in-process request")?;
        self.router
            .clone()
            .oneshot(req)
            .await
            .context("in-process hub request failed")
    }

    pub async fn read_body(resp: Response<Body>) -> Result<(StatusCode, Vec<u8>)> {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), feanorfs_server::MAX_BODY_BYTES)
            .await
            .context("read in-process response body")?;
        Ok((status, bytes.to_vec()))
    }
}
