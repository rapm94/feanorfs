use feanorfs_agent_core::{ApiClient, LocalHub, SwapHeadResult};
use feanorfs_common::{hash_bytes, SwapHeadRequest};
use http::Method;
use std::sync::Arc;

fn mk_hash(data: &[u8]) -> String {
    hash_bytes(data)
}

async fn open_anon(data: &tempfile::TempDir) -> (Arc<LocalHub>, ApiClient) {
    let hub = LocalHub::open(data.path().to_path_buf(), None)
        .await
        .expect("open hub");
    let api = ApiClient::local(Arc::clone(&hub), None);
    (hub, api)
}

async fn open_auth(data: &tempfile::TempDir, token: &str) -> (Arc<LocalHub>, ApiClient) {
    let hub = LocalHub::open(data.path().to_path_buf(), Some(token.into()))
        .await
        .expect("open hub");
    let api = ApiClient::local(Arc::clone(&hub), Some(token.into()));
    (hub, api)
}

#[tokio::test]
async fn auth_correct_token_allows_access() {
    let d = tempfile::tempdir().expect("dir");
    let (_, api) = open_auth(&d, "sekret").await;
    let h = mk_hash(b"x");
    api.upload_object("ws", &h, b"x".to_vec())
        .await
        .expect("upload");
}

#[tokio::test]
async fn auth_wrong_token_returns_401() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_auth(&d, "sekret").await;
    let bad = ApiClient::local(hub, Some("wrong".into()));
    let h = mk_hash(b"x");
    let err = bad
        .upload_object("ws", &h, b"x".to_vec())
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("requires a valid access token"));
}

#[tokio::test]
async fn auth_missing_token_returns_401() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_auth(&d, "sekret").await;
    let anon = ApiClient::local(hub, None);
    let h = mk_hash(b"x");
    let err = anon
        .upload_object("ws", &h, b"x".to_vec())
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("requires a valid access token"));
}

#[tokio::test]
async fn auth_cache_isolates_different_tokens() {
    let d = tempfile::tempdir().expect("dir");
    let hub_a = LocalHub::open(d.path().to_path_buf(), Some("tok-a".into()))
        .await
        .expect("a");
    let hub_b = LocalHub::open(d.path().to_path_buf(), Some("tok-b".into()))
        .await
        .expect("b");
    let api_a = ApiClient::local(hub_a, Some("tok-a".into()));
    let api_b = ApiClient::local(hub_b, Some("tok-b".into()));
    let h = mk_hash(b"c");
    api_a
        .upload_object("ws", &h, b"c".to_vec())
        .await
        .expect("a");
    api_b
        .upload_object("ws", &h, b"c".to_vec())
        .await
        .expect("b");
}

#[tokio::test]
async fn begin_migration_requires_header_token() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(
            Method::POST,
            "/api/workspace/migration",
            "workspace_id=ws",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn begin_migration_rejects_invalid_token_format() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(
            Method::POST,
            "/api/workspace/migration",
            "workspace_id=ws",
            vec![],
            (None, Some("not-a-hash")),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn begin_migration_idempotent_same_token() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let token = mk_hash(b"mig");
    let r1 = hub
        .request(
            Method::POST,
            "/api/workspace/migration",
            "workspace_id=ws",
            vec![],
            (None, Some(&token)),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r1.status(), http::StatusCode::OK);
    let r2 = hub
        .request(
            Method::POST,
            "/api/workspace/migration",
            "workspace_id=ws",
            vec![],
            (None, Some(&token)),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r2.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn begin_migration_blocks_conflicting_token() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let tok1 = mk_hash(b"tok1");
    let tok2 = mk_hash(b"tok2");
    hub.request(
        Method::POST,
        "/api/workspace/migration",
        "workspace_id=ws",
        vec![],
        (None, Some(&tok1)),
        None,
    )
    .await
    .expect("req");
    let r = hub
        .request(
            Method::POST,
            "/api/workspace/migration",
            "workspace_id=ws",
            vec![],
            (None, Some(&tok2)),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::LOCKED);
}

#[tokio::test]
async fn fence_blocks_upload() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let token = mk_hash(b"fence");
    hub.request(
        Method::POST,
        "/api/workspace/migration",
        "workspace_id=ws",
        vec![],
        (None, Some(&token)),
        None,
    )
    .await
    .expect("fence");
    let h = mk_hash(b"data");
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=ok.txt&hash={h}&size=4&mtime=1&mode=0"),
            b"data".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::LOCKED);
}

