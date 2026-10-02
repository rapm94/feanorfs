//! Continuous agent development coverage: guarded convergence over a real
//! HTTP hub, conflict preservation, signal-only heads, lease exclusivity,
//! head-wait wakeups, and a real-process `agent run` lifecycle (CAD-12).

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_agent_core::{
    classify_continuous_error, land_agent_continuous, refresh_agent_continuous,
    ContinuousErrorClass, ContinuousOwnerLock, SnapshotEngine,
};
use feanorfs_client::{do_push_only, do_sync, load_config, save_config, spawn_agent, SyncCtx};
use feanorfs_common::{AgentMessageInput, AgentMessageKind, ContinuousPhase};
use std::path::{Path, PathBuf};
use std::time::Duration;
use support::{
    spawn_test_client_with_server, spawn_test_server, write_workspace_file, TestClient, TestServer,
    TEST_PASSWORD, WORKSPACE_ID,
};
use tokio::io::AsyncBufReadExt as _;

/// This integration-test executable owns one isolated process profile. Its
/// real-process tests must not concurrently mutate that profile.
static REAL_PROCESS_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn require_format_v3(workspace: &Path) {
    let mut config = load_config(workspace).unwrap();
    config.format_version = 3;
    save_config(workspace, &config).unwrap();
}

async fn seed_and_push(server: &TestServer, client: &TestClient) {
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    let config = load_config(client.workspace.path()).unwrap();
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        &config.workspace_id,
        config.encryption_password.as_deref(),
    )
    .await
    .unwrap();
}

async fn spawn_test_agent(client: &TestClient, server: &TestServer, name: &str) {
    let config = load_config(client.workspace.path()).unwrap();
    spawn_agent(
        client.workspace.path(),
        &client.db,
        &server.api,
        &config.workspace_id,
        name,
        config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap();
}

fn agent_worktree(client: &TestClient, name: &str) -> PathBuf {
    feanorfs_agent_core::agent_dir(client.workspace.path(), name).unwrap()
}

async fn agent_ctx<'a>(client: &'a TestClient, server: &'a TestServer) -> SyncCtx<'a> {
    let config = load_config(client.workspace.path()).unwrap();
    SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap()
}

// ---------------------------------------------------------------------------
// Engine-level: non-overlapping convergence through the guarded entries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_overlapping_agents_converge_via_guarded_entries() {
    let server = spawn_test_server().await;
    let client_a = spawn_test_client_with_server(&server).await;
    let client_b = spawn_test_client_with_server(&server).await;
    require_format_v3(client_a.workspace.path());
    require_format_v3(client_b.workspace.path());
    seed_and_push(&server, &client_a).await;
    spawn_test_agent(&client_a, &server, "agent-a").await;
    spawn_test_agent(&client_b, &server, "agent-b").await;

    // Agent A edits src/parser.rs.
    write_workspace_file(
        &agent_worktree(&client_a, "agent-a"),
        "src/parser.rs",
        b"parser-a",
    )
    .await;
    let lock_a = ContinuousOwnerLock::try_acquire(client_a.workspace.path(), "agent-a")
        .unwrap()
        .expect("interactive lease");
    let config_a = load_config(client_a.workspace.path()).unwrap();
    let landed = land_agent_continuous(
        client_a.workspace.path(),
        &client_a.db,
        &server.api,
        &config_a.workspace_id,
        "agent-a",
        config_a.encryption_password.as_deref(),
        &lock_a,
    )
    .await
    .expect("automatic outbound land");
    assert!(landed.conflicts.is_empty(), "no overlap expected");
    assert!(
        landed
            .our_changes
            .iter()
            .any(|state| state.path == "src/parser.rs"),
        "landed parser path"
    );
    drop(lock_a);

    // Agent B refreshes; it has not touched the path, so it applies.
    let lock_b = ContinuousOwnerLock::try_acquire(client_b.workspace.path(), "agent-b")
        .unwrap()
        .expect("interactive lease");
    let config_b = load_config(client_b.workspace.path()).unwrap();
    let refreshed = refresh_agent_continuous(
        client_b.workspace.path(),
        &client_b.db,
        &server.api,
        &config_b.workspace_id,
        "agent-b",
        config_b.encryption_password.as_deref(),
        &lock_b,
    )
    .await
    .expect("automatic inbound refresh");
    assert!(
        refreshed
            .refreshed
            .iter()
            .any(|path| path == "src/parser.rs"),
        "remote-only path applied: {refreshed:?}"
    );
    assert!(refreshed.deferred.is_empty());
    let content = tokio::fs::read(agent_worktree(&client_b, "agent-b").join("src/parser.rs"))
        .await
        .unwrap();
    assert_eq!(content, b"parser-a");
    drop(lock_b);
}

