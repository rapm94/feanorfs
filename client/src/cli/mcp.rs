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
            call_tool(current_dir, &params.name, &params.arguments).await
        }
        // Legacy direct RPC (MCP-1 compat)
        other => call_tool(current_dir, other, params).await,
    }
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
        other => anyhow::bail!("unknown method: {other}"),
    }
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
        agent_identity, compact_sync_status, parse_params, response_error_code, tool_list,
        AgentInboxParams, AgentSendParams, IntegratorResumeParams,
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
    fn absent_optional_params_keep_documented_defaults() {
        let inbox: AgentInboxParams = parse_params("agent_inbox", &json!({})).unwrap();
        assert!(inbox.recipient.is_none());
        assert!(inbox.after.is_none());
        assert!(inbox.limit.is_none());

        let resume: IntegratorResumeParams = parse_params("integrator_resume", &json!({})).unwrap();
        assert!(resume.ack_timeout_ms.is_none());
        assert!(!resume.fallback_on_blocked);
    }
}
