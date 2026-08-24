//! `agent run` preserves the control workspace for nested signal commands.

feanorfs_test_support::isolate_test_process!();

use feanorfs_agent_core::local::{load_config, save_config, Config};
use feanorfs_agent_core::{
    agent_dir, ensure_workspace_state, land_agent, spawn_agent, ApiClient, ClientDb, LOCAL_HUB_URL,
};
use feanorfs_common::{generate_password, AgentInboxResult, AgentMessageKind, AgentSendResult};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run_cli(workspace: &Path, state_root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args(args)
        .current_dir(workspace)
        .env("FEANORFS_HOME", state_root)
        .env_remove("FEANORFS_WORKSPACE_ROOT")
        .output()
        .unwrap()
}

#[tokio::test]
async fn nested_agent_signal_commands_use_the_shared_workspace_root() {
    let root = tempfile::tempdir().unwrap();
    let state_root = std::path::PathBuf::from(
        std::env::var_os("FEANORFS_HOME").expect("isolated FEANORFS_HOME"),
    );
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("seed.txt"), b"seed").unwrap();

    let config = Config {
        server_url: LOCAL_HUB_URL.into(),
        workspace_id: "agent-run-context".into(),
        encryption_password: Some(generate_password().unwrap()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 3,
        hub_local: true,
        relay: None,
        mesh: None,
    };
    save_config(&workspace, &config).unwrap();
    let db = ClientDb::new(ensure_workspace_state(&workspace).unwrap())
        .await
        .unwrap();
    let api = ApiClient::from_config(&workspace, &config).await.unwrap();
    spawn_agent(
        &workspace,
        &db,
        &api,
        &config.workspace_id,
        "worker",
        config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap();

    let nested_send = run_cli(
        &workspace,
        &state_root,
        &[
            "agent",
            "run",
            "worker",
            "--",
            env!("CARGO_BIN_EXE_feanorfs"),
            "--json",
            "agent",
            "send",
            "coordinator",
            "--kind",
            "request",
            "run the focused test",
        ],
    );
    assert!(
        nested_send.status.success(),
        "nested send failed: {}",
        String::from_utf8_lossy(&nested_send.stderr)
    );
    let sent: AgentSendResult = serde_json::from_slice(&nested_send.stdout).unwrap();

    let coordinator_inbox = run_cli(
        &workspace,
        &state_root,
        &["--json", "agent", "inbox", "--for", "coordinator"],
    );
    assert!(
        coordinator_inbox.status.success(),
        "coordinator inbox failed: {}",
        String::from_utf8_lossy(&coordinator_inbox.stderr)
    );
    let coordinator_inbox: AgentInboxResult =
        serde_json::from_slice(&coordinator_inbox.stdout).unwrap();
    let delivered = coordinator_inbox
        .messages
        .iter()
        .find(|message| message.message_id == sent.message_id)
        .unwrap();
    assert_eq!(delivered.from, "worker");
    assert_eq!(delivered.kind, AgentMessageKind::Request);

    let reply = run_cli(
        &workspace,
        &state_root,
        &[
            "--json",
            "agent",
            "send",
            "worker",
            "--kind",
            "result",
            "--from",
            "coordinator",
            "focused test passed",
        ],
    );
    assert!(
        reply.status.success(),
        "reply failed: {}",
        String::from_utf8_lossy(&reply.stderr)
    );

    let nested_inbox = run_cli(
        &workspace,
        &state_root,
        &[
            "agent",
            "run",
            "worker",
            "--",
            env!("CARGO_BIN_EXE_feanorfs"),
            "--json",
            "agent",
            "inbox",
        ],
    );
    assert!(
        nested_inbox.status.success(),
        "nested inbox failed: {}",
        String::from_utf8_lossy(&nested_inbox.stderr)
    );
    let nested_inbox: AgentInboxResult = serde_json::from_slice(&nested_inbox.stdout).unwrap();
    assert!(nested_inbox.messages.iter().any(|message| {
        message.from == "coordinator"
            && message.to == "worker"
            && message.kind == AgentMessageKind::Result
            && message.body == "focused test passed"
    }));
}

async fn setup_worker_workspace(workspace: &Path, workspace_id: &str) {
    let config = Config {
        server_url: LOCAL_HUB_URL.into(),
        workspace_id: workspace_id.into(),
        encryption_password: Some(generate_password().unwrap()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 3,
        hub_local: true,
        relay: None,
        mesh: None,
    };
    save_config(workspace, &config).unwrap();
    let db = ClientDb::new(ensure_workspace_state(workspace).unwrap())
        .await
        .unwrap();
    let api = ApiClient::from_config(workspace, &config).await.unwrap();
    spawn_agent(
        workspace,
        &db,
        &api,
        &config.workspace_id,
        "worker",
        config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn nested_mcp_agent_send_uses_the_shared_workspace_root() {
    let root = tempfile::tempdir().unwrap();
    let state_root = std::path::PathBuf::from(
        std::env::var_os("FEANORFS_HOME").expect("isolated FEANORFS_HOME"),
    );
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("seed.txt"), b"seed").unwrap();
    setup_worker_workspace(&workspace, "agent-run-context-mcp").await;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "agent_send",
            "arguments": {
                "to": "coordinator",
                "kind": "request",
                "body": "MCP nested context works"
            }
        }
    });
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args([
            "agent",
            "run",
            "worker",
            "--",
            env!("CARGO_BIN_EXE_feanorfs"),
            "mcp",
        ])
        .current_dir(&workspace)
        .env("FEANORFS_HOME", &state_root)
        .env_remove("FEANORFS_WORKSPACE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    writeln!(stdin, "{request}").unwrap();
    drop(stdin);
    let output = mcp.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "nested MCP failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sent: AgentSendResult =
        serde_json::from_value(response["result"]["structuredContent"].clone()).unwrap();
    let inbox = run_cli(
        &workspace,
        &state_root,
        &["--json", "agent", "inbox", "--for", "coordinator"],
    );
    assert!(
        inbox.status.success(),
        "coordinator inbox failed: {}",
        String::from_utf8_lossy(&inbox.stderr)
    );
    let inbox: AgentInboxResult = serde_json::from_slice(&inbox.stdout).unwrap();
    let delivered = inbox
        .messages
        .iter()
        .find(|message| message.message_id == sent.message_id)
        .unwrap();
    assert_eq!(delivered.from, "worker");
    assert_eq!(delivered.kind, AgentMessageKind::Request);
}

#[tokio::test]
async fn nested_mcp_conflicts_keep_resolves_relative_and_absolute_sources() {
    let root = tempfile::tempdir().unwrap();
    let state_root = std::path::PathBuf::from(
        std::env::var_os("FEANORFS_HOME").expect("isolated FEANORFS_HOME"),
    );
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("conflict.txt"), b"base").unwrap();
    std::fs::write(workspace.join("absolute-conflict.txt"), b"base").unwrap();
    setup_worker_workspace(&workspace, "agent-run-context-conflict").await;

    let worker = agent_dir(&workspace, "worker").unwrap();
    std::fs::write(worker.join("conflict.txt"), b"agent").unwrap();
    std::fs::write(worker.join("absolute-conflict.txt"), b"agent").unwrap();
    std::fs::write(workspace.join("conflict.txt"), b"human").unwrap();
    std::fs::write(workspace.join("absolute-conflict.txt"), b"human").unwrap();
    let config = load_config(&workspace).unwrap();
    let db = ClientDb::new(ensure_workspace_state(&workspace).unwrap())
        .await
        .unwrap();
    let api = ApiClient::from_config(&workspace, &config).await.unwrap();
    let land = land_agent(
        &workspace,
        &db,
        &api,
        &config.workspace_id,
        "worker",
        config.encryption_password.as_deref(),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(land
        .conflicts
        .iter()
        .any(|conflict| conflict.path == "conflict.txt"));
    assert!(land
        .conflicts
        .iter()
        .any(|conflict| conflict.path == "absolute-conflict.txt"));

    std::fs::write(workspace.join("reconciled.txt"), b"reconciled").unwrap();
    let absolute_source = root.path().join("absolute-reconciled.txt");
    std::fs::write(&absolute_source, b"absolute-reconciled").unwrap();
    let relative_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "conflicts_keep",
            "arguments": {
                "path": "conflict.txt",
                "keep": "file",
                "file": "reconciled.txt"
            }
        }
    });
    let absolute_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "conflicts_keep",
            "arguments": {
                "path": "absolute-conflict.txt",
                "keep": "file",
                "file": absolute_source
            }
        }
    });
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args([
            "agent",
            "run",
            "worker",
            "--",
            env!("CARGO_BIN_EXE_feanorfs"),
            "mcp",
        ])
        .current_dir(&workspace)
        .env("FEANORFS_HOME", &state_root)
        .env_remove("FEANORFS_WORKSPACE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    writeln!(stdin, "{relative_request}").unwrap();
    writeln!(stdin, "{absolute_request}").unwrap();
    drop(stdin);
    let output = mcp.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "nested MCP failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    let response = &responses[0];
    assert!(
        response.get("error").is_none(),
        "nested conflicts_keep failed: {response}"
    );
    assert_eq!(
        response["result"]["structuredContent"]["resolved"],
        "conflict.txt"
    );
    assert_eq!(
        std::fs::read(workspace.join("conflict.txt")).unwrap(),
        b"reconciled"
    );
    let absolute_response = &responses[1];
    assert!(
        absolute_response.get("error").is_none(),
        "nested absolute conflicts_keep failed: {absolute_response}"
    );
    assert_eq!(
        absolute_response["result"]["structuredContent"]["resolved"],
        "absolute-conflict.txt"
    );
    assert_eq!(
        std::fs::read(workspace.join("absolute-conflict.txt")).unwrap(),
        b"absolute-reconciled"
    );
}
