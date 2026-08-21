feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{do_sync, land_agent, spawn_agent};

use support::{
    agent_path, read_workspace_file, spawn_test_client_with_server, spawn_test_server, state_path,
    write_workspace_file, TEST_PASSWORD, WORKSPACE_ID,
};

#[tokio::test]
async fn undo_materializes_file_directory_transitions_in_both_directions() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "node", b"file").await;
    do_sync(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let file_snapshot = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    tokio::fs::remove_file(base.join("node")).await.unwrap();
    write_workspace_file(base, "node/child.txt", b"child").await;
    do_sync(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let directory_snapshot = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let ctx = feanorfs_client::SyncCtx::new(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );

    feanorfs_agent_core::history::undo(&ctx, &file_snapshot[..8])
        .await
        .unwrap();
    assert!(base.join("node").is_file());
    assert_eq!(read_workspace_file(base, "node").await, b"file");

    feanorfs_agent_core::history::undo(&ctx, &directory_snapshot[..8])
        .await
        .unwrap();
    assert!(base.join("node").is_dir());
    assert_eq!(read_workspace_file(base, "node/child.txt").await, b"child");
}

#[tokio::test]
async fn undo_rejects_missing_target_blob_before_head_swap() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "undo.txt", b"before").await;
    do_sync(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let target = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let target_blob = feanorfs_common::hash_bytes(
        &feanorfs_common::pack_bytes(b"before", TEST_PASSWORD, "undo.txt").unwrap(),
    );

    write_workspace_file(base, "undo.txt", b"after").await;
    do_sync(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let head_before = server.api.get_head(WORKSPACE_ID).await.unwrap();
    tokio::fs::remove_file(server.data_dir().join("blobs").join(target_blob))
        .await
        .unwrap();
    let ctx = feanorfs_client::SyncCtx::new(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );

    feanorfs_agent_core::history::undo(&ctx, &target[..8])
        .await
        .expect_err("missing historical bytes must fail before publication");
    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap(),
        head_before
    );
    assert_eq!(read_workspace_file(base, "undo.txt").await, b"after");
}

#[tokio::test]
async fn undo_does_not_overwrite_an_edit_made_after_head_commit() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();
    write_workspace_file(base, "undo-race.txt", b"before").await;
    do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let first_head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    write_workspace_file(base, "undo-race.txt", b"after").await;
    do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let head_before_undo = server.api.get_head(WORKSPACE_ID).await.unwrap();

    let base_path = base.to_path_buf();
    let state = state_path(&base_path);
    tokio::fs::write(state.join("test-sync-pause"), b"undo-after-cas")
        .await
        .unwrap();
    let url = server.url.clone();
    let task_base = base_path.clone();
    let task_state = state.clone();
    let selector = first_head[..8].to_string();
    let undo = tokio::spawn(async move {
        let api = feanorfs_client::ApiClient::new(&url, None);
        let db = feanorfs_client::ClientDb::new(task_state).await.unwrap();
        let ctx = feanorfs_client::SyncCtx::new(
            &api,
            &db,
            &task_base,
            WORKSPACE_ID,
            Some(TEST_PASSWORD),
            feanorfs_common::LegacyPolicy::Reject,
        );
        feanorfs_agent_core::history::undo(&ctx, &selector).await
    });
    let reached = state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    write_workspace_file(&base_path, "undo-race.txt", b"post-cas user edit").await;
    tokio::fs::remove_file(state.join("test-sync-pause"))
        .await
        .unwrap();
    let error = undo
        .await
        .unwrap()
        .expect_err("undo materialization must reject the post-CAS edit");
    assert!(format!("{error:#}").contains("changed while downloads were staged"));
    assert_eq!(
        read_workspace_file(&base_path, "undo-race.txt").await,
        b"post-cas user edit"
    );
    assert_ne!(
        server.api.get_head(WORKSPACE_ID).await.unwrap(),
        head_before_undo,
        "the test must reach the documented post-CAS recovery boundary"
    );
}

