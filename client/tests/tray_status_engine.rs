feanorfs_test_support::isolate_test_process!();

// Regression: routine tray polling must be constant-cost.
//
// The managed worker publishes a bounded secret-free status snapshot after
// each sync; `do_tray_status` reads it without scanning the project or taking
// the sync lock. These tests prove polling cannot delay file-change
// synchronization even in a large workspace.

mod support;

use feanorfs_client::{
    do_push_only, do_tray_status, do_tray_status_with, invalidate_worker_status, load_config,
    publish_worker_status, register_workspace, save_config, MirrorState, TrayOverviewResult,
};
use std::time::Duration;
use support::{
    spawn_test_client_with_server, spawn_test_server, write_workspace_file, TEST_PASSWORD,
    WORKSPACE_ID,
};

const LARGE_WORKSPACE_FILES: usize = 1000;

/// Decoy files parked in an unreadable sentinel directory.
/// A refresh that scans the workspace with even one file per directory entry
/// must touch them; a constant-cost refresh never does.
const SENTINEL_DIR_FILES: usize = 2000;
const SENTINEL_ROOT_FILES: usize = 100;

fn make_v3(client: &support::TestClient) -> feanorfs_client::Config {
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    config
}

#[tokio::test]
// The sync lock is held deliberately across awaits to prove snapshot polling
// never waits on it; that is the point of the regression, not a leak.
#[allow(clippy::await_holding_lock)]
async fn tray_polling_reads_worker_snapshot_without_lock_or_scan() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let _config = make_v3(&client);
    let root = client.workspace.path();

    for i in 0..LARGE_WORKSPACE_FILES {
        write_workspace_file(root, &format!("src/f{i:04}.txt"), b"payload").await;
    }
    let pushed = do_push_only(
        &server.api,
        &client.db,
        root,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(pushed.uploads, LARGE_WORKSPACE_FILES as u32);

    // The worker publishes its snapshot after the sync completes.
    publish_worker_status(root, &MirrorState::Idle, &client.db)
        .await
        .unwrap();

    // Hold the sync lock exclusively: any tray poll that waited on it would
    // stall here, and any poll that scanned would try to take it.
    let _sync_guard = feanorfs_client::lock::try_acquire_sync_lock(root, Duration::from_secs(5))
        .await
        .expect("test must own the sync lock");

    let status = do_tray_status(root)
        .await
        .expect("snapshot poll must not wait");
    assert_eq!(status.mirror_state, "idle");
    assert!(status.pending_conflicts.is_empty());

    // A fresh disk change must NOT be visible through the snapshot path: that
    // proves the poll never scanned the project.
    write_workspace_file(root, "untracked-after-snapshot.txt", b"change").await;
    let again = do_tray_status(root).await.unwrap();
    assert_eq!(
        again.mirror_state, "idle",
        "snapshot polls must reflect the last worker sync, not disk state"
    );

    // The explicit fresh path must remain available: it scans the project
    // and therefore sees the untracked file, unlike routine snapshot polls.
    // Release the deliberately held lock first: the production sync lock is
    // non-reentrant, and the explicit fresh path intentionally acquires it.
    drop(_sync_guard);
    let fresh = do_tray_status_with(root, true).await.unwrap();
    assert_eq!(fresh.mirror_state, "out_of_sync");
}

