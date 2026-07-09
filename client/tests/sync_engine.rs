mod support;

use feanorfs_client::{commit_agent, do_pull_only, do_push_only, do_sync, land_agent, spawn_agent};
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
async fn sync_detects_concurrent_workspace_edit_without_silent_overwrite() {
    use feanorfs_client::conflicts;

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
    let pending = conflicts::pending_conflict_paths(&client_a.db)
        .await
        .unwrap();
    assert!(conflicts::conflicts_pending(Some(&pending)));
    assert!(pending.contains("notes.txt"));
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

#[tokio::test]
async fn wrong_encryption_key_fails_decrypt() {
    use feanorfs_client::load_config;

    let server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&server).await;
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
    cfg.format_version = 2;
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
async fn migrate_sets_format_v2_and_reseals_server_blobs() {
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
    assert_eq!(cfg.format_version, 2);

    let verifier = spawn_test_client_with_server(&server).await;
    let mut verifier_cfg = load_config(verifier.workspace.path()).unwrap();
    verifier_cfg.format_version = 2;
    feanorfs_client::save_config(verifier.workspace.path(), &verifier_cfg).unwrap();
    let result = do_pull_only(
        &server.api,
        &verifier.db,
        verifier.workspace.path(),
        WORKSPACE_ID,
        verifier_cfg.encryption_password.as_deref(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        result.downloads, 1,
        "v2 client must decrypt the migrated file"
    );
    assert_eq!(
        read_workspace_file(verifier.workspace.path(), "mig.txt").await,
        b"migrate me"
    );
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
        format_version: 2,
        hub_local: false,
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
        format_version: 2,
        hub_local: true,
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
async fn tray_status_and_pause() {
    use feanorfs_client::{
        do_tray_status, is_paused, list_recent_workspaces, register_workspace, set_paused,
    };

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let base = client.workspace.path();

    register_workspace(base).unwrap();
    let recent = list_recent_workspaces().unwrap();
    assert!(recent.workspaces.iter().any(|w| w.path.contains("tmp")));

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
