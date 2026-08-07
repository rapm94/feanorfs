use anyhow::Context;
use feanorfs_client::{
    check_agent, land_agent, load_config, refresh_agent, spawn_agent, AgentInboxQuery,
    AgentMessageInput, AgentMessageKind, ResolveKeep, StatusResult, SyncCtx,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;

const PROTOCOL_VERSION: &str = "2024-11-05";

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
            "error": { "code": -32000, "message": e.to_string() }
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

fn tool(name: &str, description: &str, schema: Value) -> Value {
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
            let name = params["name"]
                .as_str()
                .context("tools/call requires name")?;
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(current_dir, name, &args).await
        }
        // Legacy direct RPC (MCP-1 compat)
        other => call_tool(current_dir, other, params).await,
    }
}

async fn call_tool(current_dir: &Path, tool: &str, params: &Value) -> anyhow::Result<Value> {
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, current_dir, &config)?;
    let pw = config.encryption_password.as_deref();

    match tool {
        "agent_spawn" => {
            let name = params["name"].as_str().context("name required")?;
            let no_sync = params["no_sync"].as_bool().unwrap_or(false);
            let replace = params["replace"].as_bool().unwrap_or(false);
            let count = spawn_agent(
                current_dir,
                &db,
                &api,
                &config.workspace_id,
                name,
                pw,
                no_sync,
                replace,
            )
            .await?;
            Ok(json!({ "files_copied": count }))
        }
        "agent_check" => {
            let name = params["name"].as_str().context("name required")?;
            let r = check_agent(current_dir, &db, &api, &config.workspace_id, name, pw).await?;
            Ok(serde_json::to_value(r)?)
        }
        "agent_refresh" => {
            let name = params["name"].as_str().context("name required")?;
            let r = refresh_agent(current_dir, &db, &api, &config.workspace_id, name, pw).await?;
            Ok(serde_json::to_value(r)?)
        }
        "agent_land" => {
            let name = params["name"].as_str().context("name required")?;
            let clean = params["clean"].as_bool().unwrap_or(false);
            let propose = params["propose"].as_bool().unwrap_or(false);
            let r = land_agent(
                current_dir,
                &db,
                &api,
                &config.workspace_id,
                name,
                pw,
                clean,
                propose,
            )
            .await?;
            Ok(serde_json::to_value(r)?)
        }
        "conflicts_list" => {
            let records = db.list_conflict_records().await?;
            Ok(serde_json::to_value(records)?)
        }
        "conflicts_keep" => {
            let path = params["path"]
                .as_str()
                .context("path required")?
                .to_string();
            let keep_str = params["keep"].as_str().unwrap_or("local");
            let keep = match keep_str {
                "local" => ResolveKeep::Local,
                "cloud" => ResolveKeep::Cloud,
                "both" => ResolveKeep::Both,
                "file" => ResolveKeep::File,
                other => anyhow::bail!("unknown keep value: {other}"),
            };
            let file_source = params["file"].as_str().map(std::path::Path::new);
            if matches!(keep, ResolveKeep::File) && file_source.is_none() {
                anyhow::bail!("conflicts_keep with keep=file requires a `file` param");
            }
            feanorfs_client::conflicts::resolve_conflict(&ctx, &path, keep, file_source).await?;
            Ok(json!({ "resolved": path }))
        }
        "sync_status" => {
            let r = feanorfs_client::do_status(&api, &db, current_dir, &config.workspace_id, pw)
                .await?;
            Ok(compact_sync_status(r))
        }
        "workspace_log" => {
            let limit = params["limit"]
                .as_u64()
                .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                .unwrap_or(20);
            let result = feanorfs_agent_core::history::log(&ctx, limit).await?;
            Ok(serde_json::to_value(result)?)
        }
        "workspace_undo" => {
            let snapshot_id = params["snapshot_id"]
                .as_str()
                .context("snapshot_id required")?;
            let result = feanorfs_agent_core::history::undo(&ctx, snapshot_id).await?;
            Ok(serde_json::to_value(result)?)
        }
        "agent_send" => {
            let to = params["to"].as_str().context("to required")?.to_string();
            let kind = match params["kind"].as_str() {
                Some("request") => AgentMessageKind::Request,
                Some("status") => AgentMessageKind::Status,
                Some("result") => AgentMessageKind::Result,
                Some("blocked") => AgentMessageKind::Blocked,
                _ => anyhow::bail!("kind must be request, status, result, or blocked"),
            };
            let body = params["body"]
                .as_str()
                .context("body required")?
                .to_string();
            let result = feanorfs_agent_core::send_message(
                &ctx,
                AgentMessageInput {
                    to,
                    kind,
                    body,
                    about_snapshot: params["about_snapshot"].as_str().map(str::to_string),
                    reply_to: params["reply_to"].as_str().map(str::to_string),
                    from: Some(agent_identity(params["from"].as_str())),
                },
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "agent_inbox" => {
            let recipient = agent_identity(params["for"].as_str());
            let limit = params["limit"]
                .as_u64()
                .map(|value| usize::try_from(value).unwrap_or(50))
                .unwrap_or(50);
            let result = feanorfs_agent_core::inbox(
                &ctx,
                AgentInboxQuery {
                    recipient,
                    after: params["after"].as_str().map(str::to_string),
                    limit,
                },
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_assign" => {
            let input: feanorfs_client::IntegratorAssignInput =
                serde_json::from_value(params.clone())
                    .context("invalid integrator_assign input")?;
            let result = feanorfs_client::integrator_assign(&ctx, input).await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_status" => {
            let result =
                feanorfs_client::integrator_status(&ctx, params["assignment_id"].as_str()).await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_revoke" => {
            let assignment_id = params["assignment_id"]
                .as_str()
                .context("assignment_id required")?;
            let reason = params["reason"].as_str().context("reason required")?;
            let result = feanorfs_client::integrator_revoke(&ctx, assignment_id, reason).await?;
            Ok(serde_json::to_value(result)?)
        }
        "integrator_resume" => {
            let result = feanorfs_client::integrator_resume(
                &ctx,
                feanorfs_client::IntegratorObserveOptions {
                    ack_timeout_ms: params["ack_timeout_ms"].as_u64(),
                    fallback_on_blocked: params["fallback_on_blocked"].as_bool().unwrap_or(false),
                },
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "conflict_materialize" => {
            let about = match params["about_snapshot"].as_str() {
                Some(snapshot) => snapshot.to_string(),
                None => ctx
                    .api
                    .get_head(ctx.workspace_id())
                    .await?
                    .context("workspace has no snapshot to materialize conflicts from")?,
            };
            let paths: Vec<String> = params["paths"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let result = feanorfs_client::materialize_conflicts(&ctx, &about, &paths).await?;
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
    use super::{agent_identity, compact_sync_status, tool_list};
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
}
