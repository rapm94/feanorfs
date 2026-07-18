mod support;

use feanorfs_client::{
    commit_agent, do_pull_only, do_push_only, do_status, do_sync, land_agent, spawn_agent,
};
use support::{
    read_workspace_file, spawn_test_client, spawn_test_client_with_server, spawn_test_server,
    write_workspace_file, TEST_PASSWORD, WORKSPACE_ID,
};

#[tokio::test]
async fn push_uploads_new_file_to_server() {
    let server = spawn_test_server().await;
    let client = spawn_test_client().await;
    let base = client.workspace.path();

    write_workspace_file(base, "hello.txt", b"hello world").await;

    let result = do_push_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    assert_eq!(result.uploads, 1);
    assert_eq!(result.deletes, 0);
    assert!(!result.remote_updates_available);
}

#[tokio::test]
async fn pull_downloads_file_pushed_by_another_client() {
    let server = spawn_test_server().await;

    let uploader = spawn_test_client().await;
    write_workspace_file(uploader.workspace.path(), "shared.txt", b"shared payload").await;
    let pushed = do_push_only(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(pushed.uploads, 1);
    let downloader = spawn_test_client().await;
    let result = do_pull_only(
        &server.api,
        &downloader.db,
        downloader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert_eq!(result.downloads, 1);
    assert_eq!(
        read_workspace_file(downloader.workspace.path(), "shared.txt").await,
        b"shared payload"
    );
}

#[tokio::test]
async fn pull_does_not_advance_base_for_pending_local_edit() {
    let server = spawn_test_server().await;
    let client = spawn_test_client().await;
    let base = client.workspace.path();

    write_workspace_file(base, "directional.txt", b"base").await;
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

    write_workspace_file(base, "directional.txt", b"local edit after pull").await;
    let pulled = do_pull_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(pulled.downloads, 0);
    assert_eq!(
        read_workspace_file(base, "directional.txt").await,
        b"local edit after pull"
    );

    let synced = do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(synced.uploads, 1);
    assert_eq!(synced.downloads, 0);
    assert_eq!(
        read_workspace_file(base, "directional.txt").await,
        b"local edit after pull"
    );
}

#[tokio::test]
async fn agreed_delete_snapshot_absence_and_remote_recreation_downloads() {
    let server = spawn_test_server().await;
    let client = spawn_test_client().await;
    let base = client.workspace.path();

    write_workspace_file(base, "recreated.txt", b"original").await;
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
    tokio::fs::remove_file(base.join("recreated.txt"))
        .await
        .unwrap();
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

    let ctx = feanorfs_client::SyncCtx::new(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    let deletion_base = feanorfs_client::conflicts::load_last_synced_snapshot(&ctx)
        .await
        .unwrap();
    assert!(!deletion_base.contains_key("recreated.txt"));

    let recreator = spawn_test_client().await;
    write_workspace_file(
        recreator.workspace.path(),
        "recreated.txt",
        b"remote recreation",
    )
    .await;
    do_push_only(
        &server.api,
        &recreator.db,
        recreator.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let synced = do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(synced.downloads, 1);
    assert_eq!(
        read_workspace_file(base, "recreated.txt").await,
        b"remote recreation"
    );
}

#[tokio::test]
async fn push_leaves_remote_only_edit_pending() {
    let server = spawn_test_server().await;
    let client = spawn_test_client().await;
    let remote_editor = spawn_test_client().await;
    let base = client.workspace.path();

    write_workspace_file(base, "remote-edit.txt", b"base").await;
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
    do_sync(
        &server.api,
        &remote_editor.db,
        remote_editor.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    write_workspace_file(
        remote_editor.workspace.path(),
        "remote-edit.txt",
        b"remote edit",
    )
    .await;
    do_sync(
        &server.api,
        &remote_editor.db,
        remote_editor.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let pushed = do_push_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(pushed.uploads, 0);
    assert!(pushed.remote_updates_available);
    assert_eq!(read_workspace_file(base, "remote-edit.txt").await, b"base");

    let synced = do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(synced.downloads, 1);
    assert_eq!(
        read_workspace_file(base, "remote-edit.txt").await,
        b"remote edit"
    );
}

#[tokio::test]
async fn sync_is_idempotent_after_upload() {
    let server = spawn_test_server().await;
    let client = spawn_test_client().await;
    let base = client.workspace.path();

    write_workspace_file(base, "note.txt", b"sync me").await;
    do_push_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let result = do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert_eq!(result.uploads, 0);
    assert_eq!(result.downloads, 0);
    assert_eq!(result.placeholders, 0);
}

#[tokio::test]
async fn bulk_touch_sync_advances_local_ref_once_and_idle_writes_zero_objects() {
    use feanorfs_agent_core::{SnapshotEngine, SyncCtx};
    use feanorfs_common::LegacyPolicy;

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();
    for index in 0..20 {
        write_workspace_file(base, &format!("bulk/{index}.txt"), b"before").await;
    }
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
    let first_ref = std::fs::read_to_string(base.join(".feanorfs/refs/workspace")).unwrap();
    for index in 0..20 {
        write_workspace_file(base, &format!("bulk/{index}.txt"), b"after").await;
    }

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
    let second_ref = std::fs::read_to_string(base.join(".feanorfs/refs/workspace")).unwrap();
    assert_ne!(second_ref, first_ref);
    let ctx = SyncCtx::new(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        LegacyPolicy::Reject,
    );
    let snapshot = SnapshotEngine::new(&ctx)
        .load_snapshot(second_ref.trim())
        .await
        .unwrap();
    assert_eq!(snapshot.parents, vec![first_ref.trim().to_string()]);
    let object_count = std::fs::read_dir(base.join(".feanorfs/objects"))
        .unwrap()
        .count();

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
    assert_eq!(
        std::fs::read_to_string(base.join(".feanorfs/refs/workspace")).unwrap(),
        second_ref
    );
    assert_eq!(
        std::fs::read_dir(base.join(".feanorfs/objects"))
            .unwrap()
            .count(),
        object_count
    );
}

#[tokio::test]
async fn pull_lazy_creates_zero_byte_placeholder() {
    let server = spawn_test_server().await;

    let uploader = spawn_test_client().await;
    write_workspace_file(uploader.workspace.path(), "lazy.txt", b"lazy content").await;
    do_push_only(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let lazy_client = spawn_test_client().await;
    let result = do_pull_only(
        &server.api,
        &lazy_client.db,
        lazy_client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        true,
    )
    .await
    .unwrap();

    assert_eq!(result.placeholders, 1);
    assert_eq!(result.downloads, 0);
    let placeholder = read_workspace_file(lazy_client.workspace.path(), "lazy.txt").await;
    assert!(placeholder.is_empty());
}

#[tokio::test]
async fn agent_commit_detects_concurrent_edit() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    write_workspace_file(base, "doc.txt", b"base version").await;
    do_push_only(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "ci1",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    write_workspace_file(
        &base.join(".feanorfs/agents/ci1"),
        "doc.txt",
        b"agent version",
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    write_workspace_file(base, "doc.txt", b"server version").await;
    do_push_only(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let commit = commit_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "ci1",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    assert_eq!(commit.conflicts.len(), 1);
    assert_eq!(commit.conflicts[0].path, "doc.txt");
    assert!(commit.conflicts[0].base.is_some());
    assert!(commit.conflicts[0].ours.is_some());
    assert!(commit.conflicts[0].theirs.is_some());
    assert!(commit.our_changes.is_empty());
}

#[tokio::test]
async fn agent_conflict_snapshot_roundtrips_through_second_client() {
    use feanorfs_agent_core::{ObjectStore, SnapshotEngine, SyncCtx};
    use feanorfs_common::{LegacyPolicy, TreeEntryKind};

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    write_workspace_file(base, "conflict.txt", b"base").await;
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
    do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
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
        "portable-conflict",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    write_workspace_file(
        &base.join(".feanorfs/agents/portable-conflict"),
        "conflict.txt",
        b"agent edit",
    )
    .await;
    write_workspace_file(second.workspace.path(), "conflict.txt", b"folder edit").await;
    do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let result = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "portable-conflict",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(result.conflicts.len(), 1);
    let head = result.snapshot_id.expect("land snapshot id");
    let ctx = SyncCtx::new(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        LegacyPolicy::Reject,
    );
    let snapshot = SnapshotEngine::new(&ctx)
        .load_snapshot(&head)
        .await
        .expect("download conflict snapshot");
    let root = ObjectStore::new(&ctx)
        .get_tree(&snapshot.root)
        .await
        .expect("download conflict root");
    assert!(matches!(
        root.entries[0].kind,
        TreeEntryKind::Conflict { .. }
    ));

    let main_ctx = SyncCtx::new(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        LegacyPolicy::Reject,
    );
    feanorfs_agent_core::resolve_conflict(
        &main_ctx,
        "conflict.txt",
        feanorfs_agent_core::ResolveKeep::Local,
        None,
    )
    .await
    .expect("resolve conflict into new snapshot");
    let resolved_head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let resolved_snapshot = SnapshotEngine::new(&ctx)
        .load_snapshot(&resolved_head)
        .await
        .unwrap();
    let resolved_root = ObjectStore::new(&ctx)
        .get_tree(&resolved_snapshot.root)
        .await
        .unwrap();
    assert!(matches!(resolved_root.entries[0].kind, TreeEntryKind::File));
}

#[tokio::test]
async fn agent_greenfield_spawn_land_new_file() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    let copied = spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "green",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(copied, 0);

    write_workspace_file(
        &base.join(".feanorfs/agents/green"),
        "task.txt",
        b"new work",
    )
    .await;

    let land = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "green",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    assert!(
        land.landed.iter().any(|p| p.path == "task.txt"),
        "expected task.txt landed: {:?}",
        land.landed
    );
    assert_eq!(read_workspace_file(base, "task.txt").await, b"new work");
}

#[tokio::test]
async fn agent_spawn_replace_restores_original_workspace_on_failure() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "replace",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    write_workspace_file(&base.join(".feanorfs/agents/replace"), "task.txt", b"old").await;
    tokio::fs::write(
        base.join(".feanorfs/test-spawn-failpoint-replace"),
        b"after-stage",
    )
    .await
    .unwrap();

    let error = spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "replace",
        Some(TEST_PASSWORD),
        false,
        true,
    )
    .await
    .expect_err("replace spawn should fail at the injected failpoint");

    assert!(error.to_string().contains("injected agent spawn failure"));
    let agent_dir = base.join(".feanorfs/agents/replace");
    assert!(
        tokio::fs::try_exists(&agent_dir).await.unwrap(),
        "restored agent directory missing"
    );
    assert_eq!(read_workspace_file(&agent_dir, "task.txt").await, b"old");
}

#[tokio::test]
async fn agent_land_pre_sync_detects_no_base_add_add() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "add-add",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    write_workspace_file(base, "shared.txt", b"folder version").await;
    write_workspace_file(
        &base.join(".feanorfs/agents/add-add"),
        "shared.txt",
        b"agent version",
    )
    .await;

    let result = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "add-add",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].path, "shared.txt");
    assert!(result.conflicts[0].base.is_none());
    assert_eq!(
        read_workspace_file(base, "shared.txt").await,
        b"folder version"
    );
}

#[tokio::test]
async fn agent_land_surfaces_rename_vs_unsynced_folder_edit() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    write_workspace_file(base, "old.txt", b"base").await;
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
        "rename",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    let agent_base = base.join(".feanorfs/agents/rename");
    tokio::fs::remove_file(agent_base.join("old.txt"))
        .await
        .unwrap();
    write_workspace_file(&agent_base, "new.txt", b"base").await;
    write_workspace_file(base, "old.txt", b"folder edit").await;

    let result = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "rename",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    let conflict = result
        .conflicts
        .iter()
        .find(|conflict| conflict.path == "old.txt")
        .expect("rename deletion must conflict with folder edit");
    assert_eq!(
        conflict.kind,
        Some(feanorfs_common::ConflictKind::DeleteEdit)
    );
    assert_eq!(read_workspace_file(base, "old.txt").await, b"folder edit");
    assert_eq!(read_workspace_file(base, "new.txt").await, b"base");
}

#[tokio::test]
async fn agent_land_retry_converges_after_content_reached_server() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    write_workspace_file(base, "retry.txt", b"base").await;
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
        "retry",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    let agent_base = base.join(".feanorfs/agents/retry");
    let content = b"agent result";
    write_workspace_file(&agent_base, "retry.txt", content).await;
    let packed = feanorfs_common::pack_bytes(content, TEST_PASSWORD, "retry.txt").unwrap();
    let hash = feanorfs_common::hash_bytes(&packed);
    let ctx = feanorfs_client::SyncCtx::new(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    let base_state = feanorfs_client::conflicts::load_last_synced_snapshot(&ctx)
        .await
        .unwrap()
        .remove("retry.txt")
        .unwrap();
    server
        .api
        .upload_file(
            WORKSPACE_ID,
            &feanorfs_common::FileState {
                path: "retry.txt".to_string(),
                hash,
                size: content.len() as u64,
                mtime: base_state.mtime.saturating_add(1),
                deleted: false,
                mode: base_state.mode,
            },
            packed,
        )
        .await
        .unwrap();

    let first_retry = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "retry",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(first_retry.conflicts.is_empty());
    assert_eq!(read_workspace_file(base, "retry.txt").await, content);

    let second_retry = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "retry",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(second_retry.conflicts.is_empty());
    assert!(
        second_retry.landed.is_empty(),
        "second retry should be idle: {second_retry:?}"
    );
    assert_eq!(second_retry.message, "Nothing to land.");
}

#[tokio::test]
async fn agent_land_converges_after_each_commit_boundary_failure() {
    for (index, point) in ["after-stage", "after-cas", "after-materialize"]
        .into_iter()
        .enumerate()
    {
        let server = spawn_test_server().await;
        let main = spawn_test_client_with_server(&server).await;
        let base = main.workspace.path();
        let name = format!("crash-{index}");
        write_workspace_file(base, "recover.txt", b"base").await;
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
            &name,
            Some(TEST_PASSWORD),
            false,
            false,
        )
        .await
        .unwrap();
        write_workspace_file(
            &base.join(".feanorfs/agents").join(&name),
            "recover.txt",
            b"agent result",
        )
        .await;
        tokio::fs::write(
            base.join(".feanorfs")
                .join(format!("test-land-failpoint-{name}")),
            point,
        )
        .await
        .unwrap();

        let error = land_agent(
            base,
            &main.db,
            &server.api,
            WORKSPACE_ID,
            &name,
            Some(TEST_PASSWORD),
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("injected agent land failure"));

        let result = land_agent(
            base,
            &main.db,
            &server.api,
            WORKSPACE_ID,
            &name,
            Some(TEST_PASSWORD),
            false,
            false,
        )
        .await
        .unwrap();
        assert!(result.conflicts.is_empty());
        assert_eq!(
            read_workspace_file(base, "recover.txt").await,
            b"agent result"
        );
        let head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
        assert_eq!(
            tokio::fs::read_to_string(
                base.join(".feanorfs/agents")
                    .join(&name)
                    .join(".feanorfs/base-snapshot")
            )
            .await
            .unwrap(),
            head
        );
        let ctx = feanorfs_client::SyncCtx::new(
            &server.api,
            &main.db,
            base,
            WORKSPACE_ID,
            Some(TEST_PASSWORD),
            feanorfs_common::LegacyPolicy::Reject,
        );
        let committed = feanorfs_agent_core::SnapshotEngine::new(&ctx)
            .load_files(&head)
            .await
            .unwrap();
        let flat = feanorfs_client::conflicts::load_server_view(&ctx)
            .await
            .unwrap();
        assert_eq!(committed["recover.txt"].hash, flat["recover.txt"].hash);
    }
}

#[tokio::test]
async fn concurrent_disjoint_agent_lands_recompute_after_head_race() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "base.txt", b"base").await;
    do_sync(
        &server.api,
        &first.db,
        first.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    spawn_agent(
        first.workspace.path(),
        &first.db,
        &server.api,
        WORKSPACE_ID,
        "first",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    spawn_agent(
        second.workspace.path(),
        &second.db,
        &server.api,
        WORKSPACE_ID,
        "second",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    write_workspace_file(
        &first.workspace.path().join(".feanorfs/agents/first"),
        "first.txt",
        b"one",
    )
    .await;
    write_workspace_file(
        &second.workspace.path().join(".feanorfs/agents/second"),
        "second.txt",
        b"two",
    )
    .await;

    let (first_result, second_result) = tokio::join!(
        land_agent(
            first.workspace.path(),
            &first.db,
            &server.api,
            WORKSPACE_ID,
            "first",
            Some(TEST_PASSWORD),
            false,
            false,
        ),
        land_agent(
            second.workspace.path(),
            &second.db,
            &server.api,
            WORKSPACE_ID,
            "second",
            Some(TEST_PASSWORD),
            false,
            false,
        )
    );
    first_result.unwrap();
    second_result.unwrap();

    let head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let ctx = feanorfs_client::SyncCtx::new(
        &server.api,
        &first.db,
        first.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    let committed = feanorfs_agent_core::SnapshotEngine::new(&ctx)
        .load_files(&head)
        .await
        .unwrap();
    assert!(committed.contains_key("first.txt"));
    assert!(committed.contains_key("second.txt"));
}

#[tokio::test]
async fn agent_refresh_pulls_remote_additions_without_touching_agent_edits() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "kept.txt", b"base").await;
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
        "refresh-add",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let agent = base.join(".feanorfs/agents/refresh-add");
    write_workspace_file(&agent, "kept.txt", b"agent edit").await;
    write_workspace_file(base, "added.txt", b"remote addition").await;
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

    let result = feanorfs_client::refresh_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "refresh-add",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    assert_eq!(result.refreshed, vec!["added.txt"]);
    assert_eq!(
        read_workspace_file(&agent, "added.txt").await,
        b"remote addition"
    );
    assert_eq!(read_workspace_file(&agent, "kept.txt").await, b"agent edit");
}

#[tokio::test]
async fn agent_refresh_replace_retains_pre_operation_snapshot_parent() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "replace.txt", b"base").await;
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
        "replace",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let agent = base.join(".feanorfs/agents/replace");
    write_workspace_file(&agent, "replace.txt", b"agent draft").await;
    write_workspace_file(base, "replace.txt", b"current head").await;
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

    feanorfs_client::refresh_agent_with_options(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "replace",
        Some(TEST_PASSWORD),
        feanorfs_client::RefreshOptions { replace: true },
    )
    .await
    .unwrap();

    assert_eq!(
        read_workspace_file(&agent, "replace.txt").await,
        b"current head"
    );
    let refreshed_id = tokio::fs::read_to_string(agent.join(".feanorfs/base-snapshot"))
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
    let snapshots = feanorfs_agent_core::SnapshotEngine::new(&ctx);
    let refreshed = snapshots.load_snapshot(refreshed_id.trim()).await.unwrap();
    let before = snapshots.load_files(&refreshed.parents[0]).await.unwrap();
    let packed = feanorfs_common::pack_bytes(b"agent draft", TEST_PASSWORD, "replace.txt").unwrap();
    assert_eq!(
        before["replace.txt"].hash,
        feanorfs_common::hash_bytes(&packed)
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
    write_workspace_file(
        &base.join(".feanorfs/agents/history"),
        "history.txt",
        b"after",
    )
    .await;
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
async fn clock_skew_uses_hash_direction_and_warns_for_one_path_rollback() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();

    write_workspace_file(base, "clock.txt", b"agreed").await;
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
    let ctx = feanorfs_client::SyncCtx::new(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    let agreed = feanorfs_client::conflicts::load_last_synced_snapshot(&ctx)
        .await
        .unwrap()
        .remove("clock.txt")
        .unwrap();
    let stale_content = b"restored server backup";
    let packed = feanorfs_common::pack_bytes(stale_content, TEST_PASSWORD, "clock.txt").unwrap();
    let hash = feanorfs_common::hash_bytes(&packed);
    server
        .api
        .upload_file(
            WORKSPACE_ID,
            &feanorfs_common::FileState {
                path: "clock.txt".to_string(),
                hash,
                size: stale_content.len() as u64,
                mtime: agreed.mtime.saturating_sub(10_000),
                deleted: false,
                mode: agreed.mode,
            },
            packed,
        )
        .await
        .unwrap();

    let status = do_status(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(status.download_required.len(), 1);
    assert_eq!(status.download_required[0].path, "clock.txt");
    assert!(status.server_rollback_warning.is_some());

    let pushed = do_push_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(
        pushed.uploads, 1,
        "push must restore the regressed server path"
    );
    assert!(
        !pushed.remote_updates_available,
        "no remote-only edits remain after rollback restore"
    );
    assert_eq!(read_workspace_file(base, "clock.txt").await, b"agreed");

    let status_after_push = do_status(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(
        status_after_push.server_rollback_warning.is_none(),
        "rollback warning must clear after push restores agreed bytes"
    );

    let synced = do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        synced.downloads, 0,
        "full sync after rollback restore must be idle"
    );
    assert_eq!(read_workspace_file(base, "clock.txt").await, b"agreed");
}

#[tokio::test]
async fn local_edit_with_skewed_clock_uploads_based_on_hash_not_mtime() {
    use std::time::{Duration, SystemTime};

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();

    write_workspace_file(base, "skew.txt", b"base").await;
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

    let new_content = b"true local edit under clock skew";
    write_workspace_file(base, "skew.txt", new_content).await;
    let past = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(3600 * 24))
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(base.join("skew.txt"))
        .unwrap()
        .set_modified(past)
        .unwrap();

    let synced = do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        synced.uploads, 1,
        "local edit must upload even when mtime is lower than server"
    );
    assert_eq!(synced.downloads, 0);
    assert_eq!(read_workspace_file(base, "skew.txt").await, new_content);

    let synced2 = do_sync(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(synced2.uploads, 0);
    assert_eq!(synced2.downloads, 0);
}

#[tokio::test]
async fn sync_detects_concurrent_workspace_edit_without_silent_overwrite() {
    use feanorfs_client::conflicts;

    let server = spawn_test_server().await;
    let client_a = spawn_test_client_with_server(&server).await;
    let client_b = spawn_test_client_with_server(&server).await;
    let base_a = client_a.workspace.path();
    for workspace in [client_a.workspace.path(), client_b.workspace.path()] {
        let mut config = feanorfs_client::load_config(workspace).unwrap();
        config.format_version = 3;
        feanorfs_client::save_config(workspace, &config).unwrap();
    }

    write_workspace_file(base_a, "notes.txt", b"base").await;
    do_push_only(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    do_sync(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &client_b.db,
        client_b.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    write_workspace_file(base_a, "notes.txt", b"offline edit A").await;
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    write_workspace_file(client_b.workspace.path(), "notes.txt", b"offline edit B").await;
    do_sync(
        &server.api,
        &client_b.db,
        client_b.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    do_sync(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert_eq!(
        read_workspace_file(base_a, "notes.txt").await,
        b"offline edit A"
    );
    let pending = conflicts::pending_conflict_paths(&client_a.db)
        .await
        .unwrap();
    assert!(conflicts::conflicts_pending(Some(&pending)));
    assert!(pending.contains("notes.txt"));

    let config_a = feanorfs_client::load_config(base_a).unwrap();
    let ctx = feanorfs_client::SyncCtx::from_config(&server.api, &client_a.db, base_a, &config_a)
        .unwrap();
    conflicts::resolve_conflict(&ctx, "notes.txt", conflicts::ResolveKeep::Both, None)
        .await
        .unwrap();
    let verifier = spawn_test_client_with_server(&server).await;
    let mut verifier_config = feanorfs_client::load_config(verifier.workspace.path()).unwrap();
    verifier_config.format_version = 3;
    feanorfs_client::save_config(verifier.workspace.path(), &verifier_config).unwrap();
    let result = do_pull_only(
        &server.api,
        &verifier.db,
        verifier.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(result.downloads, 2);
    let mut contents = Vec::new();
    for entry in std::fs::read_dir(verifier.workspace.path()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            contents.push(std::fs::read(entry.path()).unwrap());
        }
    }
    contents.sort();
    assert_eq!(
        contents,
        vec![b"offline edit A".to_vec(), b"offline edit B".to_vec()]
    );
}

#[tokio::test]
async fn concurrent_delete_is_not_a_workspace_conflict() {
    use feanorfs_client::conflicts;

    let server = spawn_test_server().await;
    let client_a = spawn_test_client().await;
    let client_b = spawn_test_client().await;
    let base_a = client_a.workspace.path();
    let base_b = client_b.workspace.path();

    write_workspace_file(base_a, "gone.txt", b"bye").await;
    do_push_only(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &client_b.db,
        base_b,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    tokio::fs::remove_file(base_a.join("gone.txt"))
        .await
        .unwrap();
    tokio::fs::remove_file(base_b.join("gone.txt"))
        .await
        .unwrap();

    do_sync(
        &server.api,
        &client_b.db,
        base_b,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert!(!conflicts::conflicts_pending(Some(
        &conflicts::pending_conflict_paths(&client_a.db)
            .await
            .unwrap()
    )));
    assert!(!base_a.join("gone.txt").exists());
}

#[tokio::test]
async fn gitignored_file_is_synced() {
    let server = spawn_test_server().await;
    let uploader = spawn_test_client().await;
    let base = uploader.workspace.path();

    write_workspace_file(base, ".gitignore", b"secret.env\n").await;
    write_workspace_file(base, "secret.env", b"SUPER_SECRET=1").await;

    let result = do_push_only(
        &server.api,
        &uploader.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    assert_eq!(
        result.uploads, 2,
        "both .gitignore and secret.env must upload"
    );

    let downloader = spawn_test_client().await;
    do_pull_only(
        &server.api,
        &downloader.db,
        downloader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        read_workspace_file(downloader.workspace.path(), "secret.env").await,
        b"SUPER_SECRET=1",
        "git-ignored file must arrive on the other side"
    );
}

#[tokio::test]
async fn agent_land_applies_clean_changes() {
    use feanorfs_client::land_agent;

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    write_workspace_file(base, "land.txt", b"base").await;
    do_push_only(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
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
        "land1",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    write_workspace_file(&base.join(".feanorfs/agents/land1"), "land.txt", b"landed").await;

    let result = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "land1",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    assert!(!result.landed.is_empty());
    assert_eq!(read_workspace_file(base, "land.txt").await, b"landed");
}

#[tokio::test]
async fn empty_file_roundtrips() {
    let server = spawn_test_server().await;
    let client = spawn_test_client().await;
    let base = client.workspace.path();

    write_workspace_file(base, "empty.txt", b"").await;
    do_push_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let other = spawn_test_client().await;
    do_pull_only(
        &server.api,
        &other.db,
        other.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert_eq!(
        read_workspace_file(other.workspace.path(), "empty.txt").await,
        b""
    );
}

#[cfg(unix)]
#[tokio::test]
async fn executable_intent_roundtrips_across_clients() {
    use std::os::unix::fs::PermissionsExt;

    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    let path = first.workspace.path().join("run.sh");
    write_workspace_file(first.workspace.path(), "run.sh", b"#!/bin/sh\n").await;
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();

    do_sync(
        &server.api,
        &first.db,
        first.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let mode = std::fs::metadata(second.workspace.path().join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0);
}

#[tokio::test]
async fn wrong_encryption_key_fails_decrypt() {
    use feanorfs_client::load_config;

    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    let mut uploader_config = load_config(uploader.workspace.path()).unwrap();
    uploader_config.format_version = 3;
    feanorfs_client::save_config(uploader.workspace.path(), &uploader_config).unwrap();
    write_workspace_file(uploader.workspace.path(), "secret.txt", b"data").await;
    do_push_only(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let wrong = spawn_test_client_with_server(&server).await;
    let mut cfg = load_config(wrong.workspace.path()).unwrap();
    cfg.encryption_password = Some("wrong-key-that-is-not-the-same-as-test-password!!".into());
    cfg.format_version = 3;
    feanorfs_client::save_config(wrong.workspace.path(), &cfg).unwrap();

    let err = do_pull_only(
        &server.api,
        &wrong.db,
        wrong.workspace.path(),
        WORKSPACE_ID,
        cfg.encryption_password.as_deref(),
        false,
    )
    .await;
    assert!(err.is_err());
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("wrong encryption key") || msg.contains("decryption"),
        "expected friendly decrypt error, got: {msg}"
    );
}

#[tokio::test]
async fn migrate_sets_format_v3_and_roundtrips_snapshot_state() {
    use feanorfs_client::{load_config, migrate_workspace};

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();
    write_workspace_file(base, "mig.txt", b"migrate me").await;
    do_push_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    migrate_workspace(base, false).await.unwrap();
    let cfg = load_config(base).unwrap();
    assert_eq!(cfg.format_version, 3);

    let verifier = spawn_test_client_with_server(&server).await;
    migrate_workspace(verifier.workspace.path(), false)
        .await
        .unwrap();
    let verifier_cfg = load_config(verifier.workspace.path()).unwrap();
    assert_eq!(verifier_cfg.format_version, 3);
    assert_eq!(
        read_workspace_file(verifier.workspace.path(), "mig.txt").await,
        b"migrate me"
    );
    let flat_before = server
        .api
        .peek_sync(&feanorfs_common::SyncRequest {
            workspace_id: WORKSPACE_ID.to_string(),
            files: Vec::new(),
        })
        .await;
    assert!(flat_before.is_err());
    write_workspace_file(verifier.workspace.path(), "mig.txt", b"snapshot only").await;
    let pushed = do_push_only(
        &server.api,
        &verifier.db,
        verifier.workspace.path(),
        WORKSPACE_ID,
        verifier_cfg.encryption_password.as_deref(),
    )
    .await
    .unwrap();
    assert_eq!(pushed.uploads, 1);
    let head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let verifier_ctx = feanorfs_client::SyncCtx::from_config(
        &server.api,
        &verifier.db,
        verifier.workspace.path(),
        &verifier_cfg,
    )
    .unwrap();
    let head_files = feanorfs_agent_core::SnapshotEngine::new(&verifier_ctx)
        .load_files(&head)
        .await
        .unwrap();
    let expected = feanorfs_common::pack_bytes(b"snapshot only", TEST_PASSWORD, "mig.txt").unwrap();
    assert_eq!(
        head_files["mig.txt"].hash,
        feanorfs_common::hash_bytes(&expected)
    );
    let pulled = do_pull_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        cfg.encryption_password.as_deref(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(pulled.downloads, 1);
    assert_eq!(read_workspace_file(base, "mig.txt").await, b"snapshot only");
    let flat_after = server
        .api
        .peek_sync(&feanorfs_common::SyncRequest {
            workspace_id: WORKSPACE_ID.to_string(),
            files: Vec::new(),
        })
        .await;
    assert!(flat_after.is_err());
}

#[tokio::test]
async fn migrate_rekeys_before_committing_format_v3() {
    use feanorfs_client::{load_config, migrate_workspace};

    let server = spawn_test_server().await;
    let source = spawn_test_client_with_server(&server).await;
    write_workspace_file(source.workspace.path(), "secret.txt", b"rekeyed data").await;
    do_push_only(
        &server.api,
        &source.db,
        source.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    migrate_workspace(source.workspace.path(), true)
        .await
        .unwrap();
    let source_config = load_config(source.workspace.path()).unwrap();
    let new_key = source_config.encryption_password.clone().unwrap();
    assert_eq!(new_key.len(), 64);
    assert_ne!(new_key, TEST_PASSWORD);

    let verifier = spawn_test_client_with_server(&server).await;
    let mut verifier_config = load_config(verifier.workspace.path()).unwrap();
    verifier_config.encryption_password = Some(new_key);
    feanorfs_client::save_config(verifier.workspace.path(), &verifier_config).unwrap();
    migrate_workspace(verifier.workspace.path(), false)
        .await
        .unwrap();
    assert_eq!(
        read_workspace_file(verifier.workspace.path(), "secret.txt").await,
        b"rekeyed data"
    );
}

#[tokio::test]
async fn migrate_rekey_resumes_from_durable_journal() {
    use feanorfs_client::{load_config, migrate_workspace};

    let server = spawn_test_server().await;
    let source = spawn_test_client_with_server(&server).await;
    write_workspace_file(source.workspace.path(), "resume.txt", b"survives retry").await;
    do_push_only(
        &server.api,
        &source.db,
        source.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    tokio::fs::write(
        source
            .workspace
            .path()
            .join(".feanorfs/migration-failpoint"),
        b"after_reseal_upload",
    )
    .await
    .unwrap();
    assert!(migrate_workspace(source.workspace.path(), true)
        .await
        .is_err());
    let interrupted = load_config(source.workspace.path()).unwrap();
    assert_eq!(interrupted.format_version, 2);
    assert_eq!(
        interrupted.encryption_password.as_deref(),
        Some(TEST_PASSWORD)
    );
    tokio::fs::remove_file(
        source
            .workspace
            .path()
            .join(".feanorfs/migration-failpoint"),
    )
    .await
    .unwrap();

    migrate_workspace(source.workspace.path(), false)
        .await
        .unwrap();
    let resumed = load_config(source.workspace.path()).unwrap();
    assert_eq!(resumed.format_version, 3);
    assert_ne!(resumed.encryption_password.as_deref(), Some(TEST_PASSWORD));
    assert!(!source
        .workspace
        .path()
        .join(".feanorfs/migration-v3.json")
        .exists());
}

#[tokio::test]
async fn agent_revert_to_original_does_not_land() {
    use feanorfs_client::land_agent;

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    let content = b"same bytes";

    write_workspace_file(base, "revert.txt", content).await;
    do_push_only(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
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
        "rv",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    write_workspace_file(&base.join(".feanorfs/agents/rv"), "revert.txt", content).await;

    let result = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "rv",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(result.landed.is_empty());
    assert_eq!(result.message, "Nothing to land.");
}

#[tokio::test]
async fn agent_land_conflict_artifact_uses_agent_copy() {
    use feanorfs_client::{conflict_artifacts::resolve_artifact, land_agent, ArtifactRole};

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    write_workspace_file(base, "doc.txt", b"base version").await;
    do_push_only(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
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
        "ci1",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    write_workspace_file(
        &base.join(".feanorfs/agents/ci1"),
        "doc.txt",
        b"agent version",
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    write_workspace_file(base, "doc.txt", b"server version").await;
    do_push_only(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let result = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "ci1",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    assert_eq!(result.conflicts.len(), 1);
    let edit = &result.conflicts[0];
    let conflict_dir = edit
        .local_file
        .as_ref()
        .and_then(|p| std::path::Path::new(p).parent())
        .expect("conflict dir from local_file");
    let local_artifact = resolve_artifact(conflict_dir, "doc.txt", ArtifactRole::Local);
    let local_bytes = std::fs::read(local_artifact).unwrap();
    assert_eq!(
        local_bytes, b"agent version",
        "local artifact must preserve the agent's edit, not the main-folder copy"
    );
}

#[tokio::test]
async fn agent_land_advances_snapshot_base() {
    use feanorfs_client::{check_agent, land_agent};

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();

    write_workspace_file(base, "doc.txt", b"base").await;
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
        "snap",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    write_workspace_file(&base.join(".feanorfs/agents/snap"), "doc.txt", b"agent-v1").await;
    land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "snap",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    let check = check_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "snap",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(
        check.our_changes.is_empty() && check.conflicts.is_empty(),
        "snapshot base must advance to agent-v1 after land"
    );

    write_workspace_file(&base.join(".feanorfs/agents/snap"), "doc.txt", b"agent-v2").await;
    let land2 = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "snap",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(!land2.landed.is_empty());

    let check2 = check_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "snap",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(check2.our_changes.is_empty() && check2.conflicts.is_empty());
    assert_eq!(read_workspace_file(base, "doc.txt").await, b"agent-v2");
}

#[tokio::test]
async fn join_nonempty_folder_unions_without_silent_overwrite() {
    use feanorfs_client::{conflicts, do_sync, save_config, Config, SyncCtx};

    let server = spawn_test_server().await;
    let hub = spawn_test_client().await;
    let hub_base = hub.workspace.path();

    write_workspace_file(hub_base, "remote-only.txt", b"from-server").await;
    write_workspace_file(hub_base, "both.txt", b"server-side").await;
    do_push_only(
        &server.api,
        &hub.db,
        hub_base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let joiner = spawn_test_client().await;
    let join_base = joiner.workspace.path();
    write_workspace_file(join_base, "local-only.txt", b"local").await;
    write_workspace_file(join_base, "both.txt", b"local-side").await;

    let config = Config {
        server_url: server.url.clone(),
        workspace_id: WORKSPACE_ID.to_string(),
        encryption_password: Some(TEST_PASSWORD.to_string()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 2,
        hub_local: false,
        relay: None,
    };
    save_config(join_base, &config).unwrap();
    let db = feanorfs_client::ClientDb::new(join_base.join(".feanorfs"))
        .await
        .unwrap();
    let api = feanorfs_client::ApiClient::new(&server.url, None);
    let local_files =
        feanorfs_client::local::scan_local_directory(join_base, &db, Some(TEST_PASSWORD))
            .await
            .unwrap();
    let ctx = SyncCtx::new(
        &api,
        &db,
        join_base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    conflicts::seed_last_synced_from_server(&ctx, &local_files)
        .await
        .unwrap();
    do_sync(
        &api,
        &db,
        join_base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert_eq!(
        read_workspace_file(join_base, "local-only.txt").await,
        b"local"
    );
    assert_eq!(
        read_workspace_file(join_base, "remote-only.txt").await,
        b"from-server"
    );

    let pending = db.list_conflict_records().await.unwrap();
    assert!(
        pending.iter().any(|r| r.path == "both.txt"),
        "same-path different content must surface as needs-attention, not silent overwrite"
    );
}

#[tokio::test]
async fn local_hub_in_process_sync() {
    use feanorfs_client::{
        hub::LocalHub, save_config, validate_e2ee_key, ApiClient, Config, LOCAL_HUB_URL,
    };

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let key = feanorfs_common::generate_password().unwrap();
    validate_e2ee_key(&key, 2).unwrap();
    let config = Config {
        server_url: LOCAL_HUB_URL.to_string(),
        workspace_id: "local-ws".into(),
        encryption_password: Some(key.clone()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 2,
        hub_local: true,
        relay: None,
    };
    save_config(base, &config).unwrap();
    std::fs::create_dir_all(base.join(".feanorfs")).unwrap();
    LocalHub::open(config.hub_data_dir(base), None)
        .await
        .unwrap();
    assert!(config.is_local_hub());
    assert_eq!(config.server_url, LOCAL_HUB_URL);

    let db = feanorfs_client::ClientDb::new(base.join(".feanorfs"))
        .await
        .unwrap();
    let api = ApiClient::from_config(base, &config).await.unwrap();
    assert!(api.is_local());

    write_workspace_file(base, "local.txt", b"offline-ok").await;
    feanorfs_client::do_push_only(&api, &db, base, "local-ws", Some(&key))
        .await
        .unwrap();

    let workspaces = api.get_workspaces().await.unwrap();
    assert!(workspaces.contains(&"local-ws".to_string()));
}

#[tokio::test]
async fn fresh_format_v3_workspace_syncs_without_flat_metadata() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let mut first_config = feanorfs_client::load_config(first.workspace.path()).unwrap();
    first_config.format_version = 3;
    feanorfs_client::save_config(first.workspace.path(), &first_config).unwrap();
    write_workspace_file(first.workspace.path(), "v3.txt", b"snapshot transport").await;
    do_sync(
        &server.api,
        &first.db,
        first.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        server.api.get_workspaces().await.unwrap(),
        vec![WORKSPACE_ID]
    );
    let flat = server
        .api
        .peek_sync(&feanorfs_common::SyncRequest {
            workspace_id: WORKSPACE_ID.to_string(),
            files: Vec::new(),
        })
        .await;
    assert!(flat.is_err());

    let second = spawn_test_client_with_server(&server).await;
    let mut second_config = feanorfs_client::load_config(second.workspace.path()).unwrap();
    second_config.format_version = 3;
    feanorfs_client::save_config(second.workspace.path(), &second_config).unwrap();
    do_pull_only(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        read_workspace_file(second.workspace.path(), "v3.txt").await,
        b"snapshot transport"
    );
}

#[tokio::test]
async fn tray_status_and_pause() {
    use feanorfs_client::{do_tray_status, is_paused, set_paused};

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();

    set_paused(base, true).unwrap();
    assert!(is_paused(base));

    let status = do_tray_status(base).await.unwrap();
    assert!(status.paused);
    assert_eq!(status.mirror_state, "idle");
    assert!(status.pending_conflicts.is_empty());

    set_paused(base, false).unwrap();
    assert!(!is_paused(base));
}

#[tokio::test]
async fn tray_status_lists_working_agent() {
    use feanorfs_client::{do_tray_status, spawn_agent};

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();

    write_workspace_file(base, "task.txt", b"base").await;
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

    spawn_agent(
        base,
        &client.db,
        &server.api,
        WORKSPACE_ID,
        "ci1",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    write_workspace_file(
        &base.join(".feanorfs/agents/ci1"),
        "task.txt",
        b"agent edit",
    )
    .await;

    let status = do_tray_status(base).await.unwrap();
    assert!(
        status.agents.working >= 1,
        "expected at least one working agent: {:?}",
        status.agents
    );
    assert!(
        status
            .agents
            .entries
            .iter()
            .any(|e| e.name == "ci1" && e.change_count > 0),
        "ci1 should report local changes"
    );
}

#[tokio::test]
async fn conflicts_keep_cloud_honors_remote_deletion() {
    use feanorfs_client::{conflicts, resolve_conflict, ResolveKeep, SyncCtx};

    let server = spawn_test_server().await;
    let client_a = spawn_test_client().await;
    let client_b = spawn_test_client().await;
    let base_a = client_a.workspace.path();
    let base_b = client_b.workspace.path();

    write_workspace_file(base_a, "edited.txt", b"original").await;
    do_push_only(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &client_b.db,
        base_b,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    tokio::fs::remove_file(base_b.join("edited.txt"))
        .await
        .unwrap();
    do_sync(
        &server.api,
        &client_b.db,
        base_b,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    write_workspace_file(base_a, "edited.txt", b"local edit").await;
    do_sync(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let records = client_a.db.list_conflict_records().await.unwrap();
    let record = records
        .iter()
        .find(|r| r.path == "edited.txt")
        .expect("edit/delete conflict on edited.txt");
    assert_eq!(record.kind, feanorfs_common::ConflictKind::EditDelete);

    let ctx = SyncCtx::new(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    resolve_conflict(&ctx, "edited.txt", ResolveKeep::Cloud, None)
        .await
        .unwrap();

    assert!(!base_a.join("edited.txt").exists());
    let pending = conflicts::pending_conflict_paths(&client_a.db)
        .await
        .unwrap();
    assert!(!conflicts::conflicts_pending(Some(&pending)));

    let after = do_sync(
        &server.api,
        &client_a.db,
        base_a,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(after.uploads, 0);
    assert_eq!(after.downloads, 0);
    assert!(!base_a.join("edited.txt").exists());
}
