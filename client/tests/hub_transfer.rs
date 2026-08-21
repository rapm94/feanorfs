feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{do_sync, transfer_hub};

use support::{spawn_test_client_with_server, spawn_test_server, TEST_PASSWORD, WORKSPACE_ID};

async fn configure_v3(workspace: &std::path::Path) {
    let mut config = feanorfs_client::load_config(workspace).unwrap();
    config.format_version = 3;
    feanorfs_client::save_config(workspace, &config).unwrap();
}

#[tokio::test]
async fn hub_transfer_copies_history_between_hubs_through_concurrent_object_phase() {
    let source_server = spawn_test_server().await;
    let uploader = spawn_test_client_with_server(&source_server).await;
    configure_v3(uploader.workspace.path()).await;

    for name in ["alpha.txt", "beta.txt", "gamma.txt"] {
        support::write_workspace_file(uploader.workspace.path(), name, name.as_bytes()).await;
    }
    let uploaded = do_sync(
        &source_server.api,
        &uploader.db,
        uploader.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    assert_eq!(uploaded.uploads, 3);

    let destination_server = spawn_test_server().await;
    let receiver = spawn_test_client_with_server(&destination_server).await;
    configure_v3(receiver.workspace.path()).await;

    let result = transfer_hub(
        uploader.workspace.path(),
        &source_server.url,
        receiver.workspace.path(),
    )
    .await
    .unwrap();
    assert!(result.objects >= 4, "files plus trees and snapshots");
    assert!(result.snapshots >= 1);

    let source_head = source_server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let destination_head = destination_server
        .api
        .get_head(WORKSPACE_ID)
        .await
        .unwrap()
        .expect("destination head published after transfer");
    assert_eq!(source_head, destination_head);
}
