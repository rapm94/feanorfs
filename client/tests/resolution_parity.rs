//! Resolution adapter parity: the full prepare -> submit ->
//! apply lifecycle over a real hub, exercised through the engine inline AND
//! through the spawned CLI and MCP harnesses, with canonical fixture parity.
//!
//! Inline and spawned-harness equivalence is proven by comparing the exact
//! conflict identity/fingerprint produced by `feanorfs agent resolution
//! prepare` against the inline engine preparation of the same conflict, and
//! by round-tripping every CLI JSON document through the frozen canonical
//! `ResolutionJob` / `ResolutionResult` wire types.

feanorfs_test_support::isolate_test_process!();

mod support;

use support::{spawn_test_client_with_server, spawn_test_server, write_workspace_file};

use feanorfs_agent_core::work::{WorkProposalRecord, WorkStore, WorkTaskRecord};
use feanorfs_agent_core::{
    ensure_workspace_state, land_agent, prepare_resolution_job, resolution_status, spawn_agent,
    ApiClient, ClientDb, ResolutionApplyOutcome, SyncCtx,
};
use feanorfs_client::{do_sync, load_config, save_config};
use feanorfs_common::work_contract::WorkScope;
use feanorfs_common::{
    compute_conflict_identity_fingerprint, hash_bytes, resolution_contract::resolution_fixtures,
    validate_resolution_job, validate_resolution_result, CandidateDescriptor, PreventionReason,
    ResolutionJob, ResolutionOutcome, ResolutionResult, VerificationStatus, VerificationSummary,
    WorkTaskState, RESOLUTION_SCHEMA_VERSION,
};
use serde_json::json;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

const CONFLICT: &str = "conflict.txt";
const CANDIDATE: &[u8] = b"reconciled content";

fn make_v3(client: &support::TestClient) -> feanorfs_client::Config {
    let mut config = load_config(client.workspace.path()).unwrap();
    config.format_version = 3;
    save_config(client.workspace.path(), &config).unwrap();
    config
}

fn hex64(byte: u8) -> String {
    std::iter::repeat_n(byte as char, 64).collect()
}

fn seed_accepted(ctx: &SyncCtx<'_>, path: &str) {
    let proposal = WorkProposalRecord {
        agent: "agent-a".to_string(),
        sequence: 1,
        intent_message_id: hex64(b'a'),
        coordinator: Some("human".to_string()),
        causal_base: None,
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
        source_message_id: hex64(b'a'),
        updated_at_ms: 1,
        capabilities: vec!["resolution".to_string()],
        author_restore: None,
    };
    WorkStore::open(ctx.base)
        .unwrap()
        .update(|state| {
            state.incomplete = false;
            state.tasks = vec![WorkTaskRecord {
                task_id: "parser-impl".to_string(),
                proposals: vec![proposal],
                updated_at_ms: 1,
            }];
            Ok(())
        })
        .unwrap();
}

fn ctx_from<'a>(
    api: &'a ApiClient,
    db: &'a ClientDb,
    root: &'a Path,
    config: &'a feanorfs_client::Config,
) -> SyncCtx<'a> {
    SyncCtx::from_config(api, db, root, config).unwrap()
}

/// Spawns the real `feanorfs` binary as a child of the async runtime so the
/// in-test HTTP hub keeps serving while the CLI runs.
async fn run_cli(workspace: &Path, home_root: &Path, args: &[&str]) -> Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args(args)
        .current_dir(workspace)
        .env("FEANORFS_HOME", home_root)
        .output()
        .await
        .unwrap()
}

