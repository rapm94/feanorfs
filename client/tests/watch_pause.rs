//! Pause must preserve both filesystem and remote publication wakeups.
feanorfs_test_support::isolate_test_process!();
mod support;

use feanorfs_client::{do_sync, set_paused, watch};
use std::time::Duration;
use support::{spawn_test_client_with_server, spawn_test_server, TEST_PASSWORD, WORKSPACE_ID};

#[tokio::test]
async fn paused_watcher_converges_local_edit_after_resume() {
    pause_resume_converges(true).await;
}

#[tokio::test]
async fn paused_watcher_converges_remote_publish_after_resume() {
    pause_resume_converges(false).await;
}

async fn pause_resume_converges(local_edit: bool) {
    let server = spawn_test_server().await;
    let local = spawn_test_client_with_server(&server).await;
    let remote = spawn_test_client_with_server(&server).await;
    for client in [&local, &remote] {
        let mut config = feanorfs_client::load_config(client.workspace.path()).unwrap();
        config.format_version = 3;
        feanorfs_client::save_config(client.workspace.path(), &config).unwrap();
    }
    let root = local.workspace.path().canonicalize().unwrap();
    let watcher = watch::run_watch(
        watch::WatchTarget {
            api: &server.api,
            db: &local.db,
            dir: &root,
            workspace_id: WORKSPACE_ID,
            password: Some(TEST_PASSWORD),
        },
        false,
    );
    tokio::pin!(watcher);
    let scenario = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !feanorfs_client::is_watching(&root) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Finish the initial pass before pausing; pause remains a lock barrier.
        tokio::time::sleep(Duration::from_secs(1)).await;
        set_paused(&root, true).unwrap();
        let guard = feanorfs_client::lock::try_acquire_sync_lock(&root, Duration::from_secs(5))
            .await
            .unwrap();
        drop(guard);
        if local_edit {
            tokio::fs::write(root.join("local.txt"), b"local edit")
                .await
                .unwrap();
        } else {
            tokio::fs::write(remote.workspace.path().join("remote.txt"), b"remote edit")
                .await
                .unwrap();
        }
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
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !root.join("remote.txt").exists(),
            "pause must not apply remote files"
        );
        set_paused(&root, false).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
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
                let converged = if local_edit {
                    tokio::fs::read(remote.workspace.path().join("local.txt"))
                        .await
                        .ok()
                        .as_deref()
                        == Some(b"local edit")
                } else {
                    tokio::fs::read(root.join("remote.txt"))
                        .await
                        .ok()
                        .as_deref()
                        == Some(b"remote edit")
                };
                if converged {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("resume must reconcile without a new local event or remote publication");
    };
    tokio::select! {
        result = &mut watcher => panic!("watcher stopped unexpectedly: {result:?}"),
        _ = scenario => {}
    }
}
