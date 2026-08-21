feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{do_hydrate, do_pull_only, do_push_only, do_sync, land_agent, spawn_agent};

use support::{
    agent_path, read_workspace_file, spawn_test_client, spawn_test_client_with_server,
    spawn_test_server, state_path, write_workspace_file, TEST_PASSWORD, WORKSPACE_ID,
};

#[tokio::test]
async fn materializer_rejects_duplicate_and_conflicting_paths_before_staging() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = feanorfs_client::load_config(client.workspace.path()).unwrap();
    let ctx = feanorfs_client::SyncCtx::from_config(
        &server.api,
        &client.db,
        client.workspace.path(),
        &config,
    )
    .unwrap();
    let file = feanorfs_common::FileState {
        path: "duplicate.txt".to_string(),
        hash: "a".repeat(64),
        size: 1,
        mtime: 0,
        deleted: false,
        mode: 0,
    };
    let empty = std::collections::HashMap::new();
    let duplicate = feanorfs_common::SyncResponse {
        upload_required: Vec::new(),
        download_required: vec![file.clone(), file.clone()],
        delete_local: Vec::new(),
    };
    feanorfs_agent_core::sync_pass::process_downloads(&ctx, &duplicate, &empty, false)
        .await
        .expect_err("duplicate downloads must be rejected");
    let conflicting = feanorfs_common::SyncResponse {
        upload_required: Vec::new(),
        download_required: vec![file],
        delete_local: vec!["duplicate.txt".to_string()],
    };
    feanorfs_agent_core::sync_pass::process_downloads(&ctx, &conflicting, &empty, false)
        .await
        .expect_err("download/delete overlap must be rejected");
    assert!(std::fs::read_dir(client.workspace.path())
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
}

