use feanorfs_agent_core::{ApiClient, ClientDb, LocalHub, SnapshotEngine, SyncCtx};
use feanorfs_common::{hash_bytes, FileState, LegacyPolicy};
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn publish_server_view_is_idempotent_and_parented() {
    let hub_data = tempfile::tempdir().expect("create hub data");
    let client = tempfile::tempdir().expect("create client");
    let hub = LocalHub::open(hub_data.path().to_path_buf(), None)
        .await
        .expect("open hub");
    let api = ApiClient::local(Arc::clone(&hub), None);
    let db = ClientDb::new(client.path().join(".feanorfs"))
        .await
        .expect("open cache");
    let ctx = SyncCtx::new(
        &api,
        &db,
        client.path(),
        "workspace",
        Some("shared-key"),
        LegacyPolicy::Reject,
    );
    let engine = SnapshotEngine::new(&ctx);
    let path = "src/main.rs";
    upload_blob(&api, b"first").await;
    let first_files = HashMap::from([(
        path.to_string(),
        FileState {
            path: path.to_string(),
            hash: hash_bytes(b"first"),
            size: 5,
            mtime: 10,
            deleted: false,
            mode: 0,
        },
    )]);

    let first = engine
        .publish_server_view(&first_files, "folder")
        .await
        .expect("publish initial head");
    let object_count = tokio::fs::read_dir(client.path().join(".feanorfs/objects"))
        .await
        .expect("read object cache");
    let object_count = collect_count(object_count).await;
    let unchanged = engine
        .publish_server_view(&first_files, "folder")
        .await
        .expect("publish unchanged head");
    assert_eq!(unchanged, first);
    let after_noop = tokio::fs::read_dir(client.path().join(".feanorfs/objects"))
        .await
        .expect("read object cache");
    assert_eq!(collect_count(after_noop).await, object_count);

    let mut second_files = first_files;
    upload_blob(&api, b"second").await;
    second_files.get_mut(path).unwrap().hash = hash_bytes(b"second");
    let second = engine
        .publish_server_view(&second_files, "folder")
        .await
        .expect("publish changed head");
    assert_ne!(second, first);
    let snapshot = engine.load_snapshot(&second).await.expect("load new head");
    assert_eq!(snapshot.parents, vec![first]);
    let restored = engine.load_files(&second).await.unwrap();
    let restored = restored.get(path).unwrap();
    let expected = second_files.get(path).unwrap();
    assert_eq!(restored.hash, expected.hash);
    assert_eq!(restored.size, expected.size);
    assert_eq!(restored.mode, expected.mode);
}

#[tokio::test]
async fn local_and_last_synced_refs_skip_unchanged_views() {
    let hub_data = tempfile::tempdir().expect("create hub data");
    let client = tempfile::tempdir().expect("create client");
    let hub = LocalHub::open(hub_data.path().to_path_buf(), None)
        .await
        .expect("open hub");
    let api = ApiClient::local(hub, None);
    let db = ClientDb::new(client.path().join(".feanorfs"))
        .await
        .expect("open cache");
    let ctx = SyncCtx::new(
        &api,
        &db,
        client.path(),
        "workspace",
        Some("shared-key"),
        LegacyPolicy::Reject,
    );
    let engine = SnapshotEngine::new(&ctx);
    let path = "file.txt";
    let files = HashMap::from([(
        path.to_string(),
        FileState {
            path: path.to_string(),
            hash: hash_bytes(b"content"),
            size: 7,
            mtime: 99,
            deleted: false,
            mode: 0,
        },
    )]);

    let first = engine.snapshot_local_view(&files, "you").await.unwrap();
    let second = engine.snapshot_local_view(&files, "you").await.unwrap();
    assert_eq!(first, second);
    let agreed = engine.record_last_synced(&files, "sync").await.unwrap();
    assert_eq!(
        engine.load_last_synced().await.unwrap()[path].hash,
        files[path].hash
    );
    assert!(client.path().join(".feanorfs/refs/workspace").is_file());
    assert!(client.path().join(".feanorfs/refs/last-synced").is_file());
    assert_ne!(agreed, "");
}

#[tokio::test]
async fn one_change_in_10k_files_reads_only_changed_tree_depth() {
    let hub_data = tempfile::tempdir().expect("create hub data");
    let client = tempfile::tempdir().expect("create client");
    let hub = LocalHub::open(hub_data.path().to_path_buf(), None)
        .await
        .expect("open hub");
    let api = ApiClient::local(hub, None);
    let db = ClientDb::new(client.path().join(".feanorfs"))
        .await
        .expect("open cache");
    let ctx = SyncCtx::new(
        &api,
        &db,
        client.path(),
        "workspace",
        Some("shared-key"),
        LegacyPolicy::Reject,
    );
    let engine = SnapshotEngine::new(&ctx);
    upload_blob(&api, b"before").await;
    let mut files = HashMap::new();
    for directory in 0..100 {
        for file in 0..100 {
            let path = format!("src/{directory:03}/{file:03}.txt");
            files.insert(
                path.clone(),
                FileState {
                    path,
                    hash: hash_bytes(b"before"),
                    size: 6,
                    mtime: 1,
                    deleted: false,
                    mode: 0,
                },
            );
        }
    }
    let base = engine
        .publish_server_view(&files, "test")
        .await
        .expect("publish base");
    files.get_mut("src/042/042.txt").unwrap().hash = hash_bytes(b"after");

    let diff = engine
        .diff_file_view(&base, &files)
        .await
        .expect("diff changed view");

    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "src/042/042.txt");
    assert!(diff.object_reads <= 4, "reads: {}", diff.object_reads);
}

#[tokio::test]
async fn directory_to_file_replacement_reports_leaf_deletes_and_file_add() {
    let hub_data = tempfile::tempdir().expect("create hub data");
    let client = tempfile::tempdir().expect("create client");
    let hub = LocalHub::open(hub_data.path().to_path_buf(), None)
        .await
        .expect("open hub");
    let api = ApiClient::local(hub, None);
    let db = ClientDb::new(client.path().join(".feanorfs"))
        .await
        .expect("open cache");
    let ctx = SyncCtx::new(
        &api,
        &db,
        client.path(),
        "workspace",
        Some("shared-key"),
        LegacyPolicy::Reject,
    );
    let engine = SnapshotEngine::new(&ctx);
    let nested = "node/child.txt";
    upload_blob(&api, b"child").await;
    let before = HashMap::from([(
        nested.to_string(),
        FileState {
            path: nested.to_string(),
            hash: hash_bytes(b"child"),
            size: 5,
            mtime: 1,
            deleted: false,
            mode: 0,
        },
    )]);
    let base = engine.publish_server_view(&before, "test").await.unwrap();
    let replacement = HashMap::from([(
        "node".to_string(),
        FileState {
            path: "node".to_string(),
            hash: hash_bytes(b"file"),
            size: 4,
            mtime: 2,
            deleted: false,
            mode: 0,
        },
    )]);

    let diff = engine.diff_file_view(&base, &replacement).await.unwrap();

    assert_eq!(
        diff.changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["node", "node/child.txt"]
    );
}

async fn collect_count(mut entries: tokio::fs::ReadDir) -> usize {
    let mut count = 0;
    while entries.next_entry().await.unwrap().is_some() {
        count += 1;
    }
    count
}

async fn upload_blob(api: &ApiClient, bytes: &[u8]) {
    let hash = hash_bytes(bytes);
    api.upload_object("workspace", &hash, bytes.to_vec())
        .await
        .unwrap();
}