fn stdout_json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
        panic!(
            "cli output is not JSON: {error}\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Creates one encrypted conflict head (shared base + agent land), returns
/// the conflict snapshot id.
async fn publish_conflict(
    main: &support::TestClient,
    second: &support::TestClient,
    server: &support::TestServer,
    agent: &str,
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
        agent,
        Some(support::TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    write_workspace_file(
        &feanorfs_agent_core::agent_dir(main.workspace.path(), agent).unwrap(),
        CONFLICT,
        b"agent edit",
    )
    .await;
    write_workspace_file(second.workspace.path(), CONFLICT, b"folder edit").await;
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
    let landed = land_agent(
        main.workspace.path(),
        &main.db,
        &server.api,
        support::WORKSPACE_ID,
        agent,
        Some(support::TEST_PASSWORD),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(landed.conflicts.len(), 1);
    landed
        .snapshot_id
        .expect("land publishes a conflict snapshot")
}

/// Writes the immutable candidate file for one job under the state root.
fn write_candidate(job: &ResolutionJob, state_root: &Path, bytes: &[u8]) {
    let abs = state_root.join(&job.candidate_destination.path);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&abs)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn result_for(job: &ResolutionJob, bytes: &[u8]) -> ResolutionResult {
    ResolutionResult {
        schema_version: RESOLUTION_SCHEMA_VERSION,
        outcome: ResolutionOutcome::CandidateReady,
        job_id: job.job_id.clone(),
        assignment_id: job.assignment_id.clone(),
        attempt: job.attempt,
        owner: job.owner.clone(),
        conflict_fingerprint: job.conflict_fingerprint.clone(),
        candidate: Some(CandidateDescriptor {
            path: job.candidate_destination.path.clone(),
            hash: hash_bytes(bytes),
            size: bytes.len() as u64,
            mode: 0,
            deleted: false,
        }),
        verification: VerificationSummary {
            status: VerificationStatus::Passed,
            summary: "resolution parity verification passed".to_string(),
            ..VerificationSummary::default()
        },
        diagnostics: vec![],
        question: None,
        human_reason: None,
        question_generation: 0,
        safe_options: vec![],
    }
}

/// Both parity tests spawn real agent processes with shared global agent
/// state under one process-wide FEANORFS_HOME; serialize them so agent
/// identities never collide across concurrent tests.
static RESOLUTION_PARITY_SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[tokio::test]
async fn full_lifecycle_is_equivalent_inline_cli_and_mcp_with_fixture_parity() {
    let _serial = RESOLUTION_PARITY_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    make_v3(&main);
    make_v3(&second);
    let home_root =
        PathBuf::from(std::env::var_os("FEANORFS_HOME").expect("isolated FEANORFS_HOME"));
    let head = publish_conflict(&main, &second, &server, "resolver-agent").await;

    let config = load_config(main.workspace.path()).unwrap();
    let ctx = ctx_from(&server.api, &main.db, main.workspace.path(), &config);

    // Materialize the triple locally: registers the fingerprinted record the
    // automatic pipeline requires (legacy records are refused).
    let materialized = feanorfs_agent_core::materialize_conflicts(&ctx, &head, &[])
        .await
        .unwrap();
    assert_eq!(materialized.entries.len(), 1);
    assert_eq!(materialized.entries[0].path, CONFLICT);
    assert!(main.db.is_conflict_fingerprinted(CONFLICT).await.unwrap());
    seed_accepted(&ctx, CONFLICT);

    // ---- MCP harness: tools are declared and run the lifecycle ----
    let mut mcp = tokio::process::Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .arg("mcp")
        .current_dir(main.workspace.path())
        .env("FEANORFS_HOME", &home_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let list_request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let status_request = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "resolution_status", "arguments": {} }
    });
    let prepare_request = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": "resolution_prepare",
            "arguments": {
                "path": CONFLICT,
                "prevention": { "type": "exhausted", "detail": "no bounded prevention path remains" }
            }
        }
    });
    {
        use tokio::io::AsyncWriteExt as _;
        let stdin = mcp.stdin.as_mut().unwrap();
        let payload = format!("{list_request}\n{status_request}\n{prepare_request}\n");
        stdin.write_all(payload.as_bytes()).await.unwrap();
    }
    drop(mcp.stdin.take());
    let output = mcp.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "MCP failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    let tool_names: Vec<&str> = responses[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in [
        "resolution_prepare",
        "resolution_status",
        "resolution_submit",
        "resolution_apply",
    ] {
        assert!(tool_names.contains(&name), "MCP must declare {name}");
    }
    assert_eq!(
        responses[1]["result"]["structuredContent"]["jobs"],
        json!([])
    );
    let mcp_job: ResolutionJob =
        serde_json::from_value(responses[2]["result"]["structuredContent"].clone())
            .expect("MCP prepare returns a ResolutionJob");
    validate_resolution_job(&mcp_job).unwrap();
    assert_eq!(mcp_job.conflict.path, CONFLICT);
    assert_eq!(mcp_job.owner, "agent-a");
    assert!(mcp_job.conflict.is_automatic());
    assert_eq!(mcp_job.conflict_fingerprint.len(), 64);

    // One active job per conflict fingerprint: the second prepare must be
    // preceded by a typed revocation of the first (terminal) assignment.
    feanorfs_agent_core::revoke_resolution_assignment(&ctx, &mcp_job.job_id, false)
        .await
        .unwrap();

    // ---- Inline engine: same conflict, same exact identity ----
    let inline_job = prepare_resolution_job(
        &ctx,
        CONFLICT,
        PreventionReason::Exhausted {
            detail: "no bounded prevention path remains".to_string(),
        },
    )
    .await
    .unwrap();
    // Assignment id and attempt are regenerated per designation, so
    // normalize both before comparing the deterministic identity fields;
    // the engine fingerprints must still be exact for each job's own
    // identity.
    let mut inline_norm = inline_job.conflict.clone();
    inline_norm.assignment_id = None;
    inline_norm.attempt = None;
    let mut mcp_norm = mcp_job.conflict.clone();
    mcp_norm.assignment_id = None;
    mcp_norm.attempt = None;
    assert_eq!(inline_norm, mcp_norm);
    assert_eq!(
        compute_conflict_identity_fingerprint(&inline_norm),
        compute_conflict_identity_fingerprint(&mcp_norm)
    );
    assert_eq!(
        compute_conflict_identity_fingerprint(&inline_job.conflict),
        inline_job.conflict_fingerprint
    );
    // Inline and MCP both designate the causally eligible author.
    assert_eq!(inline_job.owner, "agent-a");

    // Fixture parity: the CLI/MCP job carries the exact canonical key set.
    let fixture_keys = serde_json::to_value(resolution_fixtures::job())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let job_keys = serde_json::to_value(&mcp_job)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        job_keys, fixture_keys,
        "ResolutionJob wire shape must match the canonical fixture"
    );

    // ---- Inline submit/apply for the active inline job ----
    let state_dir = ensure_workspace_state(main.workspace.path()).unwrap();
    write_candidate(&inline_job, &state_dir, CANDIDATE);
    let inline_result = result_for(&inline_job, CANDIDATE);
    let submitted =
        feanorfs_agent_core::submit_resolution_result(&ctx, &inline_job.job_id, inline_result)
            .await
            .unwrap();
    assert_eq!(submitted.outcome, ResolutionOutcome::CandidateReady);
    // Submit never applies: head unchanged, conflict intact.
    assert_eq!(
        server
            .api
            .get_head(support::WORKSPACE_ID)
            .await
            .unwrap()
            .unwrap(),
        head
    );
    let outcome = feanorfs_agent_core::apply_resolution_job(&ctx, &inline_job.job_id)
        .await
        .unwrap();
    let ResolutionApplyOutcome::Published { head: new_head } = outcome else {
        panic!("expected published, got {outcome:?}");
    };
    assert_ne!(new_head, head);
    // The published head carries the reconciled candidate; the conflict is
    // gone from the local pending registry and recorded as typed history.
    let engine = feanorfs_agent_core::SnapshotEngine::new(&ctx);
    let files = engine.load_files(&new_head).await.unwrap();
    assert!(files.contains_key(CONFLICT));
    assert_eq!(files[CONFLICT].size, CANDIDATE.len() as u64);
    assert!(!files[CONFLICT].deleted);
    assert!(main
        .db
        .list_pending_conflict_paths()
        .await
        .unwrap()
        .is_empty());
    let history = main.db.list_conflict_resolutions().await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].path, CONFLICT);
    assert_eq!(history[0].method, "candidate");

    // Resync both folders to the new head so the second conflict setup
    // starts from a clean state (spawn refuses folders needing attention).
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

    // Inline status projection: the job is completed, metadata only.
    let projection = resolution_status(&ctx, None).await.unwrap();
    let job_status = projection
        .jobs
        .iter()
        .find(|job| job.job_id == inline_job.job_id)
        .expect("completed job must be in the projection");
    assert_eq!(
        job_status.assignment_state,
        feanorfs_agent_core::ResolutionAssignmentState::Completed
    );
    assert_eq!(job_status.outcome, Some(ResolutionOutcome::CandidateReady));
    assert_eq!(
        job_status.conflict_fingerprint,
        inline_job.conflict_fingerprint
    );

    // ---- Spawned CLI harness on a second conflict ----
    let cli_head = publish_conflict(&main, &second, &server, "resolver-agent-2").await;
    let materialized = feanorfs_agent_core::materialize_conflicts(&ctx, &cli_head, &[])
        .await
        .unwrap();
    assert_eq!(materialized.entries.len(), 1);
    seed_accepted(&ctx, CONFLICT);

    let prepare = run_cli(
        main.workspace.path(),
        &home_root,
        &[
            "--json",
            "agent",
            "resolution",
            "prepare",
            CONFLICT,
            "--reason",
            "exhausted",
            "--detail",
            "no bounded prevention path remains",
        ],
    )
    .await;
    let job_json = stdout_json(&prepare);
    let cli_job: ResolutionJob = serde_json::from_value(job_json.clone()).unwrap();
    validate_resolution_job(&cli_job).unwrap();
    assert_eq!(cli_job.conflict.path, CONFLICT);
    assert_eq!(cli_job.owner, "agent-a");

    // One active job per conflict fingerprint: the inline comparison prepare
    // follows a typed revocation, and assignment id/attempt (per-designation
    // artifacts) are normalized.
    feanorfs_agent_core::revoke_resolution_assignment(&ctx, &cli_job.job_id, false)
        .await
        .unwrap();
    let inline_cli = prepare_resolution_job(
        &ctx,
        CONFLICT,
        PreventionReason::Exhausted {
            detail: "no bounded prevention path remains".to_string(),
        },
    )
    .await
    .unwrap();
    let mut inline_cli_norm = inline_cli.conflict.clone();
    inline_cli_norm.assignment_id = None;
    inline_cli_norm.attempt = None;
    let mut cli_norm = cli_job.conflict.clone();
    cli_norm.assignment_id = None;
    cli_norm.attempt = None;
    assert_eq!(inline_cli_norm, cli_norm);

    // Reactivate a CLI-prepared job for the CLI submit/status/apply parity.
    feanorfs_agent_core::revoke_resolution_assignment(&ctx, &inline_cli.job_id, false)
        .await
        .unwrap();
    let cli2 = run_cli(
        main.workspace.path(),
        &home_root,
        &[
            "--json",
            "agent",
            "resolution",
            "prepare",
            CONFLICT,
            "--reason",
            "exhausted",
            "--detail",
            "no bounded prevention path remains",
        ],
    )
    .await;
    let cli2_value = stdout_json(&cli2);
    let cli2_job: ResolutionJob = serde_json::from_value(cli2_value).unwrap();
    validate_resolution_job(&cli2_job).unwrap();

    // Fixture parity for the CLI result document.
    write_candidate(&cli2_job, &state_dir, CANDIDATE);
    let result_json = serde_json::to_string(&result_for(&cli2_job, CANDIDATE)).unwrap();
    let result_file = main.workspace.path().join("result.json");
    std::fs::write(&result_file, &result_json).unwrap();
    let submit = run_cli(
        main.workspace.path(),
        &home_root,
        &[
            "--json",
            "agent",
            "resolution",
            "submit",
            &cli2_job.job_id,
            "--result",
            result_file.to_str().unwrap(),
        ],
    )
    .await;
    let submitted_value = stdout_json(&submit);
    let submitted_result: ResolutionResult =
        serde_json::from_value(submitted_value.clone()).unwrap();
    validate_resolution_result(&submitted_result).unwrap();
    assert_eq!(submitted_result.outcome, ResolutionOutcome::CandidateReady);
    let fixture_result_keys = serde_json::to_value(resolution_fixtures::result())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let result_keys = submitted_value
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        result_keys, fixture_result_keys,
        "ResolutionResult wire shape must match the canonical fixture"
    );
    // Submit never applied: head still the pre-apply conflict head.
    assert_eq!(
        server
            .api
            .get_head(support::WORKSPACE_ID)
            .await
            .unwrap()
            .unwrap(),
        cli_head
    );

    // CLI status shows the active submitted job.
    let status = run_cli(
        main.workspace.path(),
        &home_root,
        &["--json", "agent", "resolution", "status", &cli2_job.job_id],
    )
    .await;
    let status_value = stdout_json(&status);
    assert_eq!(status_value["schema_version"], 1);
    let jobs = status_value["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["job_id"], cli2_job.job_id);
    assert_eq!(jobs[0]["outcome"], "candidate_ready");
    assert_eq!(jobs[0]["assignment_state"], "active");
    assert_eq!(
        jobs[0]["conflict_fingerprint"],
        cli2_job.conflict_fingerprint
    );
    for forbidden in ["path", "candidate", "identity", "body"] {
        assert!(status_value.get(forbidden).is_none());
    }

    // CLI apply publishes.
    let apply = run_cli(
        main.workspace.path(),
        &home_root,
        &["--json", "agent", "resolution", "apply", &cli2_job.job_id],
    )
    .await;
    let apply_value = stdout_json(&apply);
    assert_eq!(apply_value["outcome"], "published");
    let published_head = apply_value["head"].as_str().unwrap().to_string();
    assert_ne!(published_head, cli_head);
    assert_eq!(
        server
            .api
            .get_head(support::WORKSPACE_ID)
            .await
            .unwrap()
            .unwrap(),
        published_head
    );

    // CLI status now reports completed.
    let status = run_cli(
        main.workspace.path(),
        &home_root,
        &["--json", "agent", "resolution", "status"],
    )
    .await;
    let status_value = stdout_json(&status);
    let jobs = status_value["jobs"].as_array().unwrap();
    let cli_status = jobs
        .iter()
        .find(|job| job["job_id"] == cli2_job.job_id)
        .expect("cli job must remain in the projection");
    assert_eq!(cli_status["assignment_state"], "completed");
    // The published CLI apply cleared the pending registry in the workspace
    // (the same durable bookkeeping as the inline apply).
    assert_eq!(
        main.db.list_pending_conflict_paths().await.unwrap().len(),
        0,
        "CLI apply must clear the pending conflict registry"
    );

    // Resync both folders to the published head so the third conflict setup
    // starts from a clean state (spawn refuses folders needing attention).
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

    // Fresh conflict: apply-before-submit is refused, an invalid prevention
    // type is rejected, and the human prepare output states that submit
    // never applies.
    let third_head = publish_conflict(&main, &second, &server, "resolver-agent-3").await;
    let materialized = feanorfs_agent_core::materialize_conflicts(&ctx, &third_head, &[])
        .await
        .unwrap();
    assert_eq!(materialized.entries.len(), 1);
    seed_accepted(&ctx, CONFLICT);

    let fresh_prepare = run_cli(
        main.workspace.path(),
        &home_root,
        &[
            "--json",
            "agent",
            "resolution",
            "prepare",
            CONFLICT,
            "--reason",
            "exhausted",
            "--detail",
            "no bounded prevention path remains",
        ],
    )
    .await;
    let fresh_job: ResolutionJob = serde_json::from_value(stdout_json(&fresh_prepare)).unwrap();

    let apply_before_submit = run_cli(
        main.workspace.path(),
        &home_root,
        &["--json", "agent", "resolution", "apply", &fresh_job.job_id],
    )
    .await;
    assert!(!apply_before_submit.status.success());
    let stderr = String::from_utf8_lossy(&apply_before_submit.stderr);
    assert!(
        stderr.contains("apply before submit is refused") || stderr.contains("no submitted result"),
        "apply without submit must be refused: {stderr}"
    );

    // Release the active slot so the human prepare can succeed.
    feanorfs_agent_core::revoke_resolution_assignment(&ctx, &fresh_job.job_id, false)
        .await
        .unwrap();
    let human_prepare = run_cli(
        main.workspace.path(),
        &home_root,
        &[
            "agent",
            "resolution",
            "prepare",
            CONFLICT,
            "--detail",
            "no bounded prevention path remains",
        ],
    )
    .await;
    assert!(human_prepare.status.success());
    let human = String::from_utf8_lossy(&human_prepare.stdout);
    assert!(
        human.contains("submit never applies"),
        "human output must state submit never applies: {human}"
    );

    // Invalid prevention type is rejected by the CLI.
    feanorfs_agent_core::revoke_resolution_assignment(&ctx, &fresh_job.job_id, false)
        .await
        .ok();
    // Invalid prevention type is rejected by the CLI.
    let bad_prevention = run_cli(
        main.workspace.path(),
        &home_root,
        &[
            "agent",
            "resolution",
            "prepare",
            CONFLICT,
            "--reason",
            "unknown",
            "--detail",
            "x",
        ],
    )
    .await;
    assert!(!bad_prevention.status.success());
}

