use std::sync::Arc;

use feanorfs_agent_core::LocalHub;
use feanorfs_common::{hash_bytes, SwapHeadRequest, SyncRequest};
use http::Method;
use serde_json::Value;

mod support;
use support::{spawn_test_server, TestServer};

fn mk_hash(data: &[u8]) -> String {
    hash_bytes(data)
}

/// Canonicalize JSON by sorting object keys. Non-JSON bodies pass through unchanged.
fn canonicalize(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        v.to_string()
    } else if body.is_empty() {
        String::new()
    } else {
        String::from_utf8_lossy(body).into_owned()
    }
}

struct Harness {
    local: Arc<LocalHub>,
    _local_dir: tempfile::TempDir,
    _server: TestServer,
    http_url: String,
    http_token: Option<String>,
}

impl Harness {
    async fn new(auth_token: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let hub = LocalHub::open(dir.path().to_path_buf(), auth_token.map(str::to_string))
            .await
            .unwrap();
        let srv = spawn_test_server().await;
        let url = srv.url.clone();
        Self {
            local: hub,
            _local_dir: dir,
            _server: srv,
            http_url: url,
            http_token: auth_token.map(str::to_string),
        }
    }

    async fn local_req(
        &self,
        m: &Method,
        path: &str,
        q: &str,
        body: Vec<u8>,
        migration_token: Option<&str>,
    ) -> (u16, Vec<u8>) {
        let r = self
            .local
            .request(
                m.clone(),
                path,
                q,
                body,
                (self.http_token.as_deref(), migration_token),
                None,
            )
            .await
            .unwrap();
        let s = r.status().as_u16();
        let b = axum::body::to_bytes(r.into_body(), 200 * 1024 * 1024)
            .await
            .unwrap_or_default();
        (s, b.to_vec())
    }

    async fn http_req(
        &self,
        m: Method,
        path: &str,
        q: &str,
        body: Vec<u8>,
        migration_token: Option<&str>,
    ) -> (u16, Vec<u8>) {
        let url = if q.is_empty() {
            format!("{}{path}", self.http_url)
        } else {
            format!("{}{path}?{q}", self.http_url)
        };
        let client = reqwest::Client::new();
        let mut req = client.request(m, &url).header("X-FeanorFS-Format", "3");
        if let Some(ref t) = self.http_token {
            req = req.bearer_auth(t);
        }
        if let Some(mt) = migration_token {
            req = req.header("X-FeanorFS-Migration", mt);
        }
        if !body.is_empty() {
            req = req.header("Content-Type", "application/json").body(body);
        }
        let r = req.send().await.unwrap();
        let s = r.status().as_u16();
        let b = r.bytes().await.unwrap_or_default();
        (s, b.to_vec())
    }

    /// Assert exact parity: same status and same canonicalized body.
    async fn cmp(&self, m: Method, path: &str, q: &str, body: Vec<u8>, mtok: Option<&str>) {
        let (ls, lb) = self.local_req(&m, path, q, body.clone(), mtok).await;
        let (hs, hb) = self.http_req(m.clone(), path, q, body, mtok).await;
        let nl = canonicalize(&lb);
        let nh = canonicalize(&hb);
        assert_eq!(
            ls, hs,
            "status mismatch [{m} {path}]: local={ls} http={hs}\nlocal body: {nl}\nhttp body: {nh}"
        );
        assert_eq!(
            nl, nh,
            "body mismatch [{m} {path}]: status={ls}\nlocal: {nl}\nhttp: {nh}"
        );
    }

    async fn upload_object(&self, ws: &str, data: &[u8]) -> String {
        let h = mk_hash(data);
        self.cmp(
            Method::POST,
            "/api/upload",
            &format!(
                "workspace_id={ws}&path=obj&hash={h}&size={}&mtime=0&object=true",
                data.len()
            ),
            data.to_vec(),
            None,
        )
        .await;
        h
    }

