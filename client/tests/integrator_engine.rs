//! Randomized integrator assignment and conflict-materialization integration
//! tests over a real HTTP hub: full dispatcher lifecycle, pre-acceptance
//! timeout fallback, no silent post-acceptance fallback, blocked fallback,
//! stale-reply safety, cross-machine conflict materialization, hub-storage
//! privacy, and project-path isolation.

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_agent_core::{inbox, send_message};
use feanorfs_client::{
    do_sync, integrator_assign, integrator_observe, integrator_resume, integrator_revoke,
    integrator_status, load_config, materialize_conflicts, save_config, IntegratorObserveOptions,
    SyncCtx,
};
use feanorfs_common::{
    encode_integrator_profile, parse_integrator_profile, AgentInboxQuery, AgentMessageInput,
    AgentMessageKind, IntegratorAssignInput, IntegratorCandidate, IntegratorDigest,
    IntegratorOutcomeState, IntegratorProfile, VerificationStatus, VerificationSummary,
};
use support::{
    read_workspace_file, spawn_test_client_with_server, spawn_test_server, write_workspace_file,
    TEST_PASSWORD, WORKSPACE_ID,
};

fn make_v3(client: &support::TestClient) -> feanorfs_client::Config {
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    config
}

fn candidate(name: &str) -> IntegratorCandidate {
    IntegratorCandidate {
        name: name.to_string(),
        capabilities: vec!["rust".to_string()],
        enabled: true,
        available: true,
    }
}

fn assign_input(about: &str, task: &str) -> IntegratorAssignInput {
    IntegratorAssignInput {
        about_snapshot: about.to_string(),
        candidates: vec![candidate("agent-a"), candidate("agent-b")],
        required_capabilities: vec!["rust".to_string()],
        conflict_authors: vec![],
        excluded: vec![],
        task_summary: task.to_string(),
        ack_timeout_ms: Some(300_000),
    }
}

fn digest(
    assignment_id: &str,
    integrator: &str,
    about: &str,
    state: IntegratorOutcomeState,
) -> IntegratorDigest {
    IntegratorDigest {
        assignment_id: assignment_id.to_string(),
        integrator: integrator.to_string(),
        about_snapshot: about.to_string(),
        inspected_snapshot: about.to_string(),
        state,
        landed_paths: 12,
        resolved_conflicts: 3,
        remaining_conflicts: 0,
        verification: VerificationSummary {
            status: VerificationStatus::Passed,
            summary: "84 tests passed".to_string(),
        },
        outcome: "Integrated parser implementation and tests.".to_string(),
        risks: vec![],
        decision_required: None,
    }
}

async fn seeded_v3_ctx(
    server: &support::TestServer,
    client: &support::TestClient,
) -> feanorfs_client::Config {
    let config = make_v3(client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_sync(
        &server.api,
        &client.db,
        client.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    config
}

fn ctx_from<'a>(
    server: &'a support::TestServer,
    client: &'a support::TestClient,
    config: &'a feanorfs_client::Config,
) -> SyncCtx<'a> {
    SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), config).unwrap()
}