#[tokio::test]
async fn sync_materializes_file_directory_transitions_in_both_directions() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;

    write_workspace_file(first.workspace.path(), "node", b"file-v1").await;
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

    tokio::fs::remove_file(first.workspace.path().join("node"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "node/child.txt", b"child").await;
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
    assert!(second.workspace.path().join("node").is_dir());
    assert_eq!(
        read_workspace_file(second.workspace.path(), "node/child.txt").await,
        b"child"
    );

    tokio::fs::remove_dir_all(first.workspace.path().join("node"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "node", b"file-v2").await;
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
    assert!(second.workspace.path().join("node").is_file());
    assert_eq!(
        read_workspace_file(second.workspace.path(), "node").await,
        b"file-v2"
    );
}

#[tokio::test]
async fn lazy_pull_materializes_structural_transitions_as_placeholders() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "node", b"file-v1").await;
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

    tokio::fs::remove_file(first.workspace.path().join("node"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "node/child.txt", b"child").await;
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
    do_pull_only(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        true,
    )
    .await
    .unwrap();
    assert!(second.workspace.path().join("node").is_dir());
    assert_eq!(
        std::fs::metadata(second.workspace.path().join("node/child.txt"))
            .unwrap()
            .len(),
        0
    );

    tokio::fs::remove_dir_all(first.workspace.path().join("node"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "node", b"file-v2").await;
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
    do_pull_only(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        true,
    )
    .await
    .unwrap();
    assert!(second.workspace.path().join("node").is_file());
    assert_eq!(
        std::fs::metadata(second.workspace.path().join("node"))
            .unwrap()
            .len(),
        0
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

    let hydrated = do_hydrate(
        &server.api,
        &lazy_client.db,
        lazy_client.workspace.path(),
        Some("lazy.txt".to_string()),
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(hydrated.hydrated, vec!["lazy.txt"]);
    assert_eq!(
        read_workspace_file(lazy_client.workspace.path(), "lazy.txt").await,
        b"lazy content"
    );
    assert!(
        lazy_client
            .db
            .get_cache_entries()
            .await
            .unwrap()
            .get("lazy.txt")
            .unwrap()
            .hydrated
    );
}

#[tokio::test]
async fn hydrate_does_not_recreate_a_deleted_lazy_placeholder() {
    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    write_workspace_file(uploader.workspace.path(), "deleted-lazy.txt", b"remote").await;
    do_sync(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let lazy = spawn_test_client_with_server(&server).await;
    do_pull_only(
        &server.api,
        &lazy.db,
        lazy.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        true,
    )
    .await
    .unwrap();
    tokio::fs::remove_file(lazy.workspace.path().join("deleted-lazy.txt"))
        .await
        .unwrap();

    let result = do_hydrate(
        &server.api,
        &lazy.db,
        lazy.workspace.path(),
        Some("deleted-lazy.txt".to_string()),
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(result.skipped);
    assert!(result.hydrated.is_empty());
    assert!(!lazy.workspace.path().join("deleted-lazy.txt").exists());
    assert!(
        lazy.db.get_cache_entries().await.unwrap()["deleted-lazy.txt"]
            .deleted_at
            .is_some()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_rejects_a_touched_lazy_placeholder() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    write_workspace_file(uploader.workspace.path(), "touched-lazy.txt", b"remote").await;
    do_sync(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let lazy = spawn_test_client_with_server(&server).await;
    do_pull_only(
        &server.api,
        &lazy.db,
        lazy.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        true,
    )
    .await
    .unwrap();
    let placeholder = lazy.workspace.path().join("touched-lazy.txt");
    let mut permissions = std::fs::metadata(&placeholder).unwrap().permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(&placeholder, permissions).unwrap();

    do_hydrate(
        &server.api,
        &lazy.db,
        lazy.workspace.path(),
        Some("touched-lazy.txt".to_string()),
        Some(TEST_PASSWORD),
    )
    .await
    .expect_err("a touched placeholder must not be overwritten");
    assert!(tokio::fs::read(&placeholder).await.unwrap().is_empty());
    assert!(!lazy.db.get_cache_entries().await.unwrap()["touched-lazy.txt"].hydrated);
}

#[cfg(unix)]
#[tokio::test]
async fn hydration_rejects_same_metadata_placeholder_replacement() {
    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    write_workspace_file(uploader.workspace.path(), "lazy.txt", b"first-remote").await;
    do_sync(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let lazy = spawn_test_client_with_server(&server).await;
    do_pull_only(
        &server.api,
        &lazy.db,
        lazy.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        true,
    )
    .await
    .unwrap();
    write_workspace_file(uploader.workspace.path(), "lazy.txt", b"second-remote").await;
    do_sync(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let lazy_path = lazy.workspace.path().to_path_buf();
    let lazy_state = state_path(&lazy_path);
    tokio::fs::write(
        lazy_state.join("test-sync-pause"),
        b"before-final-validation",
    )
    .await
    .unwrap();
    let url = server.url.clone();
    let task_path = lazy_path.clone();
    let task_state = lazy_state.clone();
    let sync = tokio::spawn(async move {
        let api = feanorfs_client::ApiClient::new(&url, None);
        let db = feanorfs_client::ClientDb::new(task_state).await.unwrap();
        do_sync(
            &api,
            &db,
            &task_path,
            WORKSPACE_ID,
            Some(TEST_PASSWORD),
            false,
        )
        .await
    });
    let reached = lazy_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let placeholder = lazy_path.join("lazy.txt");
    let metadata = std::fs::metadata(&placeholder).unwrap();
    let modified = metadata.modified().unwrap();
    let permissions = metadata.permissions();
    let replacement = lazy_path.join("replacement-empty");
    let replacement_file = std::fs::File::create(&replacement).unwrap();
    replacement_file
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    drop(replacement_file);
    std::fs::set_permissions(&replacement, permissions).unwrap();
    std::fs::rename(&replacement, &placeholder).unwrap();
    assert_eq!(
        std::fs::metadata(&placeholder).unwrap().modified().unwrap(),
        modified
    );
    tokio::fs::remove_file(lazy_state.join("test-sync-pause"))
        .await
        .unwrap();

    let error = sync
        .await
        .unwrap()
        .expect_err("same-metadata placeholder replacement must abort hydration");
    assert!(format!("{error:#}").contains("was replaced"));
    assert!(tokio::fs::read(&placeholder).await.unwrap().is_empty());
    assert!(!lazy.db.get_cache_entries().await.unwrap()["lazy.txt"].hydrated);
}

#[tokio::test]
async fn agent_land_materializes_file_directory_transitions() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "node", b"file-v1").await;
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
        "shape-land",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let agent = agent_path(base, "shape-land");

    tokio::fs::remove_file(agent.join("node")).await.unwrap();
    write_workspace_file(&agent, "node/child.txt", b"child").await;
    land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "shape-land",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(base.join("node").is_dir());
    assert_eq!(read_workspace_file(base, "node/child.txt").await, b"child");

    tokio::fs::remove_dir_all(agent.join("node")).await.unwrap();
    write_workspace_file(&agent, "node", b"file-v2").await;
    land_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "shape-land",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(base.join("node").is_file());
    assert_eq!(read_workspace_file(base, "node").await, b"file-v2");
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
    let agent = agent_path(base, "refresh-add");
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
async fn agent_refresh_materializes_file_directory_transitions() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let base = main.workspace.path();
    write_workspace_file(base, "node", b"file-v1").await;
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
        "refresh-shape",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let agent = agent_path(base, "refresh-shape");

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
    feanorfs_client::refresh_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "refresh-shape",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(agent.join("node").is_dir());
    assert_eq!(
        read_workspace_file(&agent, "node/child.txt").await,
        b"child"
    );

    tokio::fs::remove_dir_all(base.join("node")).await.unwrap();
    write_workspace_file(base, "node", b"file-v2").await;
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
    feanorfs_client::refresh_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "refresh-shape",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(agent.join("node").is_file());
    assert_eq!(read_workspace_file(&agent, "node").await, b"file-v2");
}