#[tokio::test]
async fn fence_blocks_manifest() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let token = mk_hash(b"fence2");
    hub.request(
        Method::POST,
        "/api/workspace/migration",
        "workspace_id=ws",
        vec![],
        (None, Some(&token)),
        None,
    )
    .await
    .expect("fence");
    let sid = mk_hash(b"snap");
    let h = mk_hash(b"data");
    hub.request(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=ok.txt&hash={h}&size=4&mtime=1&mode=0"),
        b"data".to_vec(),
        (None, Some(&token)),
        None,
    )
    .await
    .expect("upload");
    let r = hub
        .request(
            Method::POST,
            "/api/manifest",
            &format!("workspace_id=ws&snapshot_id={sid}"),
            format!("{h}\n").into_bytes(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::LOCKED);
}

#[tokio::test]
async fn fence_blocks_swap() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let token = mk_hash(b"fence3");
    hub.request(
        Method::POST,
        "/api/workspace/migration",
        "workspace_id=ws",
        vec![],
        (None, Some(&token)),
        None,
    )
    .await
    .expect("fence");
    let sid = mk_hash(b"snap");
    let body = serde_json::to_vec(&SwapHeadRequest {
        workspace_id: "ws".into(),
        expected: None,
        new: sid,
    })
    .unwrap();
    let r = hub
        .request(
            Method::PUT,
            "/api/head",
            "",
            body,
            (None, None),
            Some("application/json"),
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::LOCKED);
}

#[tokio::test]
async fn fence_allows_read_only() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let token = mk_hash(b"fence4");
    hub.request(
        Method::POST,
        "/api/workspace/migration",
        "workspace_id=ws",
        vec![],
        (None, Some(&token)),
        None,
    )
    .await
    .expect("fence");
    let r = hub
        .request(
            Method::GET,
            "/api/head",
            "workspace_id=ws",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::OK);
    let r = hub
        .request(
            Method::GET,
            "/api/workspace/format",
            "workspace_id=ws",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn v3_workspace_rejects_legacy_sync() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, api) = open_anon(&d).await;
    let h = mk_hash(b"data");
    let sid = mk_hash(b"snap");
    api.upload_object("ws", &h, b"data".to_vec())
        .await
        .expect("obj");
    api.upload_manifest("ws", &sid, std::slice::from_ref(&h))
        .await
        .expect("man");
    api.swap_head("ws", None, &sid).await.expect("head");
    hub.request(
        Method::POST,
        "/api/workspace/format",
        "workspace_id=ws&format_version=3",
        vec![],
        (None, None),
        None,
    )
    .await
    .expect("stamp");
    let body = serde_json::to_vec(&feanorfs_common::SyncRequest {
        workspace_id: "ws".into(),
        files: vec![],
    })
    .unwrap();
    let r = hub
        .request(
            Method::POST,
            "/api/sync/peek",
            "",
            body,
            (None, None),
            Some("application/json"),
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::UPGRADE_REQUIRED);
}

