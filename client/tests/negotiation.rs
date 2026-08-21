feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{
    check_agent, do_pull_only, do_push_only, do_status, do_sync, land_agent, spawn_agent,
};

use support::{
    agent_path, read_workspace_file, spawn_test_client, spawn_test_client_with_server,
    spawn_test_server, state_path, write_workspace_file, TEST_PASSWORD, WORKSPACE_ID,
};

#[tokio::test]
async fn same_process_sync_lock_is_exclusive_until_the_owner_drops() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();
    let first = feanorfs_client::lock::SyncLock::acquire(base).unwrap();
    let sync_contention = match feanorfs_client::lock::SyncLock::acquire(base) {
        Ok(_) => panic!("same-process operations must not re-enter the sync lock"),
        Err(error) => error,
    };
    assert!(feanorfs_client::lock::is_lock_contention(&sync_contention));
    assert_eq!(
        feanorfs_agent_core::classify_continuous_error(&sync_contention),
        feanorfs_agent_core::ContinuousErrorClass::Retryable,
        "same-process sync contention is transient, not corrupt state"
    );
    assert!(state_path(base).join("sync.lock").exists());
    drop(first);
    let second = feanorfs_client::lock::SyncLock::acquire(base).unwrap();
    assert!(state_path(base).join("sync.lock").exists());
    drop(second);
    assert!(!state_path(base).join("sync.lock").exists());

    let first_land = feanorfs_client::lock::LandLock::acquire(base).unwrap();
    let land_contention = match feanorfs_client::lock::LandLock::acquire(base) {
        Ok(_) => panic!("same-process operations must not re-enter the land lock"),
        Err(error) => error,
    };
    assert!(feanorfs_client::lock::is_lock_contention(&land_contention));
    assert_eq!(
        feanorfs_agent_core::classify_continuous_error(&land_contention),
        feanorfs_agent_core::ContinuousErrorClass::Retryable,
        "same-process land contention is transient, not corrupt state"
    );
    drop(first_land);
}

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
async fn format_v3_pull_with_local_only_file_does_not_publish_it() {
    let server = spawn_test_server().await;

    let uploader = spawn_test_client_with_server(&server).await;
    let mut uploader_config = feanorfs_client::load_config(uploader.workspace.path()).unwrap();
    uploader_config.format_version = 3;
    feanorfs_client::save_config(uploader.workspace.path(), &uploader_config).unwrap();
    write_workspace_file(uploader.workspace.path(), "remote-only.txt", b"remote").await;
    do_push_only(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let head_before = server.api.get_head(WORKSPACE_ID).await.unwrap();

    let downloader = spawn_test_client_with_server(&server).await;
    let mut downloader_config = feanorfs_client::load_config(downloader.workspace.path()).unwrap();
    downloader_config.format_version = 3;
    feanorfs_client::save_config(downloader.workspace.path(), &downloader_config).unwrap();
    write_workspace_file(downloader.workspace.path(), "local-only.txt", b"local").await;

    let pulled = do_pull_only(
        &server.api,
        &downloader.db,
        downloader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert_eq!(pulled.downloads, 1);
    assert_eq!(
        read_workspace_file(downloader.workspace.path(), "remote-only.txt").await,
        b"remote"
    );
    assert_eq!(
        read_workspace_file(downloader.workspace.path(), "local-only.txt").await,
        b"local"
    );
    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap(),
        head_before
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
    let first_ref = std::fs::read_to_string(state_path(base).join("refs/workspace")).unwrap();
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
    let second_ref = std::fs::read_to_string(state_path(base).join("refs/workspace")).unwrap();
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
    let object_count = std::fs::read_dir(state_path(base).join("objects"))
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
        std::fs::read_to_string(state_path(base).join("refs/workspace")).unwrap(),
        second_ref
    );
    assert_eq!(
        std::fs::read_dir(state_path(base).join("objects"))
            .unwrap()
            .count(),
        object_count
    );
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

    write_workspace_file(&agent_path(base, "green"), "task.txt", b"new work").await;

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
async fn agent_spawn_no_sync_is_local_and_uses_last_synced_base() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let remote = spawn_test_client_with_server(&server).await;
    for workspace in [main.workspace.path(), remote.workspace.path()] {
        let mut config = feanorfs_client::load_config(workspace).unwrap();
        config.format_version = 3;
        feanorfs_client::save_config(workspace, &config).unwrap();
    }

    let base = main.workspace.path();
    write_workspace_file(base, "base.txt", b"base").await;
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
        &remote.db,
        remote.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let last_synced = tokio::fs::read_to_string(state_path(base).join("refs/last-synced"))
        .await
        .unwrap();

    write_workspace_file(remote.workspace.path(), "remote.txt", b"remote advance").await;
    do_sync(
        &server.api,
        &remote.db,
        remote.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let remote_head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    assert_ne!(last_synced, remote_head);

    let offline_api = feanorfs_client::ApiClient::new("http://127.0.0.1:1", None);
    spawn_agent(
        base,
        &main.db,
        &offline_api,
        WORKSPACE_ID,
        "no-sync-local",
        Some(TEST_PASSWORD),
        true,
        false,
    )
    .await
    .expect("--no-sync must not contact the unavailable API");

    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap(),
        remote_head,
        "--no-sync must not publish or swap the workspace head"
    );
    assert_eq!(
        tokio::fs::read_to_string(
            state_path(base).join("agents/no-sync-local/state/base-snapshot")
        )
        .await
        .unwrap(),
        last_synced,
        "the agent base must remain the local last-synced snapshot"
    );

    let check = check_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "no-sync-local",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(
        check.our_changes.is_empty(),
        "an untouched no-sync agent must not look locally edited"
    );
    assert!(check
        .their_changes
        .iter()
        .any(|change| change.path == "remote.txt"));
}

#[tokio::test]
async fn agent_spawn_no_sync_missing_local_object_never_calls_api() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "base.txt", b"base").await;
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

    let last_synced = tokio::fs::read_to_string(state_path(base).join("refs/last-synced"))
        .await
        .unwrap();
    tokio::fs::remove_file(state_path(base).join("objects").join(&last_synced))
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let request_count = Arc::new(AtomicUsize::new(0));
    let observed_count = Arc::clone(&request_count);
    let recorder = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            observed_count.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    let recording_api = feanorfs_client::ApiClient::new(&format!("http://{address}"), None);

    let error = spawn_agent(
        base,
        &main.db,
        &recording_api,
        WORKSPACE_ID,
        "no-sync-missing-object",
        Some(TEST_PASSWORD),
        true,
        false,
    )
    .await
    .expect_err("--no-sync must fail locally when its snapshot object is unavailable");
    tokio::task::yield_now().await;
    assert!(error.to_string().contains("local snapshot state"));
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        0,
        "--no-sync must not try to repair missing local objects from the API"
    );
    recorder.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn agent_spawn_no_sync_rejects_mode_only_divergence() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    let file = write_workspace_file(base, "mode.txt", b"same bytes").await;
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

    let mut permissions = std::fs::metadata(&file).unwrap().permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(&file, permissions).unwrap();
    let offline_api = feanorfs_client::ApiClient::new("http://127.0.0.1:1", None);
    let error = spawn_agent(
        base,
        &main.db,
        &offline_api,
        WORKSPACE_ID,
        "no-sync-mode",
        Some(TEST_PASSWORD),
        true,
        false,
    )
    .await
    .expect_err("--no-sync must reject executable-mode divergence");
    assert!(error.to_string().contains("mode.txt"));
}