#[tokio::test]
async fn history_log_and_undo_restore_pre_land_state_without_rewriting_history() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "history.txt", b"before").await;
    do_sync(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "history",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    write_workspace_file(&agent_path(base, "history"), "history.txt", b"after").await;
    let landed = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "history",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let landed_id = landed.snapshot_id.unwrap();
    let ctx = feanorfs_client::SyncCtx::new(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    let snapshots = feanorfs_agent_core::SnapshotEngine::new(&ctx);
    let landed_snapshot = snapshots.load_snapshot(&landed_id).await.unwrap();
    let pre_land = landed_snapshot.parents.last().unwrap();

    let before_log = feanorfs_agent_core::history::log(&ctx, 10).await.unwrap();
    assert_eq!(before_log.entries[0].snapshot_id, landed_id);
    let undone = feanorfs_agent_core::history::undo(&ctx, &pre_land[..8])
        .await
        .unwrap();

    assert_eq!(undone.restored_snapshot_id, *pre_land);
    assert_ne!(undone.snapshot_id, landed_id);
    assert_eq!(read_workspace_file(base, "history.txt").await, b"before");
    let after_log = feanorfs_agent_core::history::log(&ctx, 10).await.unwrap();
    assert_eq!(after_log.entries[0].snapshot_id, undone.snapshot_id);
    assert_eq!(after_log.entries[0].parents[0], landed_id);
    assert_eq!(after_log.entries[0].parents.len(), 2);
}

#[tokio::test]
async fn pruning_hard_excluded_metadata_preserves_local_bytes_and_clears_conflicts() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();
    let path = ".jj/repo/store";
    let content = b"local jujutsu metadata must survive";
    write_workspace_file(base, path, content).await;
    let encrypted = feanorfs_common::pack_bytes(content, TEST_PASSWORD, path).unwrap();
    let encrypted_hash = feanorfs_common::hash_bytes(&encrypted);
    let state = feanorfs_common::FileState {
        path: path.into(),
        hash: encrypted_hash.clone(),
        size: content.len() as u64,
        mtime: 1,
        deleted: false,
        mode: 0,
    };
    // Seed state that an older hub accepted before portable-path validation.
    // The public upload route correctly rejects this path now, while pruning
    // must still retire already-persisted legacy metadata safely.
    tokio::fs::write(
        server.data_dir().join("blobs").join(&encrypted_hash),
        encrypted,
    )
    .await
    .unwrap();
    server.db.upsert_file(WORKSPACE_ID, &state).await.unwrap();
    client
        .db
        .upsert_cache_entry(&feanorfs_client::local::CacheEntry {
            path: path.into(),
            plaintext_hash: feanorfs_common::hash_bytes(content),
            encrypted_hash,
            size: content.len() as u64,
            mtime: 1,
            server_mtime: 1,
            mode: 0,
            hydrated: true,
            deleted_at: None,
        })
        .await
        .unwrap();
    let conflict_dir = state_path(base).join("conflicts/1");
    std::fs::create_dir_all(&conflict_dir).unwrap();
    client
        .db
        .upsert_conflict(
            path,
            &feanorfs_common::ConflictKind::EditEdit,
            &conflict_dir.to_string_lossy(),
            1,
            feanorfs_agent_core::ConflictRecordStatus::Pending,
        )
        .await
        .unwrap();

    let result = feanorfs_client::prune_ignored(&server.api, &client.db, base, WORKSPACE_ID, false)
        .await
        .unwrap();

    assert_eq!(result.pruned, vec![path.to_string()]);
    assert_eq!(read_workspace_file(base, path).await, content);
    assert!(!client
        .db
        .get_cache_entries()
        .await
        .unwrap()
        .contains_key(path));
    assert!(client
        .db
        .list_pending_conflict_paths()
        .await
        .unwrap()
        .is_empty());
    assert!(server
        .api
        .peek_sync(&feanorfs_common::SyncRequest {
            workspace_id: WORKSPACE_ID.into(),
            files: Vec::new(),
        })
        .await
        .unwrap()
        .download_required
        .is_empty());
}