/// One dispatcher offers, the selected integrator accepts, works, and replies
/// with a verified digest; the dispatcher completes the assignment.
#[tokio::test]
async fn dispatcher_lifecycle_completes_over_http_hub() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let assigned = integrator_assign(&ctx, assign_input(&head, "Integrate parser tests"))
        .await
        .unwrap();
    assert_eq!(assigned.attempt, 0);
    assert_eq!(
        assigned.state,
        feanorfs_common::IntegratorAssignmentState::Offered
    );
    assert!(assigned.fallback_order.len() == 1);

    // The selected integrator sees exactly one ffint1 assignment request.
    let selected_inbox = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: assigned.selected.clone(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(selected_inbox.messages.len(), 1);
    let request = &selected_inbox.messages[0];
    assert_eq!(request.kind, AgentMessageKind::Request);
    let profile = parse_integrator_profile(&request.body).unwrap();
    let IntegratorProfile::Assignment {
        assignment_id,
        attempt,
        selected,
        about_snapshot,
        roster_fingerprint,
        neutral_integrator,
        task,
    } = &profile
    else {
        panic!("assignment request must carry an ffint1 assignment profile");
    };
    assert_eq!(*assignment_id, assigned.assignment_id);
    assert_eq!(*attempt, 0);
    assert_eq!(selected, &assigned.selected);
    assert_eq!(*about_snapshot, head);
    assert_eq!(*roster_fingerprint, assigned.roster_fingerprint);
    assert!(*neutral_integrator);
    assert_eq!(task, "Integrate parser tests");
    // Assignment request has no reply_to.
    assert!(request.reply_to.is_none());

    // Integrator accepts with one status checkpoint.
    let accepted = encode_integrator_profile(&IntegratorProfile::Accepted {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Status,
            body: accepted,
            about_snapshot: Some(head.clone()),
            reply_to: Some(assigned.request_message_id.clone()),
            from: Some(assigned.selected.clone()),
        },
    )
    .await
    .unwrap();

    let observed = integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        observed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Accepted)
    );
    assert_eq!(observed.messages_processed, 1);
    let status = integrator_status(&ctx, Some(&assigned.assignment_id))
        .await
        .unwrap();
    assert_eq!(
        status.state,
        feanorfs_common::IntegratorAssignmentState::Accepted
    );

    // Integrator finishes with exactly one result tied to the request.
    let result = encode_integrator_profile(&IntegratorProfile::Result {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
        digest: digest(
            &assigned.assignment_id,
            &assigned.selected,
            &head,
            IntegratorOutcomeState::Completed,
        ),
    })
    .unwrap();
    let result_sent = send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Result,
            body: result,
            about_snapshot: Some(head.clone()),
            reply_to: Some(assigned.request_message_id.clone()),
            from: Some(assigned.selected.clone()),
        },
    )
    .await
    .unwrap();

    let completed = integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        completed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Completed)
    );
    assert_eq!(completed.action, "completed");
    let status = integrator_status(&ctx, Some(&assigned.assignment_id))
        .await
        .unwrap();
    assert_eq!(
        status.state,
        feanorfs_common::IntegratorAssignmentState::Completed
    );
    let digest = status
        .digest
        .expect("completed assignment keeps its digest");
    assert_eq!(
        digest.outcome,
        "Integrated parser implementation and tests."
    );
    assert_eq!(digest.verification.summary, "84 tests passed");
    assert_eq!(
        status.attempts[0].terminal_message_id.as_deref(),
        Some(result_sent.message_id.as_str()),
        "the terminal reply must be recorded in the audit trail"
    );
}

/// Accepted and Result may arrive before one dispatcher poll; causal staging
/// must apply both instead of losing the newer terminal message.
#[tokio::test]
async fn fast_acceptance_and_result_complete_in_one_observe_pass() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let assigned = integrator_assign(&ctx, assign_input(&head, "Integrate fast reply test"))
        .await
        .unwrap();

    for (kind, body) in [
        (
            AgentMessageKind::Status,
            encode_integrator_profile(&IntegratorProfile::Accepted {
                assignment_id: assigned.assignment_id.clone(),
                attempt: 0,
                about_snapshot: head.clone(),
            })
            .unwrap(),
        ),
        (
            AgentMessageKind::Result,
            encode_integrator_profile(&IntegratorProfile::Result {
                assignment_id: assigned.assignment_id.clone(),
                attempt: 0,
                about_snapshot: head.clone(),
                digest: digest(
                    &assigned.assignment_id,
                    &assigned.selected,
                    &head,
                    IntegratorOutcomeState::Completed,
                ),
            })
            .unwrap(),
        ),
    ] {
        send_message(
            &ctx,
            AgentMessageInput {
                to: "human".into(),
                kind,
                body,
                about_snapshot: Some(head.clone()),
                reply_to: Some(assigned.request_message_id.clone()),
                from: Some(assigned.selected.clone()),
            },
        )
        .await
        .unwrap();
    }

    let observed = integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        observed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Completed)
    );
    assert_eq!(observed.messages_processed, 2);
    assert_eq!(observed.action, "completed");
}

#[tokio::test]
async fn neutral_candidates_exclude_every_conflict_author_from_fallbacks() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let mut input = assign_input(&head, "Use only the neutral integrator");
    input.candidates.push(candidate("agent-c"));
    input.conflict_authors = vec!["agent-a".into(), "agent-b".into()];

    let assigned = integrator_assign(&ctx, input).await.unwrap();
    assert!(assigned.neutral_integrator);
    assert_eq!(assigned.selected, "agent-c");
    assert!(assigned.fallback_order.is_empty());
}

