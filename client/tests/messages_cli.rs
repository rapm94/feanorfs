//! Real CLI/MCP adapter coverage for encrypted agent signals.

use feanorfs_agent_core::local::{save_config, Config};
use feanorfs_agent_core::{
    ensure_workspace_state, ApiClient, ClientDb, SnapshotEngine, SyncCtx, LOCAL_HUB_URL,
};
use feanorfs_common::{generate_password, AgentInboxResult, AgentMessageKind, AgentSendResult};
use serde_json::json;
use std::collections::HashMap;
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn run_cli(workspace: &std::path::Path, state_root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args(args)
        .current_dir(workspace)
        .env("FEANORFS_HOME", state_root)
        .output()
        .unwrap()
}

#[tokio::test]
async fn cli_json_human_and_mcp_signal_adapters_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::env::set_var("FEANORFS_HOME", &state_root);

    let config = Config {
        server_url: LOCAL_HUB_URL.into(),
        workspace_id: "messages-cli".into(),
        encryption_password: Some(generate_password().unwrap()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 3,
        hub_local: true,
        relay: None,
    };
    save_config(&workspace, &config).unwrap();
    let db = ClientDb::new(ensure_workspace_state(&workspace).unwrap())
        .await
        .unwrap();
    let api = ApiClient::from_config(&workspace, &config).await.unwrap();
    let ctx = SyncCtx::from_config(&api, &db, &workspace, &config).unwrap();
    SnapshotEngine::new(&ctx)
        .publish_server_view(&HashMap::new(), "seed")
        .await
        .unwrap();

    let unsafe_body = "Run simulator tests\n\u{1b}[31mred";
    let sent = run_cli(
        &workspace,
        &state_root,
        &[
            "--json",
            "agent",
            "send",
            "mac-test",
            "--kind",
            "request",
            "--from",
            "linux-dev",
            unsafe_body,
        ],
    );
    assert!(
        sent.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let sent: AgentSendResult = serde_json::from_slice(&sent.stdout).unwrap();

    let inbox = run_cli(
        &workspace,
        &state_root,
        &["--json", "agent", "inbox", "--for", "mac-test"],
    );
    assert!(
        inbox.status.success(),
        "inbox failed: {}",
        String::from_utf8_lossy(&inbox.stderr)
    );
    let inbox: AgentInboxResult = serde_json::from_slice(&inbox.stdout).unwrap();
    let delivered = inbox
        .messages
        .iter()
        .find(|message| message.message_id == sent.message_id)
        .unwrap();
    assert_eq!(delivered.from, "linux-dev");
    assert_eq!(delivered.kind, AgentMessageKind::Request);
    assert_eq!(delivered.body, unsafe_body);

    let human = run_cli(
        &workspace,
        &state_root,
        &["agent", "inbox", "--for", "mac-test"],
    );
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("linux-dev -> mac-test request"));
    assert!(human.contains("Run simulator tests\\n\\u{1b}[31mred"));
    assert!(!human.contains('\u{1b}'));
    assert!(human.contains("New signals since this read"));

    let history = run_cli(&workspace, &state_root, &["log", "--limit", "10"]);
    assert!(history.status.success());
    let history = String::from_utf8(history.stdout).unwrap();
    assert!(history.contains("Run simulator tests\\n\\u{1b}[31mred"));
    assert!(!history.contains('\u{1b}'));

    let missing_cursor = "f".repeat(64);
    let reset = run_cli(
        &workspace,
        &state_root,
        &[
            "agent",
            "inbox",
            "--for",
            "nobody",
            "--after",
            &missing_cursor,
        ],
    );
    assert!(reset.status.success());
    let reset = String::from_utf8(reset.stdout).unwrap();
    assert!(reset.contains("No signals for 'nobody'."));
    assert!(reset.contains("older signals may have been missed"));

    let mut mcp = Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .arg("mcp")
        .current_dir(&workspace)
        .env("FEANORFS_HOME", &state_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let send_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "agent_send",
            "arguments": {
                "from": "mcp-agent",
                "to": "mac-test",
                "kind": "status",
                "body": "MCP dispatch works"
            }
        }
    });
    let inbox_request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "agent_inbox",
            "arguments": { "for": "mac-test", "limit": 50 }
        }
    });
    {
        let stdin = mcp.stdin.as_mut().unwrap();
        writeln!(stdin, "{send_request}").unwrap();
        writeln!(stdin, "{inbox_request}").unwrap();
    }
    drop(mcp.stdin.take());
    let output = mcp.wait_with_output().unwrap();
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
    assert_eq!(responses.len(), 2);
    assert!(responses[0]["result"]["message_id"].is_string());
    assert!(responses[1]["result"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
            message["from"] == "mcp-agent"
                && message["kind"] == "status"
                && message["body"] == "MCP dispatch works"
        }));
}
