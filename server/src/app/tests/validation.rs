use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tempfile::TempDir;
use tower::util::ServiceExt;

use super::{app_state, build_router};

#[tokio::test]
async fn upload_rejects_unsafe_path() {
    let request =
        Request::post("/api/upload?workspace_id=ws&path=../etc/passwd&hash=a&size=0&mtime=0")
            .body(Body::empty())
            .expect("build request");
    assert_eq!(
        build_router(app_state().await)
            .oneshot(request)
            .await
            .expect("send request")
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn upload_rejects_invalid_hash() {
    let request =
        Request::post("/api/upload?workspace_id=ws&path=safe.txt&hash=not-a-hash&size=0&mtime=0")
            .body(Body::from("data"))
            .expect("build request");
    assert_eq!(
        build_router(app_state().await)
            .oneshot(request)
            .await
            .expect("send request")
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn download_rejects_invalid_hash() {
    let request = Request::get("/api/download/too-short")
        .body(Body::empty())
        .expect("build request");
    assert_eq!(
        build_router(app_state().await)
            .oneshot(request)
            .await
            .expect("send request")
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn download_nonexistent_blob_returns_404() {
    let request = Request::get(format!("/api/download/{}", "a".repeat(64)))
        .body(Body::empty())
        .expect("build request");
    assert_eq!(
        build_router(app_state().await)
            .oneshot(request)
            .await
            .expect("send request")
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn auth_required_when_token_set() {
    let directory = TempDir::new().expect("create temp directory");
    let state = crate::init_app_state(directory.path().to_path_buf(), Some("secret".into()))
        .await
        .expect("initialize app state");
    let request = Request::get("/api/workspaces")
        .body(Body::empty())
        .expect("build request");
    assert_eq!(
        build_router(state)
            .oneshot(request)
            .await
            .expect("send request")
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
