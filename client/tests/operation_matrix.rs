feanorfs_test_support::isolate_test_process!();

// Checked operation matrix.
//
// Every baseline operation must remain exposed on each required public
// surface: Rust SDK, CLI, C header, napi/JS facade, TypeScript declarations,
// MCP, events, docs, and the collaboration skill. Removing or renaming one
// operation on one surface fails this test and therefore CI.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("client crate lives one level under the workspace root")
        .to_path_buf()
}

fn surface(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel))
        .unwrap_or_else(|error| panic!("surface {rel} unreadable: {error}"))
}

/// Compile-time proof that the Rust SDK still exposes every baseline
/// operation with a compatible signature. Never called: failing to compile is
/// the check.
#[allow(dead_code, unused_variables)]
#[allow(clippy::too_many_arguments)]
fn rust_sdk_surface_compiles(
    workspace: &feanorfs_agent_core::Workspace,
    message_input: &feanorfs_common::AgentMessageInput,
    inbox_query: &feanorfs_common::AgentInboxQuery,
    assign_input: &feanorfs_common::IntegratorAssignInput,
    observe_options: &feanorfs_agent_core::IntegratorObserveOptions,
    propose_input: &feanorfs_common::WorkProposeInput,
    decide_input: &feanorfs_common::WorkDecideInput,
    amend_input: &feanorfs_common::WorkAmendInput,
    yield_input: &feanorfs_common::WorkYieldInput,
    settle_input: &feanorfs_common::WorkSettleInput,
    complete_input: &feanorfs_common::WorkCompleteInput,
    block_input: &feanorfs_common::WorkBlockInput,
    status_input: &feanorfs_common::WorkStatusInput,
) {
    let _ = workspace.list();
    let _ = workspace.agent_path("agent");
    let _ = workspace.spawn("agent", Default::default());
    let _ = workspace.status("agent");
    let _ = workspace.refresh("agent");
    let _ = workspace.land("agent", Default::default());
    let _ = workspace.clean("agent");
    let _ = workspace.resolve("src/main.rs", feanorfs_agent_core::ResolveKeep::Local, None);
    let _ = workspace.log(20);
    let _ = workspace.undo("snapshot");
    let _ = workspace.send_message(message_input.clone());
    let _ = workspace.inbox(inbox_query.clone());
    let _ = workspace.integrator_assign(assign_input.clone());
    let _ = workspace.integrator_status(None);
    let _ = workspace.integrator_revoke("assignment", "reason");
    let _ = workspace.integrator_resume(*observe_options);
    let _ = workspace.materialize_conflicts("snapshot", &["src/main.rs".to_string()]);
    let _ = workspace.work_propose(propose_input.clone());
    let _ = workspace.work_decide(decide_input.clone());
    let _ = workspace.work_amend(amend_input.clone());
    let _ = workspace.work_yield(yield_input.clone());
    let _ = workspace.work_settle(settle_input.clone());
    let _ = workspace.work_complete(complete_input.clone());
    let _ = workspace.work_block(block_input.clone());
    let _ = workspace.work_status(status_input.clone());
    let _ = workspace.resolution_prepare(
        "src/main.rs",
        feanorfs_common::PreventionReason::Exhausted {
            detail: "no bounded prevention path remains".to_string(),
        },
    );
    let _ = workspace.resolution_status(None);
    let _ = workspace.resolution_submit(
        "job",
        feanorfs_common::resolution_contract::resolution_fixtures::result(),
    );
    let _ = workspace.resolution_apply("job");
    let _ = workspace.resolution_materialize_legs("job");
    let _ = workspace.resolution_put_candidate("job", b"candidate bytes");
    let _ = workspace.resolution_answer(feanorfs_common::HumanResolutionAnswer {
        schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
        job_id: "0123456789abcdef0123456789abcdef".to_string(),
        assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
        attempt: 0,
        conflict_fingerprint: "a".repeat(64),
        question_generation: 0,
        chosen_option: feanorfs_common::HumanResolutionOption::Defer,
        candidate: None,
        verification: None,
    });
    let _ = workspace.resolution_defer("job");
    let _ = workspace.resolution_protocol_status(false);
    let _ = workspace.resolution_assign("job");
    let _ = workspace.resolution_reply("job");
    let _ = workspace.resolution_revoke("job", false);
    let _ = workspace.resolution_publish_answer(&feanorfs_common::HumanResolutionAnswer {
        schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
        job_id: "0123456789abcdef0123456789abcdef".to_string(),
        assignment_id: "0123456789abcdef0123456789abcdef".to_string(),
        attempt: 0,
        conflict_fingerprint: "a".repeat(64),
        question_generation: 0,
        chosen_option: feanorfs_common::HumanResolutionOption::KeepUnresolved,
        candidate: None,
        verification: None,
    });
}

