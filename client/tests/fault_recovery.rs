feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{check_agent, do_sync, spawn_agent};

use support::{
    agent_path, read_workspace_file, spawn_test_client_with_server, spawn_test_server, state_path,
    write_workspace_file, TEST_PASSWORD, WORKSPACE_ID,
};

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

    #[cfg(windows)]
    {
        let stage = std::fs::read_dir(&second_path)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(".feanorfs-tmp-materialize-")
                })
            })
            .expect("cancelled activation stage");
        let journal: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(stage.join("journal.json")).await.unwrap())
                .unwrap();
        assert_eq!(journal["published_paths"], serde_json::json!(["a.txt"]));
        assert!(journal["publishing_path"].is_null());
    }

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

    #[cfg(windows)]
    let unrelated_parent = client.workspace.path().join("unrelated-parent");
    #[cfg(windows)]
    tokio::fs::create_dir(&unrelated_parent).await.unwrap();

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
    #[cfg(windows)]
    {
        // Preserve the real interrupted topology: recovery only trusts this
        // publication when stage/new and the destination are the same file
        // identity.  The old-journal compatibility case omits only the
        // created_directories field.
        tokio::fs::create_dir_all(stage.join("new")).await.unwrap();
        tokio::fs::hard_link(
            client.workspace.path().join("recover.txt"),
            stage.join("new/recover.txt"),
        )
        .await
        .unwrap();
    }
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
    #[cfg(windows)]
    assert!(
        unrelated_parent.is_dir(),
        "old journals must not own unknown paths"
    );

    #[cfg(windows)]
    {
        let bad_stage = client
            .workspace
            .path()
            .join(".feanorfs-tmp-materialize-unrelated-proof");
        tokio::fs::create_dir(&bad_stage).await.unwrap();
        let bad_journal = serde_json::json!({
            "phase": "activating",
            "original_paths": [],
            "downloads": [],
            "delete_paths": [],
            "created_directories": [{"path": "unrelated-parent", "identity": null}]
        });
        tokio::fs::write(
            bad_stage.join("journal.json"),
            serde_json::to_vec(&bad_journal).unwrap(),
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
        let quarantine = client
            .workspace
            .path()
            .join(".feanorfs-tmp-recovery-materialize-unrelated-proof");
        assert!(!bad_stage.exists());
        assert!(quarantine.is_dir());
        assert!(unrelated_parent.is_dir());
    }
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