#[tokio::test]
async fn land_keeps_unseen_remote_paths_inbound_until_refresh_materializes_them() {
    let server = spawn_test_server().await;
    let agent_client = spawn_test_client_with_server(&server).await;
    let remote_client = spawn_test_client_with_server(&server).await;
    require_format_v3(agent_client.workspace.path());
    require_format_v3(remote_client.workspace.path());
    seed_and_push(&server, &agent_client).await;
    do_sync(
        &server.api,
        &remote_client.db,
        remote_client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    spawn_test_agent(&agent_client, &server, "agent-partial-base").await;

    write_workspace_file(
        &agent_worktree(&agent_client, "agent-partial-base"),
        "agent-only.txt",
        b"from agent",
    )
    .await;
    write_workspace_file(
        remote_client.workspace.path(),
        "remote-only.txt",
        b"from remote",
    )
    .await;
    let remote_config = load_config(remote_client.workspace.path()).unwrap();
    do_push_only(
        &server.api,
        &remote_client.db,
        remote_client.workspace.path(),
        &remote_config.workspace_id,
        remote_config.encryption_password.as_deref(),
    )
    .await
    .unwrap();

    let owner =
        ContinuousOwnerLock::try_acquire(agent_client.workspace.path(), "agent-partial-base")
            .unwrap()
            .unwrap();
    let config = load_config(agent_client.workspace.path()).unwrap();
    let landed = land_agent_continuous(
        agent_client.workspace.path(),
        &agent_client.db,
        &server.api,
        &config.workspace_id,
        "agent-partial-base",
        config.encryption_password.as_deref(),
        &owner,
    )
    .await
    .expect("land disjoint local change over a newer remote head");
    assert!(
        landed
            .their_changes
            .iter()
            .any(|change| change.path == "remote-only.txt" && !change.deleted),
        "the unseen remote file must remain explicitly inbound: {landed:?}"
    );
    assert!(
        !agent_worktree(&agent_client, "agent-partial-base")
            .join("remote-only.txt")
            .exists(),
        "land itself does not materialize remote paths into the agent"
    );

    let refreshed = refresh_agent_continuous(
        agent_client.workspace.path(),
        &agent_client.db,
        &server.api,
        &config.workspace_id,
        "agent-partial-base",
        config.encryption_password.as_deref(),
        &owner,
    )
    .await
    .expect("refresh materializes the still-inbound remote path");
    assert!(
        refreshed
            .refreshed
            .iter()
            .any(|path| path == "remote-only.txt"),
        "remote path must refresh instead of becoming a local deletion: {refreshed:?}"
    );
    assert_eq!(
        tokio::fs::read(
            agent_worktree(&agent_client, "agent-partial-base").join("remote-only.txt")
        )
        .await
        .unwrap(),
        b"from remote"
    );

    let checked = feanorfs_client::check_agent(
        agent_client.workspace.path(),
        &agent_client.db,
        &server.api,
        &config.workspace_id,
        "agent-partial-base",
        config.encryption_password.as_deref(),
    )
    .await
    .unwrap();
    assert!(
        checked.our_changes.is_empty()
            && checked.their_changes.is_empty()
            && checked.conflicts.is_empty(),
        "post-refresh agent must be settled with no false deletion: {checked:?}"
    );
    drop(owner);
}

// ---------------------------------------------------------------------------
// Overlapping edits preserve all legs and enter attention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn overlapping_edits_preserve_legs_and_pause() {
    let server = spawn_test_server().await;
    let client_a = spawn_test_client_with_server(&server).await;
    let client_b = spawn_test_client_with_server(&server).await;
    require_format_v3(client_a.workspace.path());
    require_format_v3(client_b.workspace.path());
    seed_and_push(&server, &client_a).await;
    spawn_test_agent(&client_a, &server, "agent-a").await;
    spawn_test_agent(&client_b, &server, "agent-b").await;

    // Both agents edit the same path from the same base.
    write_workspace_file(&agent_worktree(&client_a, "agent-a"), "same.rs", b"from-a").await;
    write_workspace_file(&agent_worktree(&client_b, "agent-b"), "same.rs", b"from-b").await;

    let lock_a = ContinuousOwnerLock::try_acquire(client_a.workspace.path(), "agent-a")
        .unwrap()
        .unwrap();
    let config_a = load_config(client_a.workspace.path()).unwrap();
    let landed = land_agent_continuous(
        client_a.workspace.path(),
        &client_a.db,
        &server.api,
        &config_a.workspace_id,
        "agent-a",
        config_a.encryption_password.as_deref(),
        &lock_a,
    )
    .await
    .expect("first clean CAS");
    assert!(landed.conflicts.is_empty());
    drop(lock_a);

    // B refreshes: the overlap is deferred and B's local file stays intact.
    let lock_b = ContinuousOwnerLock::try_acquire(client_b.workspace.path(), "agent-b")
        .unwrap()
        .unwrap();
    let config_b = load_config(client_b.workspace.path()).unwrap();
    let refreshed = refresh_agent_continuous(
        client_b.workspace.path(),
        &client_b.db,
        &server.api,
        &config_b.workspace_id,
        "agent-b",
        config_b.encryption_password.as_deref(),
        &lock_b,
    )
    .await
    .expect("refresh defers overlap");
    assert_eq!(refreshed.deferred, vec!["same.rs".to_string()]);
    let local = tokio::fs::read(agent_worktree(&client_b, "agent-b").join("same.rs"))
        .await
        .unwrap();
    assert_eq!(
        local, b"from-b",
        "refresh must never overwrite agent-local edits"
    );

    // B lands: the existing three-way path materializes the conflict legs.
    let landed_b = land_agent_continuous(
        client_b.workspace.path(),
        &client_b.db,
        &server.api,
        &config_b.workspace_id,
        "agent-b",
        config_b.encryption_password.as_deref(),
        &lock_b,
    )
    .await
    .expect("overlapping land");
    assert_eq!(landed_b.conflicts.len(), 1, "one explicit conflict");
    assert_eq!(landed_b.conflicts[0].path, "same.rs");
    // All three legs are preserved outside the project.
    assert!(
        landed_b.conflicts[0].original_file.is_some()
            && landed_b.conflicts[0].local_file.is_some()
            && landed_b.conflicts[0].cloud_file.is_some(),
        "all legs recoverable: {:?}",
        landed_b.conflicts[0]
    );
    // The shared contract classifies the pause as attention, not a retry.
    let error = anyhow::anyhow!("Your folder needs attention before landing agent work.");
    assert!(matches!(
        classify_continuous_error(&error),
        ContinuousErrorClass::Attention(_)
    ));
    let probe = feanorfs_agent_core::probe_agent_state(
        client_b.workspace.path(),
        &client_b.db,
        &server.api,
        "agent-b",
    )
    .await
    .unwrap();
    assert!(
        probe.conflicts > 0,
        "published conflict still pauses a restarted controller"
    );
    assert_eq!(
        probe.local_changes, 0,
        "the published local leg is the new comparison base"
    );
    assert!(
        refresh_agent_continuous(
            client_b.workspace.path(),
            &client_b.db,
            &server.api,
            &config_b.workspace_id,
            "agent-b",
            config_b.encryption_password.as_deref(),
            &lock_b,
        )
        .await
        .is_err(),
        "an unresolved tree must not replace the captured local leg"
    );

    // A later edit is not part of the user's resolution decision.
    write_workspace_file(
        &agent_worktree(&client_b, "agent-b"),
        "same.rs",
        b"later-edit",
    )
    .await;
    let ctx = agent_ctx(&client_b, &server).await;
    feanorfs_agent_core::resolve_conflict(
        &ctx,
        "same.rs",
        feanorfs_agent_core::ResolveKeep::Cloud,
        None,
    )
    .await
    .unwrap();
    let refresh = refresh_agent_continuous(
        client_b.workspace.path(),
        &client_b.db,
        &server.api,
        &config_b.workspace_id,
        "agent-b",
        config_b.encryption_password.as_deref(),
        &lock_b,
    )
    .await
    .unwrap();
    assert_eq!(refresh.deferred, ["same.rs"]);
    assert_eq!(
        std::fs::read(agent_worktree(&client_b, "agent-b").join("same.rs")).unwrap(),
        b"later-edit"
    );
    // Once only the captured leg remains, safe refresh accepts the explicit
    // resolution without replacing any newer edit or releasing ownership.
    write_workspace_file(&agent_worktree(&client_b, "agent-b"), "same.rs", b"from-b").await;
    let refresh = refresh_agent_continuous(
        client_b.workspace.path(),
        &client_b.db,
        &server.api,
        &config_b.workspace_id,
        "agent-b",
        config_b.encryption_password.as_deref(),
        &lock_b,
    )
    .await
    .unwrap();
    assert!(refresh.deferred.is_empty());
    assert_eq!(
        std::fs::read(agent_worktree(&client_b, "agent-b").join("same.rs")).unwrap(),
        b"from-a"
    );
    drop(lock_b);
}

