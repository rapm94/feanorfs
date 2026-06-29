use feanorfs_client::{ApiClient, ClientDb};
use feanorfs_server::{build_router, init_app_state};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub const TEST_PASSWORD: &str = "integration-test-password";
pub const WORKSPACE_ID: &str = "test-workspace";

pub struct TestServer {
    pub api: ApiClient,
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
    let db = ClientDb::new(workspace.path().join(".feanorfs"))
        .await
        .unwrap();
    TestClient { workspace, db }
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
