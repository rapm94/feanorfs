//! Agent signal protocol integration tests over a real HTTP hub: send/inbox
//! roundtrips, cursor deltas, multi-parent history, project-path isolation,
//! and hub-storage plaintext absence.

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{
    check_agent, do_push_only, do_status, land_agent, load_config, refresh_agent, save_config,
    spawn_agent, SyncCtx,
};
use feanorfs_common::{
    AgentInboxQuery, AgentMessageInput, AgentMessageKind, AGENT_MESSAGE_MAX_BODY_BYTES,
};
use support::{
    read_workspace_file, spawn_test_client_with_server, spawn_test_server, write_workspace_file,
    TEST_PASSWORD, WORKSPACE_ID,
};

fn send_input(to: &str, kind: AgentMessageKind, body: &str, from: &str) -> AgentMessageInput {
    AgentMessageInput {
        to: to.to_string(),
        kind,
        body: body.to_string(),
        about_snapshot: None,
        reply_to: None,
        from: Some(from.to_string()),
    }
}

fn make_v3(client: &support::TestClient) -> feanorfs_client::Config {
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    config
}

#[tokio::test]
async fn signal_roundtrip_and_cursor_delta_over_http_hub() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    let pushed = do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(pushed.uploads, 1);
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();
    let before = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();

    let first = feanorfs_agent_core::send_message(
        &ctx,
        send_input(
            "mac-test",
            AgentMessageKind::Request,
            "Run iOS simulator tests",
            "linux-dev",
        ),
    )
    .await
    .unwrap();
    let after_first = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();
    feanorfs_agent_core::send_message(
        &ctx,
        send_input("*", AgentMessageKind::Status, "heads up", "linux-dev"),
    )
    .await
    .unwrap();
    let after_broadcast = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();

    let between = feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(after_first),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        between.messages.len(),
        1,
        "delta after first signal is the broadcast"
    );
    assert_eq!(between.messages[0].to, "*");

    let full = feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        full.messages.len(),
        2,
        "direct + broadcast must both arrive"
    );
    assert!(full.messages.iter().all(|m| m.from == "linux-dev"));

    let delta = feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(before.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(delta.messages.len(), 2);
    assert!(!delta.cursor_reset);
    assert_eq!(delta.cursor, after_broadcast);

    let empty_delta = feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(delta.cursor.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(empty_delta.messages.is_empty());

    let unrelated = feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "ci1".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(unrelated.messages.len(), 1);
    assert_eq!(unrelated.messages[0].to, "*");

    let reply = feanorfs_agent_core::send_message(
        &ctx,
        AgentMessageInput {
            to: "linux-dev".into(),
            kind: AgentMessageKind::Result,
            body: "Passed 42 tests".into(),
            about_snapshot: None,
            reply_to: Some(first.message_id.clone()),
            from: Some("mac-test".into()),
        },
    )
    .await
    .unwrap();
    let reply_snapshot = feanorfs_agent_core::SnapshotEngine::new(&ctx)
        .load_snapshot(&reply.message_id)
        .await
        .unwrap();
    assert_eq!(
        reply_snapshot.parents,
        vec![after_broadcast],
        "reply must parent the latest head"
    );
}

#[tokio::test]
async fn signal_snapshots_never_materialize_project_paths() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();

    let before: Vec<_> = std::fs::read_dir(client.workspace.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    feanorfs_agent_core::send_message(
        &ctx,
        send_input(
            "mac-test",
            AgentMessageKind::Request,
            "no files please",
            "linux-dev",
        ),
    )
    .await
    .unwrap();
    feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();

    let after: Vec<_> = std::fs::read_dir(client.workspace.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        before, after,
        "signals must not create or modify project paths"
    );
    assert_eq!(
        read_workspace_file(client.workspace.path(), "seed.txt").await,
        b"seed"
    );
}

#[tokio::test]
async fn land_refresh_and_signals_leave_the_shared_worktree_in_sync() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
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
    for name in ["linux-dev", "mac-test"] {
        spawn_agent(
            root,
            &client.db,
            &server.api,
            WORKSPACE_ID,
            name,
            Some(TEST_PASSWORD),
            false,
            false,
        )
        .await
        .unwrap();
    }

    let landed_path = write_workspace_file(
        &feanorfs_agent_core::agent_dir(root, "linux-dev").unwrap(),
        "landed.txt",
        b"landed",
    )
    .await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&landed_path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&landed_path, permissions).unwrap();
    }
    let landed = land_agent(
        root,
        &client.db,
        &server.api,
        WORKSPACE_ID,
        "linux-dev",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let landed_id = landed.snapshot_id.unwrap();
    let ctx = SyncCtx::from_config(&server.api, &client.db, root, &config).unwrap();
    let request = feanorfs_agent_core::send_message(
        &ctx,
        AgentMessageInput {
            to: "mac-test".into(),
            kind: AgentMessageKind::Request,
            body: "test the landed snapshot".into(),
            about_snapshot: Some(landed_id.clone()),
            reply_to: None,
            from: Some("linux-dev".into()),
        },
    )
    .await
    .unwrap();

    let refreshed = refresh_agent(
        root,
        &client.db,
        &server.api,
        WORKSPACE_ID,
        "mac-test",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert_eq!(refreshed.refreshed, vec!["landed.txt"]);
    assert!(refreshed.deferred.is_empty());
    let mac_status = check_agent(
        root,
        &client.db,
        &server.api,
        WORKSPACE_ID,
        "mac-test",
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(mac_status.our_changes.is_empty());
    assert!(mac_status.their_changes.is_empty());
    assert!(mac_status.conflicts.is_empty());

    feanorfs_agent_core::send_message(
        &ctx,
        AgentMessageInput {
            to: "linux-dev".into(),
            kind: AgentMessageKind::Result,
            body: "landed snapshot passed".into(),
            about_snapshot: Some(landed_id),
            reply_to: Some(request.message_id),
            from: Some("mac-test".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(read_workspace_file(root, "landed.txt").await, b"landed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_ne!(
            std::fs::metadata(root.join("landed.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0,
            "agent land must preserve executable mode"
        );
    }
    let shared_status = do_status(
        &server.api,
        &client.db,
        root,
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    assert!(shared_status.upload_required.is_empty());
    assert!(shared_status.download_required.is_empty());
    assert!(shared_status.delete_local.is_empty());
}

#[tokio::test]
async fn hub_storage_contains_no_signal_plaintext() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();

    let body = "unique-plaintext-body-7f3a91c2";
    let sender = "unique-sender-9d2b44e1";
    let recipient = "unique-recipient-5c8e77a3";
    feanorfs_agent_core::send_message(
        &ctx,
        send_input(recipient, AgentMessageKind::Request, body, sender),
    )
    .await
    .unwrap();

    let mut files = Vec::new();
    collect_files(server.data_dir(), &mut files);
    assert!(!files.is_empty(), "hub data dir must contain stored state");
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        for needle in [body, sender, recipient, "ffmsg1"] {
            assert!(
                !contains_bytes(&bytes, needle.as_bytes()),
                "hub storage leaked plaintext {needle:?} in {}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn inbox_traverses_multi_parent_undo_history() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();
    let before = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();

    feanorfs_agent_core::send_message(
        &ctx,
        send_input(
            "mac-test",
            AgentMessageKind::Request,
            "first signal",
            "linux-dev",
        ),
    )
    .await
    .unwrap();
    let mid = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();

    // A two-parent undo snapshot sits between the two signals.
    let undo = feanorfs_agent_core::history::undo(&ctx, &before)
        .await
        .unwrap();
    assert_eq!(undo.restored_snapshot_id, before);

    feanorfs_agent_core::send_message(
        &ctx,
        send_input(
            "mac-test",
            AgentMessageKind::Request,
            "second signal",
            "linux-dev",
        ),
    )
    .await
    .unwrap();

    let delta = feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(before.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(!delta.cursor_reset, "cursor {before} must remain reachable");
    let bodies: Vec<&str> = delta.messages.iter().map(|m| m.body.as_str()).collect();
    assert!(
        bodies.contains(&"first signal") && bodies.contains(&"second signal"),
        "graph delta must find signals across the undo merge: {bodies:?}"
    );

    let before_second = feanorfs_agent_core::inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(mid.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(
        before_second
            .messages
            .iter()
            .any(|m| m.body == "second signal"),
        "delta after the first signal must find the second through undo history"
    );
}

#[tokio::test]
async fn send_validation_failures_leak_no_body_plaintext() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();
    let head_before = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();

    let secret_body = "never-in-errors-4e51d9a7";
    let oversized = send_input(
        "mac-test",
        AgentMessageKind::Request,
        &"x".repeat(AGENT_MESSAGE_MAX_BODY_BYTES + 1),
        "linux-dev",
    );
    let error = feanorfs_agent_core::send_message(&ctx, oversized)
        .await
        .unwrap_err();
    assert!(!error.to_string().contains(secret_body));
    assert!(error.to_string().contains("8 KiB"));

    let bad_about = AgentMessageInput {
        to: "mac-test".into(),
        kind: AgentMessageKind::Request,
        body: secret_body.to_string(),
        about_snapshot: Some(
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
        ),
        reply_to: None,
        from: Some("linux-dev".into()),
    };
    let error = feanorfs_agent_core::send_message(&ctx, bad_about)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("reachable"));
    assert!(!error.to_string().contains(secret_body));

    assert_eq!(
        ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap(),
        head_before,
        "failed sends must not advance the head"
    );
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
