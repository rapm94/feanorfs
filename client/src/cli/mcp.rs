use anyhow::Context;
use feanorfs_client::{
    check_agent, land_agent, load_config, refresh_agent, spawn_agent, AgentInboxQuery,
    AgentMessageInput, AgentMessageKind, ResolveKeep, StatusResult, SyncCtx,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug)]
struct InvalidParams(String);

impl std::fmt::Display for InvalidParams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid params: {}", self.0)
    }
}

impl std::error::Error for InvalidParams {}

fn parse_params<T: DeserializeOwned>(tool: &str, params: &Value) -> anyhow::Result<T> {
    serde_json::from_value(params.clone())
        .map_err(|error| anyhow::Error::new(InvalidParams(format!("{tool}: {error}"))))
}

fn response_error_code(error: &anyhow::Error) -> i32 {
    if error.downcast_ref::<InvalidParams>().is_some() {
        -32602
    } else {
        -32000
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsCallParams {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
    /// Optional MCP request metadata supplied by clients such as OpenCode.
    /// FeanorFS does not interpret it, but it is part of the protocol
    /// envelope rather than a tool argument.
    #[serde(default, rename = "_meta")]
    _meta: Option<serde_json::Map<String, Value>>,
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameParams {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSpawnParams {
    name: String,
    #[serde(default)]
    no_sync: bool,
    #[serde(default)]
    replace: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentLandParams {
    name: String,
    #[serde(default)]
    clean: bool,
    #[serde(default)]
    propose: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConflictsKeepParams {
    path: String,
    keep: String,
    #[serde(default)]
    file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceLogParams {
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceUndoParams {
    snapshot_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSendParams {
    #[serde(default)]
    from: Option<String>,
    to: String,
    kind: String,
    body: String,
    #[serde(default)]
    about_snapshot: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentInboxParams {
    #[serde(default, rename = "for")]
    recipient: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegratorStatusParams {
    #[serde(default)]
    assignment_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegratorRevokeParams {
    assignment_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegratorResumeParams {
    #[serde(default)]
    ack_timeout_ms: Option<u64>,
    #[serde(default)]
    fallback_on_blocked: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConflictMaterializeParams {
    #[serde(default)]
    about_snapshot: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkStatusParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionPrepareParams {
    path: String,
    prevention: feanorfs_common::PreventionReason,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionStatusParams {
    #[serde(default)]
    job_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionSubmitParams {
    job_id: String,
    result: feanorfs_common::ResolutionResult,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionApplyParams {
    job_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionJobIdParams {
    job_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionPutParams {
    job_id: String,
    /// Base64-encoded candidate bytes (engine-bound: 64 MiB plaintext).
    base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionAnswerParams {
    job_id: String,
    option: feanorfs_common::HumanResolutionOption,
    /// Required when `option` is `submit_candidate`.
    #[serde(default)]
    candidate_base64: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionProtocolStatusParams {
    #[serde(default)]
    rebuild: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionRevokeParams {
    job_id: String,
    #[serde(default)]
    superseded: bool,
}

const WORK_DECISION_KIND_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "kind": {
            "type": "string",
            "enum": ["accept", "reject", "narrow", "order", "accept_overlap"]
        },
        "reason": { "type": "string", "maxLength": 512 },
        "paths": { "type": "array", "items": { "type": "string" } },
        "concerns": { "type": "array", "items": { "type": "string" } },
        "after": { "type": "string", "description": "Proposal message id this proposal is sequenced after" },
        "overlap": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["exact_path", "directory_containment", "glob_match", "same_concern"] },
                    "path_a": { "type": "string" },
                    "path_b": { "type": "string" },
                    "concern": { "type": "string" }
                },
                "required": ["kind"]
            }
        }
    },
    "required": ["kind"]
}"#;

fn work_schema(extra: serde_json::Value) -> Value {
    let mut schema: serde_json::Value = serde_json::from_str(WORK_DECISION_KIND_SCHEMA)
        .expect("work decision kind schema is static JSON");
    let object = schema.as_object_mut().expect("schema is an object");
    if let serde_json::Value::Object(extra) = extra {
        for (key, value) in extra {
            match key.as_str() {
                "properties" => {
                    let target = object
                        .entry("properties")
                        .or_insert_with(|| json!({}))
                        .as_object_mut()
                        .expect("properties is an object");
                    for (name, subschema) in value.as_object().expect("properties is an object") {
                        target.insert(name.clone(), subschema.clone());
                    }
                }
                "required" => {
                    let target = object
                        .entry("required")
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .expect("required is an array");
                    for name in value.as_array().expect("required is an array") {
                        if !target.contains(name) {
                            target.push(name.clone());
                        }
                    }
                }
                other => {
                    object.insert(other.to_string(), value);
                }
            }
        }
    }
    schema
}

pub async fn run_mcp(current_dir: &Path) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_response(
                    &mut stdout,
                    Value::Null,
                    Err(anyhow::anyhow!("invalid JSON: {e}")),
                )?;
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));

        let result = dispatch(current_dir, method, &params).await;
        write_response(&mut stdout, id, result)?;
    }
    Ok(())
}

fn write_response(
    stdout: &mut io::Stdout,
    id: Value,
    result: anyhow::Result<Value>,
) -> anyhow::Result<()> {
    let resp = match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": response_error_code(&e), "message": e.to_string() }
        }),
    };
    writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
    stdout.flush()?;
    Ok(())
}

fn tool_list() -> Value {
    json!({
        "tools": [
            tool("agent_spawn", "Spawn an agent workspace", json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "no_sync": { "type": "boolean" },
                    "replace": { "type": "boolean" }
                },
                "required": ["name"]
            })),
            tool("agent_check", "Preview agent changes", json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            })),
            tool("agent_refresh", "Refresh agent from server", json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            })),
            tool("agent_land", "Land agent work", json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "clean": { "type": "boolean" },
                    "propose": { "type": "boolean" }
                },
                "required": ["name"]
            })),
            tool("conflicts_list", "List pending conflicts", json!({ "type": "object", "properties": {} })),
            tool("conflicts_keep", "Resolve a conflict", json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "keep": { "type": "string", "enum": ["local", "cloud", "both", "file"] },
                    "file": { "type": "string" }
                },
                "required": ["path", "keep"]
            })),
            tool("sync_status", "Concise workspace sync status", json!({ "type": "object", "properties": {} })),
            tool("workspace_log", "List workspace snapshot history", json!({
                "type": "object",
                "properties": { "limit": { "type": "integer", "minimum": 0, "maximum": 1000 } }
            })),
            tool("workspace_undo", "Restore a reachable snapshot", json!({
                "type": "object",
                "properties": { "snapshot_id": { "type": "string", "minLength": 8 } },
                "required": ["snapshot_id"]
            })),
            tool("agent_send", "Send an encrypted agent signal tied to a snapshot. All workspace participants can read messages; identity is advisory; requests and results should carry exact snapshot context. Never send credentials, recovery material, or .env values.", json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Explicit sender for controlled automation; defaults to FEANORFS_AGENT or human" },
                    "to": { "type": "string", "description": "Recipient agent name, or * to broadcast" },
                    "kind": { "type": "string", "enum": ["request", "status", "result", "blocked"] },
                    "body": { "type": "string", "minLength": 1, "maxLength": 8192 },
                    "about_snapshot": { "type": "string", "description": "Snapshot this signal concerns; defaults to the current workspace head" },
                    "reply_to": { "type": "string", "description": "Signal snapshot being answered" }
                },
                "required": ["to", "kind", "body"]
            })),
            tool("agent_inbox", "Read encrypted agent signals addressed to you or broadcast to *. All workspace participants can read messages; identity is advisory. Pass back the previous `after` cursor to read only new signals; a reset cursor means older signals may have been missed.", json!({
                "type": "object",
                "properties": {
                    "for": { "type": "string", "description": "Recipient identity; defaults to FEANORFS_AGENT or human" },
                    "after": { "type": "string", "description": "Previous inbox cursor (workspace head)" },
                    "limit": { "type": "integer", "minimum": 0, "maximum": 1000 }
                }
            })),
            tool("integrator_assign", "Randomly rank eligible candidates and offer one the integrator assignment for a batch. Identity and assignment are advisory, not access control; one dispatcher per active batch owns the roster; FeanorFS never merges file content. Requires an explicit candidate roster from the authorized dispatcher.", json!({
                "type": "object",
                "properties": {
                    "about_snapshot": { "type": "string", "description": "Full reachable format-v3 snapshot the batch concerns" },
                    "candidates": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "capabilities": { "type": "array", "items": { "type": "string" } },
                                "enabled": { "type": "boolean" },
                                "available": { "type": "boolean" }
                            },
                            "required": ["name"]
                        },
                        "minItems": 1,
                        "maxItems": 64
                    },
                    "required_capabilities": { "type": "array", "items": { "type": "string" } },
                    "conflict_authors": { "type": "array", "items": { "type": "string" } },
                    "excluded": { "type": "array", "items": { "type": "string" } },
                    "task_summary": { "type": "string", "minLength": 1, "maxLength": 1024 },
                    "ack_timeout_ms": { "type": "integer", "minimum": 0 }
                },
                "required": ["about_snapshot", "candidates", "task_summary"]
            })),
            tool("integrator_status", "Read the active integrator assignment (or one by id). Identity and assignment are advisory; this is not a security claim.", json!({
                "type": "object",
                "properties": {
                    "assignment_id": { "type": "string", "description": "Defaults to the active assignment" }
                }
            })),
            tool("integrator_revoke", "Explicitly revoke the active integrator assignment with a bounded reason. Replaces an accepted/active integrator with the next recorded candidate when one remains; revoking an offered attempt cancels the assignment.", json!({
                "type": "object",
                "properties": {
                    "assignment_id": { "type": "string" },
                    "reason": { "type": "string", "minLength": 1, "maxLength": 512 }
                },
                "required": ["assignment_id", "reason"]
            })),
            tool("integrator_resume", "Resume dispatcher observation after a restart: reads ffint1 replies since the persisted cursor and applies lifecycle transitions. Never re-sends a recorded request; cursor reset or lost state stops automatic mutation.", json!({
                "type": "object",
                "properties": {
                    "ack_timeout_ms": { "type": "integer", "description": "Pre-acceptance acknowledgement timeout" },
                    "fallback_on_blocked": { "type": "boolean", "description": "Allow fallback to the next ranked candidate after a candidate blocker" }
                }
            })),
            tool("conflict_materialize", "Materialize the encrypted conflict triple for a snapshot on this machine. Read-only: writes .original/.local/.cloud artifacts under private global FeanorFS state and registers local pending rows without changing the shared head. Refuses stale or already-resolved conflicts.", json!({
                "type": "object",
                "properties": {
                    "about_snapshot": { "type": "string", "description": "Defaults to the current workspace head" },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional path subset" }
                }
            })),
            tool("work_propose", "Propose bounded work scope for one task as an encrypted ffwork1 intent. The proposal is NOT accepted until an observed coordinator decision; never claim a proposed scope is accepted. Paths must be canonical workspace-relative paths or `dir/**` containment globs, sorted and unique.", json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "minLength": 1, "maxLength": 128 },
                    "agent": { "type": "string", "description": "Proposal author; defaults to FEANORFS_AGENT or human" },
                    "sequence": { "type": "integer", "minimum": 1 },
                    "causal_base": { "type": "string", "description": "Immutable message id this proposal builds on" },
                    "coordinator": { "type": "string", "description": "Named coordinator identity whose decisions are authorized" },
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 64 },
                    "concerns": { "type": "array", "items": { "type": "string" } },
                    "dependencies": { "type": "array", "items": { "type": "string" } },
                    "capabilities": { "type": "array", "items": { "type": "string" } },
                    "about_snapshot": { "type": "string" },
                    "to": { "type": "string" }
                },
                "required": ["task_id", "sequence", "paths"]
            })),
            tool("work_decide", "Send one coordinator decision (accept, reject, narrow, order, accept-overlap) for an exact proposal message id. Only the proposal's named coordinator (or the operating context) is authorized; the hub never decides. Applies only once the reducer observes it.", work_schema(json!({
                "properties": {
                    "proposal_message_id": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "about_snapshot": { "type": "string" },
                    "to": { "type": "string" },
                    "from": { "type": "string" }
                },
                "required": ["proposal_message_id", "kind"]
            }))),
            tool("work_amend", "Amend the accepted scope of one intent with replacement paths/concerns/dependencies. Author-side only; applies once observed.", json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "intent_message_id": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "sequence": { "type": "integer", "minimum": 1 },
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "concerns": { "type": "array", "items": { "type": "string" } },
                    "dependencies": { "type": "array", "items": { "type": "string" } },
                    "reason": { "type": "string", "maxLength": 512 },
                    "about_snapshot": { "type": "string" },
                    "to": { "type": "string" },
                    "from": { "type": "string" }
                },
                "required": ["task_id", "intent_message_id", "sequence"]
            })),
            tool("work_yield", "Explicitly relinquish accepted overlap while preserving local work. Author-side only; applies once observed.", json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "intent_message_id": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "sequence": { "type": "integer", "minimum": 1 },
                    "reason": { "type": "string", "maxLength": 512 },
                    "about_snapshot": { "type": "string" },
                    "to": { "type": "string" },
                    "from": { "type": "string" }
                },
                "required": ["task_id", "intent_message_id", "sequence"]
            })),
            tool("work_settle", "Mark accepted changes reconciled with verification evidence for one intent. Author-side only; applies once observed.", json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "intent_message_id": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "sequence": { "type": "integer", "minimum": 1 },
                    "inspected_snapshot": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "verification": {
                        "type": "object",
                        "properties": {
                            "status": { "type": "string", "enum": ["passed", "failed", "skipped"] },
                            "summary": { "type": "string", "maxLength": 512 }
                        },
                        "required": ["status", "summary"]
                    },
                    "about_snapshot": { "type": "string" },
                    "to": { "type": "string" },
                    "from": { "type": "string" }
                },
                "required": ["task_id", "intent_message_id", "sequence", "inspected_snapshot", "verification"]
            })),
            tool("work_complete", "Terminal success for a settled task. Author-side only; applies once observed.", json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "intent_message_id": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "sequence": { "type": "integer", "minimum": 1 },
                    "outcome": { "type": "string", "maxLength": 512 },
                    "about_snapshot": { "type": "string" },
                    "to": { "type": "string" },
                    "from": { "type": "string" }
                },
                "required": ["task_id", "intent_message_id", "sequence", "outcome"]
            })),
            tool("work_block", "Terminal automation blocker for an accepted intent. Author-side only; applies once observed.", json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "intent_message_id": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "sequence": { "type": "integer", "minimum": 1 },
                    "reason": { "type": "string", "minLength": 1, "maxLength": 512 },
                    "about_snapshot": { "type": "string" },
                    "to": { "type": "string" },
                    "from": { "type": "string" }
                },
                "required": ["task_id", "intent_message_id", "sequence", "reason"]
            })),
            tool("work_status", "Observe signals through the deterministic ffwork1 reducer and report the bounded projection: tasks, proposal states, accepted scope, decisions, evidence, and projection completeness. A projection_incomplete result means acceptance is not fully provable. Coordinator authority is derived from authenticated protocol state only; no caller-supplied coordinator is accepted.", json!({
                "type": "object",
                "properties": {}
            })),
            tool("resolution_prepare", "Prepare one automatic resolution job for the exact current conflict at a canonical workspace-relative path. Requires a real current conflict and a typed prevention-exhausted/violated reason. Read-only: writes a job under the protected orchestrator boundary without changing the worktree, conflict registry, artifacts, or head. Legacy unfingerprinted conflicts are refused.", json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Canonical workspace-relative conflict path" },
                    "prevention": {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["exhausted", "violated"] },
                            "detail": { "type": "string", "minLength": 1, "maxLength": 1024 }
                        },
                        "required": ["type", "detail"],
                        "additionalProperties": false
                    }
                },
                "required": ["path", "prevention"]
            })),
            tool("resolution_status", "Read the bounded resolution status projection (ids/state/counts only; never paths or bodies). Read-only and constant-cost; unknown job ids yield an empty projection.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Restrict the projection to one job" }
                }
            })),
            tool("resolution_submit", "Submit one resolution result for an exact job. Submission NEVER applies: it validates result schema/bounds, assignment/attempt/owner/fingerprint, and the immutable candidate, then records the result without mutating the worktree, conflict registry, artifacts, or head. Apply is a separate explicit operation.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "result": {
                        "type": "object",
                        "description": "Full ResolutionResult document (closed outcome set; candidate_ready requires a candidate descriptor and passed verification; requires_human carries exactly one bounded question and a typed human reason)"
                    }
                },
                "required": ["job_id", "result"]
            })),
            tool("resolution_apply", "Apply a submitted resolution result with guarded publication: revalidates every identity field and the candidate descriptor immediately before a single CAS; a lost CAS restarts complete validation. The current conflict survives unchanged for any typed stale outcome.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" }
                },
                "required": ["job_id"]
            })),
            tool("resolution_materialize", "Materialize the authenticated base/ours/theirs legs of one resolution job into the engine-owned job directory (create-new, no-follow, fsync'd) so a designated machine can reconstruct the conflict context by ID and fingerprint. Read-only: never changes the worktree, conflict registry, artifacts, or head.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" }
                },
                "required": ["job_id"]
            })),
            tool("resolution_put", "Write the immutable engine-owned candidate file for one job from bounded base64 bytes (create-new, no-follow, fsync'd) and return its plaintext descriptor. Allowed while the job is active and carries no candidate-bearing result.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "base64": { "type": "string", "description": "Base64-encoded candidate bytes (engine-bound: 64 MiB plaintext)" }
                },
                "required": ["job_id", "base64"]
            })),
            tool("resolution_answer", "Record one typed human answer bound to the exact current escalation. Every identity field (job/assignment/attempt/fingerprint/question generation) is bound to the live projection — the caller never supplies them, so stale answers are impossible by construction. Never publishes; use resolution_publish_answer for the ffres1 profile.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "option": { "type": "string", "enum": ["defer", "keep_unresolved", "submit_candidate"] },
                    "candidate_base64": { "type": "string", "description": "Required for submit_candidate; base64 candidate bytes (engine-bound: 64 MiB plaintext)" }
                },
                "required": ["job_id", "option"]
            })),
            tool("resolution_defer", "Record the terminal Deferred state for one assignment without any publication; the conflict is preserved for later manual action.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" }
                },
                "required": ["job_id"]
            })),
            tool("resolution_protocol_status", "Observe the encrypted signal stream through the deterministic ffres1 reducer and report the bounded metadata-only projection (ids/state/counts only; never paths or bodies). `rebuild` resets the cursor and re-observes the bounded window.", json!({
                "type": "object",
                "properties": {
                    "rebuild": { "type": "boolean" }
                }
            })),
            tool("resolution_assign", "Publish the ffres1 assignment profile (with the complete immutable job) for one locally prepared job.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" }
                },
                "required": ["job_id"]
            })),
            tool("resolution_reply", "Publish the ffres1 result profile for one locally submitted job.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" }
                },
                "required": ["job_id"]
            })),
            tool("resolution_revoke", "Publish the ffres1 revoke/supersede profile for one local job.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "superseded": { "type": "boolean", "description": "Mark the assignment superseded rather than revoked" }
                },
                "required": ["job_id"]
            })),
            tool("resolution_publish_answer", "Publish one typed human answer as an ffres1 profile. The answer is built exactly like resolution_answer (bound to the live projection), then validated and sent for remote observation; the local store is never mutated by publication.", json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "option": { "type": "string", "enum": ["defer", "keep_unresolved", "submit_candidate"] },
                    "candidate_base64": { "type": "string", "description": "Required for submit_candidate; base64 candidate bytes (engine-bound: 64 MiB plaintext)" }
                },
                "required": ["job_id", "option"]
            })),
        ]
    })
}

