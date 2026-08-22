use super::*;

#[cfg(unix)]
#[test]
fn supervisor_status_uses_the_native_process_start_epoch() {
    let pid = std::process::id();
    let started_at = current_supervisor_started_at();
    let native = process_start_epoch(pid).expect("current process has a native start epoch");

    // CI hosts slew their clocks (NTP steps up to ~1s); allow that slack
    // while still catching any wrong-source epoch.
    assert!(
        (started_at as i64 - native as i64).abs() <= 2,
        "recorded start {started_at} diverges from native {native}"
    );
    assert!(recorded_process_is_alive(Some(pid), started_at));
}

#[test]
fn null_pid_orphan_cleanup_is_fail_closed_for_non_stopped_states() {
    let program = std::env::current_exe().unwrap();
    let identity = OrphanIdentity::Worker {
        subcommand: "runner-run",
        operand: "/null-pid-runner".to_string(),
        process_start_id: None,
        job_owned: false,
    };
    for state in [
        ChildState::Running,
        ChildState::Backoff,
        ChildState::Stopping,
    ] {
        let mut cleanup = pending_orphan_cleanup_with_state(
            &program,
            None,
            0,
            state,
            &identity,
            managed_command_line(&program, "runner-run", "/null-pid-runner"),
            STOP_GRACE,
        );
        retry_one_pending_orphan_cleanup(&mut cleanup);
        assert!(
            !cleanup.ticket.is_complete(),
            "pid:null with {state:?} must remain unresolved"
        );
    }
    let mut stopped = pending_orphan_cleanup_with_state(
        &program,
        None,
        0,
        ChildState::Stopped,
        &identity,
        managed_command_line(&program, "runner-run", "/null-pid-runner"),
        STOP_GRACE,
    );
    retry_one_pending_orphan_cleanup(&mut stopped);
    assert!(stopped.ticket.is_complete());

    let job_owned_identity = OrphanIdentity::Worker {
        subcommand: "runner-run",
        operand: "/job-owned-null".to_string(),
        process_start_id: None,
        job_owned: true,
    };
    let mut job_owned = pending_orphan_cleanup_with_state(
        &program,
        None,
        0,
        ChildState::Stopped,
        &job_owned_identity,
        managed_command_line(&program, "runner-run", "/job-owned-null"),
        STOP_GRACE,
    );
    retry_one_pending_orphan_cleanup(&mut job_owned);
    assert!(!job_owned.ticket.is_complete());
}

#[test]
fn null_pid_workspace_without_a_verified_watcher_does_not_block_restart() {
    let program = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let canonical = CanonicalWorkspacePath::canonicalize(workspace.path()).unwrap();
    let identity = OrphanIdentity::Worker {
        subcommand: "run",
        operand: canonical.as_str().to_string(),
        process_start_id: None,
        job_owned: false,
    };
    let mut cleanup = pending_orphan_cleanup_with_state(
        &program,
        None,
        0,
        ChildState::Stopping,
        &identity,
        managed_command_line(&program, "run", canonical.as_str()),
        STOP_GRACE,
    );

    retry_one_pending_orphan_cleanup(&mut cleanup);

    assert!(cleanup.ticket.is_complete());
}