    async fn upload_manifest(&self, ws: &str, snap: &str, hashes: &[String]) {
        let body = hashes.join("\n");
        self.cmp(
            Method::POST,
            "/api/manifest",
            &format!("workspace_id={ws}&snapshot_id={snap}"),
            body.into_bytes(),
            None,
        )
        .await;
    }
}

#[tokio::test]
async fn parity_upload_object_and_download() {
    let h = Harness::new(None).await;
    let data = b"object-payload";
    let hash = mk_hash(data);
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!(
            "workspace_id=ws&path=obj&hash={hash}&size={}&mtime=0&object=true",
            data.len()
        ),
        data.to_vec(),
        None,
    )
    .await;
    h.cmp(
        Method::GET,
        &format!("/api/download/{hash}"),
        "",
        vec![],
        None,
    )
    .await;
}

#[tokio::test]
async fn parity_legacy_flat_and_tombstone() {
    let h = Harness::new(None).await;
    let body = b"flat-data";
    let hash = mk_hash(body);
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=flat.txt&hash={hash}&size=9&mtime=1&mode=0"),
        body.to_vec(),
        None,
    )
    .await;
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=flat.txt&hash={hash}&size=0&mtime=20&deleted=true"),
        vec![],
        None,
    )
    .await;
}

#[tokio::test]
async fn parity_sync_peek_and_diff() {
    let h = Harness::new(None).await;
    let hash = mk_hash(b"sync");
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=src/main.rs&hash={hash}&size=4&mtime=1&mode=0"),
        b"sync".to_vec(),
        None,
    )
    .await;
    let body = serde_json::to_vec(&SyncRequest {
        workspace_id: "ws".into(),
        files: vec![],
    })
    .unwrap();
    h.cmp(Method::POST, "/api/sync/peek", "", body.clone(), None)
        .await;
    h.cmp(Method::POST, "/api/sync/diff", "", body, None).await;
}

#[tokio::test]
async fn parity_head_cas_success_and_conflict() {
    let h = Harness::new(None).await;
    let blob = h.upload_object("ws", b"blob").await;
    let snap = mk_hash(b"snap1");
    h.upload_manifest("ws", &snap, std::slice::from_ref(&blob))
        .await;
    h.cmp(Method::GET, "/api/head", "workspace_id=ws", vec![], None)
        .await;
    let req = SwapHeadRequest {
        workspace_id: "ws".into(),
        expected: None,
        new: snap.clone(),
    };
    let body = serde_json::to_vec(&req).unwrap();
    h.cmp(Method::PUT, "/api/head", "", body.clone(), None)
        .await;
    h.cmp(Method::PUT, "/api/head", "", body, None).await;
}

#[tokio::test]
async fn parity_workspace_format_and_stamp() {
    let h = Harness::new(None).await;
    h.cmp(
        Method::GET,
        "/api/workspace/format",
        "workspace_id=ws",
        vec![],
        None,
    )
    .await;
    h.cmp(
        Method::POST,
        "/api/workspace/format",
        "workspace_id=ws&format_version=3",
        vec![],
        None,
    )
    .await;
    let blob = h.upload_object("ws", b"blob").await;
    let snap = mk_hash(b"snap");
    h.upload_manifest("ws", &snap, std::slice::from_ref(&blob))
        .await;
    let body = serde_json::to_vec(&SwapHeadRequest {
        workspace_id: "ws".into(),
        expected: None,
        new: snap,
    })
    .unwrap();
    h.cmp(Method::PUT, "/api/head", "", body, None).await;
    h.cmp(
        Method::POST,
        "/api/workspace/format",
        "workspace_id=ws&format_version=3",
        vec![],
        None,
    )
    .await;
}

