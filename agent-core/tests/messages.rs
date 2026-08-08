//! Encrypted agent signal tests over the embedded LocalHub.

feanorfs_test_support::isolate_test_process!();

use feanorfs_agent_core::local::{save_config, Config};
use feanorfs_agent_core::messages::{
    append_raw_snapshot, inbox, send_message, send_message_if_head, signals_since,
    HeadConditionalSendResult,
};
use feanorfs_agent_core::{
    ensure_workspace_state, land_agent, spawn_agent, ApiClient, ClientDb, SnapshotEngine, SyncCtx,
    LOCAL_HUB_URL,
};
use feanorfs_common::{
    generate_password, parse_agent_message, AgentInboxQuery, AgentMessageInput, AgentMessageKind,
    FileState, AGENT_MESSAGE_MAX_BODY_BYTES,
};

use std::collections::HashMap;
use std::path::PathBuf;

struct TestWorkspace {
    _workspace: tempfile::TempDir,
    root: PathBuf,
    db: ClientDb,
    api: ApiClient,
    config: Config,
    seed_head: String,
}

fn config(workspace_id: &str, key: &str) -> Config {
    Config {
        server_url: LOCAL_HUB_URL.to_string(),
        workspace_id: workspace_id.to_string(),
        encryption_password: Some(key.to_string()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 3,
        hub_local: true,
        relay: None,
    }
}

async fn setup(workspace_id: &str) -> TestWorkspace {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().to_path_buf();
    std::fs::write(root.join("seed.txt"), b"seed").unwrap();
    let key = generate_password().unwrap();
    let cfg = config(workspace_id, &key);
    save_config(&root, &cfg).unwrap();
    let db = ClientDb::new(ensure_workspace_state(&root).unwrap())
        .await
        .unwrap();
    let api = ApiClient::from_config(&root, &cfg).await.unwrap();
    let ctx = SyncCtx::from_config(&api, &db, &root, &cfg).unwrap();
    let seed_head = SnapshotEngine::new(&ctx)
        .publish_server_view(&HashMap::new(), "you")
        .await
        .unwrap();
    TestWorkspace {
        _workspace: workspace,
        root,
        db,
        api,
        config: cfg,
        seed_head,
    }
}

impl TestWorkspace {
    fn ctx(&self) -> SyncCtx<'_> {
        SyncCtx::from_config(&self.api, &self.db, &self.root, &self.config).unwrap()
    }

    fn send(&self, to: &str, kind: AgentMessageKind, body: &str) -> AgentMessageInput {
        AgentMessageInput {
            to: to.to_string(),
            kind,
            body: body.to_string(),
            about_snapshot: None,
            reply_to: None,
            from: None,
        }
    }

    async fn append_snapshot(&self, parent: &str, message: &str, author: &str) -> String {
        let ctx = self.ctx();
        append_raw_snapshot(&ctx, parent, author, message)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn send_appends_message_only_snapshot_reusing_head_root() {
    let ws = setup("send-root").await;
    let ctx = ws.ctx();
    let result = send_message(
        &ctx,
        AgentMessageInput {
            to: "mac-test".to_string(),
            kind: AgentMessageKind::Request,
            body: "Run iOS simulator tests".to_string(),
            about_snapshot: None,
            reply_to: None,
            from: Some("linux-dev".to_string()),
        },
    )
    .await
    .unwrap();

    let head = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();
    assert_eq!(result.message_id, head);
    assert_eq!(result.about_snapshot, ws.seed_head);

    let snapshot = SnapshotEngine::new(&ctx)
        .load_snapshot(&head)
        .await
        .unwrap();
    assert_eq!(snapshot.parents, vec![ws.seed_head.clone()]);
    assert_eq!(snapshot.root, ws.seed_root(&ctx).await);
    assert_eq!(snapshot.author, "linux-dev");
    let payload = parse_agent_message(snapshot.message.as_deref().unwrap()).unwrap();
    assert_eq!(payload.to, "mac-test");
    assert_eq!(payload.kind, AgentMessageKind::Request);
    assert_eq!(payload.body, "Run iOS simulator tests");
    assert_eq!(payload.about_snapshot, ws.seed_head);
    assert!(payload.reply_to.is_none());

    let entries: Vec<_> = std::fs::read_dir(&ws.root).unwrap().collect();
    assert_eq!(
        entries.len(),
        1,
        "signal must not materialize project paths"
    );
}

#[tokio::test]
async fn stale_conditional_fallback_cannot_follow_a_legitimate_terminal() {
    let ws = setup("conditional-terminal-race").await;
    let ctx = ws.ctx();
    let request = send_message(
        &ctx,
        AgentMessageInput {
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "run checks".into(),
            about_snapshot: None,
            reply_to: None,
            from: Some("requester".into()),
        },
    )
    .await
    .unwrap();
    let terminal = send_message(
        &ctx,
        AgentMessageInput {
            to: "requester".into(),
            kind: AgentMessageKind::Result,
            body: "checks passed".into(),
            about_snapshot: Some(request.about_snapshot.clone()),
            reply_to: Some(request.message_id.clone()),
            from: Some("worker".into()),
        },
    )
    .await
    .unwrap();

    let fallback = send_message_if_head(
        &ctx,
        &request.message_id,
        AgentMessageInput {
            to: "requester".into(),
            kind: AgentMessageKind::Blocked,
            body: "runner blocked".into(),
            about_snapshot: Some(request.about_snapshot.clone()),
            reply_to: Some(request.message_id.clone()),
            from: Some("worker".into()),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        fallback,
        HeadConditionalSendResult::Conflict(Some(terminal.message_id.clone()))
    );
    assert_eq!(
        ctx.api.get_head(ctx.workspace_id()).await.unwrap(),
        Some(terminal.message_id.clone())
    );
    let replies = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "requester".into(),
            after: Some(request.message_id.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap()
    .messages;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].message_id, terminal.message_id);
    assert_eq!(replies[0].kind, AgentMessageKind::Result);
}

#[tokio::test]
async fn conditional_fallback_can_retry_after_unrelated_head_advancement() {
    let ws = setup("conditional-unrelated-race").await;
    let ctx = ws.ctx();
    let request = send_message(
        &ctx,
        AgentMessageInput {
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "run checks".into(),
            about_snapshot: None,
            reply_to: None,
            from: Some("requester".into()),
        },
    )
    .await
    .unwrap();
    let unrelated = send_message(
        &ctx,
        AgentMessageInput {
            to: "observer".into(),
            kind: AgentMessageKind::Status,
            body: "unrelated progress".into(),
            about_snapshot: None,
            reply_to: None,
            from: Some("other-worker".into()),
        },
    )
    .await
    .unwrap();
    let fallback = AgentMessageInput {
        to: "requester".into(),
        kind: AgentMessageKind::Blocked,
        body: "runner blocked".into(),
        about_snapshot: Some(request.about_snapshot.clone()),
        reply_to: Some(request.message_id.clone()),
        from: Some("worker".into()),
    };

    assert_eq!(
        send_message_if_head(&ctx, &request.message_id, fallback.clone())
            .await
            .unwrap(),
        HeadConditionalSendResult::Conflict(Some(unrelated.message_id.clone()))
    );
    let sent = send_message_if_head(&ctx, &unrelated.message_id, fallback)
        .await
        .unwrap();
    let HeadConditionalSendResult::Sent(sent) = sent else {
        panic!("fallback should publish against the reread head");
    };
    let replies = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "requester".into(),
            after: Some(request.message_id.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap()
    .messages;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].message_id, sent.message_id);
    assert_eq!(replies[0].kind, AgentMessageKind::Blocked);
}

impl TestWorkspace {
    async fn seed_root(&self, ctx: &SyncCtx<'_>) -> String {
        SnapshotEngine::new(ctx)
            .load_snapshot(&self.seed_head)
            .await
            .unwrap()
            .root
    }
}

#[tokio::test]
async fn sender_defaults_to_human_when_unspecified() {
    let ws = setup("sender-default").await;
    let ctx = ws.ctx();
    let result = send_message(
        &ctx,
        ws.send("mac-test", AgentMessageKind::Status, "starting"),
    )
    .await
    .unwrap();
    let snapshot = SnapshotEngine::new(&ctx)
        .load_snapshot(&result.message_id)
        .await
        .unwrap();
    assert_eq!(snapshot.author, "human");
}

#[tokio::test]
async fn inbox_delivers_direct_and_broadcast_messages_only() {
    let ws = setup("inbox-filter").await;
    let ctx = ws.ctx();
    send_message(
        &ctx,
        ws.send("mac-test", AgentMessageKind::Request, "test this"),
    )
    .await
    .unwrap();
    send_message(&ctx, ws.send("*", AgentMessageKind::Status, "heads up"))
        .await
        .unwrap();

    let mac = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(mac.messages.len(), 2);
    assert!(mac
        .messages
        .iter()
        .all(|m| m.to == "mac-test" || m.to == "*"));

    let other = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "linux-dev".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(other.messages.len(), 1);
    assert_eq!(other.messages[0].to, "*");
}

#[tokio::test]
async fn inbox_cursor_reads_graph_delta() {
    let ws = setup("inbox-cursor").await;
    let ctx = ws.ctx();
    send_message(
        &ctx,
        ws.send("mac-test", AgentMessageKind::Request, "first"),
    )
    .await
    .unwrap();
    let after_first = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();
    send_message(
        &ctx,
        ws.send("mac-test", AgentMessageKind::Result, "second"),
    )
    .await
    .unwrap();

    let delta = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(after_first.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(delta.messages.len(), 1);
    assert_eq!(delta.messages[0].body, "second");
    assert_eq!(
        delta.cursor,
        ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap()
    );
    assert!(!delta.cursor_reset);

    let again = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(delta.cursor.clone()),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(again.messages.is_empty());
    assert!(!again.cursor_reset);

    let recent = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(recent.messages.len(), 2);
    assert_eq!(recent.messages[0].body, "second");
    assert_eq!(recent.messages[1].body, "first");
}

#[tokio::test]
async fn inbox_graph_delta_excludes_cursor_ancestry_reached_through_agent_land() {
    let ws = setup("inbox-agent-land-delta").await;
    let ctx = ws.ctx();

    let old = send_message(
        &ctx,
        ws.send("mac-test", AgentMessageKind::Status, "already delivered"),
    )
    .await
    .unwrap();

    spawn_agent(
        &ws.root,
        &ws.db,
        &ws.api,
        &ws.config.workspace_id,
        "worker",
        ws.config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap();
    std::fs::write(
        feanorfs_agent_core::agent_dir(&ws.root, "worker")
            .unwrap()
            .join("agent-change.txt"),
        b"agent change",
    )
    .unwrap();

    let between = send_message(
        &ctx,
        ws.send(
            "mac-test",
            AgentMessageKind::Status,
            "also already delivered",
        ),
    )
    .await
    .unwrap();
    let cursor = send_message(
        &ctx,
        ws.send("linux-dev", AgentMessageKind::Status, "cursor signal"),
    )
    .await
    .unwrap()
    .message_id;

    let land = land_agent(
        &ws.root,
        &ws.db,
        &ws.api,
        &ws.config.workspace_id,
        "worker",
        ws.config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(!land.landed.is_empty());
    let landed_snapshot_id = land.snapshot_id.as_deref().unwrap();

    let landed = SnapshotEngine::new(&ctx)
        .load_snapshot(landed_snapshot_id)
        .await
        .unwrap();
    assert_eq!(landed.parents.len(), 2);
    assert_eq!(landed.parents[1], cursor);

    let delta = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some(cursor),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(!delta.cursor_reset);
    assert!(
        delta.messages.iter().all(|message| {
            message.message_id != old.message_id && message.message_id != between.message_id
        }),
        "graph delta must subtract the cursor's complete reachable ancestry"
    );
    assert!(delta.messages.is_empty());
}

#[tokio::test]
async fn inbox_unreachable_cursor_resets_to_bounded_recent_view() {
    let ws = setup("inbox-reset").await;
    let ctx = ws.ctx();
    send_message(&ctx, ws.send("mac-test", AgentMessageKind::Request, "only"))
        .await
        .unwrap();

    let result = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: Some("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into()),
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(result.cursor_reset);
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.messages[0].body, "only");
}

#[tokio::test]
async fn inbox_limit_is_bounded_and_clamped() {
    let ws = setup("inbox-limit").await;
    let ctx = ws.ctx();
    for body in ["a", "b", "c"] {
        send_message(&ctx, ws.send("mac-test", AgentMessageKind::Status, body))
            .await
            .unwrap();
    }
    let two = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(two.messages.len(), 2);
    assert!(
        two.cursor_reset,
        "a truncated result must flag possible loss"
    );
    let zero = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 0,
        },
    )
    .await
    .unwrap();
    assert!(zero.messages.is_empty());
    assert!(
        zero.cursor_reset,
        "limit zero still reports omitted signals"
    );
}

#[tokio::test]
async fn concurrent_sends_preserve_both_signals() {
    let ws = setup("concurrent-sends").await;
    let ctx = ws.ctx();
    let (left, right) = tokio::join!(
        send_message(
            &ctx,
            ws.send("mac-test", AgentMessageKind::Request, "from left")
        ),
        send_message(
            &ctx,
            ws.send("mac-test", AgentMessageKind::Request, "from right")
        ),
    );
    left.unwrap();
    right.unwrap();

    let result = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.messages.len(), 2);
    let bodies: Vec<&str> = result.messages.iter().map(|m| m.body.as_str()).collect();
    assert!(bodies.contains(&"from left"));
    assert!(bodies.contains(&"from right"));
}

#[tokio::test]
async fn concurrent_send_and_file_publication_preserve_both() {
    let ws = setup("concurrent-file").await;
    let ctx = ws.ctx();
    let (file_hash, file_ciphertext) =
        feanorfs_agent_core::crypto::seal(b"file payload", ctx.password_str(), "a.txt").unwrap();
    ctx.api
        .upload_object(ctx.workspace_id(), &file_hash, file_ciphertext)
        .await
        .unwrap();
    let mut files = HashMap::new();
    files.insert(
        "a.txt".to_string(),
        FileState {
            path: "a.txt".to_string(),
            hash: file_hash.to_string(),
            size: 12,
            mtime: 0,
            deleted: false,
            mode: 0,
        },
    );
    let engine = SnapshotEngine::new(&ctx);
    let (send, publish) = tokio::join!(
        send_message(&ctx, ws.send("mac-test", AgentMessageKind::Request, "race")),
        engine.publish_server_view(&files, "you"),
    );
    send.unwrap();
    publish.unwrap();

    let head = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();
    let engine = SnapshotEngine::new(&ctx);
    let flat = engine.load_files(&head).await.unwrap();
    assert!(
        flat.contains_key("a.txt"),
        "newest file tree must survive the send race"
    );
    let inbox_result = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(inbox_result.messages.len(), 1);
    assert_eq!(inbox_result.messages[0].body, "race");
}

#[tokio::test]
async fn concurrent_send_and_agent_land_preserve_both() {
    let ws = setup("concurrent-agent-land").await;
    let ctx = ws.ctx();
    spawn_agent(
        &ws.root,
        &ws.db,
        &ws.api,
        &ws.config.workspace_id,
        "worker",
        ws.config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap();
    std::fs::write(
        feanorfs_agent_core::agent_dir(&ws.root, "worker")
            .unwrap()
            .join("landed.txt"),
        b"landed",
    )
    .unwrap();

    let (send, land) = tokio::join!(
        send_message(
            &ctx,
            ws.send("mac-test", AgentMessageKind::Request, "race land")
        ),
        land_agent(
            &ws.root,
            &ws.db,
            &ws.api,
            &ws.config.workspace_id,
            "worker",
            ws.config.encryption_password.as_deref(),
            false,
            false,
        ),
    );
    let sent = send.unwrap();
    let landed = land.unwrap();
    assert!(!landed.landed.is_empty());

    let head = ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap();
    assert!(
        SnapshotEngine::new(&ctx)
            .load_files(&head)
            .await
            .unwrap()
            .contains_key("landed.txt"),
        "agent-land tree must survive the signal race"
    );
    let messages = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap()
    .messages;
    assert!(
        messages
            .iter()
            .any(|message| message.message_id == sent.message_id),
        "signal must remain reachable after the agent-land race"
    );
}

#[tokio::test]
async fn malformed_and_unknown_messages_do_not_block_inbox() {
    let ws = setup("malformed-history").await;
    let ctx = ws.ctx();
    let mut parent = ws.seed_head.clone();
    parent = ws
        .append_snapshot(&parent, "ffmsg1:garbage", "trouble")
        .await;
    parent = ws
        .append_snapshot(&parent, "plain history text", "trouble")
        .await;
    parent = ws
        .append_snapshot(&parent, "ffmsg2:{\"to\":\"x\"}", "trouble")
        .await;
    let valid = "ffmsg1:{\"to\":\"mac-test\",\"kind\":\"request\",\"body\":\"valid\",\"about_snapshot\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"reply_to\":null}";
    parent = ws.append_snapshot(&parent, valid, "bad\nsender").await;
    ws.append_snapshot(&parent, valid, "linux-dev").await;

    let result = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.messages[0].body, "valid");
    assert_eq!(result.messages[0].from, "linux-dev");
}

#[tokio::test]
async fn send_validates_names_ids_and_bounds_before_publication() {
    let ws = setup("send-validation").await;
    let ctx = ws.ctx();

    let mut invalid_from = ws.send("mac-test", AgentMessageKind::Request, "x");
    invalid_from.from = Some("*".into());
    assert!(send_message(&ctx, invalid_from).await.is_err());
    let mut invalid_from = ws.send("mac-test", AgentMessageKind::Request, "x");
    invalid_from.from = Some("a/b".into());
    assert!(send_message(&ctx, invalid_from).await.is_err());

    let mut invalid_to = ws.send("", AgentMessageKind::Request, "x");
    invalid_to.to = "a\\b".into();
    assert!(send_message(&ctx, invalid_to).await.is_err());

    let mut empty_body = ws.send("mac-test", AgentMessageKind::Request, "");
    empty_body.body = "   ".into();
    assert!(send_message(&ctx, empty_body).await.is_err());

    let mut big_body = ws.send("mac-test", AgentMessageKind::Request, "x");
    big_body.body = "x".repeat(AGENT_MESSAGE_MAX_BODY_BYTES + 1);
    assert!(send_message(&ctx, big_body).await.is_err());

    let mut bad_about = ws.send("mac-test", AgentMessageKind::Request, "x");
    bad_about.about_snapshot = Some("short".into());
    assert!(send_message(&ctx, bad_about).await.is_err());
    let mut bad_about = ws.send("mac-test", AgentMessageKind::Request, "x");
    bad_about.about_snapshot =
        Some("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into());
    assert!(send_message(&ctx, bad_about).await.is_err());

    let mut bad_reply = ws.send("mac-test", AgentMessageKind::Request, "x");
    bad_reply.reply_to = Some(ws.seed_head.clone());
    assert!(send_message(&ctx, bad_reply).await.is_err());

    assert_eq!(
        ctx.api.get_head(ctx.workspace_id()).await.unwrap().unwrap(),
        ws.seed_head,
        "failed sends must not advance the head"
    );
}

#[tokio::test]
async fn reply_to_validates_against_a_reachable_signal() {
    let ws = setup("reply-to").await;
    let ctx = ws.ctx();
    let first = send_message(
        &ctx,
        AgentMessageInput {
            to: "mac-test".into(),
            kind: AgentMessageKind::Request,
            body: "run tests".into(),
            about_snapshot: None,
            reply_to: None,
            from: Some("linux-dev".into()),
        },
    )
    .await
    .unwrap();
    let second = send_message(
        &ctx,
        AgentMessageInput {
            to: "linux-dev".into(),
            kind: AgentMessageKind::Result,
            body: "passed".into(),
            about_snapshot: None,
            reply_to: Some(first.message_id.clone()),
            from: Some("mac-test".into()),
        },
    )
    .await
    .unwrap();
    let snapshot = SnapshotEngine::new(&ctx)
        .load_snapshot(&second.message_id)
        .await
        .unwrap();
    let payload = parse_agent_message(snapshot.message.as_deref().unwrap()).unwrap();
    assert_eq!(payload.reply_to.as_deref(), Some(first.message_id.as_str()));
}

#[tokio::test]
async fn empty_workspace_has_no_signals_and_cannot_send() {
    let workspace = tempfile::tempdir().unwrap();
    let key = generate_password().unwrap();
    let cfg = config("empty-ws", &key);
    save_config(workspace.path(), &cfg).unwrap();
    let db = ClientDb::new(ensure_workspace_state(workspace.path()).unwrap())
        .await
        .unwrap();
    let api = ApiClient::from_config(workspace.path(), &cfg)
        .await
        .unwrap();
    let ctx = SyncCtx::from_config(&api, &db, workspace.path(), &cfg).unwrap();

    let empty = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(empty.messages.is_empty());
    assert!(empty.cursor.is_empty());
    assert!(!empty.cursor_reset);

    let input = AgentMessageInput {
        to: "mac-test".into(),
        kind: AgentMessageKind::Request,
        body: "nobody home".into(),
        about_snapshot: None,
        reply_to: None,
        from: None,
    };
    assert!(send_message(&ctx, input).await.is_err());
}

#[tokio::test]
async fn signals_fail_clearly_before_format_v3() {
    let workspace = tempfile::tempdir().unwrap();
    let key = generate_password().unwrap();
    let mut cfg = config("legacy-signals", &key);
    cfg.format_version = 2;
    save_config(workspace.path(), &cfg).unwrap();
    let db = ClientDb::new(ensure_workspace_state(workspace.path()).unwrap())
        .await
        .unwrap();
    let api = ApiClient::from_config(workspace.path(), &cfg)
        .await
        .unwrap();
    let ctx = SyncCtx::from_config(&api, &db, workspace.path(), &cfg).unwrap();

    let send_error = send_message(
        &ctx,
        AgentMessageInput {
            to: "mac-test".into(),
            kind: AgentMessageKind::Request,
            body: "run tests".into(),
            about_snapshot: None,
            reply_to: None,
            from: None,
        },
    )
    .await
    .unwrap_err();
    assert!(send_error.to_string().contains("format v3"));

    let inbox_error = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: "mac-test".into(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap_err();
    assert!(inbox_error.to_string().contains("format v3"));
}

#[tokio::test]
async fn signals_since_returns_every_recipient() {
    let ws = setup("signals-since").await;
    let ctx = ws.ctx();
    send_message(
        &ctx,
        ws.send("mac-test", AgentMessageKind::Request, "for mac"),
    )
    .await
    .unwrap();
    send_message(&ctx, ws.send("ci1", AgentMessageKind::Result, "for ci"))
        .await
        .unwrap();

    let all = signals_since(&ctx, Some(&ws.seed_head), 50).await.unwrap();
    assert_eq!(all.messages.len(), 2);
    let recipients: Vec<&str> = all.messages.iter().map(|m| m.to.as_str()).collect();
    assert!(recipients.contains(&"mac-test"));
    assert!(recipients.contains(&"ci1"));
}