#[tokio::test]
async fn worker_snapshot_refreshes_after_next_sync() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let _config = make_v3(&client);
    let root = client.workspace.path();
    write_workspace_file(root, "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        root,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    publish_worker_status(root, &MirrorState::OutOfSync, &client.db)
        .await
        .unwrap();
    assert_eq!(
        do_tray_status(root).await.unwrap().mirror_state,
        "out_of_sync"
    );

    // Next sync completes normally (polling never delayed it), then the
    // worker publishes the refreshed snapshot.
    let result = feanorfs_client::do_sync(
        &server.api,
        &client.db,
        root,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(result.mirror_state, MirrorState::Idle);
    publish_worker_status(root, &MirrorState::Idle, &client.db)
        .await
        .unwrap();
    assert_eq!(do_tray_status(root).await.unwrap().mirror_state, "idle");
}

#[tokio::test]
async fn missing_worker_snapshot_never_falls_back_to_a_project_scan() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let _config = make_v3(&client);
    let root = client.workspace.path();
    write_workspace_file(root, "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        root,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();

    invalidate_worker_status(root);
    write_workspace_file(root, "after-snapshot.txt", b"not yet synced").await;

    let routine = do_tray_status(root).await.unwrap();
    assert_eq!(
        routine.mirror_state, "syncing",
        "routine status must remain constant-cost when the worker snapshot is absent"
    );
    assert_eq!(
        do_tray_status_with(root, true).await.unwrap().mirror_state,
        "out_of_sync",
        "only the explicit fresh path may scan the project"
    );
}

#[tokio::test]
async fn worker_snapshot_poll_does_not_open_the_workspace_cache() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let _config = make_v3(&client);
    let root = client.workspace.path();
    publish_worker_status(root, &MirrorState::Idle, &client.db)
        .await
        .unwrap();

    let state = feanorfs_agent_core::ensure_workspace_state(root).unwrap();
    std::fs::write(state.join("local_state.json"), b"not valid cache JSON").unwrap();

    let status = do_tray_status(root)
        .await
        .expect("routine snapshot polling must not parse local_state.json");
    assert_eq!(status.mirror_state, "idle");
}

#[tokio::test]
async fn worker_snapshot_poll_does_not_resolve_workspace_credentials() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    let root = client.workspace.path();
    publish_worker_status(root, &MirrorState::Idle, &client.db)
        .await
        .unwrap();

    let mut redacted = serde_json::to_value(&config).unwrap();
    let object = redacted.as_object_mut().unwrap();
    object.remove("encryption_password");
    object.remove("server_password");
    object.insert("credential_store".into(), "os".into());
    object.insert("credential_id".into(), "fsc1-unavailable-in-test".into());
    let state = feanorfs_agent_core::ensure_workspace_state(root).unwrap();
    std::fs::write(
        state.join("config.json"),
        serde_json::to_vec(&redacted).unwrap(),
    )
    .unwrap();

    assert!(
        load_config(root).is_err(),
        "the fixture must prove full loading would touch protected credentials"
    );
    let status = do_tray_status(root)
        .await
        .expect("routine polling needs only public config metadata");
    assert_eq!(status.workspace_id, config.workspace_id);
    assert_eq!(status.mirror_state, "idle");
}

#[tokio::test]
async fn snapshot_poll_does_not_require_a_reachable_hub() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let _config = make_v3(&client);
    let root = client.workspace.path();
    write_workspace_file(root, "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        root,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    drop(server);

    publish_worker_status(root, &MirrorState::Offline, &client.db)
        .await
        .unwrap();
    // No hub: a scanning poll would fail; the snapshot poll still answers.
    let status = do_tray_status(root).await.unwrap();
    assert_eq!(status.mirror_state, "offline");
}

#[tokio::test]
async fn tray_overview_cli_emits_one_combined_document() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    let root = client.workspace.path();
    publish_worker_status(root, &MirrorState::Idle, &client.db)
        .await
        .unwrap();
    register_workspace(root).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args(["--json", "tray", "overview"])
        .current_dir(root)
        .env(
            "FEANORFS_HOME",
            std::env::var_os("FEANORFS_HOME").expect("isolated test profile"),
        )
        .output()
        .expect("run tray overview");
    assert!(
        output.status.success(),
        "tray overview failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let overview: TrayOverviewResult =
        serde_json::from_slice(&output.stdout).expect("parse one overview document");
    assert_eq!(overview.status.workspace_id, config.workspace_id);
    assert_eq!(overview.status.mirror_state, "idle");
    assert!(overview.recent.is_some_and(|recent| {
        recent
            .workspaces
            .iter()
            .any(|workspace| workspace.workspace_id == config.workspace_id)
    }));
}

