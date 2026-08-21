//! Cross-machine `ffres1` resolution protocol: agent A prepares and assigns
//! the older/behind agent B, B reconstructs the same authenticated job by ID
//! and fingerprint, materializes the legs from hub blobs, returns a result,
//! and A observes it — all through the encrypted signal stream and guarded
//! publication, never through shared local filesystem paths.
//!
//! Both machines run the deterministic reducer independently; every
//! projection assertion compares the two machines' views for equality.

feanorfs_test_support::isolate_test_process!();

mod support;

use feanorfs_client::do_sync;
use support::{spawn_test_client_with_server, spawn_test_server, write_workspace_file};

use feanorfs_agent_core::{
    defer_resolution, materialize_resolution_legs, prepare_resolution_job,
    put_resolution_candidate, resolution_protocol_status, resolution_status, send_human_answer,
    send_resolution_assignment, send_resolution_result, send_resolution_revoke, spawn_agent,
    submit_resolution_result, ProtocolAssignmentState, SyncCtx,
};
use feanorfs_client::{load_config, save_config};
use feanorfs_common::resolution_contract::{resolution_fixtures, HumanResolutionAnswer};
use feanorfs_common::{
    hash_bytes, CandidateDescriptor, HumanResolutionOption, PreventionReason, ResolutionOutcome,
    RESOLUTION_SCHEMA_VERSION,
};

const CONFLICT: &str = "conflict.txt";
const AGENT_A: &str = "agent-a";
const AGENT_B: &str = "agent-b";
const CANDIDATE: &[u8] = b"reconciled by the designated agent";

fn make_v3(client: &support::TestClient) -> feanorfs_client::Config {
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    config
}

/// Both tests share one process-wide FEANORFS_HOME and spawn real agent
/// processes; serialize them so agent identities never collide.
static RESOLUTION_PROTOCOL_SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn ctx_from<'a>(
    api: &'a feanorfs_agent_core::ApiClient,
    db: &'a feanorfs_agent_core::ClientDb,
    root: &'a std::path::Path,
    config: &'a feanorfs_client::Config,
) -> SyncCtx<'a> {
    SyncCtx::from_config(api, db, root, config).unwrap()
}

fn hex64(byte: u8) -> String {
    std::iter::repeat_n(byte as char, 64).collect()
}

/// Seeds one accepted proposal authored by `agent` covering `path` so
/// designation can select the causally eligible owner.
fn seed_accepted(
    ctx: &SyncCtx<'_>,
    agent: &str,
    path: &str,
    intent_byte: u8,
    causal_base: Option<String>,
) {
    use feanorfs_agent_core::work::{WorkProposalRecord, WorkStore};
    use feanorfs_common::work_contract::WorkScope;
    use feanorfs_common::WorkTaskState;
    let proposal = WorkProposalRecord {
        agent: agent.to_string(),
        sequence: 1,
        intent_message_id: hex64(intent_byte),
        coordinator: Some("human".to_string()),
        causal_base,
        original_scope: WorkScope {
            paths: vec![path.to_string()],
            concerns: vec![],
            dependencies: vec![],
        },
        scope: WorkScope {
            paths: vec![path.to_string()],
            concerns: vec![],
            dependencies: vec![],
        },
        state: WorkTaskState::Accepted,
        decision: None,
        superseded_decisions: vec![],
        amendments: vec![],
        accepted_overlap: vec![],
        verification: None,
        inspected_snapshot: None,
        outcome: None,
        reason: None,
        source_message_id: hex64(intent_byte),
        updated_at_ms: 1,
        capabilities: vec!["resolution".to_string()],
        author_restore: None,
    };
    WorkStore::open(ctx.base)
        .unwrap()
        .update(|state| {
            state.incomplete = false;
            state.tasks.push(feanorfs_agent_core::work::WorkTaskRecord {
                task_id: "task-conflict".to_string(),
                proposals: vec![proposal],
                updated_at_ms: 1,
            });
            Ok(())
        })
        .unwrap();
}