// ---------------------------------------------------------------------------
// Signal-only heads wake messaging with zero file work
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signal_only_head_produces_zero_file_work() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "agent-sig").await;

    let ctx = agent_ctx(&client, &server).await;
    let engine = SnapshotEngine::new(&ctx);
    let before_head = server
        .api
        .get_head(ctx.workspace_id())
        .await
        .unwrap()
        .expect("head exists");
    let before_root = engine.load_snapshot(&before_head).await.unwrap().root;

    // Publish one signal: head changes, tree root stays identical.
    let sent = feanorfs_agent_core::send_message(
        &ctx,
        AgentMessageInput {
            to: "*".to_string(),
            kind: AgentMessageKind::Status,
            body: "checkpoint".to_string(),
            about_snapshot: Some(before_head.clone()),
            reply_to: None,
            from: Some("human".to_string()),
        },
    )
    .await
    .expect("signal publication");
    let after_head = server
        .api
        .get_head(ctx.workspace_id())
        .await
        .unwrap()
        .expect("head exists");
    assert_ne!(after_head, before_head, "signal advanced the head");
    let after_root = engine.load_snapshot(&after_head).await.unwrap().root;
    assert_eq!(
        after_root, before_root,
        "signal-only head keeps the tree root"
    );
    assert_eq!(sent.about_snapshot, before_head);

    // A refresh driven by that head must write nothing into the agent.
    let before_files = snapshot_worktree_files(&agent_worktree(&client, "agent-sig")).await;
    let lock = ContinuousOwnerLock::try_acquire(client.workspace.path(), "agent-sig")
        .unwrap()
        .unwrap();
    let config = load_config(client.workspace.path()).unwrap();
    let refreshed = refresh_agent_continuous(
        client.workspace.path(),
        &client.db,
        &server.api,
        &config.workspace_id,
        "agent-sig",
        config.encryption_password.as_deref(),
        &lock,
    )
    .await
    .expect("refresh after signal-only head");
    assert!(refreshed.refreshed.is_empty());
    assert!(refreshed.deferred.is_empty());
    let after_files = snapshot_worktree_files(&agent_worktree(&client, "agent-sig")).await;
    assert_eq!(
        before_files, after_files,
        "signal-only head must not touch the worktree"
    );
    drop(lock);
}

async fn snapshot_worktree_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if entry.file_type().await.unwrap().is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.push((rel, tokio::fs::read(&path).await.unwrap()));
            }
        }
    }
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// Lease exclusivity and dormant protection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_exclusivity_rejects_manual_mutation() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "agent-lease").await;

    let lock = ContinuousOwnerLock::try_acquire(client.workspace.path(), "agent-lease")
        .unwrap()
        .expect("first owner");
    // A second owner is rejected before any mutation.
    assert!(
        ContinuousOwnerLock::try_acquire(client.workspace.path(), "agent-lease")
            .unwrap()
            .is_none(),
        "duplicate continuous owner rejected"
    );
    // Manual land while the controller owns the agent fails clearly.
    let config = load_config(client.workspace.path()).unwrap();
    let manual = feanorfs_agent_core::land_agent(
        client.workspace.path(),
        &client.db,
        &server.api,
        &config.workspace_id,
        "agent-lease",
        config.encryption_password.as_deref(),
        false,
        false,
    )
    .await;
    assert!(
        manual.is_err(),
        "manual land must be rejected while a continuous owner is active"
    );
    let message = manual.unwrap_err().to_string();
    assert!(
        message.contains("owns continuous reconciliation"),
        "{message}"
    );
    drop(lock);

    // Dormant dirty work never publishes merely because the controller stopped.
    write_workspace_file(
        &agent_worktree(&client, "agent-lease"),
        "dormant.rs",
        b"dormant",
    )
    .await;
    let head = server.api.get_head(&config.workspace_id).await.unwrap();
    let shared = client.workspace.path().join("dormant.rs");
    assert!(
        !shared.exists(),
        "dormant agent directories never land automatically"
    );
    assert_eq!(
        head,
        server.api.get_head(&config.workspace_id).await.unwrap()
    );
}

