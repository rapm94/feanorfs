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