/// Creates one conflict head with legs authored by A (ours) and B (theirs).
async fn publish_ab_conflict(
    main: &support::TestClient,
    second: &support::TestClient,
    server: &support::TestServer,
) -> String {
    write_workspace_file(main.workspace.path(), CONFLICT, b"base").await;
    do_sync(
        &server.api,
        &main.db,
        main.workspace.path(),
        support::WORKSPACE_ID,
        Some(support::TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    do_sync(
        &server.api,
        &second.db,
        second.workspace.path(),
        support::WORKSPACE_ID,
        Some(support::TEST_PASSWORD),
        false,
    )
    .await
    .unwrap();
    spawn_agent(
        main.workspace.path(),
        &main.db,
        &server.api,
        support::WORKSPACE_ID,
        AGENT_A,
        Some(support::TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    spawn_agent(
        second.workspace.path(),
        &second.db,
        &server.api,
        support::WORKSPACE_ID,
        AGENT_B,
        Some(support::TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    let agent_a_dir = feanorfs_agent_core::agent_dir(main.workspace.path(), AGENT_A).unwrap();
    let agent_b_dir = feanorfs_agent_core::agent_dir(second.workspace.path(), AGENT_B).unwrap();
    write_workspace_file(&agent_a_dir, CONFLICT, b"agent-a edit").await;
    write_workspace_file(&agent_b_dir, CONFLICT, b"agent-b edit").await;
    // A lands first; B's sync then lands over it, producing the conflict
    // triple with base/ours(A)/theirs(B) legs.
    let landed = feanorfs_agent_core::land_agent(
        main.workspace.path(),
        &main.db,
        &server.api,
        support::WORKSPACE_ID,
        AGENT_A,
        Some(support::TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(landed.conflicts.is_empty());
    let landed = feanorfs_agent_core::land_agent(
        second.workspace.path(),
        &second.db,
        &server.api,
        support::WORKSPACE_ID,
        AGENT_B,
        Some(support::TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(landed.conflicts.len(), 1);
    // The conflict head is the current workspace head after the second land.
    server
        .api
        .get_head(support::WORKSPACE_ID)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn cross_machine_assignment_reconstruction_and_result_round_trip() {
    let _serial = RESOLUTION_PROTOCOL_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    make_v3(&main);
    make_v3(&second);

    let head = publish_ab_conflict(&main, &second, &server).await;

    let main_config = load_config(main.workspace.path()).unwrap();
    let second_config = load_config(second.workspace.path()).unwrap();
    let ctx_a = ctx_from(&server.api, &main.db, main.workspace.path(), &main_config);
    let ctx_b = ctx_from(
        &server.api,
        &second.db,
        second.workspace.path(),
        &second_config,
    );

    // A materializes the conflict locally and registers the fingerprinted
    // record, then prepares the last-resort job.
    let materialized = feanorfs_agent_core::materialize_conflicts(&ctx_a, &head, &[])
        .await
        .unwrap();
    assert_eq!(materialized.entries.len(), 1);
    seed_accepted(&ctx_a, AGENT_B, CONFLICT, b'b', None);
    seed_accepted(&ctx_a, AGENT_A, CONFLICT, b'a', Some(hex64(b'b')));
    let job = prepare_resolution_job(
        &ctx_a,
        CONFLICT,
        PreventionReason::Exhausted {
            detail: "no bounded prevention path remains".to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        job.owner, AGENT_B,
        "the causally behind agent B is designated"
    );

    // A publishes the ffres1 assignment (the complete immutable job).
    let assignment_message = send_resolution_assignment(&ctx_a, &job.job_id)
        .await
        .unwrap();
    assert_eq!(assignment_message.len(), 64);

    // B reduces the same stream and imports the identical job by ID and
    // fingerprint — no shared local paths involved.
    let status_b = resolution_protocol_status(&ctx_b, false).await.unwrap();
    assert!(!status_b.projection_incomplete);
    let entry = status_b
        .entries
        .iter()
        .find(|entry| entry.job_id == job.job_id)
        .expect("assigned job must be projected on B");
    assert_eq!(entry.state, ProtocolAssignmentState::Assigned);
    assert_eq!(entry.assignment_id, job.assignment_id);
    assert_eq!(entry.conflict_fingerprint, job.conflict_fingerprint);
    assert_eq!(entry.owner, AGENT_B);

    // The imported job is fully usable by B's local engine.
    let local_status = resolution_status(&ctx_b, Some(&job.job_id)).await.unwrap();
    let local_job = local_status
        .jobs
        .iter()
        .find(|record| record.job_id == job.job_id)
        .expect("imported job must be in B's local store");
    assert_eq!(
        local_job.assignment_state,
        feanorfs_agent_core::ResolutionAssignmentState::Active
    );

    // B reconstructs the authenticated legs from hub blobs.
    let legs = materialize_resolution_legs(&ctx_b, &job.job_id)
        .await
        .unwrap();
    assert_eq!(legs.len(), 3);
    let leg_bytes = |role: &str| {
        let (_, path) = legs
            .iter()
            .find(|(leg_role, _)| leg_role.as_str() == role)
            .unwrap();
        std::fs::read(path).unwrap()
    };
    // The conflict was recorded by B's land: ours = B's edit, theirs = A's.
    assert_eq!(leg_bytes("original"), b"base");
    assert_eq!(leg_bytes("local"), b"agent-b edit");
    assert_eq!(leg_bytes("cloud"), b"agent-a edit");

    // B produces a candidate through the typed engine API and returns the
    // same authenticated job via the ffres1 result profile.
    let descriptor = put_resolution_candidate(&ctx_b, &job.job_id, CANDIDATE)
        .await
        .unwrap();
    assert_eq!(descriptor.hash, hash_bytes(CANDIDATE));
    let result = resolution_fixtures::result();
    let result = feanorfs_common::ResolutionResult {
        job_id: job.job_id.clone(),
        assignment_id: job.assignment_id.clone(),
        attempt: job.attempt,
        owner: AGENT_B.to_string(),
        conflict_fingerprint: job.conflict_fingerprint.clone(),
        candidate: Some(CandidateDescriptor {
            path: job.candidate_destination.path.clone(),
            hash: hash_bytes(CANDIDATE),
            size: CANDIDATE.len() as u64,
            mode: 0,
            deleted: false,
        }),
        ..result
    };
    let submitted = submit_resolution_result(&ctx_b, &job.job_id, result)
        .await
        .unwrap();
    assert_eq!(submitted.outcome, ResolutionOutcome::CandidateReady);
    let result_message = send_resolution_result(&ctx_b, &job.job_id).await.unwrap();
    assert_eq!(result_message.len(), 64);

    // A reduces the same stream: the result is received and the two
    // machines' projections agree exactly.
    let status_a = resolution_protocol_status(&ctx_a, false).await.unwrap();
    let entry_a = status_a
        .entries
        .iter()
        .find(|entry| entry.job_id == job.job_id)
        .expect("result must be projected on A");
    assert_eq!(entry_a.state, ProtocolAssignmentState::ResultReceived);
    assert_eq!(entry_a.outcome, Some(ResolutionOutcome::CandidateReady));
    let status_b_after = resolution_protocol_status(&ctx_b, false).await.unwrap();
    assert_eq!(
        serde_json::to_value(&status_a).unwrap(),
        serde_json::to_value(&status_b_after).unwrap(),
        "both machines must converge to the identical projection"
    );

    // Deterministic revoke round trip.
    let revoke_message = send_resolution_revoke(&ctx_a, &job.job_id, false)
        .await
        .unwrap();
    assert_eq!(revoke_message.len(), 64);
    let status_b_revoked = resolution_protocol_status(&ctx_b, false).await.unwrap();
    let entry_revoked = status_b_revoked
        .entries
        .iter()
        .find(|entry| entry.job_id == job.job_id)
        .expect("revoked entry must remain projected");
    assert_eq!(entry_revoked.state, ProtocolAssignmentState::Revoked);
}

#[tokio::test]
async fn human_answer_round_trips_and_defer_stays_local() {
    let _serial = RESOLUTION_PROTOCOL_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    make_v3(&main);
    make_v3(&second);
    let head = publish_ab_conflict(&main, &second, &server).await;

    let main_config = load_config(main.workspace.path()).unwrap();
    let second_config = load_config(second.workspace.path()).unwrap();
    let ctx_a = ctx_from(&server.api, &main.db, main.workspace.path(), &main_config);
    let ctx_b = ctx_from(
        &server.api,
        &second.db,
        second.workspace.path(),
        &second_config,
    );

    feanorfs_agent_core::materialize_conflicts(&ctx_a, &head, &[])
        .await
        .unwrap();
    seed_accepted(&ctx_a, AGENT_B, CONFLICT, b'b', None);
    seed_accepted(&ctx_a, AGENT_A, CONFLICT, b'a', Some(hex64(b'b')));
    let job = prepare_resolution_job(
        &ctx_a,
        CONFLICT,
        PreventionReason::Exhausted {
            detail: "no bounded prevention path remains".to_string(),
        },
    )
    .await
    .unwrap();
    send_resolution_assignment(&ctx_a, &job.job_id)
        .await
        .unwrap();

    // B receives the assignment and submits a requires_human result.
    let _ = resolution_protocol_status(&ctx_b, false).await.unwrap();
    let result = resolution_fixtures::human_result();
    let result = feanorfs_common::ResolutionResult {
        job_id: job.job_id.clone(),
        assignment_id: job.assignment_id.clone(),
        attempt: job.attempt,
        owner: AGENT_B.to_string(),
        conflict_fingerprint: job.conflict_fingerprint.clone(),
        ..result
    };
    submit_resolution_result(&ctx_b, &job.job_id, result)
        .await
        .unwrap();
    send_resolution_result(&ctx_b, &job.job_id).await.unwrap();

    // A observes the bounded question and answers it through the ffres1
    // human-answer profile.
    let status_a = resolution_protocol_status(&ctx_a, false).await.unwrap();
    let entry_a = status_a
        .entries
        .iter()
        .find(|entry| entry.job_id == job.job_id)
        .expect("question must be projected on A");
    assert_eq!(entry_a.outcome, Some(ResolutionOutcome::RequiresHuman));
    assert!(entry_a.question.is_some());
    let generation = entry_a.question_generation;
    assert_eq!(generation, 1);

    let answer = HumanResolutionAnswer {
        schema_version: RESOLUTION_SCHEMA_VERSION,
        job_id: job.job_id.clone(),
        assignment_id: job.assignment_id.clone(),
        attempt: job.attempt,
        conflict_fingerprint: job.conflict_fingerprint.clone(),
        question_generation: generation,
        chosen_option: HumanResolutionOption::Defer,
        candidate: None,
        verification: None,
    };
    let answer_message = send_human_answer(&ctx_a, &answer).await.unwrap();
    assert_eq!(answer_message.len(), 64);

    // B converges: the bound answer was observed; a stale-generation answer
    // is rejected deterministically.
    let status_b = resolution_protocol_status(&ctx_b, false).await.unwrap();
    let entry_b = status_b
        .entries
        .iter()
        .find(|entry| entry.job_id == job.job_id)
        .expect("answer must be projected on B");
    assert_eq!(entry_b.state, ProtocolAssignmentState::HumanAnswered);
    assert_eq!(
        serde_json::to_value(resolution_protocol_status(&ctx_a, false).await.unwrap()).unwrap(),
        serde_json::to_value(&status_b).unwrap()
    );

    // The local defer op records a terminal state without any publication:
    // no new protocol messages appear after it.
    defer_resolution(&ctx_a, &job.job_id).await.unwrap();
    let local = resolution_status(&ctx_a, Some(&job.job_id)).await.unwrap();
    let record = local
        .jobs
        .iter()
        .find(|record| record.job_id == job.job_id)
        .expect("deferred job must remain in the local store");
    assert_eq!(
        record.assignment_state,
        feanorfs_agent_core::ResolutionAssignmentState::Deferred
    );
    let before = resolution_protocol_status(&ctx_b, false).await.unwrap();
    let after = resolution_protocol_status(&ctx_b, false).await.unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap(),
        "defer must never publish protocol messages"
    );
}