#[tokio::test]
async fn resume_adopts_a_published_unrecorded_request_without_duplication() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let assigned = integrator_assign(&ctx, assign_input(&head, "Recover published request"))
        .await
        .unwrap();

    let store = feanorfs_agent_core::IntegratorStore::open(client.workspace.path()).unwrap();
    store
        .update(|state| {
            let active = state.active.as_mut().unwrap();
            active.attempts.last_mut().unwrap().request_message_id = None;
            active.inbox_cursor = None;
            Ok(())
        })
        .unwrap();

    let resumed = integrator_resume(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(resumed.action, "recovered_offer");
    assert_eq!(
        resumed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Offered)
    );
    let candidate_inbox = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: assigned.selected,
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    let requests = candidate_inbox
        .messages
        .iter()
        .filter(|message| {
            matches!(
                parse_integrator_profile(&message.body),
                Some(IntegratorProfile::Assignment { ref assignment_id, .. })
                    if assignment_id == &assigned.assignment_id
            )
        })
        .count();
    assert_eq!(requests, 1, "resume must adopt rather than republish");
}

/// A missing acknowledgement advances to the next recorded candidate; the
/// timed-out attempt stays in the audit trail.
#[tokio::test]
async fn pre_acceptance_timeout_advances_to_next_candidate() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let mut input = assign_input(&head, "Integrate parser tests");
    input.ack_timeout_ms = Some(0);
    let assigned = integrator_assign(&ctx, input).await.unwrap();
    let first = assigned.selected.clone();
    let second = assigned.fallback_order[0].clone();

    let observed = integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(observed.action, "offered_next");

    let status = integrator_status(&ctx, Some(&assigned.assignment_id))
        .await
        .unwrap();
    assert_eq!(status.attempt, 1);
    assert_eq!(
        status.attempts[0].state,
        feanorfs_common::IntegratorAttemptState::TimedOut
    );
    assert_eq!(
        status.attempts[1].state,
        feanorfs_common::IntegratorAttemptState::Offered
    );
    assert_eq!(status.attempts[1].selected, second);

    // Only the second candidate holds an open assignment request.
    let second_inbox = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: second.clone(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(second_inbox.messages.len(), 1);
    let profile = parse_integrator_profile(&second_inbox.messages[0].body).unwrap();
    let IntegratorProfile::Assignment {
        attempt, selected, ..
    } = &profile
    else {
        panic!("expected assignment profile");
    };
    assert_eq!(*attempt, 1);
    assert_eq!(selected, &second);

    // The first candidate never accepted; no message went to it again.
    let first_inbox = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: first,
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(first_inbox.messages.len(), 1);
}

/// After acceptance, timeout alone must never activate a second integrator.
#[tokio::test]
async fn post_acceptance_timeout_never_silently_falls_back() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let assigned = integrator_assign(&ctx, assign_input(&head, "Integrate parser tests"))
        .await
        .unwrap();
    let other = assigned.fallback_order[0].clone();

    let accepted = encode_integrator_profile(&IntegratorProfile::Accepted {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Status,
            body: accepted,
            about_snapshot: Some(head.clone()),
            reply_to: Some(assigned.request_message_id.clone()),
            from: Some(assigned.selected.clone()),
        },
    )
    .await
    .unwrap();
    integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();

    let observed = integrator_observe(
        &ctx,
        IntegratorObserveOptions {
            ack_timeout_ms: Some(0),
            fallback_on_blocked: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        observed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Accepted),
        "a timeout after acceptance must not activate a fallback"
    );
    let other_inbox = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: other,
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert!(other_inbox.messages.is_empty());
}

