feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{
    check_agent, commit_agent, do_cat, do_hydrate, do_pull_only, do_push_only, do_status, do_sync,
    land_agent, spawn_agent,
};
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
async fn same_process_sync_lock_is_exclusive_until_the_owner_drops() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();
    let first = feanorfs_client::lock::SyncLock::acquire(base).unwrap();
    assert!(
        feanorfs_client::lock::SyncLock::acquire(base).is_err(),
        "same-process operations must not re-enter the sync lock"
    );
    assert!(state_path(base).join("sync.lock").exists());
    drop(first);
    let second = feanorfs_client::lock::SyncLock::acquire(base).unwrap();
    assert!(state_path(base).join("sync.lock").exists());
    drop(second);
    assert!(!state_path(base).join("sync.lock").exists());
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

#[cfg(unix)]
#[tokio::test]
async fn structural_transition_preserves_untracked_symlink_children() {
    use std::os::unix::fs::symlink;

    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
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
    symlink(
        "child.txt",
        second.workspace.path().join("node/untracked-link"),
    )
    .unwrap();

    tokio::fs::remove_dir_all(first.workspace.path().join("node"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "node", b"replacement").await;
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

    let error = do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("untracked symlink must block directory replacement");
    assert!(error.to_string().contains("untracked entry"));
    assert_eq!(
        read_workspace_file(second.workspace.path(), "node/child.txt").await,
        b"child"
    );
    assert!(second
        .workspace
        .path()
        .join("node/untracked-link")
        .is_symlink());
}

#[tokio::test]
async fn concurrent_file_directory_transition_fails_before_worktree_mutation() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;

    write_workspace_file(first.workspace.path(), "node", b"base").await;
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

    write_workspace_file(first.workspace.path(), "node", b"local-edit").await;
    tokio::fs::remove_file(second.workspace.path().join("node"))
        .await
        .unwrap();
    write_workspace_file(second.workspace.path(), "node/child.txt", b"cloud-child").await;
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
    let head_before = server.api.get_head(WORKSPACE_ID).await.unwrap();

    let error = do_sync(
        &server.api,
        &first.db,
        first.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("concurrent hierarchical edits must fail closed");
    assert!(
        error.to_string().contains("file/directory")
            || error.to_string().contains("path collision"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        read_workspace_file(first.workspace.path(), "node").await,
        b"local-edit"
    );
    assert!(!first.workspace.path().join("node/child.txt").exists());
    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap(),
        head_before
    );
}

#[tokio::test]
async fn filtered_descendant_conflict_cannot_publish_beside_local_ancestor_file() {
    let server = spawn_test_server().await;
    let local = spawn_test_client_with_server(&server).await;
    let cloud = spawn_test_client_with_server(&server).await;

    write_workspace_file(local.workspace.path(), "a/x.txt", b"base").await;
    do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &cloud.db,
        cloud.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    tokio::fs::remove_dir_all(local.workspace.path().join("a"))
        .await
        .unwrap();
    write_workspace_file(local.workspace.path(), "a", b"local-file").await;
    write_workspace_file(cloud.workspace.path(), "a/x.txt", b"cloud-edit").await;
    do_sync(
        &server.api,
        &cloud.db,
        cloud.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let head_before = server.api.get_head(WORKSPACE_ID).await.unwrap();

    let error = do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("filtered descendant conflict must block ancestor publication");
    assert!(
        error.to_string().contains("file/directory")
            || error.to_string().contains("path collision")
            || error.to_string().contains("traverses file")
            || error.to_string().contains("both file and directory"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        read_workspace_file(local.workspace.path(), "a").await,
        b"local-file"
    );
    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap(),
        head_before
    );
}

#[tokio::test]
async fn staged_download_failure_rolls_back_all_files_and_cache() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;

    write_workspace_file(first.workspace.path(), "one.txt", b"one-old").await;
    write_workspace_file(first.workspace.path(), "two.txt", b"two-old").await;
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
    let cache_before = second.db.get_cache_entries().await.unwrap();

    write_workspace_file(first.workspace.path(), "one.txt", b"one-new").await;
    write_workspace_file(first.workspace.path(), "two.txt", b"two-new").await;
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
    tokio::fs::write(
        state_path(second.workspace.path()).join("test-materialize-failpoint"),
        b"after-publish-1",
    )
    .await
    .unwrap();

    let error = do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("injected activation failure must abort the sync");
    assert!(error
        .to_string()
        .contains("injected materialization failure"));
    assert_eq!(
        read_workspace_file(second.workspace.path(), "one.txt").await,
        b"one-old"
    );
    assert_eq!(
        read_workspace_file(second.workspace.path(), "two.txt").await,
        b"two-old"
    );
    let cache_after = second.db.get_cache_entries().await.unwrap();
    for path in ["one.txt", "two.txt"] {
        assert_eq!(
            cache_after.get(path).unwrap().encrypted_hash,
            cache_before.get(path).unwrap().encrypted_hash
        );
    }
    assert!(std::fs::read_dir(second.workspace.path())
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));

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
    assert_eq!(
        read_workspace_file(second.workspace.path(), "one.txt").await,
        b"one-new"
    );
    assert_eq!(
        read_workspace_file(second.workspace.path(), "two.txt").await,
        b"two-new"
    );
}

