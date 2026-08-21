//! Durable, body-free state machine for one unattended agent runner.

mod admission;
mod contract;
mod ownership;
mod session;
mod store;
#[cfg(test)]
mod test_hooks;

pub use contract::{
    RunnerAdmission, RunnerAttention, RunnerConfig, RunnerExecutionMode, RunnerInvocation,
    RunnerLaunch, RunnerOwnership, RunnerPhase, RunnerProcessMetadata, RunnerScopeMode,
    RunnerStatus, RunnerWorkWait, RunnerWorkWaitKind, ScopeChangePublishState,
    ScopeChangeRequestKey,
};
#[cfg(test)]
use ownership::RunnerLifetimeLock;
pub(crate) use ownership::{
    configured_runner_is, runner_lifetime_held, RunnerLifecycleLock, RunnerOperationGuard,
};
pub use session::RunnerExecutionSession;
pub use store::{remove_configured, runner_process_metadata, runner_status, RunnerStore};

#[cfg(test)]
mod tests {
    use super::super::scope::{
        AcceptedWorkDescriptor, RunnerAdmissionReject, ACCEPTED_WORK_SCHEMA_VERSION,
    };
    use super::store::{runner_agent_root, runner_dir_path, validate_state, MAX_ARGS, MAX_PENDING};
    use super::test_hooks::{
        install_inbox_admission_pause, install_lifecycle_contention_hook,
        install_operation_guard_pause, install_status_discovery_pause,
        install_status_snapshot_pause, next_test_hook_id, pause_inbox_admission_if_requested,
        pause_operation_guard_if_requested, wait_for_test_pause_release, TestPauseHook,
        OPERATION_GUARD_PAUSE_HOOKS, TEST_HOOK_TIMEOUT,
    };
    use super::*;
    use feanorfs_common::{AgentInboxResult, AgentMessage, AgentMessageKind, WorkScope};
    use std::fs;
    use std::path::Path;