fn tool(name: &str, description: &str, mut schema: Value) -> Value {
    if let Some(object) = schema.as_object_mut() {
        object.insert("additionalProperties".into(), Value::Bool(false));
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema
    })
}

fn agent_identity(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("FEANORFS_AGENT").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "human".to_string())
}

async fn dispatch(current_dir: &Path, method: &str, params: &Value) -> anyhow::Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "feanorfs", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(tool_list()),
        "tools/call" => {
            let params: ToolsCallParams = parse_params("tools/call", params)?;
            let result = call_tool(current_dir, &params.name, &params.arguments).await?;
            Ok(mcp_tool_result(result)?)
        }
        // Legacy direct RPC (MCP-1 compat)
        other => call_tool(current_dir, other, params).await,
    }
}

fn mcp_tool_result(result: Value) -> anyhow::Result<Value> {
    let text = serde_json::to_string(&result)?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": result
    }))
}

async fn call_tool(current_dir: &Path, tool: &str, params: &Value) -> anyhow::Result<Value> {
    let control_root = super::agent::control_workspace_root(current_dir)?;
    let config = load_config(&control_root)?;
    let db = crate::open_client_db(&control_root).await?;
    let api = crate::open_api_client(&control_root, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, &control_root, &config)?;
    let pw = config.encryption_password.as_deref();

    match tool {
        "agent_spawn" => {
            let params: AgentSpawnParams = parse_params(tool, params)?;
            let count = spawn_agent(
                &control_root,
                &db,
                &api,
                &config.workspace_id,
                &params.name,
                pw,
                params.no_sync,
                params.replace,
            )
            .await?;
            Ok(json!({ "files_copied": count }))
        }
        "agent_check" => {
            let params: NameParams = parse_params(tool, params)?;
            let r = check_agent(
                &control_root,
                &db,
                &api,
                &config.workspace_id,
                &params.name,
                pw,
            )
            .await?;
            Ok(serde_json::to_value(r)?)
        }
        "agent_refresh" => {
            let params: NameParams = parse_params(tool, params)?;
            let r = refresh_agent(
                &control_root,
                &db,
                &api,
                &config.workspace_id,
                &params.name,
                pw,
            )
            .await?;
            Ok(serde_json::to_value(r)?)
        }
        "agent_land" => {
            let params: AgentLandParams = parse_params(tool, params)?;
            let r = land_agent(
                &control_root,
                &db,
                &api,
                &config.workspace_id,
                &params.name,
                pw,
                params.clean,
                params.propose,
            )
            .await?;
            Ok(serde_json::to_value(r)?)
        }
        "conflicts_list" => {
            let _: EmptyParams = parse_params(tool, params)?;
            let records = db.list_conflict_records().await?;
            Ok(serde_json::to_value(records)?)
        }
        "conflicts_keep" => {
            let params: ConflictsKeepParams = parse_params(tool, params)?;
            let keep = match params.keep.as_str() {
                "local" => ResolveKeep::Local,
                "cloud" => ResolveKeep::Cloud,
                "both" => ResolveKeep::Both,
                "file" => ResolveKeep::File,
                other => anyhow::bail!("unknown keep value: {other}"),
            };
            let file_source = params.file.as_deref().map(|source| {
                let source = PathBuf::from(source);
                if source.is_absolute() {
                    source
                } else {
                    control_root.join(source)
                }
            });
            if matches!(keep, ResolveKeep::File) && file_source.is_none() {
                anyhow::bail!("conflicts_keep with keep=file requires a `file` param");
            }
            feanorfs_client::conflicts::resolve_conflict(
                &ctx,
                &params.path,
                keep,
                file_source.as_deref(),
            )
            .await?;
            Ok(json!({ "resolved": params.path }))
        }
        "sync_status" => {
            let _: EmptyParams = parse_params(tool, params)?;
            let r = feanorfs_client::do_status(&api, &db, &control_root, &config.workspace_id, pw)
                .await?;
            Ok(compact_sync_status(r))
        }
        "workspace_log" => {
            let params: WorkspaceLogParams = parse_params(tool, params)?;
            let limit = params
                .limit
                .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                .unwrap_or(20);
            let result = feanorfs_agent_core::history::log(&ctx, limit).await?;
            Ok(serde_json::to_value(result)?)
        }
        "workspace_undo" => {
            let params: WorkspaceUndoParams = parse_params(tool, params)?;
            let result = feanorfs_agent_core::history::undo(&ctx, &params.snapshot_id).await?;
            Ok(serde_json::to_value(result)?)
        }
        "agent_send" => {
            let params: AgentSendParams = parse_params(tool, params)?;
            let kind = match params.kind.as_str() {
                "request" => AgentMessageKind::Request,
                "status" => AgentMessageKind::Status,
                "result" => AgentMessageKind::Result,
                "blocked" => AgentMessageKind::Blocked,
                _ => anyhow::bail!("kind must be request, status, result, or blocked"),
            };
            let result = feanorfs_agent_core::send_message(
                &ctx,
                AgentMessageInput {
                    to: params.to,
                    kind,
                    body: params.body,
                    about_snapshot: params.about_snapshot,
                    reply_to: params.reply_to,
                    from: Some(agent_identity(params.from.as_deref())),
                },
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "agent_inbox" => {
            let params: AgentInboxParams = parse_params(tool, params)?;
            let recipient = agent_identity(params.recipient.as_deref());
            let limit = params
                .limit
                .map(|value| usize::try_from(value).unwrap_or(50))
                .unwrap_or(50);
            let result = feanorfs_agent_core::inbox(
                &ctx,
                AgentInboxQuery {
                    recipient,
                    after: params.after,
                    limit,
                },
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_assign" => {
            let input: feanorfs_client::IntegratorAssignInput = parse_params(tool, params)?;
            let result = feanorfs_client::integrator_assign(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_status" => {
            let params: IntegratorStatusParams = parse_params(tool, params)?;
            let result =
                feanorfs_client::integrator_status(&ctx, params.assignment_id.as_deref()).await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_revoke" => {
            let params: IntegratorRevokeParams = parse_params(tool, params)?;
            let result =
                feanorfs_client::integrator_revoke(&ctx, &params.assignment_id, &params.reason)
                    .await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_resume" => {
            let params: IntegratorResumeParams = parse_params(tool, params)?;
            let result = feanorfs_client::integrator_resume(
                &ctx,
                feanorfs_client::IntegratorObserveOptions {
                    ack_timeout_ms: params.ack_timeout_ms,
                    fallback_on_blocked: params.fallback_on_blocked,
                },
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "conflict_materialize" => {
            let params: ConflictMaterializeParams = parse_params(tool, params)?;
            let about = match params.about_snapshot {
                Some(snapshot) => snapshot,
                None => ctx
                    .api
                    .get_head(ctx.workspace_id())
                    .await?
                    .context("workspace has no snapshot to materialize conflicts from")?,
            };
            let result =
                feanorfs_client::materialize_conflicts(&ctx, &about, &params.paths).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_propose" => {
            let input: feanorfs_common::WorkProposeInput = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_propose(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_decide" => {
            let input: feanorfs_common::WorkDecideInput = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_decide(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_amend" => {
            let input: feanorfs_common::WorkAmendInput = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_amend(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_yield" => {
            let input: feanorfs_common::WorkYieldInput = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_yield(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_settle" => {
            let input: feanorfs_common::WorkSettleInput = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_settle(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_complete" => {
            let input: feanorfs_common::WorkCompleteInput = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_complete(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_block" => {
            let input: feanorfs_common::WorkBlockInput = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_block(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "work_status" => {
            let _params: WorkStatusParams = parse_params(tool, params)?;
            let result = feanorfs_agent_core::work::work_status(
                &ctx,
                feanorfs_common::WorkStatusInput::default(),
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "resolution_prepare" => {
            let params: ResolutionPrepareParams = parse_params(tool, params)?;
            let job =
                feanorfs_agent_core::prepare_resolution_job(&ctx, &params.path, params.prevention)
                    .await?;
            Ok(serde_json::to_value(job)?)
        }
        "resolution_status" => {
            let params: ResolutionStatusParams = parse_params(tool, params)?;
            let projection =
                feanorfs_agent_core::resolution_status(&ctx, params.job_id.as_deref()).await?;
            Ok(serde_json::to_value(projection)?)
        }
        "resolution_submit" => {
            let params: ResolutionSubmitParams = parse_params(tool, params)?;
            let result =
                feanorfs_agent_core::submit_resolution_result(&ctx, &params.job_id, params.result)
                    .await?;
            Ok(serde_json::to_value(result)?)
        }
        "resolution_apply" => {
            let params: ResolutionApplyParams = parse_params(tool, params)?;
            let outcome = feanorfs_agent_core::apply_resolution_job(&ctx, &params.job_id).await?;
            Ok(serde_json::to_value(outcome)?)
        }
        "resolution_materialize" => {
            let params: ResolutionJobIdParams = parse_params(tool, params)?;
            let legs =
                feanorfs_agent_core::materialize_resolution_legs(&ctx, &params.job_id).await?;
            let mapped: Vec<serde_json::Value> = legs
                .into_iter()
                .map(|(role, path)| {
                    serde_json::to_value(&path)
                        .map(|path| serde_json::json!({ "role": role.as_str(), "path": path }))
                })
                .collect::<serde_json::Result<_>>()?;
            Ok(serde_json::Value::Array(mapped))
        }
        "resolution_put" => {
            let params: ResolutionPutParams = parse_params(tool, params)?;
            let bytes = decode_candidate_base64(&params.base64)?;
            let descriptor =
                feanorfs_agent_core::put_resolution_candidate(&ctx, &params.job_id, &bytes).await?;
            Ok(serde_json::to_value(descriptor)?)
        }
        "resolution_answer" => {
            let params: ResolutionAnswerParams = parse_params(tool, params)?;
            let answer = build_mcp_human_answer(
                &ctx,
                &params.job_id,
                params.option,
                params.candidate_base64.as_deref(),
            )
            .await?;
            let recorded = feanorfs_agent_core::answer_resolution(&ctx, answer).await?;
            Ok(serde_json::to_value(recorded)?)
        }
        "resolution_defer" => {
            let params: ResolutionJobIdParams = parse_params(tool, params)?;
            feanorfs_agent_core::defer_resolution(&ctx, &params.job_id).await?;
            Ok(serde_json::Value::Null)
        }
        "resolution_protocol_status" => {
            let params: ResolutionProtocolStatusParams = parse_params(tool, params)?;
            let status =
                feanorfs_agent_core::resolution_protocol_status(&ctx, params.rebuild).await?;
            Ok(serde_json::to_value(status)?)
        }
        "resolution_assign" => {
            let params: ResolutionJobIdParams = parse_params(tool, params)?;
            let message_id =
                feanorfs_agent_core::send_resolution_assignment(&ctx, &params.job_id).await?;
            Ok(json!({ "message_id": message_id }))
        }
        "resolution_reply" => {
            let params: ResolutionJobIdParams = parse_params(tool, params)?;
            let message_id =
                feanorfs_agent_core::send_resolution_result(&ctx, &params.job_id).await?;
            Ok(json!({ "message_id": message_id }))
        }
        "resolution_revoke" => {
            let params: ResolutionRevokeParams = parse_params(tool, params)?;
            let message_id = feanorfs_agent_core::send_resolution_revoke(
                &ctx,
                &params.job_id,
                params.superseded,
            )
            .await?;
            Ok(json!({ "message_id": message_id }))
        }
        "resolution_publish_answer" => {
            let params: ResolutionAnswerParams = parse_params(tool, params)?;
            let mut answer = build_mcp_human_answer(
                &ctx,
                &params.job_id,
                params.option,
                params.candidate_base64.as_deref(),
            )
            .await?;
            if matches!(
                answer.chosen_option,
                feanorfs_common::HumanResolutionOption::SubmitCandidate
            ) {
                // The `ffres1` profile requires verification evidence; the
                // answering machine cannot fabricate engine evidence, so the
                // published answer carries an explicit Unknown status (the
                // candidate descriptor itself was engine-validated by the
                // `put` step above).
                answer.verification = Some(feanorfs_common::VerificationSummary {
                    status: feanorfs_common::VerificationStatus::Unknown,
                    summary: "human submit_candidate answer; engine inline verification \
                              not executed on this machine"
                        .to_string(),
                    ..Default::default()
                });
            }
            feanorfs_common::validate_human_resolution_answer(&answer)?;
            let message_id = feanorfs_agent_core::send_human_answer(&ctx, &answer).await?;
            Ok(json!({ "message_id": message_id }))
        }
        other => anyhow::bail!("unknown method: {other}"),
    }
}

/// Decodes bounded base64 candidate bytes; anything over the engine's
/// 64 MiB plaintext bound fails closed before the engine call.
fn decode_candidate_base64(encoded: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| anyhow::anyhow!("invalid candidate base64: {error}"))?;
    anyhow::ensure!(
        bytes.len() as u64 <= feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES,
        "candidate base64 exceeds the 64 MiB bound"
    );
    Ok(bytes)
}

/// Builds one human answer bound to the exact current escalation: every
/// identity field (job, assignment, attempt, fingerprint, question
/// generation) is read from the bounded `resolution_status` projection so a
/// stale answer is impossible by construction. A `submit_candidate` answer
/// first records the engine-owned candidate via `put_resolution_candidate`;
/// verification evidence is left `None` and the engine's answer path runs
/// the inline verification.
async fn build_mcp_human_answer(
    ctx: &SyncCtx<'_>,
    job_id: &str,
    option: feanorfs_common::HumanResolutionOption,
    candidate_base64: Option<&str>,
) -> anyhow::Result<feanorfs_common::HumanResolutionAnswer> {
    let projection = feanorfs_agent_core::resolution_status(ctx, Some(job_id)).await?;
    let job = projection
        .jobs
        .iter()
        .find(|job| job.job_id == job_id)
        .with_context(|| format!("unknown resolution job {job_id}; answer refused"))?;
    let candidate = match candidate_base64 {
        Some(encoded) => {
            let bytes = decode_candidate_base64(encoded)?;
            Some(feanorfs_agent_core::put_resolution_candidate(ctx, job_id, &bytes).await?)
        }
        None => None,
    };
    Ok(feanorfs_common::HumanResolutionAnswer {
        schema_version: feanorfs_common::RESOLUTION_SCHEMA_VERSION,
        job_id: job.job_id.clone(),
        assignment_id: job.assignment_id.clone(),
        attempt: job.attempt,
        conflict_fingerprint: job.conflict_fingerprint.clone(),
        question_generation: job.question_generation,
        chosen_option: option,
        candidate,
        verification: None,
    })
}

fn compact_sync_status(status: StatusResult) -> Value {
    json!({
        "mirror_state": status.mirror_state,
        "local_file_count": status.local_files.len(),
        "upload_required": status.upload_required,
        "download_required": status
            .download_required
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>(),
        "delete_local": status.delete_local,
        "pending_conflicts": status.pending_conflicts,
        "offline_backlog": status.offline_backlog,
        "server_rollback_warning": status.server_rollback_warning,
        "skipped_symlink_count": status.skipped_symlinks.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        agent_identity, compact_sync_status, mcp_tool_result, parse_params, response_error_code,
        tool_list, AgentInboxParams, AgentSendParams, IntegratorResumeParams, ToolsCallParams,
    };
    use feanorfs_client::{MirrorState, StatusResult};
    use feanorfs_common::FileState;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn sync_status_is_compact_but_keeps_actionable_paths() {
        let local_files = HashMap::from([(
            "src/main.rs".to_string(),
            FileState {
                path: "src/main.rs".to_string(),
                hash: "a".repeat(64),
                size: 42,
                mtime: 7,
                deleted: false,
                mode: 0,
            },
        )]);
        let status = StatusResult {
            mirror_state: MirrorState::OutOfSync,
            upload_required: vec!["src/main.rs".to_string()],
            download_required: vec![FileState {
                path: "README.md".to_string(),
                hash: "b".repeat(64),
                size: 12,
                mtime: 8,
                deleted: false,
                mode: 0,
            }],
            delete_local: Vec::new(),
            local_files,
            pending_conflicts: Vec::new(),
            offline_backlog: 0,
            server_rollback_warning: None,
            skipped_symlinks: vec!["linked-cache".to_string()],
        };

        let value = compact_sync_status(status);
        assert_eq!(value["mirror_state"], "out_of_sync");
        assert_eq!(value["local_file_count"], 1);
        assert_eq!(value["upload_required"], json!(["src/main.rs"]));
        assert_eq!(value["download_required"], json!(["README.md"]));
        assert_eq!(value["skipped_symlink_count"], 1);
        assert!(value.get("local_files").is_none());
    }

    #[test]
    fn tool_list_declares_bounded_agent_message_schemas() {
        let list = tool_list();
        let tools = list["tools"].as_array().unwrap();
        let send = tools
            .iter()
            .find(|tool| tool["name"] == "agent_send")
            .expect("agent_send tool must be declared");
        let send_schema = &send["inputSchema"];
        assert_eq!(send_schema["additionalProperties"], false);
        assert_eq!(
            send_schema["properties"]["kind"]["enum"],
            json!(["request", "status", "result", "blocked"])
        );
        assert_eq!(send_schema["required"], json!(["to", "kind", "body"]));
        assert_eq!(send_schema["properties"]["body"]["maxLength"], 8192);

        let inbox = tools
            .iter()
            .find(|tool| tool["name"] == "agent_inbox")
            .expect("agent_inbox tool must be declared");
        let inbox_schema = &inbox["inputSchema"];
        assert_eq!(inbox_schema["additionalProperties"], false);
        assert_eq!(inbox_schema["properties"]["limit"]["maximum"], 1000);
        assert!(inbox_schema["properties"]["for"].is_object());
        assert!(inbox_schema["properties"]["after"].is_object());
        assert!(
            inbox_schema.get("required").is_none(),
            "agent_inbox has no required parameters"
        );
    }

    #[test]
    fn explicit_agent_identity_uses_human_for_blank_values() {
        assert_eq!(agent_identity(Some("mac-test")), "mac-test");
        assert_eq!(agent_identity(Some("")), "human");
    }

    #[test]
    fn resolution_tools_are_bounded_and_submit_never_applies() {
        use super::{
            ResolutionAnswerParams, ResolutionApplyParams, ResolutionJobIdParams,
            ResolutionPrepareParams, ResolutionProtocolStatusParams, ResolutionPutParams,
            ResolutionRevokeParams, ResolutionStatusParams,
        };
        let list = tool_list();
        let tools = list["tools"].as_array().unwrap();

        let prepare = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_prepare")
            .expect("resolution_prepare tool must be declared");
        let prepare_schema = &prepare["inputSchema"];
        assert_eq!(prepare_schema["additionalProperties"], false);
        assert_eq!(
            prepare_schema["properties"]["prevention"]["properties"]["type"]["enum"],
            json!(["exhausted", "violated"])
        );
        assert_eq!(prepare_schema["required"], json!(["path", "prevention"]));
        assert!(
            prepare["description"]
                .as_str()
                .unwrap()
                .contains("without changing the worktree"),
            "prepare description must state read-only semantics"
        );

        let submit = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_submit")
            .expect("resolution_submit tool must be declared");
        let submit_schema = &submit["inputSchema"];
        assert_eq!(submit_schema["additionalProperties"], false);
        assert_eq!(submit_schema["required"], json!(["job_id", "result"]));
        assert!(
            submit["description"]
                .as_str()
                .unwrap()
                .contains("NEVER applies"),
            "submit description must state that submission never applies"
        );

        let apply = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_apply")
            .expect("resolution_apply tool must be declared");
        assert!(apply["description"]
            .as_str()
            .unwrap()
            .contains("guarded publication"));

        let status = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_status")
            .expect("resolution_status tool must be declared");
        assert!(status["description"]
            .as_str()
            .unwrap()
            .contains("ids/state/counts only"));

        // The new protocol/answer tools are declared with strict bounded
        // schemas.
        for name in [
            "resolution_materialize",
            "resolution_put",
            "resolution_answer",
            "resolution_defer",
            "resolution_protocol_status",
            "resolution_assign",
            "resolution_reply",
            "resolution_revoke",
            "resolution_publish_answer",
        ] {
            let entry = tools
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} tool must be declared"));
            assert_eq!(entry["inputSchema"]["additionalProperties"], false);
        }
        let answer = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_answer")
            .unwrap();
        assert_eq!(
            answer["inputSchema"]["properties"]["option"]["enum"],
            json!(["defer", "keep_unresolved", "submit_candidate"])
        );
        assert_eq!(
            answer["inputSchema"]["required"],
            json!(["job_id", "option"])
        );
        let put = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_put")
            .unwrap();
        assert_eq!(put["inputSchema"]["required"], json!(["job_id", "base64"]));
        let revoke = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_revoke")
            .unwrap();
        assert_eq!(revoke["inputSchema"]["required"], json!(["job_id"]));
        let protocol_status = tools
            .iter()
            .find(|tool| tool["name"] == "resolution_protocol_status")
            .unwrap();
        assert!(
            protocol_status["inputSchema"].get("required").is_none(),
            "resolution_protocol_status has no required parameters"
        );

        // Strict params reject unknown fields and require the typed
        // prevention payload.
        let bad = match parse_params::<ResolutionPrepareParams>(
            "resolution_prepare",
            &json!({ "path": "src/main.rs", "prevention": { "type": "exhausted" } }),
        ) {
            Ok(_) => panic!("missing prevention detail was accepted"),
            Err(error) => error,
        };
        assert_eq!(response_error_code(&bad), -32602);

        let unknown = match parse_params::<ResolutionStatusParams>(
            "resolution_status",
            &json!({ "job_id": "abc", "unexpected": true }),
        ) {
            Ok(_) => panic!("unknown field was accepted"),
            Err(error) => error,
        };
        assert_eq!(response_error_code(&unknown), -32602);

        let defaults: ResolutionStatusParams =
            parse_params("resolution_status", &json!({})).unwrap();
        assert!(defaults.job_id.is_none());

        assert!(
            parse_params::<ResolutionApplyParams>("resolution_apply", &json!({ "job_id": 7 }),)
                .is_err()
        );

        // The new strict params reject unknown fields, missing required
        // fields, and wrong types.
        assert!(parse_params::<ResolutionJobIdParams>(
            "resolution_materialize",
            &json!({ "job_id": "abc", "unexpected": true }),
        )
        .is_err());
        assert!(parse_params::<ResolutionPutParams>("resolution_put", &json!({})).is_err());
        let missing_candidate = parse_params::<ResolutionAnswerParams>(
            "resolution_answer",
            &json!({ "job_id": "abc", "option": "submit_candidate" }),
        )
        .unwrap();
        assert!(missing_candidate.candidate_base64.is_none());
        assert!(parse_params::<ResolutionAnswerParams>(
            "resolution_answer",
            &json!({ "job_id": "abc", "option": "sideways" }),
        )
        .is_err());
        let defaults: ResolutionProtocolStatusParams =
            parse_params("resolution_protocol_status", &json!({})).unwrap();
        assert!(!defaults.rebuild);
        let revoke: ResolutionRevokeParams =
            parse_params("resolution_revoke", &json!({ "job_id": "abc" })).unwrap();
        assert!(!revoke.superseded);
    }

    #[test]
    fn strict_params_reject_unknown_and_wrong_optional_fields() {
        let unknown = match parse_params::<AgentSendParams>(
            "agent_send",
            &json!({
                "to": "worker",
                "kind": "request",
                "body": "work",
                "unexpected": true
            }),
        ) {
            Ok(_) => panic!("unknown field was accepted"),
            Err(error) => error,
        };
        assert_eq!(response_error_code(&unknown), -32602);
        assert!(unknown.to_string().contains("unknown field `unexpected`"));

        let wrong_type =
            match parse_params::<AgentInboxParams>("agent_inbox", &json!({ "limit": "50" })) {
                Ok(_) => panic!("wrong optional-field type was accepted"),
                Err(error) => error,
            };
        assert_eq!(response_error_code(&wrong_type), -32602);
        assert!(wrong_type.to_string().contains("invalid type"));
    }

    #[test]
    fn tools_call_accepts_protocol_meta_but_rejects_other_unknown_fields() {
        let params: ToolsCallParams = parse_params(
            "tools/call",
            &json!({
                "name": "agent_inbox",
                "arguments": { "for": "mac-opencode" },
                "_meta": { "progressToken": "request-1" }
            }),
        )
        .unwrap();
        assert_eq!(params.name, "agent_inbox");
        assert_eq!(params.arguments["for"], "mac-opencode");
        assert_eq!(params._meta.as_ref().unwrap()["progressToken"], "request-1");

        let error = match parse_params::<ToolsCallParams>(
            "tools/call",
            &json!({
                "name": "agent_inbox",
                "arguments": {},
                "unexpected": true
            }),
        ) {
            Ok(_) => panic!("unknown envelope field was accepted"),
            Err(error) => error,
        };
        assert_eq!(response_error_code(&error), -32602);
        assert!(error.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn tools_call_result_uses_mcp_content_envelope() {
        let result = mcp_tool_result(json!({ "cursor_reset": false, "messages": [] })).unwrap();
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(
            result["content"][0]["text"],
            r#"{"cursor_reset":false,"messages":[]}"#
        );
        assert_eq!(
            result["structuredContent"],
            json!({ "cursor_reset": false, "messages": [] })
        );
    }

    #[test]
    fn absent_optional_params_keep_documented_defaults() {
        let inbox: AgentInboxParams = parse_params("agent_inbox", &json!({})).unwrap();
        assert!(inbox.recipient.is_none());
        assert!(inbox.after.is_none());
        assert!(inbox.limit.is_none());

        let proposal: feanorfs_common::WorkProposeInput = parse_params(
            "work_propose",
            &json!({
                "task_id": "minimal-proposal",
                "sequence": 1,
                "paths": ["src/main.rs"]
            }),
        )
        .unwrap();
        assert!(proposal.concerns.is_empty());
        assert!(proposal.dependencies.is_empty());
        assert!(proposal.capabilities.is_empty());

        let resume: IntegratorResumeParams = parse_params("integrator_resume", &json!({})).unwrap();
        assert!(resume.ack_timeout_ms.is_none());
        assert!(!resume.fallback_on_blocked);
    }
}