/// Routine tray refresh must read a fixed-size/bounded set
/// of status files (`worker-status.json`, supervisor snapshots/agent cache,
/// the recent registry) independent of workspace size.
///
/// The fixture arms a permission sentinel: after the bounded status state is
/// published, the project tree is stuffed with decoy files and then made
/// unreadable. Any refresh that grows with workspace size — scanning the
/// tree, reading conflict records from project state, counting files — must
/// open the decoys and fails loudly (permission denied). A constant-cost
/// refresh never touches them: it returns the exact baseline document, and
/// the aggregate `tray overview` (routine status + one bounded recent-store
/// read) works too.
#[cfg(unix)]
#[tokio::test]
async fn tray_refresh_reads_only_bounded_status_state_regardless_of_workspace_size() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    let root = client.workspace.path();

    // Baseline: one synced file, a published worker snapshot, and a recent
    // registry entry. The interval-gated maintenance stamp is already warm
    // from fixture setup, so no one-time workspace walk can fire while the
    // sentinel permissions are armed.
    write_workspace_file(root, "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        root,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    publish_worker_status(root, &MirrorState::Idle, &client.db)
        .await
        .unwrap();
    register_workspace(root).unwrap();
    let baseline = serde_json::to_value(do_tray_status(root).await.unwrap()).unwrap();

    // Sentinel-dir fixture: a bucket of decoys plus root-level decoys. Any
    // scan must traverse them; once the sentinel permissions are applied,
    // every such open fails loudly instead of silently scaling.
    let decoys = root.join("decoys");
    std::fs::create_dir_all(&decoys).unwrap();
    for i in 0..SENTINEL_DIR_FILES {
        write_workspace_file(&decoys, &format!("f{i:05}.txt"), b"decoy").await;
    }
    for i in 0..SENTINEL_ROOT_FILES {
        write_workspace_file(root, &format!("decoy-root-{i:03}.txt"), b"decoy").await;
    }

    // Restore permissions before the TempDir cleanup so the fixture can be
    // removed; runs on unwind as well as success.
    struct RestorePermissions(Vec<PathBuf>);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
            }
        }
    }
    let restore = RestorePermissions(
        std::iter::once(decoys.clone())
            .chain((0..SENTINEL_ROOT_FILES).map(|i| root.join(format!("decoy-root-{i:03}.txt"))))
            .collect(),
    );

    std::fs::set_permissions(&decoys, std::fs::Permissions::from_mode(0o000)).unwrap();
    for i in 0..SENTINEL_ROOT_FILES {
        std::fs::set_permissions(
            root.join(format!("decoy-root-{i:03}.txt")),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }

    // Routine refresh: succeeds with byte-identical output because it reads
    // only the bounded status state, never the workspace tree. Growing the
    // workspace (SENTINEL_DIR_FILES + SENTINEL_ROOT_FILES extra files) must
    // change nothing.
    let again = serde_json::to_value(do_tray_status(root).await.unwrap()).unwrap();
    assert_eq!(again, baseline);

    // The aggregate overview (routine status plus one bounded recent-store
    // read) also succeeds with the project tree unreadable.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args(["--json", "tray", "overview"])
        .current_dir(root)
        .env(
            "FEANORFS_HOME",
            std::env::var_os("FEANORFS_HOME").expect("isolated test profile"),
        )
        .output()
        .expect("run tray overview");
    assert!(
        output.status.success(),
        "tray overview must not scan the workspace: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let overview: TrayOverviewResult =
        serde_json::from_slice(&output.stdout).expect("parse one overview document");
    assert_eq!(overview.status.workspace_id, config.workspace_id);
    assert_eq!(overview.status.mirror_state, "idle");

    drop(restore);
}