#[tokio::test]
async fn agent_spawn_no_sync_rejects_last_only_missing_path() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "missing.txt", b"last agreed").await;
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

    tokio::fs::remove_file(base.join("missing.txt"))
        .await
        .unwrap();
    main.db.delete_cache_entry("missing.txt").await.unwrap();
    let offline_api = feanorfs_client::ApiClient::new("http://127.0.0.1:1", None);
    let error = spawn_agent(
        base,
        &main.db,
        &offline_api,
        WORKSPACE_ID,
        "no-sync-missing",
        Some(TEST_PASSWORD),
        true,
        false,
    )
    .await
    .expect_err("--no-sync must reject a path present only in last-synced state");
    assert!(error.to_string().contains("missing.txt"));
}

#[tokio::test]
async fn agent_spawn_replace_resets_equal_metadata_runtime_cache() {
    use std::time::Duration;

    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "task.txt", b"base").await;
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
        "replace-cache",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let agent = agent_path(base, "replace-cache");
    let agent_file = write_workspace_file(&agent, "task.txt", b"next").await;
    let seeded = check_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "replace-cache",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(seeded
        .our_changes
        .iter()
        .any(|change| change.path == "task.txt"));
    let cached_mtime = std::fs::metadata(&agent_file).unwrap().modified().unwrap();

    let shared_file = write_workspace_file(base, "task.txt", b"next").await;
    std::fs::File::options()
        .write(true)
        .open(&shared_file)
        .unwrap()
        .set_modified(cached_mtime + Duration::from_secs(1))
        .unwrap();
    spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "replace-cache",
        Some(TEST_PASSWORD),
        false,
        true,
    )
    .await
    .unwrap();

    let replaced_file = write_workspace_file(&agent, "task.txt", b"edit").await;
    std::fs::File::options()
        .write(true)
        .open(&replaced_file)
        .unwrap()
        .set_modified(cached_mtime)
        .unwrap();
    let checked = check_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "replace-cache",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let edit = checked
        .our_changes
        .iter()
        .find(|change| change.path == "task.txt")
        .expect("equal-size/equal-mtime edit must be hashed after replacement");
    let expected = feanorfs_common::pack_bytes(b"edit", TEST_PASSWORD, "task.txt").unwrap();
    assert_eq!(edit.hash, feanorfs_common::hash_bytes(&expected));
}

