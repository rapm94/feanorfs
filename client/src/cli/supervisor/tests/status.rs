use super::*;

#[cfg(unix)]
#[tokio::test]
async fn pending_reaper_status_roundtrip_preserves_exact_handoff_identity() {
    let _guard = REAPER_TEST_LOCK.lock().await;
    let mut child = spawn_term_ignoring_child().await;
    let pid = child.id().expect("pending reaper child pid");
    let process_start_id = exact_child_process_start_id(pid).expect("native child identity");
    let since = now_epoch();
    let canonical = cwp("/pending-reaper");
    let spec = ChildSpec {
        kind: ChildKind::Runner(canonical.clone()),
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let mut managed = ManagedChild::new(spec.clone());
    // Simulate the post-deadline state: the Tokio Child has been handed to
    // the persistent reaper, while the supervisor retains only the exact
    // identity needed for a durable restart handoff.
    managed.state = ChildState::Stopping;
    managed.pending_reap = Some(ReapTicket::new());
    managed.owned_pid = Some(pid);
    managed.owned_process_start_id = Some(process_start_id.clone());
    managed.owned_since = since;
    let children = BTreeMap::from([(runner_child_key(canonical.as_str()), managed)]);

    let status = build_status(&children, 1);
    let encoded = serde_json::to_vec(&status).expect("serialize pending reaper status");
    let decoded: SupervisorStatus =
        serde_json::from_slice(&encoded).expect("deserialize pending reaper status");
    let recorded = decoded
        .runners
        .get(canonical.as_str())
        .expect("runner status entry");
    assert_eq!(recorded.pid, Some(pid));
    assert_eq!(
        recorded.process_start_id.as_deref(),
        Some(process_start_id.as_str())
    );
    assert_eq!(recorded.since, since);
    assert_eq!(recorded.state, ChildState::Stopping);

    // A replacement supervisor can carry these serialized fields into its
    // pending-orphan map. The stopping marker and exact identity prevent a
    // desired-set pass from respawning the runner before cleanup completes.
    let identity = OrphanIdentity::Worker {
        subcommand: "runner-run",
        operand: canonical.as_str().to_owned(),
        process_start_id: recorded.process_start_id.clone(),
        job_owned: false,
    };
    let pending = pending_orphan_cleanup(
        &std::env::current_exe().unwrap(),
        recorded.pid,
        recorded.since,
        &identity,
        managed_command_line(
            &std::env::current_exe().unwrap(),
            "runner-run",
            canonical.as_str(),
        ),
        STOP_GRACE,
    );
    assert_eq!(pending.pid, Some(pid));
    assert_eq!(pending.expected_since, since);
    assert_eq!(
        pending.process_start_id.as_deref(),
        Some(process_start_id.as_str())
    );
    let expected_executable_identity = pending.executable_identity.clone();
    let replacement = ManagedChild {
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
    };
    assert!(!should_respawn(&replacement));

    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[test]
fn status_liveness_rejects_stale_or_dead_children() {
    let current_start = process_start_epoch(std::process::id()).unwrap_or(1);
    let current_identity =
        process_tree::process_start_identifier(std::process::id(), "status-test");
    let mut status = SupervisorStatus {
        pid: Some(std::process::id()),
        process_start_id: Some(current_identity.clone()),
        started_at: current_start,
        ..SupervisorStatus::default()
    };
    // Alive supervisor + alive child pid (this test process) -> running.
    status.workspaces.insert(
        "/ws".into(),
        ChildStatus {
            pid: Some(std::process::id()),
            process_start_id: Some(current_identity.clone()),
            job_owned: false,
            executable_identity: None,
            state: ChildState::Running,
            restarts: 0,
            last_exit: None,
            since: current_start,
        },
    );
    assert!(child_is_running(&status, "/ws"));
    status.runners.insert(
        "/ws".into(),
        ChildStatus {
            pid: Some(std::process::id()),
            process_start_id: Some(current_identity),
            job_owned: false,
            executable_identity: None,
            state: ChildState::Running,
            restarts: 0,
            last_exit: None,
            since: current_start,
        },
    );
    assert!(runner_child_is_running(&status, "/ws"));
    #[cfg(unix)]
    {
        status.workspaces.get_mut("/ws").unwrap().since = current_start.saturating_sub(30);
    }
    #[cfg(target_os = "windows")]
    {
        // Windows intentionally does not infer process liveness from the
        // wall-clock `since` field.  Exercise the same stale-child
        // boundary with a mismatched kernel creation token instead.
        status.workspaces.get_mut("/ws").unwrap().process_start_id = Some("windows:1".to_string());
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        status.workspaces.get_mut("/ws").unwrap().state = ChildState::Stopped;
    }
    assert!(!child_is_running(&status, "/ws"));
    status.workspaces.get_mut("/ws").unwrap().since = current_start;
    // Dead child pid -> not running.
    status.workspaces.get_mut("/ws").unwrap().pid = Some(999_999);
    assert!(!child_is_running(&status, "/ws"));
    // Dead supervisor pid -> nothing is running even with a live child.
    status.workspaces.get_mut("/ws").unwrap().pid = Some(std::process::id());
    status.pid = Some(999_999);
    assert!(!child_is_running(&status, "/ws"));
    assert!(!runner_child_is_running(&status, "/ws"));
    assert!(runner_recorded_by_dead_supervisor(&status, "/ws"));
    // Missing file entry -> not running.
    assert!(!child_is_running(&status, "/missing"));
    assert!(!runner_recorded_by_dead_supervisor(&status, "/missing"));
}

#[test]
fn status_snapshot_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let mut status = SupervisorStatus {
        pid: Some(42),
        version: STATUS_VERSION,
        started_at: 1,
        updated_at: 2,
        ..SupervisorStatus::default()
    };
    status.hub = Some(ChildStatus {
        pid: Some(7),
        process_start_id: None,
        job_owned: false,
        executable_identity: None,
        state: ChildState::Running,
        restarts: 0,
        last_exit: None,
        since: 1,
    });
    status.workspaces.insert(
        "/ws".into(),
        ChildStatus {
            pid: Some(8),
            process_start_id: None,
            job_owned: false,
            executable_identity: None,
            state: ChildState::Backoff,
            restarts: 3,
            last_exit: Some(1),
            since: 2,
        },
    );
    status.runners.insert(
        "/ws".into(),
        ChildStatus {
            pid: Some(9),
            process_start_id: None,
            job_owned: false,
            executable_identity: None,
            state: ChildState::Stopped,
            restarts: 1,
            last_exit: Some(0),
            since: 3,
        },
    );
    let content = serde_json::to_vec_pretty(&status).unwrap();
    std::fs::write(dir.path().join("status.json"), content).unwrap();
    let path = dir.path().join("status.json");
    let raw = std::fs::read(&path).unwrap();
    let parsed: SupervisorStatus = serde_json::from_slice(&raw).unwrap();
    assert_eq!(parsed.pid, Some(42));
    assert_eq!(parsed.hub.as_ref().unwrap().state, ChildState::Running);
    assert_eq!(parsed.workspaces["/ws"].state, ChildState::Backoff);
    assert_eq!(parsed.runners["/ws"].state, ChildState::Stopped);

    let legacy: SupervisorStatus = serde_json::from_str(
            r#"{"pid":null,"version":1,"started_at":0,"updated_at":0,"workspaces":{},"hub":null,"tray":null}"#,
        )
        .unwrap();
    assert!(legacy.runners.is_empty());
}

#[test]
fn status_projects_runner_and_watcher_separately() {
    let watcher_spec = ChildSpec {
        kind: ChildKind::Workspace(cwp("/same")),
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let runner_spec = ChildSpec {
        kind: ChildKind::Runner(cwp("/same")),
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let mut children = BTreeMap::new();
    children.insert(
        workspace_child_key("/same"),
        ManagedChild::new(watcher_spec),
    );
    children.insert(runner_child_key("/same"), ManagedChild::new(runner_spec));
    let status = build_status(&children, 1);
    assert_eq!(status.version, STATUS_VERSION);
    assert!(status.workspaces.contains_key("/same"));
    assert!(status.runners.contains_key("/same"));
    assert_eq!(status.workspaces.len(), 1);
    assert_eq!(status.runners.len(), 1);
}
