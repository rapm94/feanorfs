use super::*;

#[cfg(unix)]
#[test]
fn non_utf8_workspace_is_rejected_before_registry_mutation() {
    use std::os::unix::ffi::OsStringExt as _;

    let _guard = ACK_TEST_LOCK.lock().unwrap();
    let registry_file = registry_path().unwrap();
    let before = fs::read(&registry_file).ok();
    let first = PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0x80]));
    let second = PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0x81]));
    assert_eq!(first.to_string_lossy(), second.to_string_lossy());

    for workspace in [&first, &second] {
        let error = workspace_registry_key(workspace).unwrap_err();
        assert!(error
            .to_string()
            .contains("canonical workspace path must be valid UTF-8"));
    }
    assert_eq!(fs::read(&registry_file).ok(), before);
}

#[test]
fn runner_stop_ack_rejects_missing_stale_and_failed_publication() {
    let _guard = ACK_TEST_LOCK.lock().unwrap();
    let registry_file = registry_path().unwrap();
    let ack_file = runner_ack_path().unwrap();
    let status_file = status_path().unwrap();
    let lock_file = supervisor_lock_path().unwrap();
    let owner_file = supervisor_lock_owner_path_at(&lock_file);
    let original_registry = fs::read(&registry_file).ok();
    let original_ack = fs::read(&ack_file).ok();
    let original_status = fs::read(&status_file).ok();
    let original_lock = fs::read(&lock_file).ok();
    let original_owner = fs::read(&owner_file).ok();
    let restore = |path: &Path, original: &Option<Vec<u8>>| match original {
        Some(content) => fs::write(path, content).unwrap(),
        None => {
            let _ = fs::remove_file(path);
        }
    };

    let supervisor_guard = acquire_supervisor_lock_at(&supervisor_lock_path().unwrap())
        .unwrap()
        .expect("ack test owns supervisor lock");
    let current_owner = fs::read(&owner_file).expect("read current supervisor owner");
    let token_a = "token-a".to_string();
    let registry = SupervisorRegistry {
        mutation_generation: 1,
        runner_stop_tokens: BTreeMap::from([(
            cwp("/ack-test"),
            RunnerStopTombstone {
                token: token_a.clone(),
                generation: 1,
            },
        )]),
        ..SupervisorRegistry::default()
    };
    create_store_dir(&registry_file).unwrap();
    save_registry(&registry_file, &registry).unwrap();
    let _ = fs::remove_file(&ack_file);
    fs::write(&status_file, b"not-json").unwrap();

    assert!(supervisor_instance_lock_held().unwrap());
    fs::write(&owner_file, b"not-json").unwrap();
    assert_eq!(supervisor_lock_owner_pid().unwrap(), None);
    fs::write(&owner_file, &current_owner).unwrap();
    assert_eq!(
        supervisor_lock_owner_pid().unwrap(),
        Some(std::process::id())
    );
    assert!(
        !runner_stop_acknowledged(&cwp("/ack-test"), None, Some(std::process::id()), None,)
            .unwrap()
    );

    TEST_ACK_PUBLISH_FAILURE.store(true, AtomicOrdering::Release);
    let children = BTreeMap::new();
    assert!(publish_runner_reconcile_ack(&children, &registry, now_epoch(), 1).is_err());
    TEST_ACK_PUBLISH_FAILURE.store(false, AtomicOrdering::Release);
    assert!(!runner_stop_acknowledged(
        &cwp("/ack-test"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());

    publish_runner_reconcile_ack(&children, &registry, now_epoch(), 2).unwrap();
    assert!(runner_stop_acknowledged(
        &cwp("/ack-test"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());
    // Unrelated registry mutations do not invalidate runner A's durable
    // stop token. The acknowledgement remains tied to this runner's
    // tombstone rather than to a global generation equality.
    let mut unrelated_registry = registry.clone();
    unrelated_registry.mutation_generation = 2;
    unrelated_registry.workspaces.push(cwp("/other"));
    save_registry(&registry_file, &unrelated_registry).unwrap();
    assert!(runner_stop_acknowledged(
        &cwp("/ack-test"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());

    // Re-adding runner A clears its tombstone, so the old token can no
    // longer acknowledge a later stop operation.
    let mut readded_registry = unrelated_registry.clone();
    readded_registry.runners.push(cwp("/ack-test"));
    readded_registry
        .runner_stop_tokens
        .remove(&cwp("/ack-test"));
    readded_registry.mutation_generation = 3;
    save_registry(&registry_file, &readded_registry).unwrap();
    assert!(!runner_stop_acknowledged(
        &cwp("/ack-test"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());

    // A second removal receives a fresh token. The first token is rejected
    // even when the runner list has returned to the same content (ABA).
    let token_b = "token-b".to_string();
    let second_removal = SupervisorRegistry {
        mutation_generation: 4,
        runner_stop_tokens: BTreeMap::from([(
            cwp("/ack-test"),
            RunnerStopTombstone {
                token: token_b.clone(),
                generation: 4,
            },
        )]),
        workspaces: readded_registry.workspaces.clone(),
        ..SupervisorRegistry::default()
    };
    save_registry(&registry_file, &second_removal).unwrap();
    publish_runner_reconcile_ack(&children, &second_removal, now_epoch(), 3).unwrap();
    assert!(!runner_stop_acknowledged(
        &cwp("/ack-test"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());
    assert!(runner_stop_acknowledged(
        &cwp("/ack-test"),
        Some(&token_b),
        Some(std::process::id()),
        None,
    )
    .unwrap());
    let mut stale = read_runner_reconcile_ack().unwrap().unwrap();
    stale.process_start_id = Some(format!("spawn:{}:stale", stale.pid));
    fs::write(&ack_file, serde_json::to_vec(&stale).unwrap()).unwrap();
    assert!(!runner_stop_acknowledged(
        &cwp("/ack-test"),
        Some(&token_b),
        Some(std::process::id()),
        None,
    )
    .unwrap());

    drop(supervisor_guard);
    restore(&registry_file, &original_registry);
    restore(&ack_file, &original_ack);
    restore(&status_file, &original_status);
    restore(&lock_file, &original_lock);
    restore(&owner_file, &original_owner);
}

#[test]
fn runner_ack_store_is_independent_per_removed_runner() {
    let _guard = ACK_TEST_LOCK.lock().unwrap();
    let registry_file = registry_path().unwrap();
    let ack_file = runner_ack_path().unwrap();
    let original_registry = fs::read(&registry_file).ok();
    let original_ack = fs::read(&ack_file).ok();
    let restore = |path: &Path, original: &Option<Vec<u8>>| match original {
        Some(content) => fs::write(path, content).unwrap(),
        None => {
            let _ = fs::remove_file(path);
        }
    };

    let token_a = "store-token-a".to_string();
    let token_b = "store-token-b".to_string();
    let registry = SupervisorRegistry {
        mutation_generation: 10,
        runner_stop_tokens: BTreeMap::from([
            (
                cwp("/store-a"),
                RunnerStopTombstone {
                    token: token_a.clone(),
                    generation: 8,
                },
            ),
            (
                cwp("/store-b"),
                RunnerStopTombstone {
                    token: token_b.clone(),
                    generation: 9,
                },
            ),
        ]),
        ..SupervisorRegistry::default()
    };
    create_store_dir(&registry_file).unwrap();
    save_registry(&registry_file, &registry).unwrap();
    let child_b = ChildSpec {
        kind: ChildKind::Runner(cwp("/store-b")),
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let children = BTreeMap::from([(runner_child_key("/store-b"), ManagedChild::new(child_b))]);
    let _ = fs::remove_file(&ack_file);
    publish_runner_reconcile_ack(&children, &registry, now_epoch(), 1).unwrap();
    let store = read_runner_reconcile_ack_store().unwrap().unwrap();
    assert!(store.acks.contains_key(&cwp("/store-a")));
    assert!(!store.acks.contains_key(&cwp("/store-b")));
    assert!(runner_stop_acknowledged(
        &cwp("/store-a"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());
    assert!(!runner_stop_acknowledged(
        &cwp("/store-b"),
        Some(&token_b),
        Some(std::process::id()),
        None,
    )
    .unwrap());

    // B can complete later without changing A's record or token.
    publish_runner_reconcile_ack(&BTreeMap::new(), &registry, now_epoch(), 2).unwrap();
    assert!(runner_stop_acknowledged(
        &cwp("/store-a"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());
    assert!(runner_stop_acknowledged(
        &cwp("/store-b"),
        Some(&token_b),
        Some(std::process::id()),
        None,
    )
    .unwrap());

    let mut unrelated = registry.clone();
    unrelated.mutation_generation = 11;
    unrelated.workspaces.push(cwp("/unrelated"));
    save_registry(&registry_file, &unrelated).unwrap();
    assert!(runner_stop_acknowledged(
        &cwp("/store-a"),
        Some(&token_a),
        Some(std::process::id()),
        None,
    )
    .unwrap());

    restore(&registry_file, &original_registry);
    restore(&ack_file, &original_ack);
}

#[test]
fn registry_roundtrips_workspaces_stopped_and_runners() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    let mut store = SupervisorRegistry::default();
    store.workspaces.push(cwp("/a"));
    store.stopped.push(cwp("/b"));
    store.runners.push(cwp("/c"));
    save_registry(&path, &store).unwrap();
    let loaded = load_registry(&path).unwrap();
    assert_eq!(loaded.workspaces, vec![cwp("/a")]);
    assert_eq!(loaded.stopped, vec![cwp("/b")]);
    assert_eq!(loaded.runners, vec![cwp("/c")]);
}

#[test]
fn registry_rejects_oversized_file_before_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    std::fs::write(&path, vec![b' '; MAX_REGISTRY_BYTES as usize + 1]).unwrap();

    let error = load_registry(&path).unwrap_err();

    assert!(error.to_string().contains("exceeds"));
}

#[cfg(unix)]
#[test]
fn registry_rejects_symlink_input() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.json");
    let path = dir.path().join("supervisor.json");
    std::fs::write(&target, r#"{"workspaces":["/target"]}"#).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();

    let error = load_registry(&path).unwrap_err();

    assert!(error.to_string().contains("not a regular file"));
}

#[cfg(unix)]
#[test]
fn registry_open_rejects_fifo_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    let path_bytes = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `path_bytes` is NUL-terminated and remains alive for this
    // synchronous POSIX call; the temporary directory owns the result.
    assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);

    let error = open_registry_for_read(&path).unwrap_err();

    assert!(error.to_string().contains("not a regular file"));
}

#[test]
fn registry_rejects_excess_entries_without_replacing_prior_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    let prior = SupervisorRegistry {
        workspaces: vec![cwp("/preserved")],
        ..SupervisorRegistry::default()
    };
    save_registry(&path, &prior).unwrap();
    let before = std::fs::read(&path).unwrap();

    let oversized = SupervisorRegistry {
        workspaces: (0..=MAX_SUPERVISOR_WORKSPACES)
            .map(|index| cwp(&format!("/workspace-{index}")))
            .collect(),
        ..SupervisorRegistry::default()
    };
    let error = save_registry(&path, &oversized).unwrap_err();

    assert!(error.to_string().contains("more than"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn registry_rejects_serialized_byte_overflow_without_replacing_prior_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    let prior = SupervisorRegistry {
        workspaces: vec![cwp("/preserved")],
        ..SupervisorRegistry::default()
    };
    save_registry(&path, &prior).unwrap();
    let before = std::fs::read(&path).unwrap();

    // This is one byte above the per-entry average needed to exceed the
    // 4 MiB stream cap with the maximum valid number of active workspaces.
    let suffix = "x".repeat(MAX_REGISTRY_BYTES as usize / MAX_SUPERVISOR_WORKSPACES + 1);
    let oversized = SupervisorRegistry {
        workspaces: (0..MAX_SUPERVISOR_WORKSPACES)
            .map(|index| cwp(&format!("/{index}-{suffix}")))
            .collect(),
        ..SupervisorRegistry::default()
    };
    let error = save_registry(&path, &oversized).unwrap_err();

    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(format!("{error:#}").contains("byte limit"));
}

#[test]
fn registry_rejects_duplicate_or_overlapping_lifecycle_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");

    std::fs::write(&path, r#"{"workspaces":["/same","/same"]}"#).unwrap();
    let duplicate = load_registry(&path).unwrap_err();
    assert!(duplicate.to_string().contains("duplicate"));

    std::fs::write(&path, r#"{"workspaces":["/same"],"stopped":["/same"]}"#).unwrap();
    let overlap = load_registry(&path).unwrap_err();
    assert!(overlap.to_string().contains("both active and stopped"));
}

#[test]
fn registry_rejects_invalid_runner_stop_tombstone_generations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");

    // The durable registry boundary must reject both a future tombstone and
    // the zero generation that legacy registries deliberately never use for
    // stop acknowledgements.
    for invalid_generation in [0, 4] {
        std::fs::write(
            &path,
            format!(
                r#"{{"mutation_generation":3,"runner_stop_tokens":{{"/runner":{{"token":"token","generation":{invalid_generation}}}}}}}"#
            ),
        )
        .unwrap();
        let error = load_registry(&path).unwrap_err();
        assert!(error
            .to_string()
            .contains("invalid runner stop tombstone generation"));
    }

    let prior = SupervisorRegistry {
        mutation_generation: 3,
        runner_stop_tokens: BTreeMap::from([(
            cwp("/preserved"),
            RunnerStopTombstone {
                token: "token".to_string(),
                generation: 3,
            },
        )]),
        ..SupervisorRegistry::default()
    };
    save_registry(&path, &prior).unwrap();
    let before = std::fs::read(&path).unwrap();

    let invalid = SupervisorRegistry {
        mutation_generation: 3,
        runner_stop_tokens: BTreeMap::from([(
            cwp("/runner"),
            RunnerStopTombstone {
                token: "token".to_string(),
                generation: 4,
            },
        )]),
        ..SupervisorRegistry::default()
    };
    let error = save_registry(&path, &invalid).unwrap_err();

    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(error
        .to_string()
        .contains("invalid runner stop tombstone generation"));
}

#[test]
fn runner_ack_allows_removed_runner_a_while_runner_b_remains_active() {
    let registry = SupervisorRegistry {
        runners: vec![cwp("/runner-b")],
        mutation_generation: 7,
        ..SupervisorRegistry::default()
    };
    let runner_b = ChildSpec {
        kind: ChildKind::Runner(cwp("/runner-b")),
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let runner_a = ChildSpec {
        kind: ChildKind::Runner(cwp("/runner-a")),
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        env: Vec::new(),
        restart_on_zero_exit: true,
    };
    let mut children = BTreeMap::new();
    children.insert(runner_child_key("/runner-b"), ManagedChild::new(runner_b));
    assert!(runner_reconciliation_complete(&children, &registry));

    children.insert(runner_child_key("/runner-a"), ManagedChild::new(runner_a));
    assert!(
        !runner_reconciliation_complete(&children, &registry),
        "a stale removed runner entry must still gate the acknowledgement"
    );
    children.remove(&runner_child_key("/runner-a"));
    assert!(runner_reconciliation_complete(&children, &registry));
}

#[test]
fn runner_stop_tombstone_capacity_rejects_unacknowledged_entries() {
    let mut registry = SupervisorRegistry {
        mutation_generation: MAX_RUNNER_STOP_TOMBSTONES as u64,
        ..SupervisorRegistry::default()
    };
    for index in 0..MAX_RUNNER_STOP_TOMBSTONES {
        let workspace = cwp(&format!("/capacity-{index}"));
        registry.runner_stop_tokens.insert(
            workspace,
            RunnerStopTombstone {
                token: format!("token-{index}"),
                generation: index as u64 + 1,
            },
        );
    }
    let before = registry.runner_stop_tokens.clone();
    let result = prune_runner_stop_tokens(&mut registry, &RunnerReconcileAckStore::default());
    assert!(result.is_err());
    assert_eq!(registry.runner_stop_tokens, before);
}

#[test]
fn runner_stop_tombstone_capacity_reclaims_only_matching_ack() {
    let mut registry = SupervisorRegistry {
        mutation_generation: MAX_RUNNER_STOP_TOMBSTONES as u64,
        ..SupervisorRegistry::default()
    };
    for index in 0..MAX_RUNNER_STOP_TOMBSTONES {
        let workspace = cwp(&format!("/capacity-{index}"));
        registry.runner_stop_tokens.insert(
            workspace,
            RunnerStopTombstone {
                token: format!("token-{index}"),
                generation: index as u64 + 1,
            },
        );
    }
    let reclaim = cwp("/capacity-0");
    let mut ack_store = RunnerReconcileAckStore::default();
    ack_store.acks.insert(
        reclaim.clone(),
        RunnerReconcileAck {
            workspace: reclaim.clone(),
            pid: std::process::id(),
            process_start_id: None,
            started_at: 1,
            registry_digest: String::new(),
            registry_generation: 1,
            stop_token: "token-0".to_string(),
            generation: 1,
        },
    );
    prune_runner_stop_tokens(&mut registry, &ack_store).unwrap();
    assert_eq!(
        registry.runner_stop_tokens.len(),
        MAX_RUNNER_STOP_TOMBSTONES - 1
    );
    assert!(!registry.runner_stop_tokens.contains_key(&reclaim));
    assert!(registry
        .runner_stop_tokens
        .contains_key(&cwp("/capacity-1")));
}

#[test]
fn absent_registry_seeding_preserves_existing_and_stopped_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    seed_registry_file_if_absent(&path, vec![cwp("/recent")]).unwrap();
    assert_eq!(
        load_registry(&path).unwrap().workspaces,
        vec![cwp("/recent")]
    );

    seed_registry_file_if_absent(&path, vec![cwp("/must-not-replace")]).unwrap();
    assert_eq!(
        load_registry(&path).unwrap().workspaces,
        vec![cwp("/recent")]
    );

    let stopped_path = dir.path().join("stopped-supervisor.json");
    let mut stopped = SupervisorRegistry::default();
    stopped.stopped.push(cwp("/stopped"));
    save_registry(&stopped_path, &stopped).unwrap();
    seed_registry_file_if_absent(&stopped_path, vec![cwp("/stopped")]).unwrap();
    let loaded = load_registry(&stopped_path).unwrap();
    assert!(loaded.workspaces.is_empty());
    assert_eq!(loaded.stopped, vec![cwp("/stopped")]);
}

#[test]
fn legacy_registry_without_tombstones_remains_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    std::fs::write(
        &path,
        r#"{"workspaces":["/a"],"stopped":["/b"],"runners":["/runner"]}"#,
    )
    .unwrap();
    let loaded = load_registry(&path).unwrap();
    assert_eq!(loaded.workspaces, vec![cwp("/a")]);
    assert_eq!(loaded.stopped, vec![cwp("/b")]);
    assert_eq!(loaded.runners, vec![cwp("/runner")]);
    assert!(loaded.runner_stop_tokens.is_empty());
    assert_eq!(loaded.mutation_generation, 0);
}

#[test]
fn missing_registry_loads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = load_registry(&dir.path().join("missing.json")).unwrap();
    assert!(store.workspaces.is_empty());
    assert!(store.stopped.is_empty());
    assert!(store.runners.is_empty());
}

#[test]
fn read_registry_if_present_does_not_create_absent_store_or_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("supervisor.json");
    let lock = dir.path().join("supervisor.lock");

    assert!(read_registry_if_present_at(&path)
        .unwrap()
        .runners
        .is_empty());
    assert!(!path.exists());
    assert!(!lock.exists());

    save_registry(&path, &SupervisorRegistry::default()).unwrap();
    assert!(read_registry_if_present_at(&path)
        .unwrap()
        .runners
        .is_empty());
    assert!(lock.exists());
}
