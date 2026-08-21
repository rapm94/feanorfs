use super::*;

#[test]
fn stray_watcher_matching_requires_exact_binary_invocation() {
    let program = Path::new("/usr/local/bin/feanorfs");
    let canonical = "/Users/raulpuigbo/p/net";
    // The real watcher command line matches.
    assert!(watcher_command_matches(
        "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net",
        program,
        canonical
    ));
    // Innocent processes whose command lines merely MENTION the product
    // name or the workspace path must never match: killing them would be
    // a real bug.
    for innocent in [
        "vim /Users/raulpuigbo/p/net/feanorfs-notes.txt",
        "code /Users/raulpuigbo/p/net/feanorfs/README.md",
        "rg feanorfs /Users/raulpuigbo/p/net",
        "/usr/bin/feanorfs-backup-tool --dir /Users/raulpuigbo/p/net",
        "/usr/local/bin/feanorfs sync /Users/raulpuigbo/p/net",
        "/usr/local/bin/feanorfs service status /Users/raulpuigbo/p/net",
        "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/network",
        "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net --extra",
        "/usr/local/bin/feanorfs-helper service run /Users/raulpuigbo/p/net",
    ] {
        assert!(
            !watcher_command_matches(innocent, program, canonical),
            "innocent command line matched: {innocent}"
        );
    }
    // A different workspace must not match either.
    assert!(!watcher_command_matches(
        "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/logs",
        program,
        canonical
    ));
}

#[test]
fn orphan_reaping_matching_requires_managed_subcommand_or_tray() {
    let program = Path::new("/usr/local/bin/feanorfs");
    assert!(managed_orphan_command_matches(
        "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net",
        program,
        "run",
        "/Users/raulpuigbo/p/net",
    ));
    assert!(managed_orphan_command_matches(
        "/usr/local/bin/feanorfs service hub-run /Users/raulpuigbo/.feanorfs/hub-data",
        program,
        "hub-run",
        "/Users/raulpuigbo/.feanorfs/hub-data",
    ));
    assert!(managed_orphan_command_matches(
        "/usr/local/bin/feanorfs service runner-run /Users/raulpuigbo/p/net",
        program,
        "runner-run",
        "/Users/raulpuigbo/p/net",
    ));
    for innocent in [
        "/usr/local/bin/feanorfs start --foreground /Users/raulpuigbo/p/net",
        "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/network",
        "/usr/local/bin/feanorfs service run /Users/raulpuigbo/p/net --extra",
        "/usr/local/bin/feanorfs-helper service run /Users/raulpuigbo/p/net",
        "vim /Users/raulpuigbo/p/net/service run notes.txt",
        "python3 /tmp/feanorfs-tray-test.py",
        "/usr/bin/feanorfs-helper --service run",
    ] {
        assert!(
            !managed_orphan_command_matches(innocent, program, "run", "/Users/raulpuigbo/p/net",),
            "innocent command line matched for reaping: {innocent}"
        );
    }
    let tray = Path::new("/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray");
    assert!(tray_orphan_command_matches(
        "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray --managed",
        tray,
    ));
    assert!(!tray_orphan_command_matches(
        "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray",
        tray,
    ));
    assert!(!tray_orphan_command_matches("/tmp/feanorfs-tray", tray,));
    assert!(!tray_orphan_command_matches(
        "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray --first-run",
        tray,
    ));
    assert!(!tray_orphan_command_matches(
        "/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray --managed --extra",
        tray,
    ));
}

#[test]
fn force_escalation_requires_identity_and_command_ownership() {
    assert!(force_termination_allowed(true, true));
    assert!(!force_termination_allowed(false, true));
    assert!(!force_termination_allowed(true, false));
    assert!(!force_termination_allowed(false, false));
}

