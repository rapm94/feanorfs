mod support;

use feanorfs_client::{commit_agent, do_pull_only, do_push_only, do_sync, spawn_agent};
use support::{
    read_workspace_file, spawn_test_client, spawn_test_server, write_workspace_file, TEST_PASSWORD,
    WORKSPACE_ID,
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
    do_push_only(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

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
    let main = spawn_test_client().await;
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

    spawn_agent(base, &main.db, "ci1", Some(TEST_PASSWORD))
        .await
        .unwrap();

    write_workspace_file(
        &base.join(".feanorfs/agents/ci1"),
        "doc.txt",
        b"agent version",
    )
    .await;

    // Ensure the main workspace edit wins the mtime race against the server copy.
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
async fn sync_detects_concurrent_workspace_edit_without_silent_overwrite() {
    use feanorfs_client::commands::conflicts_pending;

    let server = spawn_test_server().await;
    let client_a = spawn_test_client().await;
    let client_b = spawn_test_client().await;
    let base_a = client_a.workspace.path();

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
    assert!(conflicts_pending(base_a));
}

#[tokio::test]
async fn concurrent_delete_is_not_a_workspace_conflict() {
    use feanorfs_client::commands::conflicts_pending;

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

    assert!(!conflicts_pending(base_a));
    assert!(!base_a.join("gone.txt").exists());
}

#[tokio::test]
async fn sync_removes_stale_conflict_dirs_from_prior_sessions() {
    let server = spawn_test_server().await;
    let client = spawn_test_client().await;
    let base = client.workspace.path();

    write_workspace_file(base, "ok.txt", b"ok").await;
    do_push_only(
        &server.api,
        &client.db,
        base,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    let stale = base.join(".feanorfs/conflicts/1_sync");
    tokio::fs::create_dir_all(&stale).await.unwrap();

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

    assert!(!stale.exists());
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
