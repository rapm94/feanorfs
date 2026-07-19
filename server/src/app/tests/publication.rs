use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use feanorfs_common::hash_bytes;
use futures_util::future::join_all;
use tempfile::TempDir;
use tower::util::ServiceExt;

use super::{app_state, build_router};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_blob_uploads_never_expose_partial_ciphertext() {
    let app = build_router(app_state().await);
    let bytes = Bytes::from(vec![0x5a; 16 * 1024 * 1024]);
    let hash = hash_bytes(&bytes);
    let upload_uri = format!(
        "/api/upload?workspace_id=ws&path=object&hash={hash}&size={}&mtime=0&object=true",
        bytes.len()
    );
    let upload = || {
        Request::post(&upload_uri)
            .body(Body::from(bytes.clone()))
            .expect("build object upload")
    };

    assert_eq!(
        app.clone()
            .oneshot(upload())
            .await
            .expect("seed object upload")
            .status(),
        StatusCode::OK
    );

    let uploads = async { join_all((0..16).map(|_| app.clone().oneshot(upload()))).await };
    let downloads = async {
        join_all((0..64).map(|_| {
            let app = app.clone();
            let hash = hash.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::get(format!("/api/download/{hash}"))
                            .body(Body::empty())
                            .expect("build object download"),
                    )
                    .await
                    .expect("download object");
                assert_eq!(response.status(), StatusCode::OK);
                let downloaded = axum::body::to_bytes(response.into_body(), 17 * 1024 * 1024)
                    .await
                    .expect("read downloaded object");
                assert_eq!(hash_bytes(&downloaded), hash);
            }
        }))
        .await
    };

    let (upload_results, _) = tokio::join!(uploads, downloads);
    for response in upload_results {
        assert_eq!(
            response.expect("concurrent object upload").status(),
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn concurrent_head_swap_has_one_winner_and_reports_current() {
    let directory = TempDir::new().expect("create temp directory");
    let state = crate::init_app_state(directory.path().to_path_buf(), None)
        .await
        .expect("initialize app state");
    let app = build_router(state);
    let first_id = "a".repeat(64);
    let second_id = "b".repeat(64);
    let request = |new_id: &str| {
        Request::put("/api/head")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "workspace_id": "ws",
                    "expected": null,
                    "new": new_id,
                })
                .to_string(),
            ))
            .expect("build request")
    };
    let (first, second) = tokio::join!(
        app.clone().oneshot(request(&first_id)),
        app.clone().oneshot(request(&second_id)),
    );
    let first = first.expect("first response");
    let second = second.expect("second response");
    let statuses = [first.status(), second.status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT)
            .count(),
        1
    );
    let winner = if first.status() == StatusCode::OK {
        first_id
    } else {
        second_id
    };
    let conflict = if first.status() == StatusCode::CONFLICT {
        first
    } else {
        second
    };
    let body = axum::body::to_bytes(conflict.into_body(), 1024)
        .await
        .expect("read conflict body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("parse conflict body");
    assert_eq!(body["snapshot_id"], winner);
    let current = app
        .oneshot(
            Request::get("/api/head?workspace_id=ws")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("read current head");
    assert_eq!(current.status(), StatusCode::OK);
    let body = axum::body::to_bytes(current.into_body(), 1024)
        .await
        .expect("read current body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("parse current body");
    assert_eq!(body["snapshot_id"], winner);
}

#[tokio::test]
async fn format_v3_workspace_rejects_legacy_client_requests() {
    let state = app_state().await;
    let snapshot = "a".repeat(64);
    state
        .db
        .upsert_manifest("ws", &snapshot, format!("{snapshot}\n").as_bytes())
        .await
        .expect("store manifest");
    state
        .db
        .swap_head("ws", None, &snapshot)
        .await
        .expect("swap head");
    state
        .db
        .set_workspace_format("ws", 3)
        .await
        .expect("set format");
    let app = build_router(state);
    let request = Request::post("/api/sync/peek")
        .header("content-type", "application/json")
        .header("x-feanorfs-format", "3")
        .body(Body::from(
            serde_json::json!({ "workspace_id": "ws", "files": [] }).to_string(),
        ))
        .expect("build sync request");
    assert_eq!(
        app.clone()
            .oneshot(request)
            .await
            .expect("send sync request")
            .status(),
        StatusCode::UPGRADE_REQUIRED
    );
    let bytes = b"legacy flat upload";
    let hash = hash_bytes(bytes);
    let request = Request::post(format!(
        "/api/upload?workspace_id=ws&path=file.txt&hash={hash}&size={}&mtime=0&deleted=false",
        bytes.len()
    ))
    .header("x-feanorfs-format", "3")
    .body(Body::from(bytes.as_slice()))
    .expect("build upload request");
    assert_eq!(
        app.clone()
            .oneshot(request)
            .await
            .expect("send upload request")
            .status(),
        StatusCode::UPGRADE_REQUIRED
    );
    let request = Request::put("/api/head")
        .header("content-type", "application/json")
        .header("x-feanorfs-format", "3")
        .body(Body::from(
            serde_json::json!({
                "workspace_id": "ws",
                "expected": snapshot,
                "new": "b".repeat(64)
            })
            .to_string(),
        ))
        .expect("build head request");
    assert_eq!(
        app.oneshot(request)
            .await
            .expect("send head request")
            .status(),
        StatusCode::PRECONDITION_FAILED
    );
}

#[tokio::test]
async fn migration_fence_rejects_unfenced_flat_writes() {
    let state = app_state().await;
    let token = "c".repeat(64);
    state
        .db
        .begin_migration("ws", &token)
        .await
        .expect("begin migration");
    let app = build_router(state);
    let bytes = b"fenced upload";
    let hash = hash_bytes(bytes);
    let uri = format!(
        "/api/upload?workspace_id=ws&path=file.txt&hash={hash}&size={}&mtime=0&deleted=false",
        bytes.len()
    );
    let request = Request::post(&uri)
        .header("x-feanorfs-format", "3")
        .body(Body::from(bytes.as_slice()))
        .expect("build unfenced request");
    assert_eq!(
        app.clone()
            .oneshot(request)
            .await
            .expect("send unfenced request")
            .status(),
        StatusCode::LOCKED
    );
    let request = Request::post(uri)
        .header("x-feanorfs-format", "3")
        .header("x-feanorfs-migration", token)
        .body(Body::from(bytes.as_slice()))
        .expect("build fenced request");
    assert_eq!(
        app.oneshot(request)
            .await
            .expect("send fenced request")
            .status(),
        StatusCode::OK
    );
}