#[tokio::test]
async fn agent_spawn_replace_rotates_stale_state_when_worktree_is_missing() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "task.txt", b"original").await;
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
        "stale-root",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();

    let state = state_path(base).join("agents/stale-root/state");
    let old_base_ref = tokio::fs::read(state.join("base-snapshot")).await.unwrap();
    let runtime_path = state.join("runtime");
    let runtime_db = feanorfs_client::ClientDb::new(&runtime_path).await.unwrap();
    runtime_db
        .set_session_key("stale-runtime", "must-not-survive")
        .await
        .unwrap();
    drop(runtime_db);
    tokio::fs::remove_dir_all(agent_path(base, "stale-root"))
        .await
        .unwrap();

    write_workspace_file(base, "task.txt", b"replacement").await;
    spawn_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "stale-root",
        Some(TEST_PASSWORD),
        false,
        true,
    )
    .await
    .unwrap();

    assert_eq!(
        read_workspace_file(&agent_path(base, "stale-root"), "task.txt").await,
        b"replacement"
    );
    assert!(
        !runtime_path.exists(),
        "replace must not retain runtime state from a root with no worktree"
    );
    assert_ne!(
        tokio::fs::read(state.join("base-snapshot")).await.unwrap(),
        old_base_ref,
        "replace must publish a new base ref for the recreated worktree"
    );
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

    let agent_base = agent_path(base, "retry");
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
        write_workspace_file(&agent_path(base, &name), "recover.txt", b"agent result").await;
        tokio::fs::write(
            state_path(base).join(format!("test-land-failpoint-{name}")),
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

        if point == "after-cas" {
            write_workspace_file(base, "recover.txt", b"post-commit user edit").await;
            let recovered = land_agent(
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
            .expect("committed-land recovery should divert rather than overwrite the later edit");
            assert!(recovered.conflicts.is_empty());
            assert!(recovered
                .landed
                .iter()
                .any(|path| path.action == "diverted: folder changed during land"));
            assert_eq!(
                read_workspace_file(base, "recover.txt").await,
                b"post-commit user edit"
            );
            let head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
            assert_eq!(
                tokio::fs::read_to_string(
                    state_path(base)
                        .join("agents")
                        .join(&name)
                        .join("state/base-snapshot")
                )
                .await
                .unwrap(),
                head
            );
            continue;
        }

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
                state_path(base)
                    .join("agents")
                    .join(&name)
                    .join("state/base-snapshot")
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
        &agent_path(first.workspace.path(), "first"),
        "first.txt",
        b"one",
    )
    .await;
    write_workspace_file(
        &agent_path(second.workspace.path(), "second"),
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
    let agent = agent_path(base, "replace");
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
    let refreshed_id =
        tokio::fs::read_to_string(state_path(base).join("agents/replace/state/base-snapshot"))
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
async fn in_flight_sync_never_adopts_and_overwrites_a_newer_head() {
    let server = spawn_test_server().await;
    let client_a = spawn_test_client_with_server(&server).await;
    let client_b = spawn_test_client_with_server(&server).await;
    for workspace in [client_a.workspace.path(), client_b.workspace.path()] {
        let mut config = feanorfs_client::load_config(workspace).unwrap();
        config.format_version = 3;
        feanorfs_client::save_config(workspace, &config).unwrap();
    }

    write_workspace_file(client_b.workspace.path(), "remote.txt", b"remote").await;
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
    write_workspace_file(client_a.workspace.path(), "local.txt", b"local").await;

    let client_a_path = client_a.workspace.path().to_path_buf();
    let client_a_state = state_path(&client_a_path);
    tokio::fs::write(client_a_state.join("test-sync-pause"), b"after-negotiate")
        .await
        .unwrap();
    let server_url = server.url.clone();
    let sync_a = tokio::spawn(async move {
        let api = feanorfs_client::ApiClient::new(&server_url, None);
        do_sync(
            &api,
            &client_a.db,
            client_a.workspace.path(),
            WORKSPACE_ID,
            Some(TEST_PASSWORD),
            false,
        )
        .await
    });

    let reached = client_a_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "sync did not reach the deterministic post-negotiation pause"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    write_workspace_file(client_b.workspace.path(), "late.txt", b"late").await;
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
    let newer_head = server.api.get_head(WORKSPACE_ID).await.unwrap();
    tokio::fs::remove_file(client_a_state.join("test-sync-pause"))
        .await
        .unwrap();

    let error = sync_a
        .await
        .unwrap()
        .expect_err("the stale negotiated head must lose its compare-and-swap");
    assert!(error.to_string().contains("head changed during sync"));
    assert_eq!(server.api.get_head(WORKSPACE_ID).await.unwrap(), newer_head);

    let verifier = spawn_test_client_with_server(&server).await;
    let mut config = feanorfs_client::load_config(verifier.workspace.path()).unwrap();
    config.format_version = 3;
    feanorfs_client::save_config(verifier.workspace.path(), &config).unwrap();
    do_pull_only(
        &server.api,
        &verifier.db,
        verifier.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        read_workspace_file(verifier.workspace.path(), "remote.txt").await,
        b"remote"
    );
    assert_eq!(
        read_workspace_file(verifier.workspace.path(), "late.txt").await,
        b"late"
    );
    assert!(!verifier.workspace.path().join("local.txt").exists());
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
    write_workspace_file(&agent_path(base, "land1"), "land.txt", b"landed").await;

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
async fn configured_runner_rejects_land_cleanup_before_any_mutation() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    let mut config = feanorfs_client::load_config(base).unwrap();
    config.format_version = 3;
    feanorfs_client::save_config(base, &config).unwrap();

    write_workspace_file(base, "guarded.txt", b"main-before").await;
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
        "guarded-clean",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let agent = agent_path(base, "guarded-clean");
    write_workspace_file(&agent, "guarded.txt", b"agent-change").await;

    let head_before = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let base_ref = state_path(base).join("agents/guarded-clean/state/base-snapshot");
    let base_before = tokio::fs::read(&base_ref).await.unwrap();
    let main_before = read_workspace_file(base, "guarded.txt").await;
    let agent_before = read_workspace_file(&agent, "guarded.txt").await;
    let program = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    feanorfs_agent_core::RunnerStore::configure(
        base,
        "guarded-clean",
        &program,
        Vec::new(),
        3600,
        &head_before,
    )
    .unwrap();

    let error = land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "guarded-clean",
        Some(TEST_PASSWORD),
        true,
        false,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("remove the runner"));
    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap(),
        head_before
    );
    assert_eq!(tokio::fs::read(&base_ref).await.unwrap(), base_before);
    assert_eq!(read_workspace_file(base, "guarded.txt").await, main_before);
    assert_eq!(
        read_workspace_file(&agent, "guarded.txt").await,
        agent_before
    );
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
async fn migrate_rekeys_an_existing_format_v3_workspace() {
    use feanorfs_client::{load_config, migrate_workspace, save_config};

    let server = spawn_test_server().await;
    let source = spawn_test_client_with_server(&server).await;
    write_workspace_file(source.workspace.path(), "v3-secret.txt", b"stronger key").await;
    do_push_only(
        &server.api,
        &source.db,
        source.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    migrate_workspace(source.workspace.path(), false)
        .await
        .unwrap();
    let migrated = load_config(source.workspace.path()).unwrap();
    assert_eq!(migrated.format_version, 3);
    assert_eq!(migrated.encryption_password.as_deref(), Some(TEST_PASSWORD));
    write_workspace_file(
        source.workspace.path(),
        "v3-secret.txt",
        b"stronger key after v3",
    )
    .await;

    migrate_workspace(source.workspace.path(), true)
        .await
        .unwrap();
    let rekeyed = load_config(source.workspace.path()).unwrap();
    let new_key = rekeyed.encryption_password.clone().unwrap();
    assert_eq!(new_key.len(), 64);
    assert_ne!(new_key, TEST_PASSWORD);

    let verifier = spawn_test_client_with_server(&server).await;
    let mut verifier_config = load_config(verifier.workspace.path()).unwrap();
    verifier_config.format_version = 3;
    verifier_config.encryption_password = Some(new_key);
    save_config(verifier.workspace.path(), &verifier_config).unwrap();
    let pulled = do_pull_only(
        &server.api,
        &verifier.db,
        verifier.workspace.path(),
        WORKSPACE_ID,
        verifier_config.encryption_password.as_deref(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(pulled.downloads, 1);
    assert_eq!(
        read_workspace_file(verifier.workspace.path(), "v3-secret.txt").await,
        b"stronger key after v3"
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
        state_path(source.workspace.path()).join("migration-failpoint"),
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
    tokio::fs::remove_file(state_path(source.workspace.path()).join("migration-failpoint"))
        .await
        .unwrap();

    migrate_workspace(source.workspace.path(), false)
        .await
        .unwrap();
    let resumed = load_config(source.workspace.path()).unwrap();
    assert_eq!(resumed.format_version, 3);
    assert_ne!(resumed.encryption_password.as_deref(), Some(TEST_PASSWORD));
    assert!(!state_path(source.workspace.path())
        .join("migration-v3.json")
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
    write_workspace_file(&agent_path(base, "rv"), "revert.txt", content).await;

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

    let pre_land_head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    write_workspace_file(&agent_path(base, "snap"), "doc.txt", b"agent-v1").await;
    let first_land = land_agent(
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

    let ctx = feanorfs_agent_core::SyncCtx::new(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        feanorfs_common::LegacyPolicy::Reject,
    );
    let landed_snapshot = feanorfs_agent_core::SnapshotEngine::new(&ctx)
        .load_snapshot(first_land.snapshot_id.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(landed_snapshot.parents, vec![pre_land_head]);

    let status = feanorfs_client::do_status(
        &server.api,
        &main.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(status.upload_required.is_empty());
    assert!(status.download_required.is_empty());
    assert!(status.delete_local.is_empty());

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

    write_workspace_file(&agent_path(base, "snap"), "doc.txt", b"agent-v2").await;
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
    LocalHub::open(config.hub_data_dir(base).unwrap(), None)
        .await
        .unwrap();
    assert!(config.is_local_hub());
    assert_eq!(config.server_url, LOCAL_HUB_URL);

    let db = feanorfs_client::ClientDb::new(state_path(base))
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