#[test]
fn null_pid_tray_cleanup_does_not_permanently_block_relaunch() {
    let program = std::env::current_exe().unwrap();
    let tray = PathBuf::from("/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray");
    let identity = OrphanIdentity::Tray {
        path: tray.clone(),
        process_start_id: None,
    };
    for state in [
        ChildState::Running,
        ChildState::Backoff,
        ChildState::Stopping,
        ChildState::Stopped,
    ] {
        let mut cleanup = pending_orphan_cleanup_with_state(
            &program,
            None,
            0,
            state,
            &identity,
            managed_tray_command_line(&tray),
            STOP_GRACE,
        );
        assert_eq!(cleanup.spec.args, managed_tray_args());
        assert_eq!(cleanup.expected_command, managed_tray_command_line(&tray));
        retry_one_pending_orphan_cleanup(&mut cleanup);
        assert!(
            cleanup.ticket.is_complete(),
            "a pid-less tray projection in {state:?} must not block UI relaunch"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_refreshes_stale_handoff_identity_before_respawn() {
    let spec = ChildSpec {
        kind: ChildKind::Hub,
        program: PathBuf::from("/bin/sleep"),
        args: vec![OsString::from("30")],
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let expected = process_tree::executable_identity_for_path(&spec.program)
        .expect("sleep has an executable identity");
    let mut managed = ManagedChild::new(spec.clone());
    managed.expected_executable_identity = Some("unix-devino:0:0".to_string());
    let mut children = BTreeMap::from([(HUB_CHILD_KEY.to_string(), managed)]);
    let desired = BTreeMap::from([(HUB_CHILD_KEY.to_string(), spec.clone())]);

    reconcile(&mut children, &desired, false)
        .await
        .expect("replacement child starts");

    let replacement = children.get(HUB_CHILD_KEY).expect("managed hub");
    assert_eq!(replacement.spec.program, spec.program);
    assert_eq!(
        replacement.expected_executable_identity.as_deref(),
        Some(expected.as_str()),
        "the replacement must not inherit the orphaned image identity"
    );
    assert_eq!(
        build_status(&children, 1)
            .hub
            .and_then(|child| child.executable_identity),
        Some(expected)
    );

    let remaining = terminate_all_children(children).await;
    assert!(remaining.is_empty(), "replacement child is reaped");
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_removal_keeps_child_owned_when_primary_enqueue_fails() {
    let _guard = REAPER_TEST_LOCK.lock().await;
    let _reset = ReaperTestReset;
    TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
    TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);
    CHILD_REAPER.fail_worker_start_for_test(false);
    CHILD_REAPER.fail_next_enqueue_for_test();

    let child = spawn_term_ignoring_child().await;
    let pid = child.id().expect("termination test child pid");
    assert!(feanorfs_agent_core::lock::pid_alive(pid));
    let spec = ChildSpec {
        kind: ChildKind::Workspace(cwp("/reconcile-removal")),
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let mut managed = ManagedChild::new(spec);
    managed.child = Some(child);
    let mut children = BTreeMap::from([("workspace:test".to_string(), managed)]);

    let result = reconcile(&mut children, &BTreeMap::new(), false).await;
    assert!(
        result.is_ok(),
        "bounded termination is retained for reconciliation"
    );
    assert_eq!(children.len(), 1, "reconcile retains the stopping entry");
    assert_eq!(
        children.values().next().unwrap().state,
        ChildState::Stopping,
        "a deferred reaper handoff remains explicitly stopping"
    );
    assert_pid_reaped(pid).await;

    reconcile(&mut children, &BTreeMap::new(), false)
        .await
        .expect("reaper completion reconciles on the next pass");
    assert!(
        children.is_empty(),
        "completed child is removed after reaping"
    );

    TEST_FORCE_REAP_TIMEOUT.store(false, AtomicOrdering::Release);
    TEST_TERMINATION_GRACE_MILLIS.store(0, AtomicOrdering::Release);
}

#[cfg(unix)]
#[tokio::test]
async fn deferred_runner_stop_withholds_reconcile_until_reaper_completion() {
    let _guard = REAPER_TEST_LOCK.lock().await;
    let _reset = ReaperTestReset;
    TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
    TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);

    let (_dir, workspace, _store) = configured_runner_fixture();
    let child = spawn_term_ignoring_child().await;
    let pid = child.id().expect("runner termination test child pid");
    assert!(feanorfs_agent_core::lock::pid_alive(pid));
    let workspace = cwp(workspace.to_str().unwrap());
    let spec = ChildSpec {
        kind: ChildKind::Runner(workspace.clone()),
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let key = runner_child_key(workspace.as_str());
    let mut children = BTreeMap::from([(key, {
        let mut managed = ManagedChild::new(spec);
        managed.child = Some(child);
        managed
    })]);

    reconcile(&mut children, &BTreeMap::new(), false)
        .await
        .expect("deferred runner termination is retained, not dropped");
    assert_eq!(children.len(), 1);
    assert_eq!(
        children.values().next().unwrap().state,
        ChildState::Stopping
    );
    let reap_ticket = children
        .values()
        .next()
        .and_then(|managed| managed.pending_reap.clone())
        .expect("deferred runner termination retains its reaper ticket");
    assert!(
        !runner_reconciliation_complete(&children, &SupervisorRegistry::default()),
        "durable runner stop acknowledgement must remain gated"
    );
    assert!(publish_runner_reconcile_ack(
        &children,
        &SupervisorRegistry::default(),
        now_epoch(),
        1,
    )
    .is_err());

    await_reap_ticket(&reap_ticket).await;
    assert_pid_reaped(pid).await;
    reconcile(&mut children, &BTreeMap::new(), false)
        .await
        .expect("reaper completion and runner cleanup reconcile");
    assert!(children.is_empty());
    assert!(runner_reconciliation_complete(
        &children,
        &SupervisorRegistry::default()
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_keeps_child_owned_when_reaper_worker_start_fails() {
    let _guard = REAPER_TEST_LOCK.lock().await;
    let _reset = ReaperTestReset;
    TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
    TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);
    CHILD_REAPER.fail_worker_start_for_test(true);

    let child = spawn_term_ignoring_child().await;
    let pid = child.id().expect("shutdown test child pid");
    let spec = ChildSpec {
        kind: ChildKind::Hub,
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let mut managed = ManagedChild::new(spec);
    managed.child = Some(child);
    let children = BTreeMap::from([("component:hub".to_string(), managed)]);

    terminate_all_children(children).await;
    assert_pid_reaped(pid).await;

    CHILD_REAPER.fail_worker_start_for_test(false);
    TEST_FORCE_REAP_TIMEOUT.store(false, AtomicOrdering::Release);
    TEST_TERMINATION_GRACE_MILLIS.store(0, AtomicOrdering::Release);
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_recovers_child_when_termination_task_panics() {
    let _guard = REAPER_TEST_LOCK.lock().await;
    let _reset = ReaperTestReset;
    TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
    TEST_SHUTDOWN_PANIC_ONCE.store(true, AtomicOrdering::Release);

    let child = spawn_term_ignoring_child().await;
    let pid = child.id().expect("panic recovery child pid");
    let spec = ChildSpec {
        kind: ChildKind::Hub,
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let remaining = terminate_all_children(BTreeMap::from([(
        "component:hub".to_string(),
        ManagedChild {
            child: Some(child),
            ..ManagedChild::new(spec)
        },
    )]))
    .await;
    assert!(remaining.is_empty(), "panic recovery must finish cleanup");
    assert_pid_reaped(pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_awaits_normal_background_reaper_handoff() {
    let _guard = REAPER_TEST_LOCK.lock().await;
    let _reset = ReaperTestReset;
    TEST_TERMINATION_GRACE_MILLIS.store(1, AtomicOrdering::Release);
    // Force the bounded wait path to hand the still-owned Tokio Child to
    // the normal background reaper. The shutdown helper must retain the
    // ManagedChild and await this ticket before returning an empty map.
    TEST_FORCE_REAP_TIMEOUT.store(true, AtomicOrdering::Release);
    CHILD_REAPER.fail_worker_start_for_test(false);

    let child = spawn_term_ignoring_child().await;
    let pid = child.id().expect("normal handoff child pid");
    let spec = ChildSpec {
        kind: ChildKind::Hub,
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let mut managed = ManagedChild::new(spec);
    managed.child = Some(child);
    let remaining =
        terminate_all_children(BTreeMap::from([("component:hub".to_string(), managed)])).await;
    assert!(remaining.is_empty(), "normal reaper handoff was awaited");
    assert_pid_reaped(pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn unresolved_startup_runner_authority_retries_and_then_clears() {
    let (_dir, workspace, _store) = configured_runner_fixture();
    let canonical = workspace.to_string_lossy().into_owned();
    let program = std::env::current_exe().unwrap();
    let identity = OrphanIdentity::Worker {
        subcommand: "runner-run",
        operand: canonical.clone(),
        process_start_id: None,
        job_owned: false,
    };
    // The current supervisor PID is deliberately unkillable by the
    // orphan path. This models an unresolved live/mismatched authority
    // and proves that it remains in the child map rather than being
    // dropped at the first reconcile.
    let pending = pending_orphan_cleanup(
        &program,
        Some(std::process::id()),
        0,
        &identity,
        managed_command_line(&program, "runner-run", &canonical),
        Duration::from_millis(1),
    );
    let key = pending.key.clone();
    let expected_executable_identity = pending.executable_identity.clone();
    let mut children = BTreeMap::from([(
        key,
        ManagedChild {
            spec: pending.spec.clone(),
            expected_executable_identity,
            child: None,
            pending_reap: None,
            pending_orphan: Some(pending),
            process_tree: None,
            startup_gate: None,
            owned_pid: None,
            owned_process_start_id: None,
            owned_since: 0,
            state: ChildState::Stopping,
            restarts: 0,
            last_exit: None,
            backoff_until: None,
            spawned_at: None,
        },
    )]);
    reconcile(&mut children, &BTreeMap::new(), false)
        .await
        .unwrap();
    assert_eq!(children.len(), 1, "unresolved runner authority is retained");
    assert!(!runner_reconciliation_complete(
        &children,
        &SupervisorRegistry::default()
    ));

    // Recovery is deterministic: once the recorded PID is gone, the
    // retry completes the orphan ticket, checkpoints runner state, and
    // only then removes the stale runner entry.
    children
        .values_mut()
        .next()
        .unwrap()
        .pending_orphan
        .as_mut()
        .unwrap()
        .pid = Some(999_999);
    reconcile(&mut children, &BTreeMap::new(), false)
        .await
        .unwrap();
    assert!(children.is_empty(), "recovered runner authority is cleared");
}

#[test]
fn pending_orphan_status_preserves_process_identity_and_start_time() {
    let program = std::env::current_exe().unwrap();
    let identity = OrphanIdentity::Worker {
        subcommand: "runner-run",
        operand: "/pending-runner".to_string(),
        process_start_id: Some("linux:123:456".to_string()),
        job_owned: false,
    };
    let pending = pending_orphan_cleanup(
        &program,
        Some(123),
        987,
        &identity,
        managed_command_line(&program, "runner-run", "/pending-runner"),
        STOP_GRACE,
    );
    let key = pending.key.clone();
    let expected_executable_identity = pending.executable_identity.clone();
    let mut children = BTreeMap::new();
    children.insert(
        key,
        ManagedChild {
            spec: pending.spec.clone(),
            expected_executable_identity,
            child: None,
            pending_reap: None,
            pending_orphan: Some(pending),
            process_tree: None,
            startup_gate: None,
            owned_pid: None,
            owned_process_start_id: None,
            owned_since: 0,
            state: ChildState::Stopping,
            restarts: 0,
            last_exit: None,
            backoff_until: None,
            spawned_at: None,
        },
    );
    let status = build_status(&children, 1);
    let child = status.runners.get("/pending-runner").unwrap();
    assert_eq!(child.pid, Some(123));
    assert_eq!(child.process_start_id.as_deref(), Some("linux:123:456"));
    assert_eq!(child.since, 987);
    assert_eq!(child.state, ChildState::Stopping);
}

#[test]
fn supervisor_lock_is_exclusive_and_reusable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.lock");
    let owner_path = supervisor_lock_owner_path_at(&path);
    let first = acquire_supervisor_lock_at(&path)
        .unwrap()
        .expect("first supervisor owns lock");
    assert_eq!(
        read_supervisor_lock_owner_at(&path).map(|owner| owner.pid),
        Some(std::process::id())
    );
    // A second supervisor in the same process cannot re-acquire: flock is
    // per open-file-description, so this exercises the cross-process path.
    let second = acquire_supervisor_lock_at(&path).unwrap();
    assert!(second.is_none());
    drop(first);
    assert!(!owner_path.exists());
    fs::write(&owner_path, b"stale-owner").unwrap();
    let third = acquire_supervisor_lock_at(&path)
        .unwrap()
        .expect("third supervisor reuses released lock");
    assert_eq!(
        read_supervisor_lock_owner_at(&path).map(|owner| owner.pid),
        Some(std::process::id())
    );
    drop(third);
    assert!(!owner_path.exists());
}