// ---------------------------------------------------------------------------
// Bounded head waiting over the real HTTP hub
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_head_wait_wakes_after_cas() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    let config = load_config(client.workspace.path()).unwrap();
    let head = server
        .api
        .get_head(&config.workspace_id)
        .await
        .unwrap()
        .expect("seed head");

    let waiter = feanorfs_agent_core::wait_for_head_change(
        &server.api,
        &config.workspace_id,
        Some(head.as_str()),
        Duration::from_secs(5),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Publish a second file: the durable CAS wakes the waiter.
    write_workspace_file(client.workspace.path(), "second.txt", b"second").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        &config.workspace_id,
        config.encryption_password.as_deref(),
    )
    .await
    .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("waiter must wake promptly")
        .expect("wait request");
    assert!(matches!(
        outcome,
        feanorfs_agent_core::HeadWaitOutcome::Changed(_)
    ));
}

// ---------------------------------------------------------------------------
// Real-process `agent run`: continuous reconciliation with zero land/refresh
// ---------------------------------------------------------------------------

/// Child helper for the real-process test: writes one file into the agent
/// worktree, then lingers long enough for the controller to observe the
/// quiet burst and publish.
#[test]
#[ignore]
fn continuous_child_helper() {
    let agent_dir = PathBuf::from(std::env::var_os("FEANORFS_AGENT_DIR").expect("agent dir env"));
    let target = agent_dir.join("src").join("generated.txt");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, b"generated-by-agent").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
}

/// Writes and exits immediately. The controller must already be ready and its
/// final authoritative probe must reconcile the edit even if notify delivery
/// trails direct-child exit.
#[test]
#[ignore]
fn continuous_immediate_child_helper() {
    let agent_dir = PathBuf::from(std::env::var_os("FEANORFS_AGENT_DIR").expect("agent dir env"));
    std::fs::write(agent_dir.join("immediate.txt"), b"immediate-edit").unwrap();
}

/// Announces that child code has started without touching the agent tree,
/// then remains alive for one inbound refresh.
#[test]
#[ignore]
fn continuous_idle_child_helper() {
    let ready = PathBuf::from(
        std::env::var_os("FEANORFS_TEST_READY_PATH").expect("ready path environment"),
    );
    std::fs::write(ready, b"ready").unwrap();
    std::thread::sleep(Duration::from_millis(1800));
}

#[test]
#[ignore]
fn continuous_journey_child_helper() {
    let ready = std::env::var_os("FEANORFS_TEST_READY_PATH").unwrap();
    let stop = PathBuf::from(std::env::var_os("FEANORFS_TEST_STOP_PATH").unwrap());
    std::fs::write(ready, b"ready").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while stop.parent().unwrap().exists() && !stop.exists() && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        stop.exists() || !stop.parent().unwrap().exists(),
        "journey parent must stop the child within its bound"
    );
}

async fn wait_for_journey(label: &str, condition: impl Fn() -> bool) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| format!("journey timed out: {label}"))
}

