//! Work-intent protocol integration tests over a real HTTP hub: full
//! propose -> decide -> settle -> complete lifecycle through the engine and
//! reducer, send-never-mutates semantics, unauthorized coordinator evidence,
//! duplicate-delivery idempotency, and projection completeness.

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::{do_push_only, load_config, save_config, SyncCtx};
use feanorfs_common::{
    WorkDecisionAccept, WorkDecisionKind, WorkProposeInput, WorkSettleInput, WorkStatusInput,
    WorkVerification, WorkVerificationStatus,
};
use support::{
    spawn_test_client_with_server, spawn_test_server, write_workspace_file, TEST_PASSWORD,
};

fn make_v3(client: &support::TestClient) -> feanorfs_client::Config {
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    config
}

fn hex64(byte: u8) -> String {
    std::iter::repeat_n(byte as char, 64).collect()
}

#[tokio::test]
async fn full_lifecycle_propose_decide_settle_complete_via_engine_and_reducer() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        support::WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();

    // Sends never mutate the local projection.
    let before = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    assert!(before.tasks.is_empty());

    let proposed = feanorfs_agent_core::work_propose(
        &ctx,
        WorkProposeInput {
            task_id: "parser-impl".to_string(),
            agent: Some("linux-dev".to_string()),
            sequence: 1,
            causal_base: None,
            coordinator: Some("human".to_string()),
            paths: vec!["src/parser.rs".to_string(), "tests/parser.rs".to_string()],
            concerns: vec!["parser behavior".to_string()],
            dependencies: vec![],
            capabilities: vec!["rust".to_string()],
            about_snapshot: None,
            to: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(proposed.state, feanorfs_common::WorkTaskState::Proposed);
    assert_eq!(proposed.task_id, "parser-impl");
    assert_eq!(proposed.profile, "work_intent");
    let proposal_id = proposed.message_id.clone();
    assert_eq!(proposal_id.len(), 64);

    // The proposal is not accepted until the reducer observes a decision.
    let observed = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    assert_eq!(observed.tasks.len(), 1);
    assert_eq!(
        observed.tasks[0].state,
        feanorfs_common::WorkTaskState::Proposed
    );
    assert_eq!(
        observed.tasks[0].proposals[0].intent_message_id,
        proposal_id
    );

    // Unauthorized coordinator decision is retained as evidence and never
    // changes state.
    let unauthorized = feanorfs_agent_core::work_decide(
        &ctx,
        feanorfs_common::WorkDecideInput {
            proposal_message_id: proposal_id.clone(),
            kind: WorkDecisionKind::Accept(WorkDecisionAccept { reason: None }),
            about_snapshot: None,
            to: None,
            from: Some("intruder".to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(unauthorized.message_id.len(), 64);
    let after_unauthorized = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    assert_eq!(
        after_unauthorized.tasks[0].proposals[0].state,
        feanorfs_common::WorkTaskState::Proposed,
        "unauthorized decisions never change accepted state"
    );
    assert_eq!(after_unauthorized.evidence_count, 1);

    // Authorized decision (coordinator "human" named by the proposal) applies.
    let decided = feanorfs_agent_core::work_decide(
        &ctx,
        feanorfs_common::WorkDecideInput {
            proposal_message_id: proposal_id.clone(),
            kind: WorkDecisionKind::Accept(WorkDecisionAccept { reason: None }),
            about_snapshot: None,
            to: None,
            from: Some("human".to_string()),
        },
    )
    .await
    .unwrap();
    let decision_id = decided.message_id.clone();

    let accepted = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    let proposal = &accepted.tasks[0].proposals[0];
    assert_eq!(proposal.state, feanorfs_common::WorkTaskState::Accepted);
    assert_eq!(proposal.decision.as_ref().unwrap().message_id, decision_id);
    assert_eq!(proposal.accepted_scope.paths.len(), 2);
    assert!(!accepted.projection_incomplete);
    assert_eq!(accepted.messages_processed, 1);

    // A second, distinct decision message for an already-accepted proposal is
    // retained as invalid evidence and never re-applies (true duplicate
    // delivery of the *same* message id is idempotent at the reducer level).
    feanorfs_agent_core::work_decide(
        &ctx,
        feanorfs_common::WorkDecideInput {
            proposal_message_id: proposal_id.clone(),
            kind: WorkDecisionKind::Accept(WorkDecisionAccept { reason: None }),
            about_snapshot: None,
            to: None,
            from: Some("human".to_string()),
        },
    )
    .await
    .unwrap();
    let after_duplicate = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    assert_eq!(
        after_duplicate.tasks[0].proposals[0].state,
        feanorfs_common::WorkTaskState::Accepted
    );
    assert_eq!(
        after_duplicate.evidence_count, 2,
        "second decision is evidence"
    );

    // Settle with verification, then complete.
    feanorfs_agent_core::work_settle(
        &ctx,
        WorkSettleInput {
            task_id: "parser-impl".to_string(),
            intent_message_id: proposal_id.clone(),
            sequence: 2,
            inspected_snapshot: hex64(b'd'),
            verification: WorkVerification {
                status: WorkVerificationStatus::Passed,
                summary: "84 tests passed".to_string(),
            },
            about_snapshot: None,
            to: None,
            from: Some("linux-dev".to_string()),
        },
    )
    .await
    .unwrap();
    let settled = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    assert_eq!(
        settled.tasks[0].state,
        feanorfs_common::WorkTaskState::Settled
    );
    assert_eq!(
        settled.tasks[0].proposals[0]
            .verification
            .as_ref()
            .unwrap()
            .summary,
        "84 tests passed"
    );

    feanorfs_agent_core::work_complete(
        &ctx,
        feanorfs_common::WorkCompleteInput {
            task_id: "parser-impl".to_string(),
            intent_message_id: proposal_id.clone(),
            sequence: 3,
            outcome: "Parser implemented and verified.".to_string(),
            about_snapshot: None,
            to: None,
            from: Some("linux-dev".to_string()),
        },
    )
    .await
    .unwrap();
    let completed = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    assert_eq!(
        completed.tasks[0].state,
        feanorfs_common::WorkTaskState::Completed
    );
    assert_eq!(
        completed.tasks[0].proposals[0].outcome.as_deref(),
        Some("Parser implemented and verified.")
    );
    assert!(!completed.projection_incomplete);

    // The proposal and decision ids surface exactly in the projection.
    let proposal_status = &completed.tasks[0].proposals[0];
    assert_eq!(proposal_status.intent_message_id, proposal_id);
    assert_eq!(proposal_status.source_message_id.len(), 64);
    assert!(proposal_status.causal_refs.is_empty());
}

#[tokio::test]
async fn work_state_is_private_rebuildable_and_schema_versioned() {
    let server = spawn_test_server().await;
    let client = spawn_test_client_with_server(&server).await;
    let config = make_v3(&client);
    write_workspace_file(client.workspace.path(), "seed.txt", b"seed").await;
    do_push_only(
        &server.api,
        &client.db,
        client.workspace.path(),
        support::WORKSPACE_ID,
        Some(TEST_PASSWORD),
    )
    .await
    .unwrap();
    let ctx =
        SyncCtx::from_config(&server.api, &client.db, client.workspace.path(), &config).unwrap();

    feanorfs_agent_core::work_propose(
        &ctx,
        WorkProposeInput {
            task_id: "lexer-impl".to_string(),
            agent: Some("mac-test".to_string()),
            sequence: 1,
            causal_base: None,
            coordinator: None,
            paths: vec!["src/lexer.rs".to_string()],
            concerns: vec![],
            dependencies: vec![],
            capabilities: vec![],
            about_snapshot: None,
            to: None,
        },
    )
    .await
    .unwrap();
    let observed = feanorfs_agent_core::work_status(&ctx, WorkStatusInput::default())
        .await
        .unwrap();
    assert_eq!(observed.tasks.len(), 1);

    // The projection lives under the protected orchestrator boundary, not in
    // project files.
    let orchestrator = feanorfs_agent_core::ensure_workspace_state(client.workspace.path())
        .unwrap()
        .join("orchestrator");
    let state_path = orchestrator.join("work-state.json");
    assert!(state_path.exists(), "work-state.json must exist");
    let contents = std::fs::read_to_string(&state_path).unwrap();
    assert!(contents.contains("\"schema_version\": 1"));
    assert!(!client.workspace.path().join("work-state.json").exists());
}
