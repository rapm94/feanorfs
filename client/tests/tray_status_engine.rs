//! Regression: routine tray polling must be constant-cost.
//!
//! The managed worker publishes a bounded secret-free status snapshot after
//! each sync; `do_tray_status` reads it without scanning the project or taking
//! the sync lock. These tests prove polling cannot delay file-change
//! synchronization even in a large workspace.

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{
    do_push_only, do_tray_status, do_tray_status_with, invalidate_worker_status, load_config,
    publish_worker_status, save_config, MirrorState,
};
use std::time::Duration;
use support::{
    spawn_test_client_with_server, spawn_test_server, write_workspace_file, TEST_PASSWORD,
    WORKSPACE_ID,
};

const LARGE_WORKSPACE_FILES: usize = 1000;

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