#[tokio::test]
async fn two_active_agents_resume_after_resolution_and_continue_work() {
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let online = Arc::new(AtomicBool::new(true));
    let server = support::spawn_test_server_with_gate(Some(online.clone())).await;
    let a = spawn_test_client_with_server(&server).await;
    let b = spawn_test_client_with_server(&server).await;
    require_format_v3(a.workspace.path());
    require_format_v3(b.workspace.path());
    seed_and_push(&server, &a).await;
    spawn_test_agent(&a, &server, "journey-a").await;
    spawn_test_agent(&b, &server, "journey-b").await;
    let controls = tempfile::tempdir().unwrap();
    let helper = std::env::current_exe().unwrap();
    let start_agent = |client: &TestClient, name: &str| {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
            .args([
                "agent",
                "run",
                name,
                "--",
                helper.to_str().unwrap(),
                "--ignored",
                "--exact",
                "continuous_journey_child_helper",
                "--nocapture",
            ])
            .env("FEANORFS_TEST_READY_PATH", controls.path().join(name))
            .env(
                "FEANORFS_TEST_STOP_PATH",
                controls.path().join(format!("{name}.stop")),
            )
            .current_dir(client.workspace.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let mut children = vec![start_agent(&a, "journey-a"), start_agent(&b, "journey-b")];
    wait_for_journey("both controllers ready", || {
        controls.path().join("journey-a").exists() && controls.path().join("journey-b").exists()
    })
    .await
    .unwrap();
    let tree_a = agent_worktree(&a, "journey-a");
    let tree_b = agent_worktree(&b, "journey-b");
    online.store(false, Ordering::SeqCst);
    std::fs::write(tree_a.join("offline-a.txt"), b"offline-a").unwrap();
    std::fs::write(tree_b.join("offline-b.txt"), b"offline-b").unwrap();
    wait_for_journey("both controllers retain offline work", || {
        [(&a, "journey-a"), (&b, "journey-b")]
            .iter()
            .all(|(client, name)| {
                feanorfs_agent_core::read_continuous_status(client.workspace.path(), name)
                    .unwrap()
                    .is_some_and(|status| {
                        status.phase == ContinuousPhase::Offline && status.pending_local
                    })
            })
    })
    .await
    .unwrap();
    online.store(true, Ordering::SeqCst);
    wait_for_journey("offline edits converge after reconnection", || {
        [&tree_a, &tree_b].iter().all(|tree| {
            std::fs::read(tree.join("offline-a.txt")).ok().as_deref() == Some(b"offline-a")
                && std::fs::read(tree.join("offline-b.txt")).ok().as_deref() == Some(b"offline-b")
        })
    })
    .await
    .unwrap();
    // Both writes precede the quiet-period debounce; neither needs a transfer command.
    std::fs::write(tree_a.join("seed.txt"), b"version-a").unwrap();
    std::fs::write(tree_b.join("seed.txt"), b"version-b").unwrap();
    let paused = |client: &TestClient, name: &str| {
        feanorfs_agent_core::read_continuous_status(client.workspace.path(), name)
            .unwrap()
            .is_some_and(|status| status.active && status.attention.is_some())
    };
    wait_for_journey("both agents preserve the conflict", || {
        paused(&a, "journey-a") && paused(&b, "journey-b")
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(tree_a.join("seed.txt")).unwrap(),
        b"version-a"
    );
    assert_eq!(
        std::fs::read(tree_b.join("seed.txt")).unwrap(),
        b"version-b"
    );
    // Resolve on the participant that registered the conflict; both owners
    // keep running throughout this ordinary human action.
    let resolver = if a
        .db
        .get_conflict_record("seed.txt")
        .await
        .unwrap()
        .is_some()
    {
        &a
    } else {
        &b
    };
    let replacement = controls.path().join("replacement");
    std::fs::write(&replacement, b"resolved").unwrap();
    let ctx = agent_ctx(resolver, &server).await;
    feanorfs_agent_core::resolve_conflict(
        &ctx,
        "seed.txt",
        feanorfs_agent_core::ResolveKeep::File,
        Some(&replacement),
    )
    .await
    .unwrap();
    let resolution_head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let engine = SnapshotEngine::new(&ctx);
    assert_eq!(
        engine.load_files(&resolution_head).await.unwrap()["seed.txt"].size,
        8
    );
    let resumed_at = std::time::Instant::now();
    let resumed = wait_for_journey("published resolution reaches both active agents", || {
        [&tree_a, &tree_b]
            .iter()
            .all(|tree| std::fs::read(tree.join("seed.txt")).ok().as_deref() == Some(b"resolved"))
            && !paused(&a, "journey-a")
            && !paused(&b, "journey-b")
    })
    .await;
    if resumed.is_err() {
        let short = |id: &str| id[..8].to_string();
        eprintln!(
            "resolution={} after {:?}",
            short(&resolution_head),
            resumed_at.elapsed()
        );
        let mut id = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
        for _ in 0..8 {
            let snapshot = engine.load_snapshot(&id).await.unwrap();
            let seed = engine
                .load_files(&id)
                .await
                .unwrap()
                .get("seed.txt")
                .cloned();
            eprintln!(
                "head={} parents={:?} author={} message={:?} seed={:?}",
                short(&id),
                snapshot
                    .parents
                    .iter()
                    .map(|p| short(p))
                    .collect::<Vec<_>>(),
                snapshot.author,
                snapshot.message,
                seed.map(|file| (short(&file.hash), file.size)),
            );
            let Some(parent) = snapshot.parents.first() else {
                break;
            };
            id = parent.clone();
        }
        for (client, name, tree) in [(&a, "journey-a", &tree_a), (&b, "journey-b", &tree_b)] {
            let status =
                feanorfs_agent_core::read_continuous_status(client.workspace.path(), name).unwrap();
            eprintln!(
                "{name}: worktree seed={:?} status={:?}",
                std::fs::read_to_string(tree.join("seed.txt")).ok(),
                status.map(|s| (
                    s.phase,
                    s.attention,
                    s.pending_local,
                    s.observed_head.map(|h| short(&h)),
                    s.settled_snapshot.map(|h| short(&h)),
                )),
            );
        }
    }
    resumed.unwrap();
    std::fs::write(tree_a.join("a.txt"), b"continued-a").unwrap();
    std::fs::write(tree_b.join("b.txt"), b"continued-b").unwrap();
    wait_for_journey("independent work converges after resolution", || {
        [&tree_a, &tree_b].iter().all(|tree| {
            std::fs::read(tree.join("a.txt")).ok().as_deref() == Some(b"continued-a")
                && std::fs::read(tree.join("b.txt")).ok().as_deref() == Some(b"continued-b")
        })
    })
    .await
    .unwrap();
    eprintln!(
        "journey: resolution_to_continued_work_ms={} manual_resolutions=1 resolution_watcher_restarts=0",
        resumed_at.elapsed().as_millis()
    );
    // Restart one controller without recreating its agent or losing its base.
    std::fs::write(controls.path().join("journey-a.stop"), b"stop").unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        children.remove(0).wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success());
    std::fs::remove_file(controls.path().join("journey-a.stop")).unwrap();
    std::fs::remove_file(controls.path().join("journey-a")).unwrap();
    children.push(start_agent(&a, "journey-a"));
    wait_for_journey("restarted controller ready", || {
        controls.path().join("journey-a").exists()
    })
    .await
    .unwrap();
    std::fs::write(tree_b.join("after-restart.txt"), b"still-working").unwrap();
    wait_for_journey("restarted controller receives continued work", || {
        std::fs::read(tree_a.join("after-restart.txt"))
            .ok()
            .as_deref()
            == Some(b"still-working")
    })
    .await
    .unwrap();
    wait_for_journey("both agents settled before handoff", || {
        [(&a, "journey-a"), (&b, "journey-b")]
            .iter()
            .all(|(client, name)| {
                feanorfs_agent_core::read_continuous_status(client.workspace.path(), name)
                    .unwrap()
                    .is_some_and(|status| {
                        status.active
                            && status.phase == ContinuousPhase::Idle
                            && !status.pending_local
                            && status.settled_snapshot.is_some()
                    })
            })
    })
    .await
    .unwrap();
    let tested = feanorfs_agent_core::read_continuous_status(b.workspace.path(), "journey-b")
        .unwrap()
        .unwrap()
        .settled_snapshot
        .unwrap();
    let ctx_a = agent_ctx(&a, &server).await;
    let ctx_b = agent_ctx(&b, &server).await;
    let request = feanorfs_agent_core::send_message(
        &ctx_a,
        AgentMessageInput {
            from: Some("journey-a".into()),
            to: "journey-b".into(),
            kind: AgentMessageKind::Request,
            body: "Verify the recovered workspace".into(),
            about_snapshot: Some(tested.clone()),
            reply_to: None,
        },
    )
    .await
    .unwrap();
    let inspected = snapshot_worktree_files(&tree_b).await;
    assert_eq!(snapshot_worktree_files(&tree_a).await, inspected);
    assert_eq!(
        feanorfs_agent_core::read_continuous_status(b.workspace.path(), "journey-b")
            .unwrap()
            .unwrap()
            .settled_snapshot
            .as_deref(),
        Some(tested.as_str())
    );
    let report = serde_json::json!({
        "changed_paths": ["seed.txt", "offline-a.txt", "offline-b.txt", "a.txt", "b.txt", "after-restart.txt"],
        "reported_verification": {"check": "compared both complete worktrees", "status": "passed"},
        "worktree_digest": feanorfs_common::hash_bytes(&serde_json::to_vec(&inspected).unwrap()),
        "limitations": "loopback source binaries; no installed-product or LAN timing claim"
    });
    let result = feanorfs_agent_core::send_message(
        &ctx_b,
        AgentMessageInput {
            from: Some("journey-b".into()),
            to: "journey-a".into(),
            kind: AgentMessageKind::Result,
            body: report.to_string(),
            about_snapshot: Some(tested.clone()),
            reply_to: Some(request.message_id.clone()),
        },
    )
    .await
    .unwrap();
    let delivered = feanorfs_agent_core::inbox(
        &ctx_a,
        feanorfs_common::AgentInboxQuery {
            recipient: "journey-a".into(),
            after: Some(request.message_id.clone()),
            limit: 10,
        },
    )
    .await
    .unwrap();
    assert!(!delivered.cursor_reset);
    let reply = delivered
        .messages
        .iter()
        .find(|message| message.message_id == result.message_id)
        .unwrap();
    assert_eq!(reply.reply_to.as_deref(), Some(request.message_id.as_str()));
    assert_eq!(reply.about_snapshot, tested);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply.body).unwrap(),
        report
    );
    for name in ["journey-a", "journey-b"] {
        std::fs::write(controls.path().join(format!("{name}.stop")), b"stop").unwrap();
    }
    for child in children {
        let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        snapshot_worktree_files(&tree_a).await,
        snapshot_worktree_files(&tree_b).await
    );
    eprintln!("journey: forced_outages=1 controlled_restarts=1 handoff_received=true identical_worktrees=true");
}