#[tokio::test]
async fn parity_migration_fence_acquire_deny_release() {
    let h = Harness::new(None).await;
    let token = mk_hash(b"fence-tok");
    h.cmp(
        Method::POST,
        "/api/workspace/migration",
        "workspace_id=ws",
        vec![],
        Some(&token),
    )
    .await;
    let hash = mk_hash(b"blocked");
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=ok.txt&hash={hash}&size=7&mtime=1&mode=0"),
        b"blocked".to_vec(),
        None,
    )
    .await;
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=ok.txt&hash={hash}&size=7&mtime=1&mode=0"),
        b"blocked".to_vec(),
        Some(&mk_hash(b"wrong")),
    )
    .await;
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=ok.txt&hash={hash}&size=7&mtime=1&mode=0"),
        b"blocked".to_vec(),
        Some(&token),
    )
    .await;
    let blob_hash = mk_hash(b"cutover");
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=obj&hash={blob_hash}&size=7&mtime=0&object=true"),
        b"cutover".to_vec(),
        Some(&token),
    )
    .await;
    let snap = mk_hash(b"snap");
    h.cmp(
        Method::POST,
        "/api/manifest",
        &format!("workspace_id=ws&snapshot_id={snap}"),
        format!("{blob_hash}\n").into_bytes(),
        Some(&token),
    )
    .await;
    let body = serde_json::to_vec(&SwapHeadRequest {
        workspace_id: "ws".into(),
        expected: None,
        new: snap.clone(),
    })
    .unwrap();
    h.cmp(Method::PUT, "/api/head", "", body, Some(&token))
        .await;
    h.cmp(
        Method::POST,
        "/api/workspace/format",
        "workspace_id=ws&format_version=3",
        vec![],
        Some(&token),
    )
    .await;
    h.cmp(
        Method::GET,
        "/api/workspace/format",
        "workspace_id=ws",
        vec![],
        None,
    )
    .await;
    let hash2 = mk_hash(b"after-cut");
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=obj&hash={hash2}&size=9&mtime=0&object=true"),
        b"after-cut".to_vec(),
        None,
    )
    .await;
}

#[tokio::test]
async fn parity_manifest_valid_and_errors() {
    let h = Harness::new(None).await;
    let blob = h.upload_object("ws", b"blob").await;
    let snap = mk_hash(b"snap");
    h.cmp(
        Method::POST,
        "/api/manifest",
        &format!("workspace_id=ws&snapshot_id={snap}"),
        format!("{blob}\n").into_bytes(),
        None,
    )
    .await;
    h.cmp(
        Method::POST,
        "/api/manifest",
        "workspace_id=ws&snapshot_id=bad",
        b"hash\n".to_vec(),
        None,
    )
    .await;
    h.cmp(
        Method::POST,
        "/api/manifest",
        &format!("workspace_id=ws&snapshot_id={snap}"),
        format!("{}\n", mk_hash(b"missing")).into_bytes(),
        None,
    )
    .await;
}

#[tokio::test]
async fn parity_download_errors() {
    let h = Harness::new(None).await;
    h.cmp(Method::GET, "/api/download/../etc", "", vec![], None)
        .await;
    h.cmp(Method::GET, "/api/download/not-hex", "", vec![], None)
        .await;
    h.cmp(
        Method::GET,
        &format!("/api/download/{}", mk_hash(b"x")),
        "",
        vec![],
        None,
    )
    .await;
}

#[tokio::test]
async fn parity_workspace_list_before_and_after() {
    let h = Harness::new(None).await;
    h.cmp(Method::GET, "/api/workspaces", "", vec![], None)
        .await;
    let hash = mk_hash(b"d");
    h.cmp(
        Method::POST,
        "/api/upload",
        &format!("workspace_id=ws&path=f.txt&hash={hash}&size=1&mtime=1&mode=0"),
        b"d".to_vec(),
        None,
    )
    .await;
    h.cmp(Method::GET, "/api/workspaces", "", vec![], None)
        .await;
}

