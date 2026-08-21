feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::do_sync;

use support::{
    agent_path, spawn_test_client_with_server, spawn_test_server, write_workspace_file,
    TEST_PASSWORD, WORKSPACE_ID,
};

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