#[tokio::test]
async fn failed_download_preserves_preexisting_empty_parent_directory() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "existing/file.txt", b"remote").await;
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

    let second = spawn_test_client_with_server(&server).await;
    tokio::fs::create_dir(second.workspace.path().join("existing"))
        .await
        .unwrap();
    tokio::fs::write(
        state_path(second.workspace.path()).join("test-materialize-failpoint"),
        b"after-publish-mutation-1",
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
    .expect_err("the injected namespace boundary must fail");

    let parent = second.workspace.path().join("existing");
    assert!(parent.is_dir());
    assert!(std::fs::read_dir(parent).unwrap().next().is_none());
}

#[tokio::test]
async fn failed_new_nested_download_removes_transaction_created_directories() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "new/nested/file.txt", b"remote").await;
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

    for point in ["after-publish-mutation-1"] {
        let second = spawn_test_client_with_server(&server).await;
        let cache_before = second.db.get_cache_entries().await.unwrap();
        assert!(cache_before.is_empty());
        tokio::fs::write(
            state_path(second.workspace.path()).join("test-materialize-failpoint"),
            point,
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
        .expect_err("the injected namespace boundary must fail");
        assert!(!second.workspace.path().join("new").exists());
        assert!(second.db.get_cache_entries().await.unwrap().is_empty());
        assert!(std::fs::read_dir(second.workspace.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".feanorfs-tmp-materialize-")));

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
        assert_eq!(
            read_workspace_file(second.workspace.path(), "new/nested/file.txt").await,
            b"remote"
        );
    }
}

#[tokio::test]
async fn every_activation_boundary_failure_restores_bytes_modes_and_cache() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "one.txt", b"version-0").await;
    write_workspace_file(first.workspace.path(), "two.txt", b"version-0").await;
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

    for (index, point) in [
        "after-backup-mutation-1",
        "after-backup-1",
        "before-publish-1",
        "after-publish-mutation-1",
        "after-mode-1",
        "before-cache",
    ]
    .into_iter()
    .enumerate()
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tokio::fs::set_permissions(
                second.workspace.path().join("one.txt"),
                std::fs::Permissions::from_mode(0o444),
            )
            .await
            .unwrap();
        }
        let before_one = read_workspace_file(second.workspace.path(), "one.txt").await;
        let before_two = read_workspace_file(second.workspace.path(), "two.txt").await;
        let cache_before = second.db.get_cache_entries().await.unwrap();
        let next = format!("version-{}", index + 1);
        write_workspace_file(first.workspace.path(), "one.txt", next.as_bytes()).await;
        write_workspace_file(first.workspace.path(), "two.txt", next.as_bytes()).await;
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
        tokio::fs::write(
            state_path(second.workspace.path()).join("test-materialize-failpoint"),
            point,
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
        .expect_err("the injected activation boundary must fail");
        assert_eq!(
            read_workspace_file(second.workspace.path(), "one.txt").await,
            before_one
        );
        assert_eq!(
            read_workspace_file(second.workspace.path(), "two.txt").await,
            before_two
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(second.workspace.path().join("one.txt"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o444
            );
        }
        let cache_after = second.db.get_cache_entries().await.unwrap();
        for path in ["one.txt", "two.txt"] {
            assert_eq!(
                cache_after.get(path).unwrap().encrypted_hash,
                cache_before.get(path).unwrap().encrypted_hash
            );
        }
        assert!(std::fs::read_dir(second.workspace.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".feanorfs-tmp-materialize-")));

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
        assert_eq!(
            read_workspace_file(second.workspace.path(), "one.txt").await,
            next.as_bytes()
        );
    }
}