struct Operation {
    /// Stable operation name (diagnostics only).
    name: &'static str,
    /// Required marker in `feanorfs-ffi/feanorfs.h`.
    ffi: Option<&'static str>,
    /// Required marker in `bindings/ts/src/lib.rs` (napi).
    napi: Option<&'static str>,
    /// Required marker in `bindings/ts/api.mjs` (JavaScript facade).
    facade: Option<&'static str>,
    /// Required marker in `bindings/ts/contract.d.ts`.
    dts: Option<&'static str>,
    /// Required marker in `client/src/cli/mcp.rs`.
    mcp: Option<&'static str>,
    /// Required marker in `client/src/cli/events.rs`.
    events: Option<&'static str>,
    /// Required marker in `docs/agent-api.md`.
    docs: &'static str,
    /// Required marker in `skills/feanorfs-collaboration/SKILL.md`.
    skill: Option<&'static str>,
    /// Required marker in the CLI implementation files.
    cli: Option<&'static str>,
}

/// The frozen baseline matrix. New operations extend this matrix in the same
/// change that adds their surfaces.
const OPERATIONS: &[Operation] = &[
    Operation {
        name: "list_agents",
        ffi: Some("ffs_agent_list"),
        napi: Some("fn agent_list"),
        facade: Some("listAgents"),
        dts: Some("listAgents"),
        mcp: None,
        events: None,
        docs: "List agents",
        skill: Some("agent status"),
        cli: Some("run_agent_status_list"),
    },
    Operation {
        name: "agent_spawn",
        ffi: Some("ffs_agent_spawn"),
        napi: Some("fn agent_spawn"),
        facade: Some("function spawn"),
        dts: Some("function spawn"),
        mcp: Some("tool(\"agent_spawn\""),
        events: None,
        docs: "agent spawn <name>",
        skill: None,
        cli: Some("Spawn {"),
    },
    Operation {
        name: "agent_status",
        ffi: Some("ffs_agent_status"),
        napi: Some("fn agent_status"),
        facade: Some("function status"),
        dts: Some("function status"),
        mcp: Some("tool(\"agent_check\""),
        events: None,
        docs: "Preview",
        skill: Some("agent status"),
        cli: Some("Check {"),
    },
    Operation {
        name: "agent_refresh",
        ffi: Some("ffs_agent_refresh"),
        napi: Some("fn agent_refresh"),
        facade: Some("function refresh"),
        dts: Some("function refresh"),
        mcp: Some("tool(\"agent_refresh\""),
        events: None,
        docs: "agent refresh <name>",
        skill: Some("feanorfs agent refresh"),
        cli: Some("Refresh {"),
    },
    Operation {
        name: "agent_land",
        ffi: Some("ffs_agent_land"),
        napi: Some("fn agent_land"),
        facade: Some("function land"),
        dts: Some("function land"),
        mcp: Some("tool(\"agent_land\""),
        events: None,
        docs: "agent land <name>",
        skill: Some("feanorfs agent land"),
        cli: Some("Land {"),
    },
    Operation {
        name: "agent_clean",
        ffi: Some("ffs_agent_clean"),
        napi: Some("fn agent_clean"),
        facade: Some("function clean"),
        dts: Some("function clean"),
        mcp: None,
        events: None,
        docs: "agent clean <name>",
        skill: None,
        cli: Some("Clean {"),
    },
    Operation {
        name: "agent_path",
        ffi: Some("ffs_agent_path"),
        napi: Some("fn agent_path"),
        facade: Some("agentPath"),
        dts: Some("agentPath"),
        mcp: None,
        events: None,
        docs: "Agent path",
        skill: None,
        cli: Some("agent_path"),
    },
    Operation {
        name: "agent_send",
        ffi: Some("ffs_agent_send"),
        napi: Some("fn agent_send"),
        facade: Some("sendMessage"),
        dts: Some("sendMessage"),
        mcp: Some("tool(\"agent_send\""),
        events: Some("\"agent_message\""),
        docs: "Send signal",
        skill: Some("feanorfs agent send"),
        cli: Some("run_agent_send"),
    },
    Operation {
        name: "agent_inbox",
        ffi: Some("ffs_agent_inbox"),
        napi: Some("fn agent_inbox"),
        facade: Some("function inbox"),
        dts: Some("function inbox"),
        mcp: Some("tool(\"agent_inbox\""),
        events: Some("\"agent_message_cursor_reset\""),
        docs: "Inbox",
        skill: Some("feanorfs agent inbox"),
        cli: Some("run_agent_inbox"),
    },
    Operation {
        name: "log",
        ffi: Some("ffs_log"),
        napi: Some("fn history_log"),
        facade: Some("function log"),
        dts: Some("function log"),
        mcp: Some("tool(\"workspace_log\""),
        events: None,
        docs: "| History |",
        skill: None,
        cli: Some("pub async fn log"),
    },
    Operation {
        name: "undo",
        ffi: Some("ffs_undo"),
        napi: Some("fn undo"),
        facade: Some("function undo"),
        dts: Some("function undo"),
        mcp: Some("tool(\"workspace_undo\""),
        events: None,
        docs: "| Undo |",
        skill: None,
        cli: Some("pub async fn undo"),
    },
    Operation {
        name: "conflicts_keep",
        ffi: Some("ffs_conflicts_keep"),
        napi: Some("fn conflicts_keep"),
        facade: Some("conflictsKeep"),
        dts: Some("conflictsKeep"),
        mcp: Some("tool(\"conflicts_keep\""),
        events: None,
        docs: "| Resolve |",
        skill: Some("conflicts keep"),
        cli: Some("Keep {"),
    },
    Operation {
        name: "conflicts_list",
        ffi: None,
        napi: None,
        facade: None,
        dts: None,
        mcp: Some("tool(\"conflicts_list\""),
        events: None,
        docs: "conflicts",
        skill: Some("conflicts"),
        cli: Some("List,"),
    },
    Operation {
        name: "conflict_materialize",
        ffi: Some("ffs_conflict_materialize"),
        napi: Some("fn conflict_materialize"),
        facade: Some("conflictMaterialize"),
        dts: Some("conflictMaterialize"),
        mcp: Some("tool(\"conflict_materialize\""),
        events: None,
        docs: "| Materialize |",
        skill: Some("conflicts materialize"),
        cli: Some("Materialize {"),
    },
    Operation {
        name: "integrator_assign",
        ffi: Some("ffs_integrator_assign"),
        napi: Some("fn integrator_assign"),
        facade: Some("integratorAssign"),
        dts: Some("integratorAssign"),
        mcp: Some("tool(\"integrator_assign\""),
        events: Some("\"integrator_assigned\""),
        docs: "| Assign |",
        skill: None,
        cli: Some("Assign {"),
    },
    Operation {
        name: "integrator_status",
        ffi: Some("ffs_integrator_status"),
        napi: Some("fn integrator_status"),
        facade: Some("integratorStatus"),
        dts: Some("integratorStatus"),
        mcp: Some("tool(\"integrator_status\""),
        events: Some("\"integrator_accepted\""),
        docs: "agent integrator status",
        skill: None,
        cli: Some("Status {"),
    },
    Operation {
        name: "integrator_revoke",
        ffi: Some("ffs_integrator_revoke"),
        napi: Some("fn integrator_revoke"),
        facade: Some("integratorRevoke"),
        dts: Some("integratorRevoke"),
        mcp: Some("tool(\"integrator_revoke\""),
        events: None,
        docs: "| Revoke |",
        skill: None,
        cli: Some("Revoke"),
    },
    Operation {
        name: "integrator_resume",
        ffi: Some("ffs_integrator_resume"),
        napi: Some("fn integrator_resume"),
        facade: Some("integratorResume"),
        dts: Some("integratorResume"),
        mcp: Some("tool(\"integrator_resume\""),
        events: Some("\"integrator_blocked\""),
        docs: "| Resume |",
        skill: None,
        cli: Some("Resume"),
    },
    Operation {
        name: "work_propose",
        ffi: Some("ffs_work_propose"),
        napi: Some("fn work_propose"),
        facade: Some("workPropose"),
        dts: Some("workPropose"),
        mcp: Some("tool(\"work_propose\""),
        events: Some("\"work_intent\""),
        docs: "agent work propose",
        skill: Some("feanorfs agent work propose"),
        cli: Some("Propose {"),
    },
    Operation {
        name: "work_decide",
        ffi: Some("ffs_work_decide"),
        napi: Some("fn work_decide"),
        facade: Some("workDecide"),
        dts: Some("workDecide"),
        mcp: Some("tool(\"work_decide\""),
        events: Some("\"work_decision\""),
        docs: "agent work decide",
        skill: Some("feanorfs agent work decide"),
        cli: Some("Decide {"),
    },
    Operation {
        name: "work_amend",
        ffi: Some("ffs_work_amend"),
        napi: Some("fn work_amend"),
        facade: Some("workAmend"),
        dts: Some("workAmend"),
        mcp: Some("tool(\"work_amend\""),
        events: Some("\"work_amendment\""),
        docs: "agent work amend",
        skill: Some("feanorfs agent work amend"),
        cli: Some("Amend {"),
    },
    Operation {
        name: "work_yield",
        ffi: Some("ffs_work_yield"),
        napi: Some("fn work_yield"),
        facade: Some("workYield"),
        dts: Some("workYield"),
        mcp: Some("tool(\"work_yield\""),
        events: Some("\"work_yield\""),
        docs: "agent work yield",
        skill: Some("feanorfs agent work yield"),
        cli: Some("Yield {"),
    },
    Operation {
        name: "work_settle",
        ffi: Some("ffs_work_settle"),
        napi: Some("fn work_settle"),
        facade: Some("workSettle"),
        dts: Some("workSettle"),
        mcp: Some("tool(\"work_settle\""),
        events: Some("\"work_settled\""),
        docs: "agent work settle",
        skill: Some("feanorfs agent work settle"),
        cli: Some("Settle {"),
    },
    Operation {
        name: "work_complete",
        ffi: Some("ffs_work_complete"),
        napi: Some("fn work_complete"),
        facade: Some("workComplete"),
        dts: Some("workComplete"),
        mcp: Some("tool(\"work_complete\""),
        events: Some("\"work_completed\""),
        docs: "agent work complete",
        skill: Some("feanorfs agent work complete"),
        cli: Some("Complete {"),
    },
    Operation {
        name: "work_block",
        ffi: Some("ffs_work_block"),
        napi: Some("fn work_block"),
        facade: Some("workBlock"),
        dts: Some("workBlock"),
        mcp: Some("tool(\"work_block\""),
        events: Some("\"work_blocked\""),
        docs: "agent work block",
        skill: Some("feanorfs agent work block"),
        cli: Some("Block {"),
    },
    Operation {
        name: "work_status",
        ffi: Some("ffs_work_status"),
        napi: Some("fn work_status"),
        facade: Some("workStatus"),
        dts: Some("workStatus"),
        mcp: Some("tool(\"work_status\""),
        events: Some("\"work_intent\""),
        docs: "agent work status",
        skill: Some("feanorfs agent work status"),
        cli: Some("Status {"),
    },
    Operation {
        name: "resolution_prepare",
        ffi: Some("ffs_resolution_prepare"),
        napi: Some("fn resolution_prepare"),
        facade: Some("resolutionPrepare"),
        dts: Some("resolutionPrepare"),
        mcp: Some("tool(\"resolution_prepare\""),
        events: Some("\"resolution_prepared\""),
        docs: "agent resolution prepare",
        skill: Some("agent resolution prepare"),
        cli: Some("Prepare {"),
    },
    Operation {
        name: "resolution_status",
        ffi: Some("ffs_resolution_status"),
        napi: Some("fn resolution_status"),
        facade: Some("resolutionStatus"),
        dts: Some("resolutionStatus"),
        mcp: Some("tool(\"resolution_status\""),
        events: Some("\"resolution_submitted\""),
        docs: "agent resolution status",
        skill: Some("agent resolution status"),
        cli: Some("Status {"),
    },
    Operation {
        name: "resolution_submit",
        ffi: Some("ffs_resolution_submit"),
        napi: Some("fn resolution_submit"),
        facade: Some("resolutionSubmit"),
        dts: Some("resolutionSubmit"),
        mcp: Some("tool(\"resolution_submit\""),
        events: Some("\"resolution_applied\""),
        docs: "agent resolution submit",
        skill: Some("agent resolution submit"),
        cli: Some("Submit {"),
    },
    Operation {
        name: "resolution_apply",
        ffi: Some("ffs_resolution_apply"),
        napi: Some("fn resolution_apply"),
        facade: Some("resolutionApply"),
        dts: Some("resolutionApply"),
        mcp: Some("tool(\"resolution_apply\""),
        events: None,
        docs: "agent resolution apply",
        skill: Some("agent resolution apply"),
        cli: Some("Apply {"),
    },
    Operation {
        name: "resolution_materialize",
        ffi: Some("ffs_resolution_materialize"),
        napi: Some("fn resolution_materialize"),
        facade: Some("resolutionMaterialize"),
        dts: Some("resolutionMaterialize"),
        mcp: Some("tool(\"resolution_materialize\""),
        events: None,
        docs: "agent resolution materialize",
        skill: Some("agent resolution materialize"),
        cli: Some("ResolutionAction::Materialize"),
    },
    Operation {
        name: "resolution_put",
        ffi: Some("ffs_resolution_put"),
        napi: Some("fn resolution_put"),
        facade: Some("resolutionPut"),
        dts: Some("resolutionPut"),
        mcp: Some("tool(\"resolution_put\""),
        events: None,
        docs: "agent resolution put",
        skill: Some("agent resolution put"),
        cli: Some("ResolutionAction::Put"),
    },
    Operation {
        name: "resolution_answer",
        ffi: Some("ffs_resolution_answer"),
        napi: Some("fn resolution_answer"),
        facade: Some("resolutionAnswer"),
        dts: Some("resolutionAnswer"),
        mcp: Some("tool(\"resolution_answer\""),
        events: None,
        docs: "agent resolution answer",
        skill: Some("agent resolution answer"),
        cli: Some("ResolutionAction::Answer"),
    },
    Operation {
        name: "resolution_defer",
        ffi: Some("ffs_resolution_defer"),
        napi: Some("fn resolution_defer"),
        facade: Some("resolutionDefer"),
        dts: Some("resolutionDefer"),
        mcp: Some("tool(\"resolution_defer\""),
        events: None,
        docs: "agent resolution defer",
        skill: Some("agent resolution defer"),
        cli: Some("ResolutionAction::Defer"),
    },
    Operation {
        name: "resolution_protocol_status",
        ffi: Some("ffs_resolution_protocol_status"),
        napi: Some("fn resolution_protocol_status"),
        facade: Some("resolutionProtocolStatus"),
        dts: Some("resolutionProtocolStatus"),
        mcp: Some("tool(\"resolution_protocol_status\""),
        events: Some("\"resolution_assigned\""),
        docs: "agent resolution protocol-status",
        skill: Some("agent resolution protocol-status"),
        cli: Some("ResolutionAction::ProtocolStatus"),
    },
    Operation {
        name: "resolution_assign",
        ffi: Some("ffs_resolution_assign"),
        napi: Some("fn resolution_assign"),
        facade: Some("resolutionAssign"),
        dts: Some("resolutionAssign"),
        mcp: Some("tool(\"resolution_assign\""),
        events: Some("\"resolution_result_received\""),
        docs: "agent resolution assign",
        skill: Some("agent resolution assign"),
        cli: Some("ResolutionAction::Assign"),
    },
    Operation {
        name: "resolution_reply",
        ffi: Some("ffs_resolution_reply"),
        napi: Some("fn resolution_reply"),
        facade: Some("resolutionReply"),
        dts: Some("resolutionReply"),
        mcp: Some("tool(\"resolution_reply\""),
        events: Some("\"resolution_human_answered\""),
        docs: "agent resolution reply",
        skill: Some("agent resolution reply"),
        cli: Some("ResolutionAction::Reply"),
    },
    Operation {
        name: "resolution_revoke",
        ffi: Some("ffs_resolution_revoke"),
        napi: Some("fn resolution_revoke"),
        facade: Some("resolutionRevoke"),
        dts: Some("resolutionRevoke"),
        mcp: Some("tool(\"resolution_revoke\""),
        events: Some("\"resolution_revoked\""),
        docs: "agent resolution revoke",
        skill: Some("agent resolution revoke"),
        cli: Some("ResolutionAction::Revoke"),
    },
    Operation {
        name: "resolution_publish_answer",
        ffi: Some("ffs_resolution_publish_answer"),
        napi: Some("fn resolution_publish_answer"),
        facade: Some("resolutionPublishAnswer"),
        dts: Some("resolutionPublishAnswer"),
        mcp: Some("tool(\"resolution_publish_answer\""),
        events: Some("\"resolution_human_answered\""),
        docs: "agent resolution publish-answer",
        skill: Some("agent resolution publish-answer"),
        cli: Some("ResolutionAction::PublishAnswer"),
    },
];