#[tokio::test]
async fn supervisor_reaper_handoff_reaps_owned_child() {
    #[cfg(unix)]
    let _guard = REAPER_TEST_LOCK.lock().await;
    let mut child = spawn_long_running_test_child();
    let pid = child.id().expect("reaper child pid");
    let _ = child.start_kill();
    let mut child = Some(child);
    CHILD_REAPER.enqueue_or_wait(&mut child).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while feanorfs_agent_core::lock::pid_alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("supervisor reaper reaped child");
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn supervisor_termination_closes_job_owned_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let descendant_path = temp.path().join("descendant.pid");
    let program = std::env::current_exe().unwrap();
    let spec = ChildSpec {
        kind: ChildKind::Hub,
        program: program.clone(),
        args: vec![
            "--ignored".into(),
            "--exact".into(),
            "cli::supervisor::tests::platform::supervisor_job_descendant_helper".into(),
            "--nocapture".into(),
        ],
        env: vec![(
            "FEANORFS_SUPERVISOR_DESCENDANT".into(),
            descendant_path.as_os_str().to_owned(),
        )],
        restart_on_zero_exit: true,
    };
    let key = "component:hub".to_string();
    let mut desired = BTreeMap::new();
    desired.insert(key.clone(), spec);
    let mut children = BTreeMap::new();
    reconcile(&mut children, &desired, false).await.unwrap();
    #[cfg(target_os = "windows")]
    release_test_suspended_child(&mut children, &key);
    // The nested test harness starts another copy of this binary.  On a
    // loaded Windows runner that startup can exceed the supervisor's
    // normal five-second service readiness bound even though the Job
    // adoption itself is correct.  Keep a bounded test-only window wide
    // enough to observe the descendant without weakening the product
    // timeout or the ownership assertions below.
    let descendant = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(value) = fs::read_to_string(&descendant_path) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("supervisor descendant became ready");
    let _ = reconcile(&mut children, &BTreeMap::new(), false).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while feanorfs_agent_core::lock::pid_alive(descendant) && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!feanorfs_agent_core::lock::pid_alive(descendant));
}

