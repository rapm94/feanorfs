//! Shared execution loop for configured unattended agent runners.

mod cycle;
mod inbox;
mod process;
mod remote;
mod render;

pub(crate) use cycle::run_worker;

// The shared unit-test module exercises the runner's internal helpers through
// `use super::*`; the globs are therefore test-only.
#[cfg(test)]
use cycle::*;
#[cfg(test)]
use process::*;
#[cfg(test)]
use remote::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{ExitStatus, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::super::agent_live::RunnerControllerHandle;
    use super::super::process_tree;
    use super::super::process_tree::{ChildReaper, ReadyChildReaper};
    use feanorfs_agent_core::messages::HeadConditionalSendResult;
    use feanorfs_agent_core::{
        RunnerAttention, RunnerExecutionMode, RunnerExecutionSession, RunnerLaunch, RunnerPhase,
        RunnerStore,
    };
    use feanorfs_common::{AgentMessage, AgentMessageKind};

    fn id(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn test_reaper() -> &'static ChildReaper {
        Box::leak(Box::new(ChildReaper::new()))
    }

    fn reap_child_command() -> tokio::process::Command {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_reap_helper",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        process_tree::configure_process_group(&mut command).unwrap();
        command
    }

    fn spawn_reap_child(_reaper: ReadyChildReaper) -> tokio::process::Child {
        reap_child_command().spawn().unwrap()
    }

    fn spawn_managed_reap_child(reaper: &'static ChildReaper) -> ManagedChild {
        let mut command = reap_child_command();
        spawn_managed_child(reaper, || command.spawn()).unwrap()
    }

    #[cfg(windows)]
    fn release_suspended_child(child: &ManagedChild) {
        let tree = child
            .process_tree
            .as_ref()
            .expect("managed child has a Windows Job Object");
        let process = child
            .child
            .as_ref()
            .expect("managed child retains its process handle");
        tree.release_child(process)
            .expect("release adopted suspended child");
    }

    #[cfg(any(unix, windows))]
    async fn wait_for_descendant_pid(path: &Path, timeout: Duration) -> u32 {
        tokio::time::timeout(timeout, async {
            loop {
                if let Ok(value) = std::fs::read_to_string(path) {
                    if let Ok(pid) = value.parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant became ready")
    }

    async fn wait_for_reaper_idle(reaper: &'static ChildReaper) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !reaper.is_idle() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child reaper became idle");
    }

    fn setup_active_runner() -> (crate::cli::RunnerTestWorkspace, RunnerStore) {
        let base = crate::cli::RunnerTestWorkspace::new();
        feanorfs_client::save_config(
            base.path(),
            &feanorfs_client::Config {
                server_url: "http://127.0.0.1:1".into(),
                workspace_id: "runner-test".into(),
                encryption_password: Some("e".repeat(64)),
                server_password: None,
                tls_ca_pem: None,
                format_version: 3,
                hub_local: false,
                relay: None,
                mesh: None,
            },
        )
        .unwrap();
        let worktree = feanorfs_agent_core::agent_dir(base.path(), "worker").unwrap();
        let agent_root = worktree.parent().unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(agent_root.join("state")).unwrap();
        std::fs::write(agent_root.join("state/base-snapshot"), id('f')).unwrap();
        let program = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let store =
            RunnerStore::configure(base.path(), "worker", &program, Vec::new(), 60, &id('a'))
                .unwrap();
        store.set_enabled(true).unwrap();
        (base, store)
    }

    fn begin_active_request(session: &RunnerExecutionSession<'_>) -> (RunnerLaunch, AgentMessage) {
        let request = AgentMessage {
            message_id: id('1'),
            from: "requester".into(),
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "private request".into(),
            about_snapshot: id('f'),
            reply_to: None,
            created_at_ms: 1,
        };
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('b'),
                cursor_reset: false,
                messages: vec![request.clone()],
            })
            .unwrap();
        let launch = session.begin_next(&id('b')).unwrap();
        session
            .mark_spawned(&launch.message_id, std::process::id(), "test-process")
            .unwrap();
        (launch, request)
    }

    fn terminal_for(
        request: &AgentMessage,
        message_id: String,
        kind: AgentMessageKind,
    ) -> AgentMessage {
        AgentMessage {
            message_id,
            from: "worker".into(),
            to: request.from.clone(),
            kind,
            body: "legitimate terminal".into(),
            about_snapshot: request.about_snapshot.clone(),
            reply_to: Some(request.message_id.clone()),
            created_at_ms: 2,
        }
    }

    fn assert_delivery_unknown(store: &RunnerStore, launch: &RunnerLaunch, enabled: bool) {
        let status = store.status().unwrap();
        assert_eq!(status.enabled, enabled);
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::DeliveryUnknown));
        assert_eq!(status.pending_count, 1);
        assert_eq!(
            status.active_message_id.as_deref(),
            Some(launch.message_id.as_str())
        );
        assert_eq!(
            status.active_session_id.as_deref(),
            Some(launch.session_id.as_str())
        );
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
    }

    async fn wait_for_first_terminal_failure(store: &RunnerStore) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.status().unwrap().inbox_failure_count == 1 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal read entered retry backoff");
    }

    #[test]
    fn backoff_is_deterministic_and_bounded() {
        assert_eq!(RUNNER_BACKOFF.delay(0), Duration::ZERO);
        assert_eq!(RUNNER_BACKOFF.delay(1), Duration::from_secs(1));
        assert_eq!(RUNNER_BACKOFF.delay(2), Duration::from_secs(2));
        assert_eq!(RUNNER_BACKOFF.delay(7), Duration::from_secs(60));
        assert_eq!(RUNNER_BACKOFF.delay(u32::MAX), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn closed_controller_generation_reports_failure_without_spinning() {
        let (_base, store) = setup_active_runner();
        let controller = RunnerControllerHandle::stopped_for_test("injected controller failure");
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let error = tokio::time::timeout(
            Duration::from_millis(250),
            wait_for_head_wakeup(
                &controller,
                &store,
                RunnerExecutionMode::Supervised,
                &shutdown,
            ),
        )
        .await
        .expect("closed generation channel returns promptly")
        .unwrap_err();
        assert!(error.to_string().contains("injected controller failure"));
    }

    #[tokio::test]
    async fn transport_refresh_failure_keeps_the_remote_retry_path() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let request = AgentMessage {
            message_id: id('1'),
            from: "requester".into(),
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "private request".into(),
            about_snapshot: id('f'),
            reply_to: None,
            created_at_ms: 1,
        };
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('b'),
                cursor_reset: false,
                messages: vec![request],
            })
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let api = feanorfs_client::ApiClient::new(&format!("http://{address}"), None);

        let outcome = refresh_before_launch(&session, api.get_head("runner-test"))
            .await
            .unwrap();

        assert_eq!(outcome, Some(CycleOutcome::RemoteUnavailable));
        let status = store.status().unwrap();
        assert_eq!(status.pending_count, 1);
        assert!(status.active_message_id.is_none());
        assert!(status.attention.is_none());
        let retry = session.record_inbox_failure().unwrap();
        assert_eq!(
            RUNNER_BACKOFF.delay(retry.inbox_failure_count),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn local_refresh_failure_stops_before_child_launch_and_requires_reset() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let request = AgentMessage {
            message_id: id('1'),
            from: "requester".into(),
            to: "worker".into(),
            kind: AgentMessageKind::Request,
            body: "private request".into(),
            about_snapshot: id('f'),
            reply_to: None,
            created_at_ms: 1,
        };
        session
            .admit_inbox(&feanorfs_common::AgentInboxResult {
                cursor: id('b'),
                cursor_reset: false,
                messages: vec![request],
            })
            .unwrap();

        let outcome = refresh_before_launch(
            &session,
            std::future::ready(Err::<(), _>(anyhow::anyhow!(
                "injected local runner refresh failure: private details"
            ))),
        )
        .await
        .unwrap();

        assert_eq!(outcome, Some(CycleOutcome::NeedsAttention));
        let status = store.status().unwrap();
        assert_eq!(status.phase, RunnerPhase::NeedsAttention);
        assert_eq!(status.attention, Some(RunnerAttention::PreparationFailed));
        assert_eq!(status.pending_count, 1);
        assert!(status.active_message_id.is_none());
        assert_eq!(store.committed_cursor().unwrap(), id('a'));
        let persisted = std::fs::read_to_string(store.path()).unwrap();
        assert!(persisted.contains("preparation_failed"));
        assert!(!persisted.contains("injected local runner refresh failure"));
        assert!(!persisted.contains("private details"));
        assert!(session.begin_next(&id('b')).is_err());

        store.set_enabled(false).unwrap();
        drop(session);
        let reset = store.reset_to_current_cursor(&id('c'), true).unwrap();
        assert_eq!(reset.phase, RunnerPhase::Idle);
        assert!(reset.attention.is_none());
        assert_eq!(reset.pending_count, 0);
    }

    #[tokio::test]
    async fn supervised_disable_interrupts_terminal_retry_backoff() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, _request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let read_attempts = Arc::clone(&attempts);
        let read = read_terminal_batch(
            &store,
            &session,
            &launch,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            move || {
                read_attempts.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<feanorfs_common::AgentInboxResult, _>(anyhow::anyhow!("hub offline"))
                }
            },
        );
        let disable = async {
            wait_for_first_terminal_failure(&store).await;
            store.set_enabled(false).unwrap();
        };
        let (result, ()) = tokio::join!(read, disable);

        assert!(matches!(
            result.unwrap(),
            TerminalReadOutcome::NeedsAttention
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_delivery_unknown(&store, &launch, false);
    }

    #[tokio::test]
    async fn pre_cancelled_terminal_read_is_bounded_by_completion_deadline() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        store.set_enabled(false).unwrap();
        let deadline = cancellation_completion_deadline(
            &store,
            ProcessOutcome::Cancellation,
            RunnerExecutionMode::Supervised,
            &shutdown,
            Duration::from_millis(50),
        )
        .unwrap()
        .expect("disabled cancellation receives a completion deadline");
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let send_count = Arc::clone(&send_attempts);

        // The pending remote can complete only through the cancellation
        // deadline. The outer timeout is a hang guard; a tighter wall-clock
        // assertion would measure executor scheduling rather than behavior.
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            complete_request_with_remote(
                &store,
                &session,
                "worker",
                &launch,
                &request,
                ProcessOutcome::Cancellation,
                RunnerExecutionMode::Supervised,
                &shutdown,
                Some(deadline),
                None,
                std::future::pending::<anyhow::Result<feanorfs_common::AgentInboxResult>>,
                move |_, _| {
                    send_count.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<anyhow::Result<HeadConditionalSendResult>>()
                },
            ),
        )
        .await
        .expect("pre-cancelled terminal read respects its deadline")
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_eq!(send_attempts.load(Ordering::SeqCst), 0);
        assert_delivery_unknown(&store, &launch, false);
    }

    #[tokio::test]
    async fn pre_cancelled_read_and_send_share_one_completion_deadline() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        store.set_enabled(false).unwrap();
        let deadline = cancellation_completion_deadline(
            &store,
            ProcessOutcome::Cancellation,
            RunnerExecutionMode::Supervised,
            &shutdown,
            Duration::from_millis(500),
        )
        .unwrap()
        .expect("disabled cancellation receives a completion deadline");
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let send_count = Arc::clone(&send_attempts);

        let result = tokio::time::timeout_at(
            deadline + Duration::from_millis(100),
            complete_request_with_remote(
                &store,
                &session,
                "worker",
                &launch,
                &request,
                ProcessOutcome::Cancellation,
                RunnerExecutionMode::Supervised,
                &shutdown,
                Some(deadline),
                None,
                || async {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Ok(feanorfs_common::AgentInboxResult {
                        cursor: id('c'),
                        cursor_reset: false,
                        messages: Vec::new(),
                    })
                },
                move |_, _| {
                    send_count.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<anyhow::Result<HeadConditionalSendResult>>()
                },
            ),
        )
        .await
        .expect("fallback publication cannot reset the shared deadline")
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_eq!(send_attempts.load(Ordering::SeqCst), 1);
        assert_delivery_unknown(&store, &launch, false);
    }

    #[tokio::test]
    async fn conflicting_fallback_rereads_and_accepts_concurrent_terminal() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let first_head = id('c');
        let terminal_head = id('d');
        let terminal = terminal_for(&request, terminal_head.clone(), AgentMessageKind::Result);
        let read_attempts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&read_attempts);
        let expected_heads = Arc::new(Mutex::new(Vec::new()));
        let sent_heads = Arc::clone(&expected_heads);
        let expected_result_snapshot = request.about_snapshot.clone();

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            Some(expected_result_snapshot),
            move || {
                let attempt = reads.fetch_add(1, Ordering::SeqCst);
                let batch = match attempt {
                    0 => feanorfs_common::AgentInboxResult {
                        cursor: first_head.clone(),
                        cursor_reset: false,
                        messages: Vec::new(),
                    },
                    1 => feanorfs_common::AgentInboxResult {
                        cursor: terminal_head.clone(),
                        cursor_reset: false,
                        messages: vec![terminal.clone()],
                    },
                    _ => panic!("unexpected terminal reread"),
                };
                std::future::ready(Ok(batch))
            },
            move |expected, input| {
                sent_heads.lock().unwrap().push(expected);
                assert_eq!(input.kind, AgentMessageKind::Blocked);
                std::future::ready(Ok(HeadConditionalSendResult::Conflict(Some(id('d')))))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::Completed);
        assert_eq!(read_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(*expected_heads.lock().unwrap(), vec![id('c')]);
        let status = store.status().unwrap();
        assert_eq!(status.phase, RunnerPhase::Idle);
        assert_eq!(status.last_terminal_kind, Some(AgentMessageKind::Result));
    }

    #[tokio::test]
    async fn result_for_pre_final_snapshot_falls_back_to_blocked() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let mut stale = terminal_for(&request, id('c'), AgentMessageKind::Result);
        stale.about_snapshot = id('e');
        let settled_snapshot = request.about_snapshot.clone();
        let expected_fallback_about = settled_snapshot.clone();
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let sends = Arc::clone(&send_attempts);

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            Some(settled_snapshot),
            move || {
                std::future::ready(Ok(feanorfs_common::AgentInboxResult {
                    cursor: id('c'),
                    cursor_reset: false,
                    messages: vec![stale.clone()],
                }))
            },
            move |_, input| {
                sends.fetch_add(1, Ordering::SeqCst);
                assert_eq!(input.kind, AgentMessageKind::Blocked);
                assert_eq!(
                    input.about_snapshot.as_deref(),
                    Some(expected_fallback_about.as_str())
                );
                std::future::ready(Ok(HeadConditionalSendResult::Sent(
                    feanorfs_common::AgentSendResult {
                        message_id: id('d'),
                        about_snapshot: expected_fallback_about.clone(),
                    },
                )))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::Completed);
        assert_eq!(send_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.status().unwrap().last_terminal_kind,
            Some(AgentMessageKind::Blocked)
        );
    }

    #[tokio::test]
    async fn unrelated_head_conflict_retries_against_reread_head() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let read_attempts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&read_attempts);
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let sends = Arc::clone(&send_attempts);
        let expected_heads = Arc::new(Mutex::new(Vec::new()));
        let sent_heads = Arc::clone(&expected_heads);
        let about_snapshot = request.about_snapshot.clone();

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            None,
            move || {
                let cursor = match reads.fetch_add(1, Ordering::SeqCst) {
                    0 => id('c'),
                    1 => id('d'),
                    _ => panic!("unexpected terminal reread"),
                };
                std::future::ready(Ok(feanorfs_common::AgentInboxResult {
                    cursor,
                    cursor_reset: false,
                    messages: Vec::new(),
                }))
            },
            move |expected, _| {
                sent_heads.lock().unwrap().push(expected);
                let outcome = match sends.fetch_add(1, Ordering::SeqCst) {
                    0 => HeadConditionalSendResult::Conflict(Some(id('d'))),
                    1 => HeadConditionalSendResult::Sent(feanorfs_common::AgentSendResult {
                        message_id: id('e'),
                        about_snapshot: about_snapshot.clone(),
                    }),
                    _ => panic!("unexpected fallback retry"),
                };
                std::future::ready(Ok(outcome))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::Completed);
        assert_eq!(read_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(send_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(*expected_heads.lock().unwrap(), vec![id('c'), id('d')]);
        assert_eq!(
            store.status().unwrap().last_terminal_kind,
            Some(AgentMessageKind::Blocked)
        );
    }

    #[tokio::test]
    async fn fallback_conflict_retries_are_bounded() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let read_attempts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&read_attempts);
        let send_attempts = Arc::new(AtomicUsize::new(0));
        let sends = Arc::clone(&send_attempts);
        let cursors = [id('c'), id('d'), id('e'), id('f'), id('7')];

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            None,
            move || {
                let attempt = reads.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(feanorfs_common::AgentInboxResult {
                    cursor: cursors[attempt].clone(),
                    cursor_reset: false,
                    messages: Vec::new(),
                }))
            },
            move |_, _| {
                sends.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(HeadConditionalSendResult::Conflict(Some(id('8')))))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_eq!(send_attempts.load(Ordering::SeqCst), FALLBACK_CAS_ATTEMPTS);
        assert_eq!(
            read_attempts.load(Ordering::SeqCst),
            FALLBACK_CAS_ATTEMPTS + 1
        );
        assert_delivery_unknown(&store, &launch, true);
    }

    #[tokio::test]
    async fn uncertain_fallback_publication_records_delivery_unknown() {
        let (base, store) = setup_active_runner();
        let session = store
            .execution_session(base.path(), RunnerExecutionMode::Supervised)
            .unwrap();
        let (launch, request) = begin_active_request(&session);
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);

        let result = complete_request_with_remote(
            &store,
            &session,
            "worker",
            &launch,
            &request,
            ProcessOutcome::Exited,
            RunnerExecutionMode::Supervised,
            &shutdown,
            None,
            None,
            || {
                std::future::ready(Ok(feanorfs_common::AgentInboxResult {
                    cursor: id('c'),
                    cursor_reset: false,
                    messages: Vec::new(),
                }))
            },
            |_, _| {
                std::future::ready(Err(anyhow::anyhow!(
                    "CAS response lost after request transmission"
                )))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, CycleOutcome::NeedsAttention);
        assert_delivery_unknown(&store, &launch, true);
    }

    #[tokio::test]
    async fn unresolved_post_kill_wait_is_bounded() {
        // A genuinely uninterruptible child is not portable or reliable in a
        // unit test; a pending future deterministically exercises the same
        // timeout decision that transfers the child to the detached reaper.
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            post_kill_wait(
                std::future::pending::<std::io::Result<ExitStatus>>(),
                Duration::from_millis(20),
            ),
        )
        .await
        .expect("bounded reap wait returned");
        assert!(matches!(outcome, PostKillWait::TimedOut));
    }

    #[test]
    fn reaper_initialization_failure_prevents_process_spawn() {
        let reaper = test_reaper();
        reaper.fail_next_start();
        let spawn_attempted = AtomicBool::new(false);

        let error = spawn_managed_child(reaper, || {
            spawn_attempted.store(true, Ordering::SeqCst);
            Err(std::io::Error::other("process spawn closure was called"))
        })
        .err()
        .expect("reaper initialization failed");

        assert!(error
            .to_string()
            .contains("injected reaper coordinator start failure"));
        assert!(!spawn_attempted.load(Ordering::SeqCst));
        assert_eq!(reaper.coordinator_start_count(), 0);
        assert!(!reaper.is_ready());
        assert!(reaper.is_idle());
    }

    #[tokio::test]
    async fn managed_child_drop_outside_runtime_recovers_poisoned_queue() {
        let reaper = test_reaper();
        let poisoned = std::panic::catch_unwind(|| reaper.poison_pending_for_test());
        assert!(poisoned.is_err());
        let child = spawn_managed_reap_child(reaper);
        let pid = child.id().unwrap();
        std::thread::spawn(move || {
            assert!(tokio::runtime::Handle::try_current().is_err());
            drop(child);
        })
        .join()
        .unwrap();

        assert_eq!(reaper.transfer_count(), 1);
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(pid));
        assert_eq!(reaper.coordinator_start_count(), 1);
    }

    #[tokio::test]
    async fn post_kill_wait_error_retains_child_for_reaping() {
        let reaper = test_reaper();
        let ready = reaper.ensure_ready().unwrap();
        let mut child = spawn_reap_child(ready);
        let pid = child.id().unwrap();
        let _ = child.start_kill();
        let outcome = post_kill_wait(
            std::future::ready(Err(std::io::Error::other("injected child wait error"))),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(outcome, PostKillWait::WaitError(_)));

        finish_post_kill_wait(child, outcome, ready);
        assert_eq!(reaper.transfer_count(), 1);
        wait_for_reaper_idle(reaper).await;
        assert!(!feanorfs_agent_core::lock::pid_alive(pid));
    }

    #[test]
    #[ignore]
    fn runner_reap_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(any(unix, target_os = "windows"))]
    #[test]
    #[ignore]
    fn runner_timeout_tree_helper() {
        let descendant_path = std::env::var_os("FEANORFS_RUNNER_DESCENDANT")
            .map(PathBuf::from)
            .expect("descendant pid path");
        let executable = std::env::current_exe().expect("test executable");
        let mut descendant = std::process::Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_timeout_descendant_helper",
                "--nocapture",
            ])
            .spawn()
            .expect("spawn descendant");
        std::fs::write(descendant_path, descendant.id().to_string())
            .expect("record descendant pid");
        std::thread::sleep(Duration::from_secs(30));
        let _ = descendant.wait();
    }

    #[cfg(any(unix, target_os = "windows"))]
    #[test]
    #[ignore]
    fn runner_timeout_descendant_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(any(unix, target_os = "windows"))]
    #[tokio::test]
    async fn timeout_kills_the_child_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let descendant_path = temp.path().join("descendant.pid");
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_timeout_tree_helper",
                "--nocapture",
            ])
            .env("FEANORFS_RUNNER_DESCENDANT", &descendant_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        process_tree::configure_process_group(&mut command).unwrap();
        let reaper = test_reaper();
        let mut child = spawn_managed_child(reaper, || command.spawn()).unwrap();
        // `configure_process_group` creates a suspended child on Windows so
        // adoption is atomic with respect to user code. These tests bypass
        // `run_configured_process`, so release the verified Job-owned child
        // explicitly before waiting for the helper's readiness marker.
        #[cfg(windows)]
        release_suspended_child(&child);
        let descendant = wait_for_descendant_pid(&descendant_path, Duration::from_secs(5)).await;
        let outcome = wait_for_child_until(
            &mut child,
            tokio::time::Instant::now() + Duration::from_millis(50),
            || Ok(false),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ProcessOutcome::Timeout);
        let dead_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while feanorfs_agent_core::lock::pid_alive(descendant)
            && tokio::time::Instant::now() < dead_deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!feanorfs_agent_core::lock::pid_alive(descendant));
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn runner_direct_exit_tree_helper() {
        let descendant_path = std::env::var_os("FEANORFS_RUNNER_DESCENDANT")
            .map(PathBuf::from)
            .expect("descendant pid path");
        let executable = std::env::current_exe().expect("test executable");
        let descendant = std::process::Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_timeout_descendant_helper",
                "--nocapture",
            ])
            .spawn()
            .expect("spawn descendant");
        std::fs::write(descendant_path, descendant.id().to_string())
            .expect("record descendant pid");
        // Returning without waiting exercises the direct-child-exit path; the
        // retained Job Object must still terminate the surviving descendant.
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn direct_child_exit_kills_job_owned_descendant() {
        let temp = tempfile::tempdir().unwrap();
        let descendant_path = temp.path().join("descendant.pid");
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "cli::agent_runner::tests::runner_direct_exit_tree_helper",
                "--nocapture",
            ])
            .env("FEANORFS_RUNNER_DESCENDANT", &descendant_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Windows children must enter the suspended/adopted production path
        // before the helper can spawn its surviving descendant. Without this
        // call the test would exercise an unsuspended process outside the Job
        // Object ownership protocol it is intended to verify.
        process_tree::configure_process_group(&mut command).unwrap();
        let reaper = test_reaper();
        let mut child = spawn_managed_child(reaper, || command.spawn()).unwrap();
        // See the timeout test above: direct test spawning bypasses the
        // production startup gate, so the adopted suspended process must be
        // released explicitly after Job membership is verified.
        release_suspended_child(&child);
        // Observe the descendant's readiness before waiting for the helper.
        // Otherwise the direct-child-exit cleanup can close the Job Object in
        // the small window between the helper exiting and its marker write.
        let descendant = wait_for_descendant_pid(&descendant_path, Duration::from_secs(5)).await;
        let outcome = wait_for_child_until(
            &mut child,
            tokio::time::Instant::now() + Duration::from_secs(5),
            || Ok(false),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ProcessOutcome::Exited);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while feanorfs_agent_core::lock::pid_alive(descendant)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!feanorfs_agent_core::lock::pid_alive(descendant));
    }
}