/// A candidate-specific blocker with explicit dispatcher policy advances to
/// the next recorded candidate.
#[tokio::test]
async fn blocked_reply_with_fallback_policy_offers_next() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let assigned = integrator_assign(&ctx, assign_input(&head, "Integrate parser tests"))
        .await
        .unwrap();

    let blocked = encode_integrator_profile(&IntegratorProfile::Blocked {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
        reason: "Missing iOS toolchain".to_string(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Blocked,
            body: blocked,
            about_snapshot: Some(head.clone()),
            reply_to: Some(assigned.request_message_id.clone()),
            from: Some(assigned.selected.clone()),
        },
    )
    .await
    .unwrap();

    // Without the fallback policy the assignment stops at Blocked.
    let stopped = integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        stopped.state,
        Some(feanorfs_common::IntegratorAssignmentState::Blocked)
    );

    // The dispatcher may not have persisted a fallback decision; simulate a
    // fresh assignment for the fallback path.
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();
    let second = integrator_assign(&ctx, assign_input(&head, "Integrate parser tests"))
        .await
        .unwrap();
    let blocked2 = encode_integrator_profile(&IntegratorProfile::Blocked {
        assignment_id: second.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
        reason: "Missing iOS toolchain".to_string(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Blocked,
            body: blocked2,
            about_snapshot: Some(head.clone()),
            reply_to: Some(second.request_message_id.clone()),
            from: Some(second.selected.clone()),
        },
    )
    .await
    .unwrap();
    let fallback = integrator_observe(
        &ctx,
        IntegratorObserveOptions {
            ack_timeout_ms: None,
            fallback_on_blocked: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(fallback.action, "offered_next");
    let status = integrator_status(&ctx, Some(&second.assignment_id))
        .await
        .unwrap();
    assert_eq!(status.attempt, 1);
    assert_eq!(
        status.attempts[0].state,
        feanorfs_common::IntegratorAttemptState::Blocked
    );
    assert_eq!(
        status.attempts[1].state,
        feanorfs_common::IntegratorAttemptState::Offered
    );
}

/// A late acceptance after supersession is rejected and harmless.
#[tokio::test]
async fn late_acceptance_after_supersession_is_rejected_end_to_end() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let mut input = assign_input(&head, "Integrate parser tests");
    input.ack_timeout_ms = Some(0);
    let assigned = integrator_assign(&ctx, input).await.unwrap();
    let first = assigned.selected.clone();
    let second = assigned.fallback_order[0].clone();

    // Time out attempt 0, offer attempt 1.
    integrator_observe(
        &ctx,
        IntegratorObserveOptions {
            ack_timeout_ms: Some(0),
            fallback_on_blocked: false,
        },
    )
    .await
    .unwrap();

    // The superseded candidate accepts late.
    let late = encode_integrator_profile(&IntegratorProfile::Accepted {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Status,
            body: late,
            about_snapshot: Some(head.clone()),
            reply_to: Some(assigned.request_message_id.clone()),
            from: Some(first.clone()),
        },
    )
    .await
    .unwrap();

    let observed = integrator_observe(
        &ctx,
        IntegratorObserveOptions {
            ack_timeout_ms: Some(u64::MAX),
            fallback_on_blocked: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        observed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Offered),
        "late acceptance must not change the assignment"
    );
    let status = integrator_status(&ctx, Some(&assigned.assignment_id))
        .await
        .unwrap();
    assert_eq!(status.attempt, 1);
    assert_eq!(status.selected.as_deref(), Some(second.as_str()));

    // The second candidate can still accept cleanly.
    let accepted = encode_integrator_profile(&IntegratorProfile::Accepted {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 1,
        about_snapshot: head.clone(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Status,
            body: accepted,
            about_snapshot: Some(head.clone()),
            reply_to: status.attempts[1].request_message_id.clone(),
            from: Some(second),
        },
    )
    .await
    .unwrap();
    let observed = integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        observed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Accepted)
    );
}

