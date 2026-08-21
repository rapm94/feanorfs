feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{do_cat, do_hydrate, do_pull_only, do_sync};

use support::{
    read_workspace_file, spawn_test_client_with_server, spawn_test_server, state_path,
    write_workspace_file, TEST_PASSWORD, WORKSPACE_ID,
};

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

#[cfg(windows)]
#[tokio::test]
async fn windows_user_parent_created_after_validation_survives_rollback() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
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

    // The parent was absent when validation paused, but belongs to the user
    // when the publication helper observes it. Rollback must not infer that
    // it was transaction-created merely because it is now present.
    tokio::fs::create_dir_all(second_path.join("new/nested"))
        .await
        .unwrap();
    tokio::fs::write(
        second_state.join("test-materialize-failpoint"),
        b"after-publish-mutation-1",
    )
    .await
    .unwrap();
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();
    sync.await
        .unwrap()
        .expect_err("injected publication failure must abort the sync");

    assert!(second_path.join("new/nested").is_dir());
    assert!(std::fs::read_dir(second_path.join("new/nested"))
        .unwrap()
        .next()
        .is_none());
    assert!(std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_directory_to_file_rollback_restores_removed_parent() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "d/f", b"old").await;
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
    tokio::fs::remove_file(first.workspace.path().join("d/f"))
        .await
        .unwrap();
    tokio::fs::remove_dir(first.workspace.path().join("d"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "d", b"remote-new").await;
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

    let original_file = second.workspace.path().join("d/f");
    let mut readonly_permissions = std::fs::metadata(&original_file).unwrap().permissions();
    readonly_permissions.set_readonly(true);
    std::fs::set_permissions(&original_file, readonly_permissions).unwrap();
    let before_cache = second.db.get_cache_entries().await.unwrap();
    let force_basic_delete = state_path(second.workspace.path()).join("test-force-basic-delete");
    tokio::fs::write(&force_basic_delete, b"1").await.unwrap();
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
    .expect_err("directory-to-file publication failure must roll back");

    assert_eq!(
        read_workspace_file(second.workspace.path(), "d/f").await,
        b"old"
    );
    assert!(second.workspace.path().join("d").is_dir());
    assert!(!second.workspace.path().join("d").is_file());
    assert!(std::fs::metadata(&original_file)
        .unwrap()
        .permissions()
        .readonly());
    assert!(!std::fs::read_dir(second.workspace.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
    let cache_after = second.db.get_cache_entries().await.unwrap();
    assert_eq!(
        cache_after.get("d/f").map(|entry| &entry.encrypted_hash),
        before_cache.get("d/f").map(|entry| &entry.encrypted_hash)
    );
    assert!(!cache_after.contains_key("d"));
    tokio::fs::remove_file(force_basic_delete).await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_directory_to_file_recovery_restores_removed_parent_before_retry() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "d/f", b"old").await;
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
    tokio::fs::remove_file(first.workspace.path().join("d/f"))
        .await
        .unwrap();
    tokio::fs::remove_dir(first.workspace.path().join("d"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "d", b"remote-new").await;
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

    let before_cache = second.db.get_cache_entries().await.unwrap();
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

    // Force the recovery pass to complete before negotiation. This leaves the
    // restored pre-transaction bytes observable while proving the journal is
    // no longer a wedge if the next hub attempt is unavailable.
    let unavailable = feanorfs_client::ApiClient::new("http://127.0.0.1:1", None);
    do_sync(
        &unavailable,
        &second.db,
        &second_path,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("recovery should finish before the unavailable retry fails");
    assert_eq!(read_workspace_file(&second_path, "d/f").await, b"old");
    assert!(second_path.join("d").is_dir());
    let cache_after = second.db.get_cache_entries().await.unwrap();
    assert_eq!(
        cache_after.get("d/f").map(|entry| &entry.encrypted_hash),
        before_cache.get("d/f").map(|entry| &entry.encrypted_hash)
    );
    assert!(!cache_after.contains_key("d"));
    assert!(std::fs::read_dir(&second_path)
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".feanorfs-tmp-materialize-")));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_directory_to_file_recovery_before_first_publication_restores_parent() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "d/f", b"old").await;
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
    tokio::fs::remove_file(first.workspace.path().join("d/f"))
        .await
        .unwrap();
    tokio::fs::remove_dir(first.workspace.path().join("d"))
        .await
        .unwrap();
    write_workspace_file(first.workspace.path(), "d", b"remote-new").await;
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

    let before_cache = second.db.get_cache_entries().await.unwrap();
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
        .expect("cancelled pre-publication stage");
    let journal: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(stage.join("journal.json")).await.unwrap())
            .unwrap();
    assert_eq!(journal["publication_progress_recorded"], true);
    assert!(journal["published_paths"].is_null());
    assert!(journal["publishing_path"].is_null());

    sync.abort();
    assert!(sync.await.unwrap_err().is_cancelled());
    tokio::fs::remove_file(second_state.join("test-sync-pause"))
        .await
        .unwrap();
    assert!(stage.exists());

    let unavailable = feanorfs_client::ApiClient::new("http://127.0.0.1:1", None);
    do_sync(
        &unavailable,
        &second.db,
        &second_path,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("recovery should finish before the unavailable retry fails");
    assert_eq!(read_workspace_file(&second_path, "d/f").await, b"old");
    assert!(second_path.join("d").is_dir());
    assert!(!second_path.join("d").is_file());
    let cache_after = second.db.get_cache_entries().await.unwrap();
    assert_eq!(
        cache_after.get("d/f").map(|entry| &entry.encrypted_hash),
        before_cache.get("d/f").map(|entry| &entry.encrypted_hash)
    );
    assert!(!cache_after.contains_key("d"));
    assert!(!stage.exists());
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

#[cfg(windows)]
#[tokio::test]
async fn windows_cancelled_activation_refuses_in_place_edit_during_recovery() {
    let server = spawn_test_server().await;
    let first = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    write_workspace_file(first.workspace.path(), "a.txt", b"old").await;
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
    write_workspace_file(first.workspace.path(), "a.txt", b"remote-new").await;
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

    // The destination and stage/new/a.txt are hard links. An in-place edit of
    // either path changes both, so identity alone must not authorize recovery.
    write_workspace_file(&second_path, "a.txt", b"user-edit-before-recovery").await;
    let error = do_sync(
        &server.api,
        &second.db,
        &second_path,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .expect_err("recovery must refuse an in-place edit of the published hard link");
    assert!(format!("{error:#}").contains("changed"));
    assert_eq!(
        read_workspace_file(&second_path, "a.txt").await,
        b"user-edit-before-recovery"
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
}

#[cfg(windows)]
#[tokio::test]
async fn old_journal_nested_ancestor_survives_recovery() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    write_workspace_file(client.workspace.path(), "nested/recover.txt", b"old").await;
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
        .join(".feanorfs-tmp-materialize-old-nested-test");
    tokio::fs::create_dir_all(stage.join("backup/nested"))
        .await
        .unwrap();
    tokio::fs::rename(
        client.workspace.path().join("nested/recover.txt"),
        stage.join("backup/nested/recover.txt"),
    )
    .await
    .unwrap();
    // Replace the now-empty ancestor with a user-owned directory.  An old
    // journal has no directory proofs, so recovery must never remove it.
    tokio::fs::remove_dir(client.workspace.path().join("nested"))
        .await
        .unwrap();
    tokio::fs::create_dir(client.workspace.path().join("nested"))
        .await
        .unwrap();
    write_workspace_file(
        client.workspace.path(),
        "nested/recover.txt",
        b"interrupted-new",
    )
    .await;
    tokio::fs::create_dir_all(stage.join("new/nested"))
        .await
        .unwrap();
    tokio::fs::hard_link(
        client.workspace.path().join("nested/recover.txt"),
        stage.join("new/nested/recover.txt"),
    )
    .await
    .unwrap();
    let ciphertext =
        feanorfs_common::pack_bytes(b"interrupted-new", TEST_PASSWORD, "nested/recover.txt")
            .unwrap();
    let journal = serde_json::json!({
        "phase": "activating",
        "original_paths": ["nested/recover.txt"],
        "downloads": [{
            "file": {
                "path": "nested/recover.txt",
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
        read_workspace_file(client.workspace.path(), "nested/recover.txt").await,
        b"old"
    );
    assert!(client.workspace.path().join("nested").is_dir());
    assert!(!stage.exists());
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