#[tokio::test]
async fn upload_rejects_unsafe_path_for_objects() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let h = mk_hash(b"data");
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!(
                "workspace_id=ws&path=../etc/passwd&hash={h}&size=4&mtime=1&mode=0&object=true"
            ),
            b"data".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_rejects_deleted_object() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let h = mk_hash(b"data");
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!(
                "workspace_id=ws&path=ok.txt&hash={h}&size=0&mtime=1&deleted=true&object=true"
            ),
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_rejects_hash_mismatch() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let wrong = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=ok.txt&hash={wrong}&size=4&mtime=1&mode=0"),
            b"data".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_hash_mismatch_leaves_no_blob() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let wrong = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    hub.request(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=ok.txt&hash={wrong}&size=4&mtime=1&mode=0"),
        b"data".to_vec(),
        (None, None),
        None,
    )
    .await
    .expect("req");
    let r = hub
        .request(
            Method::GET,
            &format!("/api/download/{wrong}"),
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_rejects_missing_numeric_params() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let h = mk_hash(b"data");
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=ok.txt&hash={h}"),
            b"data".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_computes_hash_for_empty_body() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let empty_hash = hash_bytes(b"");
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=empty.txt&hash={empty_hash}&size=0&mtime=1&mode=0&object=true"),
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn download_invalid_hash_returns_400() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(
            Method::GET,
            "/api/download/not-hex",
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn download_missing_blob_returns_404() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let hash = mk_hash(b"x");
    let r = hub
        .request(
            Method::GET,
            &format!("/api/download/{hash}"),
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_head_missing_workspace_id_returns_400() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(Method::GET, "/api/head", "", vec![], (None, None), None)
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn head_cas_rejects_invalid_ids() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let body = serde_json::to_vec(&SwapHeadRequest {
        workspace_id: "ws".into(),
        expected: None,
        new: "bad".into(),
    })
    .unwrap();
    let r = hub
        .request(
            Method::PUT,
            "/api/head",
            "",
            body,
            (None, None),
            Some("application/json"),
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn head_cas_requires_manifest() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let sid = mk_hash(b"snap");
    let body = serde_json::to_vec(&SwapHeadRequest {
        workspace_id: "ws".into(),
        expected: None,
        new: sid,
    })
    .unwrap();
    let r = hub
        .request(
            Method::PUT,
            "/api/head",
            "",
            body,
            (None, None),
            Some("application/json"),
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn head_cas_success_and_conflict_json() {
    let d = tempfile::tempdir().expect("dir");
    let (_, api) = open_anon(&d).await;
    let h = mk_hash(b"b");
    let sid1 = mk_hash(b"s1");
    let sid2 = mk_hash(b"s2");
    api.upload_object("ws", &h, b"b".to_vec())
        .await
        .expect("obj");
    api.upload_manifest("ws", &sid1, std::slice::from_ref(&h))
        .await
        .expect("m1");
    api.upload_manifest("ws", &sid2, std::slice::from_ref(&h))
        .await
        .expect("m2");
    let r1 = api.swap_head("ws", None, &sid1).await.expect("swap1");
    assert_eq!(r1, SwapHeadResult::Swapped);
    let r2 = api.swap_head("ws", None, &sid2).await.expect("swap2");
    assert!(matches!(r2, SwapHeadResult::Conflict(_)));
}

#[tokio::test]
async fn manifest_rejects_invalid_snapshot_id() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(
            Method::POST,
            "/api/manifest",
            "workspace_id=ws&snapshot_id=bad",
            b"hash\n".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn manifest_rejects_invalid_line_hash() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let sid = mk_hash(b"snap");
    let r = hub
        .request(
            Method::POST,
            "/api/manifest",
            &format!("workspace_id=ws&snapshot_id={sid}"),
            b"not-a-hash\n".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn manifest_rejects_missing_blob() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let sid = mk_hash(b"snap");
    let missing = mk_hash(b"missing");
    let r = hub
        .request(
            Method::POST,
            "/api/manifest",
            &format!("workspace_id=ws&snapshot_id={sid}"),
            format!("{missing}\n").into_bytes(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn manifest_accepts_empty_body() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let sid = mk_hash(b"snap");
    let r = hub
        .request(
            Method::POST,
            "/api/manifest",
            &format!("workspace_id=ws&snapshot_id={sid}"),
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn format_stamp_rejects_non_3() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(
            Method::POST,
            "/api/workspace/format",
            "workspace_id=ws&format_version=2",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn format_stamp_requires_manifested_head() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(
            Method::POST,
            "/api/workspace/format",
            "workspace_id=ws&format_version=3",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn format_stamp_clears_files_and_fence_atomically() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, api) = open_anon(&d).await;
    let h = mk_hash(b"data");
    let sid = mk_hash(b"snap");
    api.upload_object("ws", &h, b"data".to_vec())
        .await
        .expect("obj");
    api.upload_manifest("ws", &sid, std::slice::from_ref(&h))
        .await
        .expect("man");
    api.swap_head("ws", None, &sid).await.expect("head");
    api.upload_file(
        "ws",
        &feanorfs_common::FileState {
            path: "legacy.txt".into(),
            hash: h.clone(),
            size: 4,
            mtime: 1,
            deleted: false,
            mode: 0,
        },
        b"data".to_vec(),
    )
    .await
    .expect("flat");
    let token = mk_hash(b"fence");
    hub.request(
        Method::POST,
        "/api/workspace/migration",
        "workspace_id=ws",
        vec![],
        (None, Some(&token)),
        None,
    )
    .await
    .expect("fence");
    hub.request(
        Method::POST,
        "/api/workspace/format",
        "workspace_id=ws&format_version=3",
        vec![],
        (None, Some(&token)),
        None,
    )
    .await
    .expect("stamp");
    let body = serde_json::to_vec(&feanorfs_common::SyncRequest {
        workspace_id: "ws".into(),
        files: vec![],
    })
    .unwrap();
    let r = hub
        .request(
            Method::POST,
            "/api/sync/peek",
            "",
            body,
            (None, None),
            Some("application/json"),
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::UPGRADE_REQUIRED);
    let new_tok = mk_hash(b"after");
    let r = hub
        .request(
            Method::POST,
            "/api/workspace/migration",
            "workspace_id=ws",
            vec![],
            (None, Some(&new_tok)),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn workspace_list_excludes_manifest_only() {
    let d = tempfile::tempdir().expect("dir");
    let (_, api) = open_anon(&d).await;
    let workspaces: Vec<String> = api.get_workspaces().await.expect("list");
    assert!(workspaces.is_empty());
    let h = mk_hash(b"x");
    api.upload_object("manifest-only", &h, b"x".to_vec())
        .await
        .expect("obj");
    let sid = mk_hash(b"snap");
    api.upload_manifest("manifest-only", &sid, &[h])
        .await
        .expect("man");
    let workspaces: Vec<String> = api.get_workspaces().await.expect("list");
    assert!(!workspaces.contains(&"manifest-only".to_string()));
}

#[tokio::test]
async fn get_format_missing_workspace_id_returns_400() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let r = hub
        .request(
            Method::GET,
            "/api/workspace/format",
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn body_over_100mb_returns_413() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let big = vec![0u8; 101 * 1024 * 1024];
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            "workspace_id=ws&path=x&hash=aaaa&size=0&mtime=0&mode=0",
            big,
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn hub_db_rejects_future_schema() {
    let d = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(d.path().join("blobs")).expect("blobs dir");
    std::fs::write(
        d.path().join("hub_state.json"),
        r#"{"schema_version":99,"workspaces":{}}"#,
    )
    .expect("write");
    std::fs::write(d.path().join("hub_state.json.lock"), b"").expect("lock");
    let err = LocalHub::open(d.path().to_path_buf(), None)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("newer than supported"));
}

#[tokio::test]
async fn hub_db_rejects_malformed_json() {
    let d = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(d.path().join("blobs")).expect("blobs dir");
    std::fs::write(d.path().join("hub_state.json"), b"not json").expect("write");
    std::fs::write(d.path().join("hub_state.json.lock"), b"").expect("lock");
    let err = LocalHub::open(d.path().to_path_buf(), None)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("parse state JSON"));
}

#[tokio::test]
async fn v3_flat_upload_rejected_before_blob_write() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, api) = open_anon(&d).await;
    let h = mk_hash(b"data");
    let sid = mk_hash(b"snap");
    api.upload_object("ws", &h, b"data".to_vec())
        .await
        .expect("obj");
    api.upload_manifest("ws", &sid, std::slice::from_ref(&h))
        .await
        .expect("man");
    api.swap_head("ws", None, &sid).await.expect("head");
    hub.request(
        Method::POST,
        "/api/workspace/format",
        "workspace_id=ws&format_version=3",
        vec![],
        (None, None),
        None,
    )
    .await
    .expect("stamp");

    let flat_hash = mk_hash(b"flat");
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=ok.txt&hash={flat_hash}&size=4&mtime=1&mode=0"),
            b"flat".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::UPGRADE_REQUIRED);
    let dl = hub
        .request(
            Method::GET,
            &format!("/api/download/{flat_hash}"),
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("dl");
    assert_eq!(dl.status(), http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn malformed_upload_params_leave_no_blob() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let h = mk_hash(b"data");
    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=ok.txt&hash={h}"),
            b"data".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);
    let dl = hub
        .request(
            Method::GET,
            &format!("/api/download/{h}"),
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("dl");
    assert_eq!(dl.status(), http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn new_workspace_format_defaults_to_2() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, api) = open_anon(&d).await;
    let h = mk_hash(b"data");
    api.upload_object("ws", &h, b"data".to_vec())
        .await
        .expect("obj");
    let sid = mk_hash(b"snap");
    api.upload_manifest("ws", &sid, std::slice::from_ref(&h))
        .await
        .expect("man");

    let r = hub
        .request(
            Method::GET,
            "/api/workspace/format",
            "workspace_id=ws",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("req");
    assert_eq!(r.status(), http::StatusCode::OK);
    let body = axum::body::to_bytes(r.into_body(), 1024 * 1024)
        .await
        .expect("read");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse");
    assert_eq!(json["format_version"], serde_json::json!(2));
}

#[tokio::test]
async fn reupload_keeps_referenced_blob() {
    let d = tempfile::tempdir().expect("dir");
    let (hub, _) = open_anon(&d).await;
    let h = mk_hash(b"data");

    let r = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=ok.txt&hash={h}&size=4&mtime=1&mode=0"),
            b"data".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("upload");
    assert_eq!(r.status(), http::StatusCode::OK);
    let dl = hub
        .request(
            Method::GET,
            &format!("/api/download/{h}"),
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("dl");
    assert_eq!(dl.status(), http::StatusCode::OK);

    let r2 = hub
        .request(
            Method::POST,
            "/api/upload",
            &format!("workspace_id=ws&path=ok.txt&hash={h}&size=4&mtime=2&mode=0"),
            b"data".to_vec(),
            (None, None),
            None,
        )
        .await
        .expect("re-upload");
    assert_eq!(r2.status(), http::StatusCode::OK);
    let dl2 = hub
        .request(
            Method::GET,
            &format!("/api/download/{h}"),
            "",
            vec![],
            (None, None),
            None,
        )
        .await
        .expect("dl");
    assert_eq!(dl2.status(), http::StatusCode::OK);
}
