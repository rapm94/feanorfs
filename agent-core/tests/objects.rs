use feanorfs_agent_core::{ApiClient, ClientDb, LocalHub, ObjectStore, SyncCtx};
use feanorfs_common::{
    flat_to_tree, hash_bytes, FileState, LegacyPolicy, Snapshot, TreeEntryKind, AEAD_PREFIX_BYTE,
};
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn fresh_client_resolves_encrypted_snapshot_chain() {
    let hub_data = tempfile::tempdir().expect("create hub data");
    let client_a = tempfile::tempdir().expect("create client A");
    let client_b = tempfile::tempdir().expect("create client B");
    let hub = LocalHub::open(hub_data.path().to_path_buf(), None)
        .await
        .expect("open local hub");
    let api_a = ApiClient::local(Arc::clone(&hub), None);
    let api_b = ApiClient::local(hub, None);
    let db_a = ClientDb::new(client_a.path().join(".feanorfs"))
        .await
        .expect("open client A cache");
    let db_b = ClientDb::new(client_b.path().join(".feanorfs"))
        .await
        .expect("open client B cache");
    let ctx_a = SyncCtx::new(
        &api_a,
        &db_a,
        client_a.path(),
        "workspace",
        Some("shared-key"),
        LegacyPolicy::Reject,
    );
    let ctx_b = SyncCtx::new(
        &api_b,
        &db_b,
        client_b.path(),
        "workspace",
        Some("shared-key"),
        LegacyPolicy::Reject,
    );
    let store_a = ObjectStore::new(&ctx_a);
    let store_b = ObjectStore::new(&ctx_b);

    let path = "private/filename.txt";
    let files = HashMap::from([(
        path.to_string(),
        FileState {
            path: path.to_string(),
            hash: hash_bytes(b"file contents"),
            size: 13,
            mtime: 0,
            deleted: false,
            mode: 0,
        },
    )]);
    let bundle = flat_to_tree(&files).expect("build tree bundle");
    let encrypted_root = store_a
        .put_bundle(&bundle)
        .await
        .expect("upload tree bundle");
    let snapshot = Snapshot {
        root: encrypted_root,
        parents: Vec::new(),
        author: "agent:test".to_string(),
        created_at_ms: 1,
        message: None,
    };
    let snapshot_id = store_a
        .put_snapshot(&snapshot)
        .await
        .expect("upload snapshot");

    let restored_snapshot = store_b
        .get_snapshot(&snapshot_id)
        .await
        .expect("download snapshot");
    assert_eq!(restored_snapshot, snapshot);
    let root = store_b
        .get_tree(&restored_snapshot.root)
        .await
        .expect("download root tree");
    let directory = root.entries.first().expect("private directory");
    assert!(matches!(directory.kind, TreeEntryKind::Dir));
    let child = store_b
        .get_tree(&directory.hash)
        .await
        .expect("download child tree");
    assert_eq!(child.entries[0].name, "filename.txt");

    let remote = tokio::fs::read(hub_data.path().join("blobs").join(snapshot_id))
        .await
        .expect("read opaque snapshot blob");
    assert_eq!(remote.first(), Some(&AEAD_PREFIX_BYTE));
    assert!(!remote
        .windows("filename.txt".len())
        .any(|window| window == b"filename.txt"));
    assert!(client_b
        .path()
        .join(".feanorfs/objects")
        .read_dir()
        .expect("read client B object cache")
        .next()
        .is_some());

    let server_files = api_b
        .peek_sync(&feanorfs_common::SyncRequest {
            workspace_id: "workspace".to_string(),
            files: Vec::new(),
        })
        .await
        .expect("read flat server view");
    assert!(server_files.download_required.is_empty());
}

#[tokio::test]
async fn corrupted_cached_object_is_refetched_and_verified() {
    let hub_data = tempfile::tempdir().expect("create hub data");
    let client = tempfile::tempdir().expect("create client");
    let hub = LocalHub::open(hub_data.path().to_path_buf(), None)
        .await
        .expect("open local hub");
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
    let store = ObjectStore::new(&ctx);
    let tree = feanorfs_common::Tree::default();
    let id = store.put_tree(&tree).await.expect("upload tree");
    tokio::fs::write(
        client.path().join(".feanorfs/objects").join(&id),
        b"corrupt cache",
    )
    .await
    .expect("corrupt local cache");

    assert_eq!(store.get_tree(&id).await.expect("refetch tree"), tree);
}
