use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tempfile::TempDir;
use tower::util::ServiceExt;

use super::{app_state, build_router};

#[tokio::test]
async fn version_probe_requires_auth_and_reports_this_build() {
    let directory = TempDir::new().expect("create temp directory");
    let state = crate::init_app_state(directory.path().to_path_buf(), Some("secret".into()))
        .await
        .expect("initialize app state");
    let router = build_router(state);
    let unauthenticated = router
        .clone()
        .oneshot(
            Request::get("/api/version")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send request");
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let response = router
        .oneshot(
            Request::get("/api/version")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send authenticated request");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("read version body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("parse version JSON");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn protected_request_permits_live_until_response_bodies_are_dropped() {
    let router = build_router(app_state().await);
    let mut responses = Vec::new();
    for _ in 0..super::super::MAX_PROTECTED_REQUESTS {
        let response = router
            .clone()
            .oneshot(Request::get("/api/version").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        responses.push(response);
    }
    let saturated = router
        .clone()
        .oneshot(Request::get("/api/version").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(saturated.status(), StatusCode::SERVICE_UNAVAILABLE);

    drop(responses.pop());
    let admitted = router
        .oneshot(Request::get("/api/version").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(admitted.status(), StatusCode::OK);
}

#[tokio::test]
async fn manifest_content_length_is_capped_before_body_extraction() {
    let response = build_router(app_state().await)
        .oneshot(
            Request::post("/api/manifest?workspace_id=ws&snapshot_id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .header("content-length", (crate::MAX_MANIFEST_BYTES + 1).to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn chunked_manifest_body_is_capped_by_the_route_limit() {
    let chunk = axum::body::Bytes::from(vec![b'a'; 1024 * 1024]);
    let stream = futures_util::stream::iter(
        (0..65).map(move |_| Ok::<_, std::convert::Infallible>(chunk.clone())),
    );
    let response = build_router(app_state().await)
        .oneshot(
            Request::post("/api/manifest?workspace_id=ws&snapshot_id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .body(Body::from_stream(stream))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

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
async fn unsafe_legacy_path_can_only_tombstone_an_existing_row() {
    let state = app_state().await;
    let hash = "a".repeat(64);
    state
        .db
        .upsert_file(
            "ws",
            &feanorfs_common::FileState {
                path: ".jj/repo/store".into(),
                hash: hash.clone(),
                size: 1,
                mtime: 1,
                deleted: false,
                mode: 0,
            },
        )
        .await
        .unwrap();
    let db = std::sync::Arc::clone(&state.db);
    let router = build_router(state);

    let retired = router
        .clone()
        .oneshot(
            Request::post(format!(
                "/api/upload?workspace_id=ws&path=.jj/repo/store&hash={hash}&size=0&mtime=2&deleted=true"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retired.status(), StatusCode::OK);
    assert!(
        db.get_workspace_files("ws")
            .await
            .unwrap()
            .into_iter()
            .find(|file| file.path == ".jj/repo/store")
            .unwrap()
            .deleted
    );

    let invented = router
        .oneshot(
            Request::post(format!(
                "/api/upload?workspace_id=ws&path=../invented&hash={hash}&size=0&mtime=2&deleted=true"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invented.status(), StatusCode::BAD_REQUEST);
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