#[tokio::test]
async fn parity_body_over_100mb_rejected() {
    let h = Harness::new(None).await;
    let (_, lb_before) = h
        .local_req(&Method::GET, "/api/workspaces", "", vec![], None)
        .await;
    let (_, hb_before) = h
        .http_req(Method::GET, "/api/workspaces", "", vec![], None)
        .await;
    assert_eq!(canonicalize(&lb_before), canonicalize(&hb_before));

    let big = vec![0u8; 101 * 1024 * 1024];
    let (ls, lb_body) = h
        .local_req(
            &Method::POST,
            "/api/upload",
            "workspace_id=ws&path=x&hash=aa&size=0&mtime=0&mode=0",
            big.clone(),
            None,
        )
        .await;
    let (hs, hb_body) = h
        .http_req(
            Method::POST,
            "/api/upload",
            "workspace_id=ws&path=x&hash=aa&size=0&mtime=0&mode=0",
            big,
            None,
        )
        .await;
    assert_eq!(ls, 413);
    assert_eq!(hs, 413);
    assert_eq!(canonicalize(&lb_body), canonicalize(&hb_body));

    let (_, lb_after) = h
        .local_req(&Method::GET, "/api/workspaces", "", vec![], None)
        .await;
    let (_, hb_after) = h
        .http_req(Method::GET, "/api/workspaces", "", vec![], None)
        .await;
    assert_eq!(
        lb_before, lb_after,
        "local state changed after rejected oversize"
    );
    assert_eq!(
        hb_before, hb_after,
        "http state changed after rejected oversize"
    );
}

#[tokio::test]
async fn parity_auth_missing_wrong_correct() {
    let token = "shared-secret";
    let srv_dir = tempfile::tempdir().unwrap();
    let state = feanorfs_server::init_app_state(srv_dir.path().to_path_buf(), Some(token.into()))
        .await
        .unwrap();
    let app = feanorfs_server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let http_url = format!("http://{addr}");
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let local_dir = tempfile::tempdir().unwrap();
    let local = LocalHub::open(local_dir.path().to_path_buf(), Some(token.into()))
        .await
        .unwrap();
    let hc = reqwest::Client::new();

    let hash = mk_hash(b"auth-test");
    let q = &format!("workspace_id=ws&path=ok.txt&hash={hash}&size=9&mtime=1&mode=0&object=true");
    let body = b"auth-test".to_vec();

    async fn both(
        local: &LocalHub,
        hc: &reqwest::Client,
        http_url: &str,
        m: Method,
        q: &str,
        body: Vec<u8>,
        tok: Option<&str>,
    ) {
        let lr = local
            .request(m.clone(), "/api/upload", q, body.clone(), (tok, None), None)
            .await
            .unwrap();
        let ls = lr.status().as_u16();
        let lb = axum::body::to_bytes(lr.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let mut hr = hc
            .post(format!("{http_url}/api/upload?{q}"))
            .header("X-FeanorFS-Format", "3");
        if let Some(t) = tok {
            hr = hr.bearer_auth(t);
        }
        let hr = hr.body(body).send().await.unwrap();
        let hs = hr.status().as_u16();
        let hb = hr.bytes().await.unwrap_or_default();
        let nl = canonicalize(&lb);
        let nh = canonicalize(&hb);
        assert_eq!(
            ls, hs,
            "status mismatch for token={tok:?}: local={ls} http={hs}"
        );
        assert_eq!(
            nl, nh,
            "body mismatch for token={tok:?}: local={nl} http={nh}"
        );
    }

    both(&local, &hc, &http_url, Method::POST, q, body.clone(), None).await;
    both(
        &local,
        &hc,
        &http_url,
        Method::POST,
        q,
        body.clone(),
        Some("wrong"),
    )
    .await;
    both(
        &local,
        &hc,
        &http_url,
        Method::POST,
        q,
        body.clone(),
        Some(token),
    )
    .await;
}