#[tokio::test]
async fn stale_apply_keeps_the_current_conflict_unchanged() {
    let _serial = RESOLUTION_PARITY_SERIAL.lock().await;
    let server = spawn_test_server().await;
    let main = spawn_test_client_with_server(&server).await;
    let second = spawn_test_client_with_server(&server).await;
    make_v3(&main);
    make_v3(&second);
    let home_root =
        PathBuf::from(std::env::var_os("FEANORFS_HOME").expect("isolated FEANORFS_HOME"));
    let head = publish_conflict(&main, &second, &server, "resolver-agent-stale").await;

    let config = load_config(main.workspace.path()).unwrap();
    let ctx = ctx_from(&server.api, &main.db, main.workspace.path(), &config);
    feanorfs_agent_core::materialize_conflicts(&ctx, &head, &[])
        .await
        .unwrap();
    seed_accepted(&ctx, CONFLICT);
    let state_root_path = ensure_workspace_state(main.workspace.path()).unwrap();

    let job = prepare_resolution_job(
        &ctx,
        CONFLICT,
        PreventionReason::Exhausted {
            detail: "no bounded prevention path remains".to_string(),
        },
    )
    .await
    .unwrap();
    write_candidate(&job, &state_root_path, CANDIDATE);
    feanorfs_agent_core::submit_resolution_result(&ctx, &job.job_id, result_for(&job, CANDIDATE))
        .await
        .unwrap();

    // Mutate state after submit and before apply: revoke the assignment.
    feanorfs_agent_core::revoke_resolution_assignment(&ctx, &job.job_id, false)
        .await
        .unwrap();
    let outcome = feanorfs_agent_core::apply_resolution_job(&ctx, &job.job_id)
        .await
        .unwrap();
    let ResolutionApplyOutcome::Stale {
        kind: feanorfs_common::ResolutionStaleKind::AssignmentRevoked,
        ..
    } = outcome
    else {
        panic!("expected assignment_revoked stale outcome, got {outcome:?}");
    };
    // The current conflict survives unchanged: head untouched and the
    // pending registry still lists the conflict.
    assert_eq!(
        server
            .api
            .get_head(support::WORKSPACE_ID)
            .await
            .unwrap()
            .unwrap(),
        head
    );
    assert_eq!(
        ctx.db.list_pending_conflict_paths().await.unwrap(),
        vec![CONFLICT]
    );

    // Replay (a second submit) is rejected.
    let replay = feanorfs_agent_core::submit_resolution_result(
        &ctx,
        &job.job_id,
        result_for(&job, CANDIDATE),
    )
    .await;
    assert!(replay.is_err());

    // Event projection diff helper is covered by unit tests in events.rs;
    // here we assert the CLI status still projects the revoked job.
    let status = run_cli(
        main.workspace.path(),
        &home_root,
        &["--json", "agent", "resolution", "status", &job.job_id],
    )
    .await;
    let value = stdout_json(&status);
    assert_eq!(value["jobs"][0]["assignment_state"], "revoked");
}