#[tokio::test]
async fn agent_run_reconciles_without_explicit_transfer_commands() {
    // This integration-test executable owns one isolated process profile; the
    // real `feanorfs` CLI child must share it.
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "worker").await;
    let config = load_config(client.workspace.path()).unwrap();
    let head_before = server
        .api
        .get_head(&config.workspace_id)
        .await
        .unwrap()
        .expect("seed head");

    let helper = std::env::current_exe().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args([
            "agent",
            "run",
            "worker",
            "--",
            helper.to_str().unwrap(),
            "--ignored",
            "--exact",
            "continuous_child_helper",
            "--nocapture",
        ])
        .current_dir(client.workspace.path())
        .kill_on_drop(true)
        .output()
        .await
        .expect("run agent command");
    assert!(
        output.status.success(),
        "agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The outcome summary is written to stderr so a nested `--json` child's
    // stdout stays parseable; the terminal still sees the report.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("settled"),
        "final outcome must report settled: {stderr}"
    );

    // The child's file reached the shared encrypted workspace and was
    // materialized into the shared folder — no explicit land/push/pull.
    assert_eq!(
        tokio::fs::read(client.workspace.path().join("src/generated.txt"))
            .await
            .unwrap(),
        b"generated-by-agent"
    );
    let head_after = server
        .api
        .get_head(&config.workspace_id)
        .await
        .unwrap()
        .expect("post-run head");
    assert_ne!(head_before, head_after, "automatic land advanced the head");

    // The persisted status projection reports a deactivated, settled owner.
    let status = feanorfs_agent_core::read_continuous_status(client.workspace.path(), "worker")
        .unwrap()
        .expect("status persisted");
    assert!(!status.active);
    assert!(status.settled_snapshot.is_some());
    assert!(matches!(
        status.phase,
        ContinuousPhase::Stopping | ContinuousPhase::Idle
    ));
    // No stale ownership: the lease released with the process.
    assert!(
        ContinuousOwnerLock::try_acquire(client.workspace.path(), "worker")
            .unwrap()
            .is_some(),
        "lease released after agent run exit"
    );
    // Status remains bounded and secret-free.
    let serialized = serde_json::to_string(&status).unwrap();
    assert!(serialized.len() < 64 * 1024);
    assert!(!serialized.contains(TEST_PASSWORD));
}

#[tokio::test]
async fn immediate_child_edit_and_exit_is_reconciled() {
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "immediate").await;
    let config = load_config(client.workspace.path()).unwrap();
    let head_before = server.api.get_head(&config.workspace_id).await.unwrap();
    let helper = std::env::current_exe().unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args([
            "agent",
            "run",
            "immediate",
            "--",
            helper.to_str().unwrap(),
            "--ignored",
            "--exact",
            "continuous_immediate_child_helper",
            "--nocapture",
        ])
        .current_dir(client.workspace.path())
        .kill_on_drop(true)
        .output()
        .await
        .expect("run immediate agent command");

    assert!(
        output.status.success(),
        "agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        tokio::fs::read(client.workspace.path().join("immediate.txt"))
            .await
            .unwrap(),
        b"immediate-edit"
    );
    assert_ne!(
        head_before,
        server.api.get_head(&config.workspace_id).await.unwrap()
    );
}

