mod db;

use axum::extract::DefaultBodyLimit;
use axum::{
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use constant_time_eq::constant_time_eq;
use db::Db;
use feanorfs_common::{is_valid_hash, FileState, SyncRequest, SyncResponse};
use serde::Deserialize;
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::fs;

#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    storage_dir: PathBuf,
    auth_token: Option<String>,
}

#[derive(Parser)]
#[command(name = "feanorfs-server")]
#[command(about = "Content-addressed blob storage and sync metadata server for FeanorFS")]
struct Cli {
    /// Authentication token. Clients must send this as a Bearer token. In SaaS mode, this becomes a per-user API key.
    #[arg(long, env = "FEANORFS_TOKEN", visible_alias = "password")]
    token: Option<String>,

    /// Enable mDNS service advertisement for LAN discovery (off by default for internet deployments)
    #[arg(long)]
    mdns: bool,

    /// Port to listen on (default: 3030). Use different ports when running multiple instances behind a reverse proxy.
    #[arg(long, default_value = "3030", env = "FEANORFS_PORT")]
    port: u16,

    /// Data directory for SQLite DB and blob storage (default: ./server-data). Each instance should have its own.
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    data_dir: PathBuf,
}

#[derive(Deserialize)]
struct UploadParams {
    workspace_id: String,
    path: String,
    hash: String,
    size: u64,
    mtime: i64,
}

fn local_ip() -> anyhow::Result<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    Ok(socket.local_addr()?.ip().to_string())
}

fn register_mdns(port: u16) -> anyhow::Result<mdns_sd::ServiceDaemon> {
    use mdns_sd::{ServiceDaemon, ServiceInfo};

    let daemon = ServiceDaemon::new()?;
    let ip = local_ip()?;
    let props: &[(&str, &str)] = &[("v", "1")];
    let service_info = ServiceInfo::new(
        "_feanorfs._tcp.local.",
        "feanorfs-server",
        "feanorfs-server",
        &ip,
        port,
        props,
    )?;
    daemon.register(service_info)?;
    Ok(daemon)
}

async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    match &state.auth_token {
        None => Ok(next.run(request).await),
        Some(expected) => {
            let provided = request
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match provided {
                Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => {
                    Ok(next.run(request).await)
                }
                _ => Err(StatusCode::UNAUTHORIZED),
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "feanorfs_server=info,tower_http=info".into()),
        )
        .init();

    let base_dir = cli.data_dir.clone();
    let db_path = base_dir.join("db.sqlite");
    let blobs_dir = base_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).await?;

    let db = Db::new(&db_path).await?;
    let state = AppState {
        db: Arc::new(db),
        storage_dir: base_dir,
        auth_token: cli.token.clone(),
    };

    let app = Router::new()
        .route("/api/sync/diff", post(handle_sync_diff))
        .route("/api/upload", post(handle_upload))
        .route("/api/download/:hash", get(handle_download))
        .route("/api/workspaces", get(handle_get_workspaces))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!(
        "FeanorFS Sync Server starting on http://{} (data: {})",
        addr,
        cli.data_dir.display()
    );

    let _mdns_daemon = if cli.mdns {
        match register_mdns(addr.port()) {
            Ok(d) => {
                tracing::info!("mDNS service registered (discoverable on local network)");
                Some(d)
            }
            Err(e) => {
                tracing::warn!("Failed to register mDNS service: {}", e);
                None
            }
        }
    } else {
        tracing::info!("mDNS disabled (default). Use --mdns to enable LAN discovery.");
        None
    };

    if cli.token.is_some() {
        tracing::info!("Authentication enabled (token required)");
    } else {
        tracing::warn!("No auth token set. Run with --token <TOKEN> for authenticated access.");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_sync_diff(
    State(state): State<AppState>,
    Json(payload): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, StatusCode> {
    let workspace_id = payload.workspace_id;
    let client_files = payload.files;

    let server_files = state
        .db
        .get_workspace_files(&workspace_id)
        .await
        .map_err(|e| {
            tracing::error!("Error fetching workspace files: {:?}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut server_map: HashMap<String, FileState> = server_files
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();

    let mut upload_required = Vec::new();
    let mut download_required = Vec::new();
    let mut delete_local = Vec::new();

    let client_map: HashMap<String, FileState> = client_files
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();

    // 1. Process client files compared with server
    for (path, client_file) in &client_map {
        if let Some(server_file) = server_map.get(path) {
            // File exists on both
            if client_file.mtime > server_file.mtime {
                // Client has a newer version
                if client_file.deleted {
                    // Update server state to deleted
                    state
                        .db
                        .upsert_file(&workspace_id, client_file)
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    server_map.insert(path.clone(), client_file.clone());
                } else {
                    upload_required.push(path.clone());
                }
            } else if server_file.mtime > client_file.mtime {
                // Server has a newer version
                if server_file.deleted {
                    delete_local.push(path.clone());
                } else {
                    download_required.push(server_file.clone());
                }
            } else {
                // Mtimes are equal, verify hash consistency
                if client_file.hash != server_file.hash && !client_file.deleted {
                    upload_required.push(path.clone());
                }
            }
        } else {
            // Client has it, server doesn't
            if client_file.deleted {
                // Client deleted it before server saw it, mark as deleted on server anyway
                state
                    .db
                    .upsert_file(&workspace_id, client_file)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            } else {
                upload_required.push(path.clone());
            }
        }
    }

    // 2. Process server files that client does not know about
    for (path, server_file) in &server_map {
        if !client_map.contains_key(path) && !server_file.deleted {
            download_required.push(server_file.clone());
        }
    }

    Ok(Json(SyncResponse {
        upload_required,
        download_required,
        delete_local,
    }))
}

async fn handle_upload(
    State(state): State<AppState>,
    Query(params): Query<UploadParams>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_valid_hash(&params.hash) {
        tracing::warn!("Rejected upload with invalid hash: {}", params.hash);
        return Err(StatusCode::BAD_REQUEST);
    }

    let computed_hash = feanorfs_common::hash_bytes(&body);
    if computed_hash != params.hash {
        tracing::warn!(
            "Hash mismatch for {}: expected {}, computed {}",
            params.path,
            params.hash,
            computed_hash
        );
        return Err(StatusCode::BAD_REQUEST);
    }

    // Write file content to blobs directory
    let blob_path = state.storage_dir.join("blobs").join(&params.hash);
    if let Err(e) = fs::write(&blob_path, &body).await {
        tracing::error!("Failed to write blob: {:?}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Update database metadata
    let file_state = FileState {
        path: params.path,
        hash: params.hash,
        size: params.size,
        mtime: params.mtime,
        deleted: false,
    };

    if let Err(e) = state
        .db
        .upsert_file(&params.workspace_id, &file_state)
        .await
    {
        tracing::error!("Failed to upsert file in db: {:?}", e);
        let _ = fs::remove_file(&blob_path).await;
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok(StatusCode::OK)
}

async fn handle_download(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_valid_hash(&hash) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let blob_path = state.storage_dir.join("blobs").join(&hash);

    // Single read attempt — eliminates the exists()/read() TOCTOU window
    // and still returns 404 cleanly if the blob was removed between calls.
    let file_content = match fs::read(&blob_path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StatusCode::NOT_FOUND);
        }
        Err(e) => {
            tracing::error!("Failed to read blob file: {:?}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    Ok(file_content)
}

async fn handle_get_workspaces(
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, StatusCode> {
    let workspaces = state.db.get_workspaces().await.map_err(|e| {
        tracing::error!("Error fetching workspaces: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(workspaces))
}
