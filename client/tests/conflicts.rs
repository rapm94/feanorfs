feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{
    commit_agent, do_pull_only, do_push_only, do_status, do_sync, land_agent, spawn_agent,
};

use support::{
    agent_path, read_workspace_file, spawn_test_client, spawn_test_client_with_server,
    spawn_test_server, state_path, write_workspace_file, TEST_PASSWORD, WORKSPACE_ID,
};

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
        mesh: None,
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
            mesh: None,
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