#[tokio::test]
async fn controller_setup_failure_prevents_child_execution() {
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "not-ready").await;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable = listener.local_addr().unwrap();
    drop(listener);
    let mut config = load_config(client.workspace.path()).unwrap();
    config.server_url = format!("http://{unavailable}");
    save_config(client.workspace.path(), &config).unwrap();
    let helper = std::env::current_exe().unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args([
            "agent",
            "run",
            "not-ready",
            "--",
            helper.to_str().unwrap(),
            "--ignored",
            "--exact",
            "continuous_immediate_child_helper",
            "--nocapture",
        ])
        .current_dir(client.workspace.path())
        .kill_on_drop(true)
        .output()
        .await
        .expect("run agent command against unavailable hub");

    assert!(!output.status.success(), "setup must fail closed");
    assert!(
        !agent_worktree(&client, "not-ready")
            .join("immediate.txt")
            .exists(),
        "child must not execute before controller readiness"
    );
}

#[tokio::test]
async fn inbound_refresh_settles_without_echo_snapshot() {
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client_a = spawn_test_client_with_server(&server).await;
    let client_b = spawn_test_client_with_server(&server).await;
    require_format_v3(client_a.workspace.path());
    require_format_v3(client_b.workspace.path());
    seed_and_push(&server, &client_a).await;
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
    spawn_test_agent(&client_a, &server, "inbound").await;

    let ready_dir = tempfile::tempdir().unwrap();
    let ready_path = ready_dir.path().join("ready");
    let helper = std::env::current_exe().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    child
        .args([
            "agent",
            "run",
            "inbound",
            "--",
            helper.to_str().unwrap(),
            "--ignored",
            "--exact",
            "continuous_idle_child_helper",
            "--nocapture",
        ])
        .env("FEANORFS_TEST_READY_PATH", &ready_path)
        .current_dir(client_a.workspace.path())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = child.spawn().expect("start inbound agent run");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready_path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("controller became ready before child started");

    write_workspace_file(client_b.workspace.path(), "remote.txt", b"remote-edit").await;
    let config_b = load_config(client_b.workspace.path()).unwrap();
    do_push_only(
        &server.api,
        &client_b.db,
        client_b.workspace.path(),
        &config_b.workspace_id,
        config_b.encryption_password.as_deref(),
    )
    .await
    .unwrap();
    let remote_head = server
        .api
        .get_head(&config_b.workspace_id)
        .await
        .unwrap()
        .expect("remote publication head");

    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        tokio::fs::read(agent_worktree(&client_a, "inbound").join("remote.txt"))
            .await
            .unwrap(),
        b"remote-edit"
    );
    assert_eq!(
        server.api.get_head(&config_b.workspace_id).await.unwrap(),
        Some(remote_head.clone()),
        "refresh-generated watcher events must not publish a no-op head"
    );
    let status = feanorfs_agent_core::read_continuous_status(client_a.workspace.path(), "inbound")
        .unwrap()
        .expect("continuous status");
    assert_eq!(
        status.settled_snapshot.as_deref(),
        Some(remote_head.as_str())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn signal_terminated_child_returns_a_nonzero_cli_status() {
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "signalled").await;

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args([
            "agent",
            "run",
            "signalled",
            "--",
            "sh",
            "-c",
            "kill -TERM $$",
        ])
        .current_dir(client.workspace.path())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(143));
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_to_agent_run_controller_terminates_the_interactive_process_group() {
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "controller-term").await;

    let pid_dir = tempfile::tempdir().unwrap();
    let child_pid_path = pid_dir.path().join("child.pid");
    let descendant_pid_path = pid_dir.path().join("descendant.pid");
    let mut controller = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    controller
        .args([
            "agent",
            "run",
            "controller-term",
            "--",
            "sh",
            "-c",
            "echo $$ > \"$1\"; sleep 30 & echo $! > \"$2\"; wait",
            "interactive-child",
            child_pid_path.to_str().unwrap(),
            descendant_pid_path.to_str().unwrap(),
        ])
        .current_dir(client.workspace.path())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let controller = controller.spawn().expect("start agent run controller");
    let controller_pid = controller.id().expect("controller pid");

    let (child_pid, descendant_pid) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let child = tokio::fs::read_to_string(&child_pid_path)
                .await
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            let descendant = tokio::fs::read_to_string(&descendant_pid_path)
                .await
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if let (Some(child), Some(descendant)) = (child, descendant) {
                break (child, descendant);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("interactive child published its process ids");
    assert!(feanorfs_agent_core::lock::pid_alive(child_pid));
    assert!(feanorfs_agent_core::lock::pid_alive(descendant_pid));

    // SAFETY: the exact controller pid was returned by this test's child
    // handle and remains owned until wait_with_output below.
    assert_eq!(
        unsafe { libc::kill(controller_pid as libc::pid_t, libc::SIGTERM) },
        0
    );
    let output = tokio::time::timeout(Duration::from_secs(10), controller.wait_with_output())
        .await
        .expect("SIGTERM controller teardown is bounded")
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(143),
        "controller should report SIGTERM after cleanup: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        while feanorfs_agent_core::lock::pid_alive(child_pid)
            || feanorfs_agent_core::lock::pid_alive(descendant_pid)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("controller SIGTERM must reap the child and every descendant");
    assert!(
        ContinuousOwnerLock::try_acquire(client.workspace.path(), "controller-term")
            .unwrap()
            .is_some(),
        "controller teardown releases continuous ownership"
    );
}

// ---------------------------------------------------------------------------
// Real-process events loop: signal wakeups and reconcile projections
// ---------------------------------------------------------------------------