fn check_marker(operation: &str, surface: &str, content: &str, marker: &str) {
    assert!(
        content.contains(marker),
        "operation `{operation}` lost its `{surface}` exposure: marker {marker:?} not found"
    );
}

#[test]
fn every_baseline_operation_stays_exposed_on_every_required_surface() {
    let root = workspace_root();
    let header = surface(&root, "feanorfs-ffi/feanorfs.h");
    let napi_src = surface(&root, "bindings/ts/src/lib.rs");
    let facade = surface(&root, "bindings/ts/api.mjs");
    let dts = surface(&root, "bindings/ts/contract.d.ts");
    let mcp = surface(&root, "client/src/cli/mcp.rs");
    let events = surface(&root, "client/src/cli/events.rs");
    let docs = surface(&root, "docs/agent-api.md");
    let skill = surface(&root, "skills/feanorfs-collaboration/SKILL.md");
    let cli_sources = [
        surface(&root, "client/src/cli/agent.rs"),
        surface(&root, "client/src/cli/conflicts.rs"),
        surface(&root, "client/src/cli/integrator.rs"),
        surface(&root, "client/src/cli/history.rs"),
        surface(&root, "client/src/cli/work.rs"),
        surface(&root, "client/src/cli/resolution.rs"),
    ]
    .concat();

    for operation in OPERATIONS {
        if let Some(marker) = operation.ffi {
            check_marker(operation.name, "C header", &header, marker);
        }
        if let Some(marker) = operation.napi {
            check_marker(operation.name, "napi", &napi_src, marker);
        }
        if let Some(marker) = operation.facade {
            check_marker(operation.name, "JS facade", &facade, marker);
        }
        if let Some(marker) = operation.dts {
            check_marker(operation.name, "TypeScript", &dts, marker);
        }
        if let Some(marker) = operation.mcp {
            check_marker(operation.name, "MCP", &mcp, marker);
        }
        if let Some(marker) = operation.events {
            check_marker(operation.name, "events", &events, marker);
        }
        check_marker(operation.name, "docs", &docs, operation.docs);
        if let Some(marker) = operation.skill {
            check_marker(operation.name, "skill", &skill, marker);
        }
        if let Some(marker) = operation.cli {
            check_marker(operation.name, "CLI", &cli_sources, marker);
        }
    }
}

#[test]
fn cli_json_results_match_documented_result_types() {
    // The docs table is the JSON-schema authority for CLI results; each
    // documented result type must still exist in the common wire crate.
    let root = workspace_root();
    let docs = surface(&root, "docs/agent-api.md");
    for result_type in [
        "AgentListOfflineResult",
        "SpawnResult",
        "AgentCheckResult",
        "AgentRefreshResult",
        "AgentLandResult",
        "AgentCleanResult",
        "LogResult",
        "UndoResult",
        "AgentSendResult",
        "AgentInboxResult",
        "IntegratorAssignResult",
        "IntegratorStatusResult",
        "IntegratorObserveResult",
        "ConflictMaterializeResult",
        "WorkSendResult",
        "WorkStatusResult",
        "ResolutionJob",
        "ResolutionResult",
        "ResolutionStatusProjection",
        "ResolutionApplyOutcome",
        "ResolutionProtocolStatus",
        "HumanResolutionAnswer",
        "CandidateDescriptor",
    ] {
        assert!(
            docs.contains(result_type),
            "documented result type `{result_type}` disappeared from docs/agent-api.md"
        );
    }
}
