use anyhow::Context;
use feanorfs_client::{
    check_agent, land_agent, load_config, refresh_agent, spawn_agent, ResolveKeep, SyncCtx,
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
            tool("sync_status", "Workspace sync status", json!({ "type": "object", "properties": {} })),
            tool("workspace_log", "List workspace snapshot history", json!({
                "type": "object",
                "properties": { "limit": { "type": "integer", "minimum": 0, "maximum": 1000 } }
            })),
            tool("workspace_undo", "Restore a reachable snapshot", json!({
                "type": "object",
                "properties": { "snapshot_id": { "type": "string", "minLength": 8 } },
                "required": ["snapshot_id"]
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
            Ok(serde_json::to_value(r)?)
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
        other => anyhow::bail!("unknown method: {other}"),
    }
}
