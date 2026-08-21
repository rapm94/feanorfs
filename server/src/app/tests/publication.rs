use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use feanorfs_common::hash_bytes;
use futures_util::future::join_all;
use tempfile::TempDir;
use tower::util::ServiceExt;

use super::{app_state, build_router};

#[tokio::test]
async fn metadata_failure_never_deletes_an_existing_cas_object() {
    let directory = TempDir::new().expect("create temp directory");
    let state = crate::init_app_state(directory.path().to_path_buf(), None)
        .await
        .expect("initialize app state");
    let app = build_router(state);
    let bytes = Bytes::from_static(b"shared ciphertext");
    let hash = hash_bytes(&bytes);
    let object_uri = format!(
        "/api/upload?workspace_id=first&path=object&hash={hash}&size={}&mtime=0&object=true",
        bytes.len()
    );
    assert_eq!(
        app.clone()
            .oneshot(
                Request::post(&object_uri)
                    .body(Body::from(bytes.clone()))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(directory.path().join("db.sqlite")),
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER injected_file_failure BEFORE INSERT ON files          BEGIN SELECT RAISE(FAIL, 'injected metadata failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let file_uri = format!(
        "/api/upload?workspace_id=second&path=file.txt&hash={hash}&size={}&mtime=0",
        bytes.len()
    );
    assert_eq!(
        app.clone()
            .oneshot(
                Request::post(&file_uri)
                    .body(Body::from(bytes.clone()))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let download = app
        .oneshot(
            Request::get(format!("/api/download/{hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(download.into_body(), bytes.len())
            .await
            .unwrap(),
        bytes
    );
    pool.close().await;
}

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

    let uploads = async { join_all((0..4).map(|_| app.clone().oneshot(upload()))).await };
    let downloads = async {
        join_all((0..32).map(|_| {
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
    let first_id = "a".repeat(64);
    let second_id = "b".repeat(64);
    state
        .db
        .upsert_manifest("ws", &first_id, format!("{first_id}\n").as_bytes())
        .await
        .expect("store first manifest");
    state
        .db
        .upsert_manifest("ws", &second_id, format!("{second_id}\n").as_bytes())
        .await
        .expect("store second manifest");
    let app = build_router(state);
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
async fn head_swap_requires_manifest_for_every_client_format() {
    let app = build_router(app_state().await);
    let snapshot = "a".repeat(64);
    for format in ["1", "2", "3"] {
        let response = app
            .clone()
            .oneshot(
                Request::put("/api/head")
                    .header("content-type", "application/json")
                    .header("x-feanorfs-format", format)
                    .body(Body::from(
                        serde_json::json!({
                            "workspace_id": "ws",
                            "expected": null,
                            "new": snapshot,
                        })
                        .to_string(),
                    ))
                    .expect("build head request"),
            )
            .await
            .expect("send head request");
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }
}

#[tokio::test]
async fn missing_manifest_precedes_stale_expected_head() {
    let state = app_state().await;
    let current = "b".repeat(64);
    state
        .db
        .upsert_manifest("ws", &current, format!("{current}\n").as_bytes())
        .await
        .expect("store current manifest");
    assert_eq!(
        state.db.swap_head("ws", None, &current).await.unwrap(),
        crate::db::HeadSwap::Swapped
    );
    let app = build_router(state);
    let missing = "c".repeat(64);
    let stale = "d".repeat(64);
    let response = app
        .oneshot(
            Request::put("/api/head")
                .header("content-type", "application/json")
                .header("x-feanorfs-format", "1")
                .body(Body::from(
                    serde_json::json!({
                        "workspace_id": "ws",
                        "expected": stale,
                        "new": missing,
                    })
                    .to_string(),
                ))
                .expect("build stale head request"),
        )
        .await
        .expect("send stale head request");
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn snapshot_manifest_is_root_bound_and_immutable() {
    let state = app_state().await;
    let root = "a".repeat(64);
    let child = "b".repeat(64);
    assert!(state.db.upsert_manifest("ws", &root, b"").await.is_err());
    assert!(state
        .db
        .upsert_manifest("ws", &root, format!("{child}\n").as_bytes())
        .await
        .is_err());
    state
        .db
        .upsert_manifest("ws", &root, format!("{child}\n{root}\n").as_bytes())
        .await
        .expect("store complete manifest");
    state
        .db
        .upsert_manifest(
            "ws",
            &root,
            format!("{root}\n{child}\n{child}\n").as_bytes(),
        )
        .await
        .expect("accept the same canonical closure");
    assert!(state
        .db
        .upsert_manifest("ws", &root, format!("{root}\n").as_bytes())
        .await
        .is_err());
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

// ---------------------------------------------------------------------------
// Bounded opaque head-change waiting (CAD-2)
// ---------------------------------------------------------------------------

fn wait_request(workspace: &str, after: Option<&str>, wait_ms: Option<u64>) -> Request<Body> {
    let mut query = format!("workspace_id={workspace}");
    if let Some(after) = after {
        query.push_str(&format!("&after={after}"));
    }
    if let Some(wait_ms) = wait_ms {
        query.push_str(&format!("&wait_ms={wait_ms}"));
    }
    Request::get(format!("/api/head?{query}"))
        .header("x-feanorfs-format", "3")
        .body(Body::empty())
        .expect("build wait request")
}

fn swap_request(workspace: &str, expected: Option<&str>, new: &str) -> Request<Body> {
    Request::put("/api/head")
        .header("content-type", "application/json")
        .header("x-feanorfs-format", "3")
        .body(Body::from(
            serde_json::json!({
                "workspace_id": workspace,
                "expected": expected,
                "new": new,
            })
            .to_string(),
        ))
        .expect("build swap request")
}

async fn head_json(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read head body");
    serde_json::from_slice(&body).expect("parse head body")
}

#[tokio::test]
async fn head_wait_returns_immediately_when_head_already_differs() {
    let state = app_state().await;
    let head = "c".repeat(64);
    state
        .db
        .upsert_manifest("ws", &head, format!("{head}\n").as_bytes())
        .await
        .expect("store manifest");
    state.db.swap_head("ws", None, &head).await.unwrap();
    let app = build_router(state);
    let response = app
        .clone()
        .oneshot(wait_request("ws", Some(&"d".repeat(64)), Some(5000)))
        .await
        .expect("send wait request");
    assert_eq!(response.status(), StatusCode::OK);
    let body = head_json(response).await;
    assert_eq!(body["snapshot_id"], head);
    assert_eq!(body["wait_supported"], true);
}

#[tokio::test]
async fn head_wait_wakes_after_durable_cas() {
    let state = app_state().await;
    let first = "e".repeat(64);
    let second = "f".repeat(64);
    for id in [&first, &second] {
        state
            .db
            .upsert_manifest("ws", id, format!("{id}\n").as_bytes())
            .await
            .expect("store manifest");
    }
    state.db.swap_head("ws", None, &first).await.unwrap();
    let app = build_router(state);
    let wait = app
        .clone()
        .oneshot(wait_request("ws", Some(&first), Some(5000)));
    // Publish after a short delay so the waiter is registered first.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let swap = app
        .oneshot(swap_request("ws", Some(&first), &second))
        .await
        .expect("swap head");
    assert_eq!(swap.status(), StatusCode::OK);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), wait)
        .await
        .expect("waiter must wake")
        .expect("waiter response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = head_json(response).await;
    assert_eq!(body["snapshot_id"], second);
}

#[tokio::test]
async fn head_wait_does_not_wake_after_rejected_cas() {
    let state = app_state().await;
    let first = "11".repeat(32);
    let second = "22".repeat(32);
    let third = "33".repeat(32);
    for id in [&first, &second, &third] {
        state
            .db
            .upsert_manifest("ws", id, format!("{id}\n").as_bytes())
            .await
            .expect("store manifest");
    }
    state.db.swap_head("ws", None, &first).await.unwrap();
    let app = build_router(state);
    let wait = app
        .clone()
        .oneshot(wait_request("ws", Some(&first), Some(500)));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // A rejected CAS must never wake the waiter.
    let rejected = app
        .clone()
        .oneshot(swap_request("ws", Some(&"99".repeat(32)), &third))
        .await
        .expect("rejected swap");
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    // The accepted swap wakes the waiter with the new head.
    let accepted = app
        .oneshot(swap_request("ws", Some(&first), &second))
        .await
        .expect("accepted swap");
    assert_eq!(accepted.status(), StatusCode::OK);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), wait)
        .await
        .expect("waiter must resolve")
        .expect("waiter response");
    let body = head_json(response).await;
    assert_eq!(body["snapshot_id"], second);
    assert_eq!(body["wait_supported"], true);
}

#[tokio::test]
async fn head_wait_timeout_returns_unchanged_head_without_error() {
    let state = app_state().await;
    let head = "44".repeat(32);
    state
        .db
        .upsert_manifest("ws", &head, format!("{head}\n").as_bytes())
        .await
        .expect("store manifest");
    state.db.swap_head("ws", None, &head).await.unwrap();
    let app = build_router(state);
    let started = std::time::Instant::now();
    let response = app
        .oneshot(wait_request("ws", Some(&head), Some(200)))
        .await
        .expect("send wait request");
    assert!(started.elapsed() >= std::time::Duration::from_millis(150));
    assert_eq!(response.status(), StatusCode::OK);
    let body = head_json(response).await;
    assert_eq!(body["snapshot_id"], head);
}

#[tokio::test]
async fn exhausted_workspace_waiters_return_retryable_status() {
    let state = app_state().await;
    let head = "45".repeat(32);
    state
        .db
        .upsert_manifest("ws", &head, format!("{head}\n").as_bytes())
        .await
        .expect("store manifest");
    state.db.swap_head("ws", None, &head).await.unwrap();
    let app = build_router(state);
    let mut waiters = Vec::new();
    for _ in 0..super::super::head_wait::MAX_WORKSPACE_WAITERS {
        let request = wait_request("ws", Some(&head), Some(5000));
        let app = app.clone();
        waiters.push(tokio::spawn(async move { app.oneshot(request).await }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let overflow = app
        .oneshot(wait_request("ws", Some(&head), Some(5000)))
        .await
        .expect("send overflow wait request");
    assert_eq!(overflow.status(), StatusCode::SERVICE_UNAVAILABLE);

    for waiter in waiters {
        waiter.abort();
    }
}

#[tokio::test]
async fn head_wait_auth_and_format_checks_still_fail_closed() {
    let state = crate::init_app_state(
        tempfile::TempDir::new()
            .expect("temp dir")
            .path()
            .to_path_buf(),
        Some("secret".to_string()),
    )
    .await
    .expect("init app state");
    let app = build_router(state);
    let response = app
        .oneshot(wait_request("ws", Some(&"55".repeat(32)), Some(1000)))
        .await
        .expect("send unauthorized wait");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn head_wait_workspace_a_publication_never_wakes_b() {
    let state = app_state().await;
    let head_a = "66".repeat(32);
    let head_b = "77".repeat(32);
    let next_a = "88".repeat(32);
    for (workspace, id) in [("a", &head_a), ("b", &head_b), ("a", &next_a)] {
        state
            .db
            .upsert_manifest(workspace, id, format!("{id}\n").as_bytes())
            .await
            .expect("store manifest");
    }
    state.db.swap_head("a", None, &head_a).await.unwrap();
    state.db.swap_head("b", None, &head_b).await.unwrap();
    let app = build_router(state);
    let wait_b = app
        .clone()
        .oneshot(wait_request("b", Some(&head_b), Some(400)));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let swap_a = app
        .oneshot(swap_request("a", Some(&head_a), &next_a))
        .await
        .expect("swap workspace a");
    assert_eq!(swap_a.status(), StatusCode::OK);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), wait_b)
        .await
        .expect("waiter b must resolve by timeout");
    let response = response.expect("waiter b response");
    let body = head_json(response).await;
    assert_eq!(
        body["snapshot_id"], head_b,
        "workspace b must not wake for a"
    );
}

#[tokio::test]
async fn head_wait_rejects_malformed_after() {
    let app = build_router(app_state().await);
    let response = app
        .oneshot(wait_request("ws", Some("not-a-hash"), Some(100)))
        .await
        .expect("send malformed wait");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn plain_head_get_keeps_the_immediate_shape() {
    let app = build_router(app_state().await);
    let response = app
        .oneshot(
            Request::get("/api/head?workspace_id=ws")
                .header("x-feanorfs-format", "3")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send plain get");
    assert_eq!(response.status(), StatusCode::OK);
    let body = head_json(response).await;
    // Old clients parse exactly this shape; `wait_supported` stays omitted.
    assert_eq!(body["snapshot_id"], serde_json::Value::Null);
    assert!(body.get("wait_supported").is_none());
}
