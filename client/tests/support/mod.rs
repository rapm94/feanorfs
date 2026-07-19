#![allow(dead_code)]

use feanorfs_client::{ApiClient, ClientDb, Config};
use feanorfs_server::{build_router, init_app_state};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub const TEST_PASSWORD: &str = "integration-test-password";
pub const WORKSPACE_ID: &str = "test-workspace";

pub fn state_path(workspace: &Path) -> PathBuf {
    feanorfs_agent_core::ensure_workspace_state(workspace).unwrap()
}

pub fn agent_path(workspace: &Path, name: &str) -> PathBuf {
    feanorfs_agent_core::agent_dir(workspace, name).unwrap()
}

pub fn write_test_config(workspace: &Path, server_url: &str) {
    let state = state_path(workspace);
    let cfg = Config {
        server_url: server_url.to_string(),
        workspace_id: WORKSPACE_ID.to_string(),
        encryption_password: Some(TEST_PASSWORD.to_string()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 2,
        hub_local: false,
        relay: None,
    };
    let json = serde_json::to_string_pretty(&cfg).unwrap();
    std::fs::write(state.join("config.json"), json).unwrap();
    assert!(!workspace.join(".feanorfs").exists());
}

pub struct TestServer {
    pub api: ApiClient,
    pub url: String,
    _data_dir: TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

pub async fn spawn_test_server() -> TestServer {
    let data_dir = TempDir::new().unwrap();
    let state = init_app_state(data_dir.path().to_path_buf(), None)
        .await
        .unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("http://{addr}");
    TestServer {
        api: ApiClient::new(&url, None),
        url,
        _data_dir: data_dir,
        _handle: handle,
    }
}

pub struct TestClient {
    pub workspace: TempDir,
    pub db: ClientDb,
}

pub async fn spawn_test_client() -> TestClient {
    let workspace = TempDir::new().unwrap();
    let db = ClientDb::new(state_path(workspace.path())).await.unwrap();
    TestClient { workspace, db }
}

pub async fn spawn_test_client_with_server(server: &TestServer) -> TestClient {
    let client = spawn_test_client().await;
    write_test_config(client.workspace.path(), &server.url);
    client
}

pub async fn write_workspace_file(workspace: &Path, rel: &str, content: &[u8]) -> PathBuf {
    let path = workspace.join(rel);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.unwrap();
    }
    tokio::fs::write(&path, content).await.unwrap();
    path
}

pub async fn read_workspace_file(workspace: &Path, rel: &str) -> Vec<u8> {
    tokio::fs::read(workspace.join(rel)).await.unwrap()
}