#[tokio::test]
async fn events_loop_wakes_on_signals_and_projects_reconcile_events() {
    let _serial = REAL_PROCESS_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    require_format_v3(client.workspace.path());
    seed_and_push(&server, &client).await;
    spawn_test_agent(&client, &server, "agent-events").await;
    let config = load_config(client.workspace.path()).unwrap();

    // Simulate an active controller owned by this process: hold the lease and
    // publish a bounded status projection the events loop can observe.
    let lock = ContinuousOwnerLock::try_acquire(client.workspace.path(), "agent-events")
        .unwrap()
        .expect("interactive lease");
    let write_status = |phase: ContinuousPhase| {
        feanorfs_agent_core::write_continuous_status(
            client.workspace.path(),
            "agent-events",
            &feanorfs_agent_core::build_status(
                "agent-events",
                true,
                phase,
                Some("0".repeat(64)),
                Some("1".repeat(64)),
                Some("2".repeat(64)),
                false,
                0,
                None,
                Some(std::process::id()),
                None,
            ),
        )
        .unwrap()
    };
    write_status(feanorfs_common::ContinuousPhase::ReconcilingLocal);

    let mut events = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args(["events"])
        .current_dir(client.workspace.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn events loop");
    let stdout = events.stdout.take().expect("events stdout");
    let mut lines = tokio::io::BufReader::new(stdout).lines();

    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();
    let publish = || {
        feanorfs_agent_core::send_message(
            &ctx,
            AgentMessageInput {
                to: "*".to_string(),
                kind: AgentMessageKind::Status,
                body: "secret-checkpoint-body".to_string(),
                about_snapshot: None,
                reply_to: None,
                from: Some("human".to_string()),
            },
        )
    };

    // Wait for the startup sync_state line so the events loop has already
    // observed its initial head before we publish; a signal published before
    // the first observation would simply become the initial cursor.
    let startup = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
        .await
        .expect("events loop must emit its startup sync_state")
        .expect("startup line")
        .expect("startup sync_state");
    assert!(
        startup.contains("\"event\":\"sync_state\""),
        "startup line was: {startup}"
    );

    // First signal wakes the loop: it projects the reconciling phase and the
    // new signal (metadata only).
    publish().await.expect("signal a");

    let mut saw_message = false;
    let mut saw_started = false;
    let mut saw_reconciled = false;
    let mut leaked = false;
    let mut captured: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !(saw_message && saw_started && saw_reconciled)
    {
        let Ok(Some(line)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
                .await
                .map_err(|_| anyhow::anyhow!("events line timeout"))
                .and_then(|result| result.map_err(anyhow::Error::from))
        else {
            // Timeout or stream end: the outer deadline bounds the loop.
            continue;
        };
        captured.push(line.clone());
        if line.contains("secret-checkpoint-body") {
            leaked = true;
        }
        if line.contains("\"event\":\"agent_message\"") {
            saw_message = true;
        }
        if line.contains("\"event\":\"agent_reconcile_started\"") {
            saw_started = true;
        }
        if line.contains("\"event\":\"agent_reconciled\"") {
            saw_reconciled = true;
        }
        if saw_message && saw_started && !saw_reconciled {
            // Settle the controller and wake the loop again.
            write_status(feanorfs_common::ContinuousPhase::Idle);
            publish().await.expect("signal b");
        }
    }
    let _ = events.kill().await;
    assert!(
        saw_message,
        "signal wakeup emitted agent_message; saw: {captured:?}"
    );
    assert!(
        saw_started,
        "controller phase projected agent_reconcile_started; saw: {captured:?}"
    );
    assert!(
        saw_reconciled,
        "settled phase projected agent_reconciled; saw: {captured:?}"
    );
    assert!(!leaked, "events must never carry message bodies");
    let _ = events.kill().await;
    drop(lock);
}

async fn land_test_agent(
    client: &TestClient,
    server: &TestServer,
    name: &str,
) -> feanorfs_common::AgentLandResult {
    let config = load_config(client.workspace.path()).unwrap();
    feanorfs_client::land_agent(
        client.workspace.path(),
        &client.db,
        &server.api,
        &config.workspace_id,
        name,
        config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap()
}

/// Regression (CI, 2026-10-02): a no-op land made a head that still carried
/// an unresolved shared conflict the agent's base. Against that base the
/// agent's own published leg read as a fresh edit, so after the conflict was
/// resolved refresh kept the leg and the next land published it over the
/// resolution.
#[tokio::test]
async fn a_no_op_land_never_adopts_an_unresolved_conflict_as_the_agent_base() {
    let server = spawn_test_server().await;
    let a = spawn_test_client_with_server(&server).await;
    let b = spawn_test_client_with_server(&server).await;
    require_format_v3(a.workspace.path());
    require_format_v3(b.workspace.path());
    seed_and_push(&server, &a).await;
    spawn_test_agent(&a, &server, "edit-a").await;
    spawn_test_agent(&b, &server, "edit-b").await;
    let tree_a = agent_worktree(&a, "edit-a");
    let tree_b = agent_worktree(&b, "edit-b");

    std::fs::write(tree_b.join("seed.txt"), b"version-b").unwrap();
    assert!(land_test_agent(&b, &server, "edit-b")
        .await
        .conflicts
        .is_empty());
    std::fs::write(tree_a.join("seed.txt"), b"version-a").unwrap();
    assert_eq!(
        land_test_agent(&a, &server, "edit-a").await.conflicts.len(),
        1
    );
    // Nothing new to publish while the shared head is conflicted.
    land_test_agent(&b, &server, "edit-b").await;

    let replacement = a.workspace.path().join("..").join("replacement");
    std::fs::write(&replacement, b"resolved").unwrap();
    let ctx = agent_ctx(&a, &server).await;
    feanorfs_agent_core::resolve_conflict(
        &ctx,
        "seed.txt",
        feanorfs_agent_core::ResolveKeep::File,
        Some(&replacement),
    )
    .await
    .unwrap();

    let config = load_config(b.workspace.path()).unwrap();
    feanorfs_client::refresh_agent(
        b.workspace.path(),
        &b.db,
        &server.api,
        &config.workspace_id,
        "edit-b",
        config.encryption_password.as_deref(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(tree_b.join("seed.txt")).unwrap(), b"resolved");
    land_test_agent(&b, &server, "edit-b").await;
    let head = server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let engine = SnapshotEngine::new(&ctx);
    assert_eq!(
        engine.load_files(&head).await.unwrap()["seed.txt"].size,
        b"resolved".len() as u64
    );
}