#[cfg(unix)]
#[tokio::test]
async fn owned_child_falls_back_to_direct_kill_when_identity_probe_is_unavailable() {
    let _guard = REAPER_TEST_LOCK.lock().await;
    TEST_IDENTITY_UNAVAILABLE.store(true, AtomicOrdering::Release);
    let child = spawn_term_ignoring_child().await;
    let pid = child.id().expect("identity fallback child pid");
    let spec = ChildSpec {
        kind: ChildKind::Hub,
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let mut managed = ManagedChild::new(spec);
    managed.child = Some(child);
    let result = terminate_child(&mut managed).await;
    TEST_IDENTITY_UNAVAILABLE.store(false, AtomicOrdering::Release);
    assert!(
        result.is_ok(),
        "direct Child kill should remain recoverable"
    );
    assert_pid_reaped(pid).await;
}

#[tokio::test]
async fn runner_exit_cleanup_waits_for_the_exact_session_then_checkpoints() {
    let (_dir, workspace, store) = configured_runner_fixture();
    let session = store
        .execution_session(
            &workspace,
            feanorfs_agent_core::RunnerExecutionMode::Foreground,
        )
        .unwrap();
    let request_id = id('b');
    session
        .admit_inbox(&feanorfs_common::AgentInboxResult {
            cursor: id('c'),
            cursor_reset: false,
            messages: vec![feanorfs_common::AgentMessage {
                message_id: request_id.clone(),
                from: "requester".to_string(),
                to: "worker".to_string(),
                kind: feanorfs_common::AgentMessageKind::Request,
                body: "run".to_string(),
                about_snapshot: id('a'),
                reply_to: None,
                created_at_ms: 1,
            }],
        })
        .unwrap();
    session.begin_next(&id('c')).unwrap();
    session
        .mark_spawned(
            &request_id,
            std::process::id(),
            "unsupported-test-process-identity",
        )
        .unwrap();

    let _ = finish_runner_workspace_exit(&workspace);
    assert_eq!(
        store.status().unwrap().phase,
        feanorfs_agent_core::RunnerPhase::Running
    );

    drop(session);
    let _ = finish_runner_workspace_exit(&workspace);
    let status = store.status().unwrap();
    assert_eq!(
        status.phase,
        feanorfs_agent_core::RunnerPhase::NeedsAttention
    );
    assert_eq!(
        status.attention,
        Some(feanorfs_agent_core::RunnerAttention::AmbiguousExecution)
    );
}

#[test]
fn workers_restart_on_clean_exit_but_tray_respects_quit() {
    // Hub, workspace, and runner workers are "always running" services: a clean
    // exit (exit code 0) must still be restarted.
    let mut hub = ManagedChild::new(ChildSpec {
        kind: ChildKind::Hub,
        program: PathBuf::from("/usr/local/bin/feanorfs"),
        args: vec![],
        env: vec![],
        restart_on_zero_exit: true,
    });
    hub.last_exit = Some(0);
    assert!(should_respawn(&hub));
    let mut runner = ManagedChild::new(ChildSpec {
        kind: ChildKind::Runner(cwp("/workspace")),
        program: PathBuf::from("/usr/local/bin/feanorfs"),
        args: vec![],
        env: vec![],
        restart_on_zero_exit: true,
    });
    runner.last_exit = Some(0);
    assert!(should_respawn(&runner));
    // The tray exits 0 when the user quits it; it must stay stopped.
    let mut tray = ManagedChild::new(ChildSpec {
        kind: ChildKind::Tray,
        program: PathBuf::from("/usr/local/bin/feanorfs-tray"),
        args: vec![],
        env: vec![],
        restart_on_zero_exit: false,
    });
    tray.last_exit = Some(0);
    assert!(!should_respawn(&tray));
    // A crashed tray (nonzero exit) is restarted.
    tray.last_exit = Some(1);
    assert!(should_respawn(&tray));
}

#[cfg(not(target_os = "windows"))]
#[test]
fn desired_tray_spec_marks_supervised_contention_as_retryable() {
    let tray_program = PathBuf::from("/usr/local/bin/feanorfs-tray");
    let desired = desired_specs(&SupervisorRegistry::default(), &Some(tray_program.clone()))
        .expect("build desired supervisor children");
    let tray = &desired[TRAY_CHILD_KEY];
    assert_eq!(tray.kind, ChildKind::Tray);
    assert_eq!(tray.program, tray_program);
    assert!(!tray.restart_on_zero_exit);
    assert_eq!(tray.args, managed_tray_args());
}

#[tokio::test]
async fn clean_runner_exit_enters_bounded_restart_backoff() {
    let (_dir, workspace, store) = configured_runner_fixture();
    store.set_enabled(true).unwrap();
    let workspace = cwp(workspace.to_str().unwrap());
    let spec = ChildSpec {
        kind: ChildKind::Runner(workspace.clone()),
        program: which::which("true").unwrap(),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let key = runner_child_key(workspace.as_str());
    let mut desired = BTreeMap::new();
    desired.insert(key.clone(), spec);
    let mut children = BTreeMap::new();
    assert!(reconcile(&mut children, &desired, false).await.unwrap());
    // Isolate the first exit-to-backoff transition. On a loaded Windows
    // worker, cleanup can consume the one-second backoff and otherwise
    // let this test observe a newly spawned (and suspended) replacement.
    store.set_enabled(false).unwrap();
    #[cfg(target_os = "windows")]
    release_test_suspended_child(&mut children, &key);
    // Process creation is materially slower on Windows when the complete
    // client test binary is starting children in parallel.  A fixed 20 ms
    // sleep races the first `true` process and makes this test assert on
    // the still-running state.  Poll the exact managed child until its
    // exit is observed instead; the backoff assertion remains unchanged.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let changed = reconcile(&mut children, &desired, false).await.unwrap();
        if children.get(&key).is_some_and(|managed| {
            managed.last_exit == Some(0) && managed.state == ChildState::Backoff
        }) {
            assert!(changed);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "clean runner test child did not exit within the bounded test window"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let managed = &children[&key];
    assert_eq!(managed.last_exit, Some(0));
    assert_eq!(managed.state, ChildState::Backoff);
    assert_eq!(managed.restarts, 1);
    assert!(managed.backoff_until.is_some());
}

#[tokio::test]
async fn stale_desired_runner_is_not_spawned_after_disable() {
    let (_dir, workspace, store) = configured_runner_fixture();
    store.set_enabled(true).unwrap();
    let workspace = cwp(workspace.to_str().unwrap());
    let key = runner_child_key(workspace.as_str());
    let mut desired = BTreeMap::new();
    desired.insert(
        key,
        ChildSpec {
            kind: ChildKind::Runner(workspace),
            program: which::which("true").unwrap(),
            args: Vec::new(),
            env: Vec::new(),
            restart_on_zero_exit: true,
        },
    );
    store.set_enabled(false).unwrap();

    let mut children = BTreeMap::new();
    assert!(!reconcile(&mut children, &desired, false).await.unwrap());
    assert!(children.is_empty());
}

#[test]
fn stray_watcher_requires_fresh_marker() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    let state = feanorfs_agent_core::ensure_workspace_state(&workspace).unwrap();
    std::fs::write(state.join("watch.pid"), format!("{}\n", std::process::id())).unwrap();
    let marker = state.join("watch.pid");
    // Simulate an ancient marker (belongs to a dead watcher).
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let file = std::fs::File::options().write(true).open(&marker).unwrap();
    file.set_modified(old).unwrap();
    assert!(
        stray_workspace_watcher(&workspace.to_string_lossy()).is_none(),
        "stale watch markers must not produce killable pids"
    );
    // A fresh marker for an unrelated process (no feanorfs in argv) is
    // also not a stray: pid reuse must never kill an innocent process.
    // Use a live non-feanorfs process: the cargo test harness itself.
    std::fs::write(
        &marker,
        format!(
            "{}\n{}\n{}\n",
            std::process::id(),
            now_epoch(),
            process_start_epoch(std::process::id()).unwrap_or(0)
        ),
    )
    .unwrap();
    assert!(
        stray_workspace_watcher(&workspace.to_string_lossy()).is_none(),
        "live pids without a FeanorFS command line must not be killed"
    );
}

#[test]
fn clean_exit_without_restart_policy_stays_stopped() {
    let spec = ChildSpec {
        kind: ChildKind::Tray,
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: false,
    };
    let mut managed = ManagedChild::new(spec.clone());
    // Fresh child: spawn it.
    assert!(should_respawn(&managed));
    // Simulate a clean exit: never respawn (user quit the tray, hub
    // shutdown on purpose), and no backoff is engaged (no tight loop).
    managed.last_exit = Some(0);
    assert!(!should_respawn(&managed));
    managed.last_exit = Some(1);
    assert!(should_respawn(&managed), "crash exits must restart");
    // With an explicit restart-on-zero policy, clean exits restart.
    let mut managed = ManagedChild::new(ChildSpec {
        restart_on_zero_exit: true,
        ..spec
    });
    managed.last_exit = Some(0);
    assert!(should_respawn(&managed));
}

#[test]
fn backoff_holds_respawns_until_deadline() {
    let spec = ChildSpec {
        kind: ChildKind::Tray,
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: false,
    };
    let mut managed = ManagedChild::new(spec);
    managed.backoff_until = Some(Instant::now() + Duration::from_secs(60));
    assert!(!should_respawn(&managed));
    managed.backoff_until = None;
    assert!(should_respawn(&managed));
}

#[test]
fn backoff_grows_and_caps() {
    assert_eq!(CHILD_RESTART_BACKOFF.delay(1), Duration::from_secs(1));
    assert_eq!(CHILD_RESTART_BACKOFF.delay(2), Duration::from_secs(2));
    assert_eq!(CHILD_RESTART_BACKOFF.delay(3), Duration::from_secs(4));
    assert_eq!(CHILD_RESTART_BACKOFF.delay(20), Duration::from_secs(60));
}

#[tokio::test]
async fn desired_runner_spec_is_exact_redacted_and_state_gated() {
    let (_dir, workspace, store) = configured_runner_fixture();
    store.set_enabled(true).unwrap();
    let canonical = cwp(workspace.to_str().unwrap());
    let mut registry = SupervisorRegistry::default();
    registry.workspaces.push(canonical.clone());
    registry.runners.push(canonical.clone());

    let desired = desired_specs(&registry, &None).unwrap();
    let watcher = &desired[&workspace_child_key(canonical.as_str())];
    assert_eq!(watcher.kind, ChildKind::Workspace(canonical.clone()));
    assert!(watcher.restart_on_zero_exit);
    let runner = &desired[&runner_child_key(canonical.as_str())];
    assert_eq!(runner.kind, ChildKind::Runner(canonical.clone()));
    assert_eq!(
        runner.args,
        vec![
            OsString::from("service"),
            OsString::from("runner-run"),
            workspace.as_os_str().to_owned(),
        ]
    );
    assert!(runner.env.is_empty());
    assert!(runner.restart_on_zero_exit);
    assert_eq!(runner.program, std::env::current_exe().unwrap());
    assert_ne!(
        workspace_child_key(canonical.as_str()),
        runner_child_key(canonical.as_str())
    );

    store.set_enabled(false).unwrap();
    assert!(!desired_specs(&registry, &None)
        .unwrap()
        .contains_key(&runner_child_key(canonical.as_str())));

    store.set_enabled(true).unwrap();
    let session = store
        .execution_session(
            &workspace,
            feanorfs_agent_core::RunnerExecutionMode::Supervised,
        )
        .unwrap();
    session
        .admit_inbox(&feanorfs_common::AgentInboxResult {
            cursor: id('b'),
            cursor_reset: false,
            messages: vec![feanorfs_common::AgentMessage {
                message_id: id('1'),
                from: "human".to_string(),
                to: "worker".to_string(),
                kind: feanorfs_common::AgentMessageKind::Request,
                body: "private request body".to_string(),
                about_snapshot: id('a'),
                reply_to: None,
                created_at_ms: 1,
            }],
        })
        .unwrap();
    session.begin_next(&id('b')).unwrap();
    drop(session);
    drop(
        store
            .execution_session(
                &workspace,
                feanorfs_agent_core::RunnerExecutionMode::Supervised,
            )
            .unwrap(),
    );
    assert_eq!(
        store.status().unwrap().phase,
        feanorfs_agent_core::RunnerPhase::NeedsAttention
    );
    assert!(!desired_specs(&registry, &None)
        .unwrap()
        .contains_key(&runner_child_key(canonical.as_str())));
}

#[test]
fn desired_specs_skip_unavailable_workspaces() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = SupervisorRegistry::default();
    registry.workspaces.push(cwp(dir.path().to_str().unwrap()));
    registry.workspaces.push(cwp("/definitely/missing"));
    let desired = desired_specs(&registry, &None).unwrap();
    // The missing workspace is skipped; the real temp dir is included only
    // when it is a configured FeanorFS workspace, which it is not. The hub
    // and tray entries may legitimately appear on a dev machine.
    let workspace_keys: Vec<_> = desired
        .values()
        .filter(|spec| matches!(&spec.kind, ChildKind::Workspace(_) | ChildKind::Runner(_)))
        .map(|spec| &spec.kind)
        .collect();
    assert!(
        workspace_keys.is_empty(),
        "unexpected workspace keys: {workspace_keys:?}"
    );
}