#[tokio::test]
async fn edit_after_revalidation_aborts_and_rolls_back_without_loss() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    for path in ["a.txt", "z.txt"] {
        write_workspace_file(first.workspace.path(), path, b"old").await;
    }
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
    let cache_before = second.db.get_cache_entries().await.unwrap();
    for path in ["a.txt", "z.txt"] {
        write_workspace_file(first.workspace.path(), path, b"remote-new").await;
    }
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

    let second_path = second.workspace.path().to_path_buf();
    let second_state = state_path(&second_path);
    tokio::fs::write(second_state.join("test-sync-pause"), b"after-backup-1")
        .await
        .unwrap();
    let url = server.url.clone();
    let task_path = second_path.clone();
    let task_state = second_state.clone();
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
    let reached = second_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    write_workspace_file(&second_path, "z.txt", b"user-edit-during-stage").await;
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();
    let error = sync
        .await
        .unwrap()
        .expect_err("the later local edit must invalidate activation");
    assert!(error
        .to_string()
        .contains("changed while downloads were staged"));
    assert_eq!(read_workspace_file(&second_path, "a.txt").await, b"old");
    assert_eq!(
        read_workspace_file(&second_path, "z.txt").await,
        b"user-edit-during-stage"
    );
    let cache_after = second.db.get_cache_entries().await.unwrap();
    for path in ["a.txt", "z.txt"] {
        assert_eq!(
            cache_after.get(path).unwrap().encrypted_hash,
            cache_before.get(path).unwrap().encrypted_hash
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cat_rejects_unsafe_and_symlinked_paths_but_reads_safe_untracked_file() {
    use std::os::unix::fs::symlink;

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    write_workspace_file(client.workspace.path(), "plain.txt", b"plain").await;
    let safe = do_cat(
        &server.api,
        &client.db,
        client.workspace.path(),
        "plain.txt",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(safe.content, b"plain");
    assert!(safe.untracked);

    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    tokio::fs::write(&secret, b"outside-secret").await.unwrap();
    symlink(&secret, client.workspace.path().join("file-link")).unwrap();
    symlink(outside.path(), client.workspace.path().join("dir-link")).unwrap();
    for target in [
        "../secret.txt",
        secret.to_str().unwrap(),
        "file-link",
        "dir-link/secret.txt",
    ] {
        do_cat(
            &server.api,
            &client.db,
            client.workspace.path(),
            target,
            Some(TEST_PASSWORD),
        )
        .await
        .expect_err("cat must reject paths outside its descriptor-anchored workspace root");
    }
    assert_eq!(tokio::fs::read(secret).await.unwrap(), b"outside-secret");
}

#[cfg(unix)]
#[tokio::test]
async fn upload_source_replaced_by_symlink_after_scan_never_reaches_head() {
    use std::os::unix::fs::symlink;

    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
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
    let original_head = server.api.get_head(WORKSPACE_ID).await.unwrap();

    write_workspace_file(first.workspace.path(), "leak.txt", b"innocent").await;
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(outside.path().join("secret.txt"), b"outside-secret")
        .await
        .unwrap();
    let first_path = first.workspace.path().to_path_buf();
    let held = first.workspace.path().join("held-innocent.txt");
    let first_state = state_path(&first_path);
    tokio::fs::write(first_state.join("test-sync-pause"), b"after-negotiate")
        .await
        .unwrap();
    let url = server.url.clone();
    let task_path = first_path.clone();
    let task_state = first_state.clone();
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
    let reached = first_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::fs::rename(first_path.join("leak.txt"), &held)
        .await
        .unwrap();
    symlink(
        outside.path().join("secret.txt"),
        first_path.join("leak.txt"),
    )
    .unwrap();
    tokio::fs::remove_file(first_state.join("test-sync-pause"))
        .await
        .unwrap();

    sync.await
        .unwrap()
        .expect_err("a symlink substituted after scanning must abort upload");
    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap(),
        original_head
    );
    assert_eq!(
        tokio::fs::read(outside.path().join("secret.txt"))
            .await
            .unwrap(),
        b"outside-secret"
    );
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
    assert!(!second.workspace.path().join("leak.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_ancestor_inserted_before_backup_cannot_move_outside_file() {
    use std::os::unix::fs::symlink;

    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "dir/file.txt", b"old").await;
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
    write_workspace_file(first.workspace.path(), "dir/file.txt", b"remote-new").await;
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

    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(outside.path().join("file.txt"), b"outside-must-survive")
        .await
        .unwrap();
    let second_path = second.workspace.path().to_path_buf();
    let held = second.workspace.path().join("held-dir");
    let second_state = state_path(&second_path);
    tokio::fs::write(second_state.join("test-sync-pause"), b"before-backup-1")
        .await
        .unwrap();
    let url = server.url.clone();
    let task_path = second_path.clone();
    let task_state = second_state.clone();
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
    let reached = second_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::fs::rename(second_path.join("dir"), &held)
        .await
        .unwrap();
    symlink(outside.path(), second_path.join("dir")).unwrap();
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();

    let error = sync
        .await
        .unwrap()
        .expect_err("a symlink ancestor must abort backup");
    let message = format!("{error:#}");
    assert!(
        message.contains("back up local path")
            || message.contains("Not a directory")
            || message.contains("symlink"),
        "unexpected error: {message}"
    );
    assert_eq!(
        tokio::fs::read(outside.path().join("file.txt"))
            .await
            .unwrap(),
        b"outside-must-survive"
    );
    assert_eq!(
        tokio::fs::read(held.join("file.txt")).await.unwrap(),
        b"old"
    );
    assert!(std::fs::symlink_metadata(second_path.join("dir"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_ancestor_inserted_before_publication_cannot_escape_workspace() {
    use std::os::unix::fs::symlink;

    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
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
    write_workspace_file(first.workspace.path(), "dir/file.txt", b"remote").await;
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

    let outside = tempfile::tempdir().unwrap();
    let second_path = second.workspace.path().to_path_buf();
    let second_state = state_path(&second_path);
    tokio::fs::write(
        second_state.join("test-sync-pause"),
        b"after-final-validation-1",
    )
    .await
    .unwrap();
    let url = server.url.clone();
    let task_path = second_path.clone();
    let task_state = second_state.clone();
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
    let reached = second_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    symlink(outside.path(), second_path.join("dir")).unwrap();
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();
    let error = sync
        .await
        .unwrap()
        .expect_err("a symlink ancestor must abort publication");
    let message = format!("{error:#}");
    assert!(
        message.contains("no-follow destination ancestor"),
        "unexpected error: {message}"
    );
    assert!(!outside.path().join("file.txt").exists());
    assert!(std::fs::symlink_metadata(second_path.join("dir"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!second
        .db
        .get_cache_entries()
        .await
        .unwrap()
        .contains_key("dir/file.txt"));
}

#[tokio::test]
async fn rollback_drift_retains_user_edit_and_recovery_backups() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    for path in ["a.txt", "z.txt"] {
        write_workspace_file(first.workspace.path(), path, b"old").await;
    }
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
    for path in ["a.txt", "z.txt"] {
        write_workspace_file(first.workspace.path(), path, b"remote-new").await;
    }
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

    let second_path = second.workspace.path().to_path_buf();
    let second_state = state_path(&second_path);
    tokio::fs::write(second_state.join("test-sync-pause"), b"after-publish-1")
        .await
        .unwrap();
    let url = server.url.clone();
    let task_path = second_path.clone();
    let task_state = second_state.clone();
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
    let reached = second_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    write_workspace_file(&second_path, "a.txt", b"user-edit-after-publish").await;
    tokio::fs::write(
        second_state.join("test-materialize-failpoint"),
        b"after-publish-1",
    )
    .await
    .unwrap();
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();
    let error = sync
        .await
        .unwrap()
        .expect_err("rollback must refuse to delete the concurrent user edit");
    assert!(error.to_string().contains("rollback failed"));
    assert_eq!(
        read_workspace_file(&second_path, "a.txt").await,
        b"user-edit-after-publish"
    );
    let stages = std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".feanorfs-tmp-materialize-")
        })
        .collect::<Vec<_>>();
    assert_eq!(stages.len(), 1);
    assert_eq!(
        tokio::fs::read(stages[0].join("backup/a.txt"))
            .await
            .unwrap(),
        b"old"
    );
    assert_eq!(
        tokio::fs::read(stages[0].join("backup/z.txt"))
            .await
            .unwrap(),
        b"old"
    );
}

#[tokio::test]
async fn cancellation_before_stage_journal_does_not_block_next_sync() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "cancel.txt", b"old").await;
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
    write_workspace_file(first.workspace.path(), "cancel.txt", b"remote-new").await;
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

    let second_path = second.workspace.path().to_path_buf();
    let second_state = state_path(&second_path);
    tokio::fs::write(
        second_state.join("test-sync-pause"),
        b"stage-before-journal",
    )
    .await
    .unwrap();
    let url = server.url.clone();
    let task_path = second_path.clone();
    let task_state = second_state.clone();
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
    let reached = second_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    sync.abort();
    assert!(sync.await.unwrap_err().is_cancelled());
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();

    do_sync(
        &server.api,
        &second.db,
        &second_path,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        read_workspace_file(&second_path, "cancel.txt").await,
        b"remote-new"
    );
    assert!(std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
}

#[tokio::test]
async fn cancelled_activation_recovers_from_its_journal_on_next_sync() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    for path in ["a.txt", "z.txt"] {
        write_workspace_file(first.workspace.path(), path, b"old").await;
    }
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
    for path in ["a.txt", "z.txt"] {
        write_workspace_file(first.workspace.path(), path, b"remote-new").await;
    }
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

    let second_path = second.workspace.path().to_path_buf();
    let second_state = state_path(&second_path);
    tokio::fs::write(second_state.join("test-sync-pause"), b"after-publish-1")
        .await
        .unwrap();
    let url = server.url.clone();
    let task_path = second_path.clone();
    let task_state = second_state.clone();
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
    let reached = second_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    sync.abort();
    assert!(sync.await.unwrap_err().is_cancelled());
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();
    assert!(std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));

    do_sync(
        &server.api,
        &second.db,
        &second_path,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    for path in ["a.txt", "z.txt"] {
        assert_eq!(read_workspace_file(&second_path, path).await, b"remote-new");
    }
    assert!(std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
}

#[tokio::test]
async fn cache_post_commit_uncertainty_keeps_activated_journal_for_retry() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "file.txt", b"old").await;
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
    write_workspace_file(first.workspace.path(), "file.txt", b"new").await;
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

    let second_path = second.workspace.path().to_path_buf();
    let second_state = state_path(&second_path);
    tokio::fs::write(second_state.join("test-sync-pause"), b"before-cache")
        .await
        .unwrap();
    let url = server.url.clone();
    let task_path = second_path.clone();
    let task_state = second_state.clone();
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
    let reached = second_state.join("test-sync-pause-reached");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reached.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::fs::write(second_state.join("test-atomic-post-commit-fault"), b"1")
        .await
        .unwrap();
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();
    let error = sync
        .await
        .unwrap()
        .expect_err("post-commit durability uncertainty must surface");
    assert!(
        format!("{error:#}").contains("committed-but-durability-uncertain"),
        "unexpected error: {error:#}"
    );
    assert_eq!(read_workspace_file(&second_path, "file.txt").await, b"new");
    assert!(std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));

    do_sync(
        &server.api,
        &second.db,
        &second_path,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(read_workspace_file(&second_path, "file.txt").await, b"new");
    assert!(std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
}

#[tokio::test]
async fn interrupted_activation_journal_restores_backups_before_next_sync() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    write_workspace_file(client.workspace.path(), "recover.txt", b"old").await;
    do_sync(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let cache_before = client
        .db
        .get_cache_entries()
        .await
        .unwrap()
        .get("recover.txt")
        .unwrap()
        .encrypted_hash
        .clone();

    let stage = client
        .workspace
        .path()
        .join(".feanorfs-tmp-materialize-recovery-test");
    tokio::fs::create_dir_all(stage.join("backup"))
        .await
        .unwrap();
    tokio::fs::rename(
        client.workspace.path().join("recover.txt"),
        stage.join("backup/recover.txt"),
    )
    .await
    .unwrap();
    write_workspace_file(client.workspace.path(), "recover.txt", b"interrupted-new").await;
    let ciphertext =
        feanorfs_common::pack_bytes(b"interrupted-new", TEST_PASSWORD, "recover.txt").unwrap();
    let journal = serde_json::json!({
        "phase": "activating",
        "original_paths": ["recover.txt"],
        "downloads": [{
            "file": {
                "path": "recover.txt",
                "hash": feanorfs_common::hash_bytes(&ciphertext),
                "size": 15,
                "mtime": 0,
                "deleted": false,
                "mode": 0
            },
            "plaintext_hash": feanorfs_common::hash_bytes(b"interrupted-new"),
            "hydrated": true
        }],
        "delete_paths": []
    });
    tokio::fs::write(
        stage.join("journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .await
    .unwrap();

    do_sync(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        read_workspace_file(client.workspace.path(), "recover.txt").await,
        b"old"
    );
    assert!(!stage.exists());
    assert_eq!(
        client
            .db
            .get_cache_entries()
            .await
            .unwrap()
            .get("recover.txt")
            .unwrap()
            .encrypted_hash,
        cache_before
    );
}

#[cfg(unix)]
#[tokio::test]
async fn interrupted_recovery_rejects_symlink_ancestor_and_retains_every_copy() {
    use std::os::unix::fs::symlink;

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    write_workspace_file(client.workspace.path(), "dir/recover.txt", b"old").await;
    do_sync(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let stage = client
        .workspace
        .path()
        .join(".feanorfs-tmp-materialize-recovery-symlink-test");
    tokio::fs::create_dir_all(stage.join("backup/dir"))
        .await
        .unwrap();
    tokio::fs::rename(
        client.workspace.path().join("dir/recover.txt"),
        stage.join("backup/dir/recover.txt"),
    )
    .await
    .unwrap();
    write_workspace_file(
        client.workspace.path(),
        "dir/recover.txt",
        b"interrupted-new",
    )
    .await;
    let ciphertext =
        feanorfs_common::pack_bytes(b"interrupted-new", TEST_PASSWORD, "dir/recover.txt").unwrap();
    let journal = serde_json::json!({
        "phase": "activating",
        "original_paths": ["dir/recover.txt"],
        "downloads": [{
            "file": {
                "path": "dir/recover.txt",
                "hash": feanorfs_common::hash_bytes(&ciphertext),
                "size": 15,
                "mtime": 0,
                "deleted": false,
                "mode": 0
            },
            "plaintext_hash": feanorfs_common::hash_bytes(b"interrupted-new"),
            "hydrated": true
        }],
        "delete_paths": []
    });
    tokio::fs::write(
        stage.join("journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .await
    .unwrap();

    let held = client.workspace.path().join("held-publication");
    tokio::fs::rename(client.workspace.path().join("dir"), &held)
        .await
        .unwrap();
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(outside.path().join("recover.txt"), b"outside-must-survive")
        .await
        .unwrap();
    symlink(outside.path(), client.workspace.path().join("dir")).unwrap();

    do_sync(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("recovery must fail closed when an ancestor becomes a symlink");
    assert_eq!(
        tokio::fs::read(outside.path().join("recover.txt"))
            .await
            .unwrap(),
        b"outside-must-survive"
    );
    assert_eq!(
        tokio::fs::read(held.join("recover.txt")).await.unwrap(),
        b"interrupted-new"
    );
    assert_eq!(
        tokio::fs::read(stage.join("backup/dir/recover.txt"))
            .await
            .unwrap(),
        b"old"
    );
    assert!(stage.join("journal.json").exists());
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
async fn format_v3_cloud_resolution_does_not_publish_unrelated_local_file() {
    use feanorfs_client::{resolve_conflict, ResolveKeep, SyncCtx};

    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    for client in [&first, &second] {
        let mut config = feanorfs_client::load_config(client.workspace.path()).unwrap();
        config.format_version = 3;
        feanorfs_client::save_config(client.workspace.path(), &config).unwrap();
    }

    write_workspace_file(first.workspace.path(), "shared.txt", b"base").await;
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

    write_workspace_file(first.workspace.path(), "shared.txt", b"cloud").await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let path = first.workspace.path().join("shared.txt");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
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
    write_workspace_file(second.workspace.path(), "shared.txt", b"local").await;
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
    assert_eq!(
        second.db.list_pending_conflict_paths().await.unwrap(),
        vec!["shared.txt"]
    );

    write_workspace_file(second.workspace.path(), "local-only.txt", b"local-only").await;
    let config = feanorfs_client::load_config(second.workspace.path()).unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &second.db, second.workspace.path(), &config).unwrap();
    resolve_conflict(&ctx, "shared.txt", ResolveKeep::Cloud, None)
        .await
        .unwrap();

    assert_eq!(
        read_workspace_file(second.workspace.path(), "shared.txt").await,
        b"cloud"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_ne!(
            std::fs::metadata(second.workspace.path().join("shared.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0,
            "keep-cloud must restore the cloud leg's executable intent",
        );
    }
    assert_eq!(
        read_workspace_file(second.workspace.path(), "local-only.txt").await,
        b"local-only"
    );
    let status = do_status(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(status.upload_required, vec!["local-only.txt"]);
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

#[cfg(unix)]
#[tokio::test]
async fn hydrate_rejects_symlinked_ancestor_without_touching_outside_file() {
    use std::os::unix::fs::symlink;

    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    write_workspace_file(uploader.workspace.path(), "dir/lazy.txt", b"remote-content").await;
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

    let held = lazy.workspace.path().join("held-dir");
    tokio::fs::rename(lazy.workspace.path().join("dir"), &held)
        .await
        .unwrap();
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(outside.path().join("lazy.txt"), b"outside-must-survive")
        .await
        .unwrap();
    symlink(outside.path(), lazy.workspace.path().join("dir")).unwrap();

    do_hydrate(
        &server.api,
        &lazy.db,
        lazy.workspace.path(),
        Some("dir/lazy.txt".to_string()),
        Some(TEST_PASSWORD),
    )
    .await
    .expect_err("hydrate must not traverse a substituted symlink ancestor");
    assert_eq!(
        tokio::fs::read(outside.path().join("lazy.txt"))
            .await
            .unwrap(),
        b"outside-must-survive"
    );
    assert!(tokio::fs::read(held.join("lazy.txt"))
        .await
        .unwrap()
        .is_empty());
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

    write_workspace_file(&agent_path(base, "ci1"), "doc.txt", b"agent version").await;

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
        &agent_path(base, "portable-conflict"),
        "conflict.txt",
        b"agent edit",
    )
    .await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let path = agent_path(base, "portable-conflict").join("conflict.txt");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
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
    let TreeEntryKind::Conflict { modes, .. } = &root.entries[0].kind else {
        panic!("landed conflict was not encoded as a conflict entry");
    };
    #[cfg(unix)]
    assert_eq!(
        modes,
        &feanorfs_common::ConflictModes {
            base: 0,
            ours: feanorfs_common::EXECUTABLE_MODE,
            theirs: 0,
        }
    );

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
        feanorfs_agent_core::ResolveKeep::File,
        Some(std::path::Path::new(
            result.conflicts[0]
                .local_file
                .as_deref()
                .expect("materialized local conflict leg"),
        )),
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            resolved_root.entries[0].mode,
            feanorfs_common::EXECUTABLE_MODE
        );
        assert_ne!(
            std::fs::metadata(base.join("conflict.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    assert_eq!(
        read_workspace_file(base, "conflict.txt").await,
        b"agent edit"
    );
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
    std::fs::File::open(&shared_file)
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
    std::fs::File::open(&replaced_file)
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

    let original_base_ref_path = state_path(base).join("agents/replace/state/base-snapshot");
    let original_base_ref = tokio::fs::read(&original_base_ref_path).await.unwrap();
    write_workspace_file(&agent_path(base, "replace"), "task.txt", b"old").await;
    check_agent(
        base,
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "replace",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let runtime_path = state_path(base).join("agents/replace/state/runtime");
    let runtime_db = feanorfs_client::ClientDb::new(&runtime_path).await.unwrap();
    runtime_db
        .set_session_key("replace-rollback", "preserve-runtime")
        .await
        .unwrap();
    drop(runtime_db);
    let original_runtime = tokio::fs::read(runtime_path.join("local_state.json"))
        .await
        .unwrap();
    tokio::fs::write(
        state_path(base).join("test-spawn-failpoint-replace"),
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
    let agent_dir = agent_path(base, "replace");
    assert!(
        tokio::fs::try_exists(&agent_dir).await.unwrap(),
        "restored agent directory missing"
    );
    assert_eq!(read_workspace_file(&agent_dir, "task.txt").await, b"old");
    assert_eq!(
        tokio::fs::read(original_base_ref_path).await.unwrap(),
        original_base_ref,
        "rollback must restore the original base snapshot ref"
    );
    assert_eq!(
        tokio::fs::read(runtime_path.join("local_state.json"))
            .await
            .unwrap(),
        original_runtime,
        "rollback must restore the original agent runtime cache"
    );
    assert!(
        std::fs::read_dir(state_path(base).join("agents"))
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with("replace.replace-backup-")),
        "successful rollback must consume the replacement backup"
    );
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
    write_workspace_file(&agent_path(base, "add-add"), "shared.txt", b"agent version").await;

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

    let agent_base = agent_path(base, "rename");
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
async fn bulk_keep_local_resolves_and_publishes_all_workspace_conflicts() {
    use feanorfs_client::{conflicts, SyncCtx};

    let server = spawn_test_server().await;
    let local = spawn_test_client_with_server(&server).await;
    let remote = spawn_test_client_with_server(&server).await;
    for workspace in [local.workspace.path(), remote.workspace.path()] {
        let mut config = feanorfs_client::load_config(workspace).unwrap();
        config.format_version = 3;
        feanorfs_client::save_config(workspace, &config).unwrap();
    }

    for path in ["first.txt", "nested/second.txt"] {
        write_workspace_file(local.workspace.path(), path, b"base").await;
    }
    do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
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

    write_workspace_file(
        local.workspace.path(),
        "first.txt",
        b"authoritative local first",
    )
    .await;
    write_workspace_file(
        local.workspace.path(),
        "nested/second.txt",
        b"authoritative local second",
    )
    .await;
    write_workspace_file(remote.workspace.path(), "first.txt", b"remote first").await;
    write_workspace_file(
        remote.workspace.path(),
        "nested/second.txt",
        b"remote second",
    )
    .await;
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
    do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    let records = local.db.list_conflict_records().await.unwrap();
    assert_eq!(records.len(), 2);
    let conflict_dirs: std::collections::HashSet<_> = records
        .iter()
        .map(|record| record.conflict_dir.clone())
        .collect();
    let config = feanorfs_client::load_config(local.workspace.path()).unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &local.db, local.workspace.path(), &config).unwrap();
    let mut resolved = conflicts::resolve_all_local_conflicts(&ctx).await.unwrap();
    resolved.sort();
    assert_eq!(resolved, vec!["first.txt", "nested/second.txt"]);
    assert!(local.db.list_conflict_records().await.unwrap().is_empty());
    assert!(conflict_dirs
        .iter()
        .all(|path| !std::path::Path::new(path).exists()));
    let history = local.db.list_conflict_resolutions().await.unwrap();
    assert_eq!(history.len(), 2);
    assert!(history.iter().all(|entry| entry.method == "local"));

    let verifier = spawn_test_client_with_server(&server).await;
    let mut verifier_config = feanorfs_client::load_config(verifier.workspace.path()).unwrap();
    verifier_config.format_version = 3;
    feanorfs_client::save_config(verifier.workspace.path(), &verifier_config).unwrap();
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
        read_workspace_file(verifier.workspace.path(), "first.txt").await,
        b"authoritative local first"
    );
    assert_eq!(
        read_workspace_file(verifier.workspace.path(), "nested/second.txt").await,
        b"authoritative local second"
    );

    let after = do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(after.uploads, 0);
    assert_eq!(after.downloads, 0);
}

#[tokio::test]
async fn bulk_keep_cloud_materializes_and_publishes_all_mirror_versions() {
    use feanorfs_client::{conflicts, SyncCtx};

    let server = spawn_test_server().await;
    let local = spawn_test_client_with_server(&server).await;
    let remote = spawn_test_client_with_server(&server).await;
    for workspace in [local.workspace.path(), remote.workspace.path()] {
        let mut config = feanorfs_client::load_config(workspace).unwrap();
        config.format_version = 3;
        feanorfs_client::save_config(workspace, &config).unwrap();
    }

    for path in ["first.txt", "second.txt"] {
        write_workspace_file(local.workspace.path(), path, b"base").await;
    }
    do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
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

    write_workspace_file(local.workspace.path(), "first.txt", b"local first").await;
    write_workspace_file(local.workspace.path(), "second.txt", b"local second").await;
    write_workspace_file(remote.workspace.path(), "first.txt", b"mirror first").await;
    tokio::fs::remove_file(remote.workspace.path().join("second.txt"))
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
    do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();

    assert_eq!(local.db.list_conflict_records().await.unwrap().len(), 2);
    let config = feanorfs_client::load_config(local.workspace.path()).unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &local.db, local.workspace.path(), &config).unwrap();
    let mut resolved = conflicts::resolve_all_cloud_conflicts(&ctx).await.unwrap();
    resolved.sort();
    assert_eq!(resolved, vec!["first.txt", "second.txt"]);
    assert_eq!(
        read_workspace_file(local.workspace.path(), "first.txt").await,
        b"mirror first"
    );
    assert!(!local.workspace.path().join("second.txt").exists());
    assert!(local
        .db
        .list_conflict_resolutions()
        .await
        .unwrap()
        .iter()
        .all(|resolution| resolution.method == "cloud"));

    let after = do_sync(
        &server.api,
        &local.db,
        local.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(after.uploads, 0);
    assert_eq!(after.downloads, 0);
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
            "pending",
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

    write_workspace_file(&agent_path(base, "ci1"), "doc.txt", b"agent version").await;

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
    let db = feanorfs_client::ClientDb::new(state_path(join_base))
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
async fn join_preflight_classifies_nonempty_folder_without_mutating_it() {
    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    let mut uploader_config = feanorfs_client::load_config(uploader.workspace.path()).unwrap();
    uploader_config.format_version = 3;
    feanorfs_client::save_config(uploader.workspace.path(), &uploader_config).unwrap();
    write_workspace_file(uploader.workspace.path(), "remote-only.txt", b"remote").await;
    write_workspace_file(uploader.workspace.path(), "same.txt", b"same").await;
    write_workspace_file(uploader.workspace.path(), "conflict.txt", b"remote").await;
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

    let destination = tempfile::tempdir().unwrap();
    write_workspace_file(destination.path(), "local-only.txt", b"local").await;
    write_workspace_file(destination.path(), "same.txt", b"same").await;
    write_workspace_file(destination.path(), "conflict.txt", b"local").await;
    let destination_state = state_path(destination.path());
    std::fs::write(destination_state.join("ignore"), b"local-cache/\n").unwrap();
    let before = std::fs::read(destination_state.join("ignore")).unwrap();

    let preview = feanorfs_client::preview_join(
        destination.path(),
        &feanorfs_client::WorkspaceInvite {
            server_url: server.url.clone(),
            workspace_id: WORKSPACE_ID.into(),
            server_token: None,
            encryption_key: TEST_PASSWORD.into(),
            tls_ca_pem: None,
            hub_local: false,
            relay: None,
            ignore_policy: Some("remote-cache/\n".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(preview.local_only.count, 1);
    assert_eq!(preview.remote_only.count, 1);
    assert_eq!(preview.same.count, 1);
    assert_eq!(preview.conflicts.count, 1);
    assert!(preview.ignore_policy_differs);
    assert!(preview.needs_confirmation());
    assert!(!destination.path().join(".feanorfs").exists());
    assert!(!destination.path().join(".feanorfsignore").exists());
    assert_eq!(
        std::fs::read(destination_state.join("ignore")).unwrap(),
        before
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

#[tokio::test]
#[ignore = "101 MiB authenticated chunk transport product proof"]
async fn format_v3_syncs_file_above_100_mib_through_authenticated_chunks() {
    use std::io::Seek as _;
    use std::io::Write as _;

    const SIZE: u64 = 101 * 1024 * 1024 + 17;
    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
    let mut uploader_config = feanorfs_client::load_config(uploader.workspace.path()).unwrap();
    uploader_config.format_version = 3;
    feanorfs_client::save_config(uploader.workspace.path(), &uploader_config).unwrap();

    let source = uploader.workspace.path().join("large.bin");
    let mut file = std::fs::File::create(&source).unwrap();
    file.set_len(SIZE).unwrap();
    file.seek(std::io::SeekFrom::Start(SIZE - 17)).unwrap();
    file.write_all(b"authenticated-end").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let uploaded = do_sync(
        &server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(uploaded.uploads, 1);

    let downloader = spawn_test_client_with_server(&server).await;
    let mut downloader_config = feanorfs_client::load_config(downloader.workspace.path()).unwrap();
    downloader_config.format_version = 3;
    feanorfs_client::save_config(downloader.workspace.path(), &downloader_config).unwrap();
    let downloaded = do_sync(
        &server.api,
        &downloader.db,
        downloader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(downloaded.downloads, 1);
    let destination = downloader.workspace.path().join("large.bin");
    assert_eq!(std::fs::metadata(&destination).unwrap().len(), SIZE);

    let mut source_file = std::fs::File::open(source).unwrap();
    let mut destination_file = std::fs::File::open(destination).unwrap();
    let mut source_hasher = blake3::Hasher::new();
    source_hasher.update_reader(&mut source_file).unwrap();
    let mut destination_hasher = blake3::Hasher::new();
    destination_hasher
        .update_reader(&mut destination_file)
        .unwrap();
    assert_eq!(source_hasher.finalize(), destination_hasher.finalize());
}

#[tokio::test]
async fn tray_status_and_pause() {
    use feanorfs_client::{do_tray_status_with, is_paused, set_paused};

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();

    set_paused(base, true).unwrap();
    assert!(is_paused(base));

    let status = do_tray_status_with(base, true).await.unwrap();
    assert!(status.paused);
    assert_eq!(status.mirror_state, "idle");
    assert!(status.pending_conflicts.is_empty());

    set_paused(base, false).unwrap();
    assert!(!is_paused(base));
}

#[tokio::test]
async fn tray_status_lists_working_agent() {
    use feanorfs_client::{do_tray_status_with, spawn_agent};

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

    write_workspace_file(&agent_path(base, "ci1"), "task.txt", b"agent edit").await;

    let status = do_tray_status_with(base, true).await.unwrap();
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