    fn id(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
            })
            .unwrap_or_else(|| "non-string panic payload".to_string())
    }

    fn prepare_baseline(base: &Path, agent: &str, format_version: u32) {
        crate::local::save_config(
            base,
            &crate::local::Config {
                server_url: "http://127.0.0.1:1".into(),
                workspace_id: "runner-test".into(),
                encryption_password: Some("e".repeat(64)),
                server_password: None,
                tls_ca_pem: None,
                format_version,
                hub_local: false,
                relay: None,
            },
        )
        .unwrap();
        let root = runner_agent_root(base, agent).unwrap();
        fs::create_dir_all(root.join("worktree")).unwrap();
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("state/base-snapshot"), id('f')).unwrap();
    }

    fn setup_disabled() -> (tempfile::TempDir, RunnerStore) {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let store = RunnerStore::configure(
            base.path(),
            "worker",
            &program,
            vec!["--fixed".into()],
            3600,
            &id('a'),
        )
        .unwrap();
        (base, store)
    }

    fn setup() -> (tempfile::TempDir, RunnerStore) {
        let (base, store) = setup_disabled();
        store.set_enabled(true).unwrap();
        (base, store)
    }

    fn message(
        message_id: char,
        from: &str,
        to: &str,
        kind: AgentMessageKind,
        reply_to: Option<char>,
    ) -> AgentMessage {
        AgentMessage {
            message_id: id(message_id),
            from: from.into(),
            to: to.into(),
            kind,
            body: "private task body".into(),
            about_snapshot: id('f'),
            reply_to: reply_to.map(id),
            created_at_ms: message_id as i64,
        }
    }

    fn inbox(cursor: char, messages: Vec<AgentMessage>) -> AgentInboxResult {
        AgentInboxResult {
            cursor: id(cursor),
            cursor_reset: false,
            messages,
        }
    }

    #[test]
    fn baseline_cursor_and_single_configuration_guards() {
        let (base, store) = setup_disabled();
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        assert!(!store.status().unwrap().enabled);
        assert!(RunnerStore::open_configured(base.path()).is_ok());
        assert_eq!(
            runner_status(base.path()).unwrap(),
            Some(store.status().unwrap())
        );
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert!(
            RunnerStore::configure(base.path(), "worker", &program, vec![], 60, &id('b'))
                .unwrap_err()
                .to_string()
                .contains("use reconfigure")
        );
        prepare_baseline(base.path(), "other", 3);
        assert!(
            RunnerStore::configure(base.path(), "other", &program, vec![], 60, &id('b'))
                .unwrap_err()
                .to_string()
                .contains("already configured")
        );

        let unconfigured = tempfile::tempdir().unwrap();
        assert_eq!(runner_status(unconfigured.path()).unwrap(), None);
    }

    #[test]
    fn configured_only_lease_does_not_create_state_for_unconfigured_agents() {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        prepare_baseline(base.path(), "other", 3);
        let worker_runner = runner_dir_path(base.path(), "worker").unwrap();
        let other_runner = runner_dir_path(base.path(), "other").unwrap();

        assert!(
            RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
                .unwrap()
                .is_none()
        );
        assert!(!worker_runner.exists());
        assert!(!other_runner.exists());

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let _store =
            RunnerStore::configure(base.path(), "worker", &program, Vec::new(), 3600, &id('a'))
                .unwrap();
        assert!(
            RunnerLifetimeLock::try_acquire_configured(base.path(), "other")
                .unwrap()
                .is_none()
        );
        assert!(!other_runner.exists());
        assert!(
            RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn configure_rejects_an_active_interactive_owner() {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let owner =
            crate::agent::continuous::ContinuousOwnerLock::try_acquire(base.path(), "worker")
                .unwrap()
                .expect("interactive owner");
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();

        let error =
            RunnerStore::configure(base.path(), "worker", &program, Vec::new(), 3600, &id('a'))
                .unwrap_err();
        assert!(error.to_string().contains("active `agent run`"));
        drop(owner);
    }

    #[test]
    fn runner_lifetime_probe_reports_live_contention_without_error() {
        let (base, store) = setup();
        assert!(!runner_lifetime_held(base.path(), "worker").unwrap());

        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert!(
            runner_lifetime_held(base.path(), "worker").unwrap(),
            "read-only liveness probes must recognize the active lease"
        );

        drop(session);
        assert!(!runner_lifetime_held(base.path(), "worker").unwrap());
    }

    #[test]
    fn execution_session_owns_exact_lease_and_reacquire_fails_closed() {
        let (base, store) = setup();
        let (other_base, _other_store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert!(RunnerLifetimeLock::try_acquire_configured(base.path(), "worker").is_err());
        assert!(store
            .execution_session(other_base.path(), RunnerExecutionMode::Supervised)
            .unwrap_err()
            .to_string()
            .contains("configuration"));

        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, 42, "session-owned-child")
            .unwrap();
        session
            .observe_terminals(
                &request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        store
            .admit_inbox(&inbox(
                'c',
                vec![message(
                    '3',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        session.begin_next(&id('c')).unwrap();
        drop(session);

        let resumed = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert_eq!(
            resumed.checkpoint_startup().unwrap().attention,
            Some(RunnerAttention::AmbiguousExecution)
        );
        assert!(resumed.begin_next(&id('d')).is_err());
    }

    #[test]
    fn runner_ownership_token_is_bound_to_workspace_and_agent() {
        let (base, store) = setup_disabled();
        prepare_baseline(base.path(), "other", 3);
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Foreground)
            .unwrap();
        let ownership = RunnerOwnership::from_session(&session);
        assert!(ownership.verify(base.path(), "worker").is_ok());
        assert!(ownership.verify(base.path(), "other").is_err());

        let (other_base, _other_store) = setup_disabled();
        assert!(ownership.verify(other_base.path(), "worker").is_err());
    }

    #[test]
    fn disabled_live_session_blocks_reset_until_drop() {
        let (base, store) = setup();
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, 42, "live-reset-child")
            .unwrap();
        store.set_enabled(false).unwrap();
        let checkpoint = store.status().unwrap();

        let error = store.reset_to_current_cursor(&id('c'), true).unwrap_err();
        assert!(error.to_string().contains("already active"));
        assert_eq!(store.status().unwrap(), checkpoint);

        drop(session);
        let reset = store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(reset.phase, RunnerPhase::Idle);
        assert_eq!(reset.pending_count, 0);
        assert!(reset.active_message_id.is_none());
        assert!(reset.attention.is_none());
        assert_eq!(store.committed_cursor().unwrap(), id('c'));
    }

    #[test]
    fn disable_preserves_supervised_checkpoint_and_allows_terminal_observation() {
        let (base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox(
                'b',
                vec![
                    request.clone(),
                    message('2', "human", "worker", AgentMessageKind::Request, None),
                ],
            ))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();

        let disabled = store.set_enabled(false).unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.pending_count, 2);
        assert_eq!(
            disabled.active_message_id.as_deref(),
            Some(launch.message_id.as_str())
        );
        session
            .mark_spawned(&launch.message_id, 42, "disabled-session-child")
            .unwrap();
        assert_eq!(
            runner_process_metadata(base.path()).unwrap(),
            Some(RunnerProcessMetadata {
                pid: 42,
                process_start_id: "disabled-session-child".to_string(),
            })
        );
        let terminal = session
            .observe_terminals(
                &request,
                &[message(
                    '3',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap()
            .unwrap();
        assert!(!terminal.enabled);
        assert_eq!(terminal.pending_count, 1);
        assert!(terminal.active_message_id.is_none());
        assert_eq!(runner_process_metadata(base.path()).unwrap(), None);

        let error = session.begin_next(&id('c')).unwrap_err();
        assert!(error.to_string().contains("enabled=true"));
        assert_eq!(store.status().unwrap().pending_count, 1);
    }

    #[test]
    fn disable_linearizes_before_in_flight_supervised_admission() {
        let (base, store) = setup();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let batch = inbox(
            'b',
            vec![message(
                '1',
                "human",
                "worker",
                AgentMessageKind::Request,
                None,
            )],
        );
        let mut admission_pause = install_inbox_admission_pause(base.path(), "worker").unwrap();

        std::thread::scope(|scope| {
            let admission = scope.spawn(|| session.admit_inbox(&batch));
            admission_pause.wait("session admission did not reach its pre-update boundary");
            let disabled = store.set_enabled(false).unwrap();
            assert!(!disabled.enabled);
            let after_disable = store.status().unwrap();
            let cursor_after_disable = store.committed_cursor().unwrap();
            let state_after_disable = fs::read(store.path()).unwrap();

            admission_pause.release().unwrap();
            let error = admission.join().unwrap().unwrap_err();
            assert!(error.to_string().contains("enabled=true"));
            assert_eq!(store.status().unwrap(), after_disable);
            assert_eq!(store.committed_cursor().unwrap(), cursor_after_disable);
            assert_eq!(fs::read(store.path()).unwrap(), state_after_disable);
        });
    }

    #[test]
    fn production_inbox_admission_is_session_bound_and_mode_checked() {
        let (supervised_base, supervised_store) = setup();
        assert!(supervised_store
            .execution_session(supervised_base.path(), RunnerExecutionMode::Foreground)
            .is_err());
        let supervised = supervised_store
            .execution_session(supervised_base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert_eq!(
            supervised
                .admit_inbox(&inbox(
                    'b',
                    vec![message(
                        '1',
                        "human",
                        "worker",
                        AgentMessageKind::Request,
                        None,
                    )],
                ))
                .unwrap()
                .admitted,
            1
        );

        let (foreground_base, foreground_store) = setup_disabled();
        assert!(foreground_store
            .execution_session(foreground_base.path(), RunnerExecutionMode::Supervised)
            .is_err());
        let foreground = foreground_store
            .execution_session(foreground_base.path(), RunnerExecutionMode::Foreground)
            .unwrap();
        assert_eq!(
            foreground
                .admit_inbox(&inbox(
                    'c',
                    vec![message(
                        '2',
                        "human",
                        "worker",
                        AgentMessageKind::Request,
                        None,
                    )],
                ))
                .unwrap()
                .admitted,
            1
        );
    }

    #[test]
    fn foreground_execution_session_blocks_enable_until_drop() {
        let (base, store) = setup_disabled();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Foreground)
            .unwrap();

        let error = store.set_enabled(true).unwrap_err();
        assert!(error.to_string().contains("already active"));
        assert!(!store.status().unwrap().enabled);

        drop(session);
        assert!(store.set_enabled(true).unwrap().enabled);
    }

    #[test]
    fn stale_store_cannot_apply_control_mutations_to_a_recreated_configuration() {
        let (base, stale) = setup_disabled();
        let stale_generation = stale.identity.generation_id.clone();
        remove_configured(base.path(), false).unwrap();
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let fresh =
            RunnerStore::configure(base.path(), "worker", &program, Vec::new(), 3600, &id('b'))
                .unwrap();
        assert_ne!(fresh.identity.generation_id, stale_generation);
        for error in [
            stale.set_enabled(true).unwrap_err(),
            stale.set_enabled(false).unwrap_err(),
            stale.reset_to_current_cursor(&id('c'), true).unwrap_err(),
        ] {
            assert!(error.to_string().contains("stale"));
        }
        assert!(!fresh.status().unwrap().enabled);
    }

    #[tokio::test]
    async fn removal_requires_disabled_explicit_discard_and_preserves_agent_state() {
        let (base, store) = setup();
        let root = runner_agent_root(base.path(), "worker").unwrap();
        fs::write(root.join("worktree/keep.txt"), b"worktree").unwrap();
        fs::create_dir_all(root.join("state/runtime")).unwrap();
        fs::write(root.join("state/runtime/keep"), b"runtime").unwrap();

        let enabled_error = remove_configured(base.path(), true).unwrap_err();
        assert!(enabled_error.to_string().contains("disable"));
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        store.set_enabled(false).unwrap();
        let pending_error = remove_configured(base.path(), false).unwrap_err();
        assert!(pending_error.to_string().contains("discard_pending=true"));

        remove_configured(base.path(), true).unwrap();
        assert_eq!(runner_status(base.path()).unwrap(), None);
        assert!(!root.join("state/runner").exists());
        assert_eq!(
            fs::read(root.join("worktree/keep.txt")).unwrap(),
            b"worktree"
        );
        assert!(root.join("state/base-snapshot").is_file());
        assert_eq!(
            fs::read(root.join("state/runtime/keep")).unwrap(),
            b"runtime"
        );

        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        crate::agent::clean_agent(base.path(), &db, "worker")
            .await
            .unwrap();
        assert!(
            !root.exists(),
            "ordinary cleanup is unblocked after removal"
        );
    }

    #[test]
    fn status_and_process_metadata_tolerate_concurrent_runner_removal() {
        let (status_base, _status_store) = setup_disabled();
        let mut status_pause =
            install_status_discovery_pause(status_base.path(), "worker").unwrap();
        let status_base_path = status_base.path().to_path_buf();
        let status_reader = std::thread::spawn(move || runner_status(&status_base_path));
        status_pause.wait("runner status did not finish configuration discovery");
        remove_configured(status_base.path(), false).unwrap();
        status_pause.release().unwrap();
        assert_eq!(status_reader.join().unwrap().unwrap(), None);

        let (process_base, _process_store) = setup_disabled();
        let mut process_pause =
            install_status_discovery_pause(process_base.path(), "worker").unwrap();
        let process_base_path = process_base.path().to_path_buf();
        let process_reader =
            std::thread::spawn(move || runner_process_metadata(&process_base_path));
        process_pause.wait("runner process metadata did not finish configuration discovery");
        remove_configured(process_base.path(), false).unwrap();
        process_pause.release().unwrap();
        assert_eq!(process_reader.join().unwrap().unwrap(), None);
    }

    #[test]
    fn status_returns_one_atomic_snapshot_during_reconfigure() {
        let (base, store) = setup_disabled();
        let before = store.status().unwrap();
        let mut status_pause = install_status_snapshot_pause(base.path(), "worker").unwrap();
        let base_path = base.path().to_path_buf();
        let status_reader = std::thread::spawn(move || runner_status(&base_path));
        status_pause.wait("runner status did not capture its configuration snapshot");

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let reconfigured = RunnerStore::reconfigure(
            base.path(),
            "worker",
            &program,
            vec!["--replacement".into()],
            7200,
        )
        .unwrap();
        status_pause.release().unwrap();

        assert_eq!(status_reader.join().unwrap().unwrap(), Some(before));
        assert_eq!(
            runner_status(base.path()).unwrap(),
            Some(reconfigured.status().unwrap())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_clean_contention_keeps_the_executor_responsive() {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();

        let (acquired_sender, acquired_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let holder_base = base.path().to_path_buf();
        let holder = std::thread::spawn(move || {
            let _guard = RunnerLifecycleLock::acquire(&holder_base).unwrap();
            acquired_sender.send(()).unwrap();
            release_receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
        });
        acquired_receiver
            .recv_timeout(TEST_HOOK_TIMEOUT)
            .expect("lifecycle holder did not acquire the lock");

        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            release_sender.send(()).is_ok()
        });
        // The holder's one-second watchdog proves executor responsiveness.
        // This outer timeout only bounds cleanup after the lock is released,
        // so use the standard hook allowance for loaded CI filesystems.
        let cleaned = tokio::time::timeout(
            TEST_HOOK_TIMEOUT,
            crate::agent::clean_agent(base.path(), &db, "worker"),
        )
        .await;

        assert!(
            release.await.unwrap(),
            "executor did not run the lock releaser"
        );
        assert!(
            holder.join().unwrap(),
            "lifecycle holder reached its watchdog instead of the async release"
        );
        cleaned
            .expect("clean blocked the current-thread executor")
            .unwrap();
    }

    #[test]
    fn removal_refuses_an_active_lifetime_lease() {
        let (base, _store) = setup_disabled();
        let lease = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
            .unwrap()
            .unwrap();
        let error = remove_configured(base.path(), true).unwrap_err();
        assert!(error.to_string().contains("already active"));
        assert!(runner_dir_path(base.path(), "worker").unwrap().is_dir());
        drop(lease);
        remove_configured(base.path(), false).unwrap();
        assert_eq!(runner_status(base.path()).unwrap(), None);
    }

    #[test]
    fn same_path_lifecycle_probes_are_independent_and_share_contention() {
        let base = tempfile::tempdir().unwrap();
        let held = RunnerLifecycleLock::acquire(base.path()).unwrap();
        let discarded = install_lifecycle_contention_hook(base.path()).unwrap();
        let second = install_lifecycle_contention_hook(base.path()).unwrap();
        let third = install_lifecycle_contention_hook(base.path()).unwrap();
        drop(discarded);
        let base_path = base.path().to_path_buf();
        let contender = std::thread::spawn(move || RunnerLifecycleLock::acquire(&base_path));

        second.wait("second same-path lifecycle probe was not notified");
        third.wait("third same-path lifecycle probe was not notified");
        drop(held);
        drop(contender.join().unwrap().unwrap());
    }

    #[test]
    fn same_key_operation_pauses_are_consumed_one_at_a_time() {
        let base = tempfile::tempdir().unwrap();
        let mut first = install_operation_guard_pause(base.path(), "worker").unwrap();
        let mut second = install_operation_guard_pause(base.path(), "worker").unwrap();

        std::thread::scope(|scope| {
            let first_worker =
                scope.spawn(|| pause_operation_guard_if_requested(base.path(), "worker"));
            first.wait("first same-key operation pause was not entered");
            assert_eq!(
                second.entered.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty),
                "one worker must not consume both same-key pauses"
            );
            first.release().unwrap();
            first_worker.join().unwrap();

            let second_worker =
                scope.spawn(|| pause_operation_guard_if_requested(base.path(), "worker"));
            second.wait("second same-key operation pause was not entered");
            second.release().unwrap();
            second_worker.join().unwrap();
        });
    }

    #[test]
    fn same_key_inbox_pause_drop_removes_only_its_token() {
        let (base, store) = setup_disabled();
        let first = install_inbox_admission_pause(base.path(), "worker").unwrap();
        let mut second = install_inbox_admission_pause(base.path(), "worker").unwrap();
        drop(first);
        let identity = store.identity.clone();

        std::thread::scope(|scope| {
            let worker = scope.spawn(|| pause_inbox_admission_if_requested(&identity));
            second.wait("remaining same-key inbox pause was not entered");
            second.release().unwrap();
            worker.join().unwrap();
        });
    }

    #[test]
    fn pause_timeout_and_disconnect_fail_loudly_and_cleanup_owner() {
        let base = tempfile::tempdir().unwrap();
        let pause = install_operation_guard_pause(base.path(), "worker").unwrap();
        let pause_id = pause.id;
        let observer_timeout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            pause.wait_with_timeout(
                "simulated operation pause observer timeout",
                std::time::Duration::from_millis(10),
            );
        }))
        .unwrap_err();
        assert!(panic_text(observer_timeout).contains("simulated operation pause observer timeout"));
        assert!(
            !OPERATION_GUARD_PAUSE_HOOKS
                .lock()
                .unwrap()
                .iter()
                .any(|hook| hook.id == pause_id),
            "observer timeout must remove its exact unconsumed hook"
        );

        let pause = install_operation_guard_pause(base.path(), "worker").unwrap();
        let paused_id = pause.id;
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| pause_operation_guard_if_requested(base.path(), "worker"));
            pause.wait("worker did not enter before simulated observer panic");
            let observer_panic =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let _pause = pause;
                    panic!("simulated observer panic after worker entry");
                }));
            assert!(panic_text(observer_panic.unwrap_err()).contains("simulated observer panic"));
            worker.join().unwrap();
        });
        assert!(
            !OPERATION_GUARD_PAUSE_HOOKS
                .lock()
                .unwrap()
                .iter()
                .any(|hook| hook.id == paused_id),
            "observer unwind must release its worker and remove its exact hook"
        );

        let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        drop(release_sender);
        let disconnected = TestPauseHook {
            id: next_test_hook_id(),
            canonical_base: fs::canonicalize(base.path()).unwrap(),
            agent: "worker".to_string(),
            entered: entered_sender,
            release: release_receiver,
        };
        let worker_disconnect = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for_test_pause_release(
                disconnected,
                "disconnect proof",
                std::time::Duration::from_millis(10),
            );
        }))
        .unwrap_err();
        entered_receiver.recv().unwrap();
        let disconnect_text = panic_text(worker_disconnect);
        assert!(disconnect_text.contains("paused worker"));
        assert!(disconnect_text.contains("Disconnected"));

        let (entered_sender, entered_receiver) = std::sync::mpsc::channel();
        let (_release_sender, release_receiver) = std::sync::mpsc::channel();
        let timed_out = TestPauseHook {
            id: next_test_hook_id(),
            canonical_base: fs::canonicalize(base.path()).unwrap(),
            agent: "worker".to_string(),
            entered: entered_sender,
            release: release_receiver,
        };
        let worker_timeout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for_test_pause_release(
                timed_out,
                "timeout proof",
                std::time::Duration::from_millis(10),
            );
        }))
        .unwrap_err();
        entered_receiver.recv().unwrap();
        let timeout_text = panic_text(worker_timeout);
        assert!(timeout_text.contains("paused worker"));
        assert!(timeout_text.contains("Timeout"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_lock_blocks_actual_clean_until_destructive_window_opens() {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let root = runner_agent_root(base.path(), "worker").unwrap();
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let held = RunnerLifecycleLock::acquire(base.path()).unwrap();
        let attempted = install_lifecycle_contention_hook(base.path()).unwrap();
        let base_path = base.path().to_path_buf();
        let clean =
            tokio::spawn(async move { crate::agent::clean_agent(&base_path, &db, "worker").await });

        attempted.wait("clean did not reach the contended lifecycle lock");
        assert!(
            !clean.is_finished(),
            "contended clean cannot pass the held lock"
        );
        assert!(
            root.is_dir(),
            "clean cannot mutate the agent root while blocked"
        );
        drop(held);
        clean.await.unwrap().unwrap();
        assert!(
            !root.exists(),
            "clean completes after the lifecycle window opens"
        );
    }

    #[test]
    fn configure_requires_format_three_real_agent_and_full_refs() {
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let unconfigured = tempfile::tempdir().unwrap();
        assert!(RunnerStore::configure(
            unconfigured.path(),
            "worker",
            &program,
            vec![],
            60,
            &id('a')
        )
        .is_err());

        let legacy = tempfile::tempdir().unwrap();
        prepare_baseline(legacy.path(), "worker", 2);
        assert!(
            RunnerStore::configure(legacy.path(), "worker", &program, vec![], 60, &id('a'))
                .unwrap_err()
                .to_string()
                .contains("format-v3")
        );

        let missing_ref = tempfile::tempdir().unwrap();
        prepare_baseline(missing_ref.path(), "worker", 3);
        fs::remove_file(
            runner_agent_root(missing_ref.path(), "worker")
                .unwrap()
                .join("state/base-snapshot"),
        )
        .unwrap();
        assert!(RunnerStore::configure(
            missing_ref.path(),
            "worker",
            &program,
            vec![],
            60,
            &id('a')
        )
        .is_err());

        let invalid_ref = tempfile::tempdir().unwrap();
        prepare_baseline(invalid_ref.path(), "worker", 3);
        fs::write(
            runner_agent_root(invalid_ref.path(), "worker")
                .unwrap()
                .join("state/base-snapshot"),
            "not-a-hash",
        )
        .unwrap();
        assert!(RunnerStore::configure(
            invalid_ref.path(),
            "worker",
            &program,
            vec![],
            60,
            &id('a')
        )
        .is_err());

        let empty_cursor = tempfile::tempdir().unwrap();
        prepare_baseline(empty_cursor.path(), "worker", 3);
        assert!(
            RunnerStore::configure(empty_cursor.path(), "worker", &program, vec![], 60, "")
                .is_err()
        );
    }

    #[test]
    fn direct_requests_only_and_empty_or_ignored_reads_advance() {
        let (_base, store) = setup();
        let result = inbox(
            'b',
            vec![
                message('1', "human", "worker", AgentMessageKind::Request, None),
                message('2', "human", "*", AgentMessageKind::Request, None),
                message('3', "human", "worker", AgentMessageKind::Status, None),
                message('4', "human", "other", AgentMessageKind::Request, None),
            ],
        );
        let admitted = store.admit_inbox(&result).unwrap();
        assert_eq!(admitted.admitted, 1);
        assert_eq!(store.status().unwrap().pending_count, 1);
        store.set_enabled(false).unwrap();
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        let ignored = store
            .admit_inbox(&inbox(
                'd',
                vec![message('5', "human", "*", AgentMessageKind::Status, None)],
            ))
            .unwrap();
        assert!(ignored.cursor_advanced);
        assert_eq!(store.committed_cursor().unwrap(), id('d'));
        let empty = store.admit_inbox(&inbox('e', vec![])).unwrap();
        assert!(empty.cursor_advanced);
        let unchanged = store.admit_inbox(&inbox('e', vec![])).unwrap();
        assert!(!unchanged.cursor_advanced);
    }

    #[test]
    fn execution_modes_require_matching_persisted_enablement() {
        let (_foreground_base, foreground) = setup_disabled();
        foreground
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let supervised_error = foreground
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap_err();
        assert!(supervised_error.to_string().contains("enabled=true"));
        assert!(foreground.status().unwrap().active_message_id.is_none());
        foreground
            .begin_next(RunnerExecutionMode::Foreground, &id('b'))
            .unwrap();

        let (_supervised_base, supervised) = setup();
        supervised
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '2',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let foreground_error = supervised
            .begin_next(RunnerExecutionMode::Foreground, &id('b'))
            .unwrap_err();
        assert!(foreground_error.to_string().contains("enabled=false"));
        assert!(supervised.status().unwrap().active_message_id.is_none());
        supervised
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
    }

    #[test]
    fn batch_cursor_advances_only_after_all_correlated_terminals() {
        let (_base, store) = setup();
        let second_request = message('2', "human", "worker", AgentMessageKind::Request, None);
        let first_request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox(
                'b',
                vec![second_request.clone(), first_request.clone()],
            ))
            .unwrap();
        let first = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .mark_spawned(&first.message_id, 42, "start-1")
            .unwrap();
        store
            .observe_terminals(
                &first_request,
                &[message(
                    '3',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let second = store
            .begin_next(RunnerExecutionMode::Supervised, &id('c'))
            .unwrap();
        assert_eq!(second.message_id, id('2'));
        store
            .observe_terminals(
                &second_request,
                &[message(
                    '4',
                    "worker",
                    "human",
                    AgentMessageKind::Blocked,
                    Some('2'),
                )],
            )
            .unwrap();
        assert_eq!(store.committed_cursor().unwrap(), id('b'));
        assert_eq!(
            store.status().unwrap().last_terminal_kind,
            Some(AgentMessageKind::Blocked)
        );
    }

    #[test]
    fn completed_ids_dedupe_replayed_requests() {
        let (_base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .observe_terminals(
                &request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        let replay = store.admit_inbox(&inbox('c', vec![request])).unwrap();
        assert_eq!(replay.admitted, 0);
        assert_eq!(store.status().unwrap().pending_count, 0);
        assert_eq!(store.committed_cursor().unwrap(), id('c'));
    }

    #[test]
    fn reconfigure_updates_only_configuration_and_disables() {
        let (base, store) = setup();
        let completed = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![completed.clone()]))
            .unwrap();
        store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .observe_terminals(
                &completed,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        store
            .admit_inbox(&inbox(
                'c',
                vec![message(
                    '3',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let reset = AgentInboxResult {
            cursor: id('d'),
            cursor_reset: true,
            messages: Vec::new(),
        };
        store.admit_inbox(&reset).unwrap();
        let before = store.status().unwrap();
        let cursor_before = store.committed_cursor().unwrap();

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let replaced = RunnerStore::reconfigure(
            base.path(),
            "worker",
            &program,
            vec!["--replacement".into()],
            7200,
        )
        .unwrap();
        let after = replaced.status().unwrap();
        assert!(!after.enabled);
        assert_eq!(after.pending_count, before.pending_count);
        assert_eq!(after.attention, before.attention);
        assert_eq!(after.last_terminal_kind, before.last_terminal_kind);
        assert_eq!(
            after.last_terminal_message_id,
            before.last_terminal_message_id
        );
        assert_eq!(replaced.committed_cursor().unwrap(), cursor_before);
        assert_eq!(replaced.config().unwrap().fixed_args, ["--replacement"]);
        assert_eq!(replaced.config().unwrap().timeout_secs, 7200);
        replaced.set_enabled(true).unwrap();
        replaced.set_enabled(false).unwrap();
        replaced.reset_to_current_cursor(&id('f'), true).unwrap();
        replaced.set_enabled(true).unwrap();
        assert_eq!(
            replaced
                .admit_inbox(&inbox('9', vec![completed]))
                .unwrap()
                .admitted,
            0,
            "reconfigure must preserve the completed-request ledger"
        );
    }

    #[test]
    fn reconfigure_refuses_launching_or_running_work() {
        for running in [false, true] {
            let (base, store) = setup();
            store
                .admit_inbox(&inbox(
                    'b',
                    vec![message(
                        '1',
                        "human",
                        "worker",
                        AgentMessageKind::Request,
                        None,
                    )],
                ))
                .unwrap();
            let launch = store
                .begin_next(RunnerExecutionMode::Supervised, &id('b'))
                .unwrap();
            if running {
                store
                    .mark_spawned(&launch.message_id, 42, "process-start")
                    .unwrap();
            }
            let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
            assert!(
                RunnerStore::reconfigure(base.path(), "worker", &program, vec![], 3600,)
                    .unwrap_err()
                    .to_string()
                    .contains("launching or running")
            );
        }
    }

    #[test]
    fn overflow_needs_explicit_disabled_reset() {
        let (_base, store) = setup();
        let messages = (0..=MAX_PENDING)
            .map(|index| {
                let ch = char::from_digit((index % 10) as u32, 10).unwrap();
                let mut item = message(ch, "human", "worker", AgentMessageKind::Request, None);
                item.message_id = format!("{index:064x}");
                item
            })
            .collect();
        let admitted = store.admit_inbox(&inbox('b', messages)).unwrap();
        assert!(admitted.needs_attention);
        assert_eq!(
            store.status().unwrap().attention,
            Some(RunnerAttention::PendingOverflow)
        );
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        assert!(store.reset_to_current_cursor(&id('c'), true).is_err());
        store.set_enabled(false).unwrap();
        assert!(store.reset_to_current_cursor(&id('c'), false).is_err());
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Idle);
    }

    #[test]
    fn cursor_reset_needs_attention_without_advancing_or_staging() {
        let (_base, store) = setup();
        let result = AgentInboxResult {
            cursor: id('b'),
            cursor_reset: true,
            messages: vec![message(
                '1',
                "human",
                "worker",
                AgentMessageKind::Request,
                None,
            )],
        };
        let admission = store.admit_inbox(&result).unwrap();
        assert!(admission.needs_attention);
        assert_eq!(admission.admitted, 0);
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let status = store.status().unwrap();
        assert_eq!(status.pending_count, 0);
        assert_eq!(status.attention, Some(RunnerAttention::CursorReset));
    }

    #[test]
    fn preparation_failure_requires_idle_pending_work_and_preserves_it_until_reset() {
        let (base, store) = setup();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        assert!(session
            .record_preparation_failed()
            .unwrap_err()
            .to_string()
            .contains("no pending request"));

        session
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let before =
            serde_json::from_slice::<serde_json::Value>(&fs::read(store.path()).unwrap()).unwrap();
        let status = session.record_preparation_failed().unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::PreparationFailed));
        assert_eq!(status.pending_count, 1);
        assert!(status.active_message_id.is_none());
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let after =
            serde_json::from_slice::<serde_json::Value>(&fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(after["runtime"]["pending"], before["runtime"]["pending"]);
        assert_eq!(
            after["runtime"]["staged_cursor"],
            before["runtime"]["staged_cursor"]
        );
        assert_eq!(after["runtime"]["attention"], "preparation_failed");
        assert!(session.begin_next(&id('b')).is_err());
        assert!(session.record_preparation_failed().is_err());

        store.set_enabled(false).unwrap();
        drop(session);
        let reset = store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(reset.phase, RunnerPhase::Idle);
        assert!(reset.attention.is_none());
        assert_eq!(reset.pending_count, 0);

        let (active_base, active_store) = setup();
        let active_session = active_store
            .execution_session(active_base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        active_session
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        active_session.begin_next(&id('b')).unwrap();
        assert!(active_session
            .record_preparation_failed()
            .unwrap_err()
            .to_string()
            .contains("active request"));
    }

    #[test]
    fn prelaunch_checkpoint_persists_without_body_or_output() {
        let (_base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let launch = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        let bytes = fs::read_to_string(store.path()).unwrap();
        assert!(bytes.contains(&launch.session_id));
        assert!(!bytes.contains("private task body"));
        assert!(!bytes.contains("output"));
        assert!(!bytes.contains("about_snapshot"));
        assert!(!bytes.contains("\"from\""));
        let state: serde_json::Value = serde_json::from_str(&bytes).unwrap();
        assert_eq!(
            state["runtime"]["pending"],
            serde_json::json!([id('1')]),
            "pending persistence must contain message IDs only"
        );
        let invocation = RunnerInvocation::new(&launch, "worker", request).unwrap();
        assert_eq!(invocation.schema_version, 2);
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Launching);
    }

    #[test]
    fn store_rejects_directory_identity_mismatch() {
        let (_base, store) = setup();
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        state["config"]["agent"] = serde_json::json!("other");
        fs::write(store.path(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(store
            .status()
            .unwrap_err()
            .to_string()
            .contains("directory identity"));
    }

    #[cfg(unix)]
    #[test]
    fn configure_rejects_symlinked_agent_layout_ancestors() {
        use std::os::unix::fs::symlink;

        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        for component in ["root", "worktree", "state"] {
            let base = tempfile::tempdir().unwrap();
            prepare_baseline(base.path(), "worker", 3);
            let root = runner_agent_root(base.path(), "worker").unwrap();
            let (original, replacement) = match component {
                "root" => (root.clone(), root.with_file_name("real-worker")),
                name => (root.join(name), root.join(format!("real-{name}"))),
            };
            fs::rename(&original, &replacement).unwrap();
            symlink(&replacement, &original).unwrap();
            assert!(
                RunnerStore::configure(base.path(), "worker", &program, vec![], 60, &id('a'))
                    .unwrap_err()
                    .to_string()
                    .contains("not a real directory")
            );
        }
    }

    #[test]
    fn execution_session_requires_complete_terminal_correlation() {
        let (base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, 42, "correlation-test-child")
            .unwrap();

        let mut mismatched_request = request.clone();
        mismatched_request.message_id = id('9');
        assert!(session
            .observe_terminals(
                &mismatched_request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('9'),
                )],
            )
            .is_err());
        let mut indirect_request = request.clone();
        indirect_request.to = "other".into();
        assert!(session
            .observe_terminals(
                &indirect_request,
                &[message(
                    '3',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .is_err());

        let unrelated = [
            message('4', "worker", "other", AgentMessageKind::Result, Some('1')),
            message('5', "other", "human", AgentMessageKind::Result, Some('1')),
            message('6', "worker", "human", AgentMessageKind::Status, Some('1')),
            message('7', "worker", "human", AgentMessageKind::Result, Some('9')),
        ];
        for terminal in unrelated {
            assert!(session
                .observe_terminals(&request, &[terminal])
                .unwrap()
                .is_none());
        }
        let mut stale_result = message('8', "worker", "human", AgentMessageKind::Result, Some('1'));
        stale_result.about_snapshot = id('e');
        assert!(
            session
                .observe_terminals_at_snapshot(
                    &request,
                    &[stale_result],
                    Some(&request.about_snapshot),
                )
                .unwrap()
                .is_none()
        );
        let unsettled_result = message('9', "worker", "human", AgentMessageKind::Result, Some('1'));
        assert!(session
            .observe_terminals_at_snapshot(&request, &[unsettled_result], None)
            .unwrap()
            .is_none());
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Running);
        assert!(session
            .observe_terminals_at_snapshot(
                &request,
                &[message(
                    'a',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1')
                )],
                Some(&request.about_snapshot),
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn startup_marks_ambiguous_and_never_replays() {
        let (_base, store) = setup();
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let launch = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        let status = store.checkpoint_startup().unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::AmbiguousExecution));
        assert_eq!(
            status.active_message_id.as_deref(),
            Some(launch.message_id.as_str())
        );
        assert!(store
            .begin_next(RunnerExecutionMode::Supervised, &id('c'))
            .is_err());
        assert_eq!(store.status().unwrap().pending_count, 1);

        store.set_enabled(false).unwrap();
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        store.set_enabled(true).unwrap();
        store
            .admit_inbox(&inbox(
                'd',
                vec![message(
                    '2',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let running = store
            .begin_next(RunnerExecutionMode::Supervised, &id('d'))
            .unwrap();
        store
            .mark_spawned(&running.message_id, 42, "process-start")
            .unwrap();
        assert_eq!(store.status().unwrap().phase, RunnerPhase::Running);
        assert_eq!(
            store.checkpoint_startup().unwrap().attention,
            Some(RunnerAttention::AmbiguousExecution)
        );
    }

    #[test]
    fn delivery_unknown_retains_active_queue_and_cursor_without_replay() {
        let (_base, store) = setup();
        store
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let launch = store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .mark_spawned(&launch.message_id, 42, "private-process-start-id")
            .unwrap();
        let before = store.status().unwrap();
        let cursor_before = store.committed_cursor().unwrap();

        let status = store
            .record_delivery_unknown(&launch.message_id, &launch.session_id)
            .unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::DeliveryUnknown));
        assert_eq!(status.pending_count, 1);
        assert_eq!(status.active_message_id, before.active_message_id);
        assert_eq!(status.active_session_id, before.active_session_id);
        assert_eq!(status.active_started_at_ms, before.active_started_at_ms);
        assert_eq!(status.active_spawned_at_ms, before.active_spawned_at_ms);
        assert_eq!(store.committed_cursor().unwrap(), cursor_before);
        assert!(store
            .begin_next(RunnerExecutionMode::Supervised, &id('c'))
            .is_err());
        assert!(store.admit_inbox(&inbox('c', Vec::new())).is_err());
        assert_eq!(store.status().unwrap().pending_count, 1);
        assert_eq!(store.committed_cursor().unwrap(), cursor_before);

        let redacted = serde_json::to_string(&status).unwrap();
        assert!(redacted.contains("delivery_unknown"));
        assert!(!redacted.contains("private-process-start-id"));
        assert!(!redacted.contains("private task body"));
        assert!(!redacted.contains("output"));
    }

    #[test]
    fn reset_retains_completed_dedupe_and_offline_recovery_preserves_tasks() {
        let (_base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        store
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        store
            .observe_terminals(
                &request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        store.record_inbox_failure().unwrap();
        assert_eq!(store.status().unwrap().inbox_failure_count, 1);
        assert_eq!(store.status().unwrap().pending_count, 0);
        store.record_inbox_recovery().unwrap();
        assert_eq!(store.status().unwrap().inbox_failure_count, 0);
        store.set_enabled(false).unwrap();
        store.reset_to_current_cursor(&id('c'), true).unwrap();
        store.set_enabled(true).unwrap();
        assert_eq!(
            store
                .admit_inbox(&inbox('d', vec![request]))
                .unwrap()
                .admitted,
            0
        );
    }

    fn assert_offline_accounting_preserves_lifecycle(store: &RunnerStore) {
        let before = store.status().unwrap();
        let cursor = store.committed_cursor().unwrap();
        let failed = store.record_inbox_failure().unwrap();
        let mut expected_failed = before.clone();
        expected_failed.updated_at_ms = failed.updated_at_ms;
        expected_failed.inbox_failure_count = 1;
        assert_eq!(failed, expected_failed);
        assert_eq!(store.committed_cursor().unwrap(), cursor);

        let recovered = store.record_inbox_recovery().unwrap();
        let mut expected_recovered = before;
        expected_recovered.updated_at_ms = recovered.updated_at_ms;
        expected_recovered.inbox_failure_count = 0;
        assert_eq!(recovered, expected_recovered);
        assert_eq!(store.committed_cursor().unwrap(), cursor);
    }

    #[test]
    fn offline_failure_and_recovery_preserve_pending_launching_and_running_state() {
        let (_pending_base, pending) = setup();
        pending
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        assert_eq!(pending.status().unwrap().phase, RunnerPhase::Idle);
        assert_offline_accounting_preserves_lifecycle(&pending);

        let (_launching_base, launching) = setup();
        launching
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        launching
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        assert_eq!(launching.status().unwrap().phase, RunnerPhase::Launching);
        assert_offline_accounting_preserves_lifecycle(&launching);

        let (_running_base, running) = setup();
        running
            .admit_inbox(&inbox(
                'b',
                vec![message(
                    '1',
                    "human",
                    "worker",
                    AgentMessageKind::Request,
                    None,
                )],
            ))
            .unwrap();
        let launch = running
            .begin_next(RunnerExecutionMode::Supervised, &id('b'))
            .unwrap();
        running
            .mark_spawned(&launch.message_id, 42, "process-start")
            .unwrap();
        assert_eq!(running.status().unwrap().phase, RunnerPhase::Running);
        assert_offline_accounting_preserves_lifecycle(&running);
    }

    #[tokio::test]
    async fn refresh_and_land_refuse_while_configured_runner_owns_the_lease() {
        let (base, _store) = setup_disabled();
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
        let lease = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
            .unwrap()
            .unwrap();

        let refresh_error = crate::agent::refresh_agent(
            base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
        )
        .await
        .unwrap_err();
        assert!(refresh_error.to_string().contains("already active"));

        let land_error = crate::agent::land_agent(
            base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(land_error.to_string().contains("already active"));
        drop(lease);
    }

    #[derive(Clone, Copy)]
    enum TestWorktreeMutation {
        Refresh,
        Land,
    }

    async fn assert_configure_waits_for_unconfigured_operation(operation: TestWorktreeMutation) {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let state = crate::workspace_layout::ensure_workspace_state(base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let mut operation_pause = install_operation_guard_pause(base.path(), "worker").unwrap();
        let operation_base = base.path().to_path_buf();
        let operation_task = tokio::spawn(async move {
            let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
            let password = "e".repeat(64);
            match operation {
                TestWorktreeMutation::Refresh => crate::agent::refresh_agent(
                    &operation_base,
                    &db,
                    &api,
                    "runner-test",
                    "worker",
                    Some(&password),
                )
                .await
                .map(|_| ()),
                TestWorktreeMutation::Land => crate::agent::land_agent(
                    &operation_base,
                    &db,
                    &api,
                    "runner-test",
                    "worker",
                    Some(&password),
                    false,
                    false,
                )
                .await
                .map(|_| ()),
            }
        });
        operation_pause.wait("worktree mutation did not acquire its unconfigured runner guard");

        let contended = install_lifecycle_contention_hook(base.path()).unwrap();
        let configure_base = base.path().to_path_buf();
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let configure = tokio::task::spawn_blocking(move || {
            RunnerStore::configure(
                &configure_base,
                "worker",
                &program,
                Vec::new(),
                3600,
                &id('a'),
            )
        });
        contended.wait("runner configure did not contend on the held operation guard");
        assert!(!configure.is_finished());

        operation_pause.release().unwrap();
        assert!(operation_task.await.unwrap().is_err());
        configure.await.unwrap().unwrap();
        assert!(runner_status(base.path()).unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn configure_cannot_race_unconfigured_refresh_or_land() {
        assert_configure_waits_for_unconfigured_operation(TestWorktreeMutation::Refresh).await;
        assert_configure_waits_for_unconfigured_operation(TestWorktreeMutation::Land).await;
    }

    #[tokio::test]
    async fn guarded_refresh_requires_exact_identity_and_does_not_self_lock() {
        let (owned_base, owned_store) = setup_disabled();
        let (other_base, _other_store) = setup_disabled();
        let state = crate::workspace_layout::ensure_workspace_state(owned_base.path()).unwrap();
        let db = crate::local::ClientDb::new(state).await.unwrap();
        let api = crate::api::ApiClient::new("http://127.0.0.1:1", None);
        let session = owned_store
            .execution_session(owned_base.path(), RunnerExecutionMode::Foreground)
            .unwrap();

        let mismatch = crate::agent::refresh_agent_guarded(
            other_base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
            &session,
        )
        .await
        .unwrap_err();
        assert!(mismatch.to_string().contains("does not own agent"));

        fs::remove_file(
            runner_agent_root(owned_base.path(), "worker")
                .unwrap()
                .join("state/base-snapshot"),
        )
        .unwrap();
        let validation = crate::agent::refresh_agent_guarded(
            owned_base.path(),
            &db,
            &api,
            "runner-test",
            "worker",
            Some(&"e".repeat(64)),
            &session,
        )
        .await
        .unwrap_err();
        assert!(validation.to_string().contains("base snapshot"));
        assert!(!validation.to_string().contains("already active"));
    }

    #[test]
    fn bounds_corrupt_unknown_and_future_state_fail_closed() {
        let (base, store) = setup();
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert!(RunnerStore::reconfigure(base.path(), "worker", &program, vec![], 59,).is_err());
        assert!(RunnerStore::reconfigure(
            base.path(),
            "worker",
            &program,
            vec![String::new(); MAX_ARGS + 1],
            60,
        )
        .is_err());
        fs::write(store.path(), "not-json").unwrap();
        assert!(store.status().is_err());
        assert!(runner_status(base.path()).is_err());
        assert!(runner_process_metadata(base.path()).is_err());

        let (_base, store) = setup();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        value["unknown"] = serde_json::json!(true);
        fs::write(store.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(store
            .status()
            .unwrap_err()
            .to_string()
            .contains("parse runner state"));

        let (_base, store) = setup();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(99);
        fs::write(store.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(store
            .status()
            .unwrap_err()
            .to_string()
            .contains("unsupported runner state schema 99"));
    }

    #[cfg(unix)]
    #[test]
    fn configure_requires_executable_program_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let program_dir = tempfile::tempdir().unwrap();
        let program = program_dir.path().join("runner-program");
        fs::write(&program, b"not executable").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();
        let program = fs::canonicalize(program).unwrap();
        assert!(
            RunnerStore::configure(base.path(), "worker", &program, vec![], 60, &id('a'))
                .unwrap_err()
                .to_string()
                .contains("must be executable")
        );
    }

    #[test]
    fn second_lifetime_lock_refuses_without_waiting() {
        let (base, _store) = setup();
        let _first = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker")
            .unwrap()
            .unwrap();
        let error = RunnerLifetimeLock::try_acquire_configured(base.path(), "worker").unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    fn setup_scoped(scope_mode: RunnerScopeMode) -> (tempfile::TempDir, RunnerStore) {
        let base = tempfile::tempdir().unwrap();
        prepare_baseline(base.path(), "worker", 3);
        let program = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let store = RunnerStore::configure_scoped(
            base.path(),
            "worker",
            &program,
            vec!["--fixed".into()],
            3600,
            &id('a'),
            scope_mode,
        )
        .unwrap();
        store.set_enabled(true).unwrap();
        (base, store)
    }

    fn descriptor_for(request: &AgentMessage) -> AcceptedWorkDescriptor {
        AcceptedWorkDescriptor {
            schema_version: ACCEPTED_WORK_SCHEMA_VERSION,
            task_id: "task-one".to_string(),
            intent_message_id: request.message_id.clone(),
            agent: "worker".to_string(),
            sequence: 1,
            scope: WorkScope {
                paths: vec!["src/lib.rs".to_string()],
                concerns: vec!["lib behavior".to_string()],
                dependencies: vec![],
            },
            capabilities: vec![],
            coordinator: Some("human".to_string()),
            causal_base: None,
            base_snapshot: id('f'),
            message_fingerprint: crate::agent::scope::message_fingerprint(request),
            source_message_id: id('1'),
            updated_at_ms: 1,
        }
    }

    #[test]
    fn legacy_state_migrates_to_explicit_legacy_unenforced_and_never_claims_scope() {
        let (base, store) = setup_disabled();
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        state["schema_version"] = serde_json::json!(2);
        state["config"]
            .as_object_mut()
            .unwrap()
            .remove("scope_mode");
        fs::write(store.path(), serde_json::to_vec(&state).unwrap()).unwrap();

        // A fresh read migrates in memory to explicit legacy_unenforced.
        let reopened = RunnerStore::open_configured(base.path()).unwrap();
        assert_eq!(
            reopened.config().unwrap().scope_mode,
            RunnerScopeMode::LegacyUnenforced
        );
        // The next state update persists the normalized schema.
        reopened.set_enabled(false).unwrap();
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(migrated["schema_version"], 3);
        assert_eq!(migrated["config"]["scope_mode"], "legacy_unenforced");
    }

    #[test]
    fn scope_mode_round_trips_and_newer_schema_fails_closed() {
        let (base, store) = setup_scoped(RunnerScopeMode::Enforced);
        assert_eq!(
            store.config().unwrap().scope_mode,
            RunnerScopeMode::Enforced
        );
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(raw["config"]["scope_mode"], "enforced");

        // A future schema is rejected on read, never silently downgraded.
        let mut newer = raw.clone();
        newer["schema_version"] = serde_json::json!(99);
        fs::write(store.path(), serde_json::to_vec(&newer).unwrap()).unwrap();
        assert!(RunnerStore::open_configured(base.path()).is_err());

        // A corrupted work_wait shape fails closed on load.
        let mut corrupt = raw.clone();
        corrupt["runtime"]["work_wait"] = serde_json::json!({
            "kind": "waiting_acceptance",
            "message_id": "not-a-hash",
        });
        fs::write(store.path(), serde_json::to_vec(&corrupt).unwrap()).unwrap();
        assert!(RunnerStore::open_configured(base.path()).is_err());
    }

    #[test]
    fn enforced_launch_requires_and_binds_accepted_work() {
        let (base, store) = setup_scoped(RunnerScopeMode::Enforced);
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();

        // Plain begin_next is refused on an enforced runner.
        assert!(session.begin_next(&id('b')).is_err());

        // A descriptor for a different request is refused.
        let mut wrong = descriptor_for(&request);
        wrong.intent_message_id = id('9');
        assert!(session
            .begin_next_admitted(&id('b'), wrong)
            .unwrap_err()
            .to_string()
            .contains("does not match"));

        // A descriptor for the wrong agent is refused.
        let mut other_agent = descriptor_for(&request);
        other_agent.agent = "other".to_string();
        assert!(session.begin_next_admitted(&id('b'), other_agent).is_err());

        // The valid descriptor binds into the checkpoint and invocation.
        let descriptor = descriptor_for(&request);
        let launch = session
            .begin_next_admitted(&id('b'), descriptor.clone())
            .unwrap();
        assert_eq!(launch.accepted_work, Some(descriptor.clone()));
        let invocation = RunnerInvocation::new(&launch, "worker", request).unwrap();
        assert_eq!(invocation.schema_version, 2);
        assert_eq!(
            invocation
                .accepted_work
                .as_ref()
                .map(|d| d.task_id.as_str()),
            Some("task-one")
        );

        // The persisted active checkpoint carries the descriptor and the
        // fingerprint binds the exact message.
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(
            persisted["runtime"]["active"]["accepted_work"]["task_id"],
            "task-one"
        );
        assert!(!persisted.to_string().contains("private task body"));
        assert_eq!(store.active_accepted_work().unwrap(), Some(descriptor));
    }

    #[test]
    fn legacy_runner_rejects_an_accepted_work_claim() {
        let (base, store) = setup();
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        store
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let descriptor = descriptor_for(&request);
        let error = session
            .begin_next_admitted(&id('b'), descriptor)
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-enforced runner launch cannot claim accepted work"));
        assert!(session.begin_next(&id('b')).is_ok());
        assert_eq!(store.active_accepted_work().unwrap(), None);
    }

    #[test]
    fn work_wait_is_typed_bounded_and_cleared_on_launch_and_terminal() {
        let (base, store) = setup_scoped(RunnerScopeMode::Enforced);
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        // No pending request: the wait cannot be recorded.
        assert!(session
            .record_work_wait(&RunnerWorkWait {
                kind: RunnerWorkWaitKind::WaitingAcceptance,
                message_id: request.message_id.clone(),
                reason: Some(RunnerAdmissionReject::RequestWithoutIntent),
                out_of_scope_count: 0,
                observed_at_ms: 1,
            })
            .is_err());

        session
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let wait = RunnerWorkWait {
            kind: RunnerWorkWaitKind::WaitingAcceptance,
            message_id: request.message_id.clone(),
            reason: Some(RunnerAdmissionReject::RequestWithoutIntent),
            out_of_scope_count: 0,
            observed_at_ms: 2,
        };
        session.record_work_wait(&wait).unwrap();
        let status = store.status().unwrap();
        assert_eq!(status.work_wait, Some(wait.clone()));
        // Waiting is not attention: the runner stays live.
        assert_eq!(status.attention, None);
        assert_eq!(status.phase, RunnerPhase::Idle);

        // Launching clears the wait and binds the descriptor.
        let descriptor = descriptor_for(&request);
        session.begin_next_admitted(&id('b'), descriptor).unwrap();
        assert_eq!(store.status().unwrap().work_wait, None);
        session
            .mark_spawned(&request.message_id, 42, "wait-clear-child")
            .unwrap();
        session
            .observe_terminals(
                &request,
                &[message(
                    '2',
                    "worker",
                    "human",
                    AgentMessageKind::Result,
                    Some('1'),
                )],
            )
            .unwrap();
        assert_eq!(store.status().unwrap().work_wait, None);
        assert_eq!(
            store.status().unwrap().scope_mode,
            RunnerScopeMode::Enforced
        );
    }

    #[test]
    fn scope_amendment_wait_and_dedup_record_validate() {
        let (base, store) = setup_scoped(RunnerScopeMode::Enforced);
        let request = message('1', "human", "worker", AgentMessageKind::Request, None);
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        session
            .admit_inbox(&inbox('b', vec![request.clone()]))
            .unwrap();
        let wait = RunnerWorkWait {
            kind: RunnerWorkWaitKind::ScopeAmendmentRequested,
            message_id: request.message_id.clone(),
            reason: None,
            out_of_scope_count: 3,
            observed_at_ms: 2,
        };
        session.record_work_wait(&wait).unwrap();
        assert!(
            store
                .record_scope_change_request_locked(
                    &id('7'),
                    "task-one",
                    &request.message_id,
                    "abc123",
                )
                .unwrap()
        );
        // Identical (task, intent, fingerprint) is not republished.
        assert!(
            !store
                .record_scope_change_request_locked(
                    &id('8'),
                    "task-one",
                    &request.message_id,
                    "abc123",
                )
                .unwrap()
        );
        // A changed fingerprint republishes.
        assert!(
            store
                .record_scope_change_request_locked(
                    &id('9'),
                    "task-one",
                    &request.message_id,
                    "def456",
                )
                .unwrap()
        );
        let status = store.status().unwrap();
        assert_eq!(
            status.work_wait.as_ref().map(|w| w.kind),
            Some(RunnerWorkWaitKind::ScopeAmendmentRequested)
        );
        // Work wait with a reason on a scope wait is rejected on load.
        let mut bad = wait.clone();
        bad.reason = Some(RunnerAdmissionReject::RequestWithoutIntent);
        let mut state = store.load().unwrap();
        state.runtime.work_wait = Some(bad);
        assert!(validate_state(&state).is_err());
    }
}