/// The integrator on a third machine can materialize the authenticated
/// conflict triple without changing the head or the project directory, then
/// resolve explicitly with `conflicts keep --file`-style cloud choice.
#[tokio::test]
async fn conflict_materialization_is_portable_read_only_and_staleness_safe() {
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    let third = spawn_test_client_with_server(&server).await;
    for client in [&main, &second, &third] {
        make_v3(client);
    }

    // Shared base, then a land conflict published into the encrypted head.
    write_workspace_file(main.workspace.path(), "conflict.txt", b"base").await;
    do_sync(
        &server.api,
        &main.db,
        main.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    feanorfs_client::spawn_agent(
        main.workspace.path(),
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "portable-conflict",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    write_workspace_file(
        &feanorfs_agent_core::agent_dir(main.workspace.path(), "portable-conflict").unwrap(),
        "conflict.txt",
        b"agent edit",
    )
    .await;
    write_workspace_file(second.workspace.path(), "conflict.txt", b"folder edit").await;
    do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        WORKSPACE_ID,
        Some(TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    let landed = feanorfs_client::land_agent(
        main.workspace.path(),
        &main.db,
        &server.api,
        WORKSPACE_ID,
        "portable-conflict",
        Some(TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(landed.conflicts.len(), 1);
    let head = landed
        .snapshot_id
        .expect("land publishes a conflict snapshot");
    let head_before = head.clone();

    let third_config = load_config(third.workspace.path()).unwrap();
    let third_ctx = SyncCtx::from_config(
        &server.api,
        &third.db,
        third.workspace.path(),
        &third_config,
    )
    .unwrap();

    // Third machine materializes the triple from the encrypted head.
    let materialized = materialize_conflicts(&third_ctx, &head, &[]).await.unwrap();
    assert_eq!(materialized.entries.len(), 1);
    let entry = &materialized.entries[0];
    assert_eq!(entry.path, "conflict.txt");
    assert_eq!(entry.kind, feanorfs_common::ConflictKind::EditEdit);
    assert!(entry.original_available && entry.local_available && entry.cloud_available);
    assert!(!entry.already_materialized);

    let dir = std::path::Path::new(&materialized.conflict_dir);
    assert_eq!(
        std::fs::read(dir.join("conflict.txt.original")).unwrap(),
        b"base"
    );
    assert_eq!(
        std::fs::read(dir.join("conflict.txt.local")).unwrap(),
        b"agent edit"
    );
    assert_eq!(
        std::fs::read(dir.join("conflict.txt.cloud")).unwrap(),
        b"folder edit"
    );
    // Materialization is read-only: the head and the project directory are
    // untouched, and a pending row was registered locally for resolution.
    assert_eq!(
        server.api.get_head(WORKSPACE_ID).await.unwrap().unwrap(),
        head_before
    );
    assert_eq!(
        third.db.list_pending_conflict_paths().await.unwrap(),
        vec!["conflict.txt"]
    );
    assert_eq!(
        std::fs::read_dir(third.workspace.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .count(),
        0,
        "materialization must not create project files"
    );

    // Idempotent re-materialization reuses the pending row.
    let again = materialize_conflicts(&third_ctx, &head_before, &[])
        .await
        .unwrap();
    assert!(again.entries[0].already_materialized);

    // The integrator resolves explicitly with the mirror version; the head
    // advances and the conflict is gone.
    feanorfs_client::resolve_conflict(
        &third_ctx,
        "conflict.txt",
        feanorfs_client::ResolveKeep::Cloud,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        read_workspace_file(third.workspace.path(), "conflict.txt").await,
        b"folder edit"
    );
    assert!(third
        .db
        .list_pending_conflict_paths()
        .await
        .unwrap()
        .is_empty());

    // Stale materialization against the pre-resolution snapshot is refused.
    let stale =
        materialize_conflicts(&third_ctx, &head_before, &["conflict.txt".to_string()]).await;
    assert!(
        stale.is_err(),
        "materializing a resolved conflict must fail closed: {stale:?}"
    );
    assert!(stale.unwrap_err().to_string().contains("already resolved"));
}

#[tokio::test]
async fn hub_storage_contains_no_plaintext_assignment_traffic() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let task = "Integrate parser implementation and tests 9f3c7a1e";
    let assigned = integrator_assign(&ctx, assign_input(&head, task))
        .await
        .unwrap();
    let accepted = encode_integrator_profile(&IntegratorProfile::Accepted {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Status,
            body: accepted,
            about_snapshot: Some(head.clone()),
            reply_to: Some(assigned.request_message_id.clone()),
            from: Some(assigned.selected.clone()),
        },
    )
    .await
    .unwrap();
    integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();

    let mut files = Vec::new();
    collect_files(server.data_dir(), &mut files);
    assert!(!files.is_empty(), "hub storage must exist");
    for file in &files {
        let bytes = std::fs::read(file).unwrap();
        for needle in [
            task,
            "ffint1",
            "agent-a",
            "agent-b",
            "IntegratorAssignResult",
            "feanorfs-integrator-selection-v1",
        ] {
            assert!(
                !contains_bytes(&bytes, needle.as_bytes()),
                "hub storage leaked plaintext {needle:?} in {}",
                file.display()
            );
        }
    }
}

/// Resume after a restart observes the existing request without duplicating
/// it and can complete the lifecycle from durable state.
#[tokio::test]
async fn resume_after_restart_completes_without_duplicate_request() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let assigned = integrator_assign(&ctx, assign_input(&head, "Integrate parser tests"))
        .await
        .unwrap();

    // A fresh observe (simulating a restarted dispatcher) must not re-send.
    let resumed = integrator_resume(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(resumed.action, "none");

    let selected_inbox = inbox(
        &ctx,
        AgentInboxQuery {
            recipient: assigned.selected.clone(),
            after: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        selected_inbox.messages.len(),
        1,
        "resume must never duplicate an assignment request"
    );

    // The lifecycle still completes through resume.
    let accepted = encode_integrator_profile(&IntegratorProfile::Accepted {
        assignment_id: assigned.assignment_id.clone(),
        attempt: 0,
        about_snapshot: head.clone(),
    })
    .unwrap();
    send_message(
        &ctx,
        AgentMessageInput {
            to: "human".to_string(),
            kind: AgentMessageKind::Status,
            body: accepted,
            about_snapshot: Some(head.clone()),
            reply_to: Some(assigned.request_message_id.clone()),
            from: Some(assigned.selected.clone()),
        },
    )
    .await
    .unwrap();
    let resumed = integrator_resume(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        resumed.state,
        Some(feanorfs_common::IntegratorAssignmentState::Accepted)
    );
}

/// Two sequential batches on one dispatcher work; a second active assignment
/// fails closed until the first completes.
#[tokio::test]
async fn second_active_assignment_fails_closed() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let first = integrator_assign(&ctx, assign_input(&head, "batch one"))
        .await
        .unwrap();
    let second = integrator_assign(&ctx, assign_input(&head, "batch two")).await;
    assert!(
        second.is_err(),
        "a second active assignment must fail closed: {second:?}"
    );
    assert!(second.unwrap_err().to_string().contains("already active"));

    let revoked = integrator_revoke(&ctx, &first.assignment_id, "cancelled by test")
        .await
        .unwrap();
    assert_eq!(
        revoked.state,
        feanorfs_common::IntegratorAssignmentState::Cancelled
    );

    // After revocation, a fresh batch is allowed.
    let next = integrator_assign(&ctx, assign_input(&head, "batch three"))
        .await
        .unwrap();
    assert_eq!(next.attempt, 0);
}

/// A cursor reset fails closed into `requires_human`; the dispatcher can then
/// explicitly revoke to recover the workspace for the next batch.
#[tokio::test]
async fn cursor_reset_fails_closed_and_revocable() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = seeded_v3_ctx(&server, &client).await;
    let ctx = ctx_from(&server, &client, &config);
    let head = ctx.api.get_head(WORKSPACE_ID).await.unwrap().unwrap();

    let assigned = integrator_assign(&ctx, assign_input(&head, "Integrate parser tests"))
        .await
        .unwrap();

    // Corrupt the persisted cursor so the next observation cannot prove the
    // graph delta (simulates lost dispatcher state).
    let state_dir = feanorfs_agent_core::ensure_workspace_state(client.workspace.path()).unwrap();
    let state_path = state_dir.join("orchestrator").join("integrator-state.json");
    let raw = std::fs::read_to_string(&state_path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    value["active"]["inbox_cursor"] = serde_json::Value::String("f".repeat(64));
    std::fs::write(&state_path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    let observed = integrator_observe(&ctx, IntegratorObserveOptions::default())
        .await
        .unwrap();
    assert!(observed.cursor_reset);
    assert_eq!(
        observed.state,
        Some(feanorfs_common::IntegratorAssignmentState::RequiresHuman)
    );

    // Automatic mutation stops: no new offer, no completion.
    let status = integrator_status(&ctx, Some(&assigned.assignment_id))
        .await
        .unwrap();
    assert_eq!(
        status.state,
        feanorfs_common::IntegratorAssignmentState::RequiresHuman
    );

    // Explicit revocation recovers the workspace for the next batch.
    let revoked = integrator_revoke(
        &ctx,
        &assigned.assignment_id,
        "cursor reset; dispatcher recovered by operator",
    )
    .await
    .unwrap();
    assert_eq!(
        revoked.state,
        feanorfs_common::IntegratorAssignmentState::Cancelled
    );
    let next = integrator_assign(&ctx, assign_input(&head, "batch two"))
        .await
        .unwrap();
    assert_eq!(next.attempt, 0);
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
