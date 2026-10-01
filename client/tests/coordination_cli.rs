//! Real CLI/MCP coverage for the unified coordination surface: compact MCP
//! tools, `agent next`, the edit guard (plain and hook mode), and typed
//! integrator replies including supersession.

feanorfs_test_support::isolate_test_process!();

use feanorfs_agent_core::local::{save_config, Config};
use feanorfs_agent_core::{
    ensure_workspace_state, ApiClient, ClientDb, SnapshotEngine, SyncCtx, LOCAL_HUB_URL,
};
use feanorfs_common::{
    generate_password, CoordinationStatus, IntegratorAssignResult, IntegratorStatusResult,
    LifecycleStage, LogResult,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    state_root: PathBuf,
}

impl Fixture {
    fn cli(&self, args: &[&str]) -> Output {
        self.cli_with_stdin(args, None)
    }

    fn cli_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_feanorfs"))
            .args(args)
            .current_dir(&self.workspace)
            .env("FEANORFS_HOME", &self.state_root)
            .env_remove("FEANORFS_AGENT")
            .env_remove("FEANORFS_AGENT_DIR")
            .env_remove("FEANORFS_WORKSPACE_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(stdin) = stdin {
            input.write_all(stdin.as_bytes()).unwrap();
        }
        drop(input);
        child.wait_with_output().unwrap()
    }

    fn json<T: serde::de::DeserializeOwned>(&self, args: &[&str]) -> T {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn next(&self, agent: &str) -> CoordinationStatus {
        self.json(&["--json", "agent", "next", "--for", agent])
    }

    fn head(&self) -> String {
        let log: LogResult = self.json(&["--json", "log", "--limit", "1"]);
        log.entries[0].snapshot_id.clone()
    }

    fn mcp(&self, calls: &[Value]) -> Vec<Value> {
        let mut requests = String::new();
        for (index, call) in calls.iter().enumerate() {
            let request = json!({ "jsonrpc": "2.0", "id": index + 1, "method": call["method"], "params": call["params"] });
            requests.push_str(&request.to_string());
            requests.push('\n');
        }
        let output = self.cli_with_stdin(&["mcp"], Some(&requests));
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

async fn fixture(id: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let state_root = PathBuf::from(std::env::var_os("FEANORFS_HOME").expect("isolated home"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let config = Config {
        server_url: LOCAL_HUB_URL.into(),
        workspace_id: id.into(),
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
    let ctx = SyncCtx::from_config(&api, &db, &workspace, &config).unwrap();
    SnapshotEngine::new(&ctx)
        .publish_server_view(&HashMap::new(), "seed")
        .await
        .unwrap();
    Fixture {
        _root: root,
        workspace: workspace.canonicalize().unwrap(),
        state_root,
    }
}

fn call(name: &str, arguments: Value) -> Value {
    json!({ "method": "tools/call", "params": { "name": name, "arguments": arguments } })
}

fn result(response: &Value) -> &Value {
    assert!(response.get("error").is_none(), "MCP error: {response}");
    &response["result"]["structuredContent"]
}

#[tokio::test]
async fn compact_mcp_next_actions_and_guard_drive_one_work_lifecycle() {
    let fx = fixture("coordination-work").await;

    let responses = fx.mcp(&[
        json!({ "method": "tools/list", "params": {} }),
        call(
            "work",
            json!({ "op": "propose", "agent": "linux", "task_id": "parser", "sequence": 1, "coordinator": "human", "paths": ["src/**"] }),
        ),
    ]);
    assert_eq!(responses[0]["result"]["tools"].as_array().unwrap().len(), 9);
    let proposal = result(&responses[1])["message_id"]
        .as_str()
        .unwrap()
        .to_string();

    let human = fx.next("human");
    assert_eq!(human.items[0].stage, LifecycleStage::Proposed);
    let decide = &human.next_actions[0];
    assert_eq!(decide.actor, "human");
    assert_eq!(decide.tool, "work");
    assert_eq!(decide.args["proposal_message_id"], proposal);

    // The prefilled action is directly callable, including the flat decision form.
    let responses = fx.mcp(&[
        call(&decide.tool, Value::Object(decide.args.clone())),
        call("status", json!({ "agent": "linux" })),
    ]);
    result(&responses[0]);
    let status = result(&responses[1]);
    assert_eq!(status["coordination"]["items"][0]["stage"], "accepted");
    assert_eq!(
        status["coordination"]["next_actions"][0]["args"]["op"],
        "settle"
    );
    assert!(status["sync"]["mirror_state"].is_string());

    let denied = fx.cli(&["agent", "guard", "src/lib.rs", "--for", "codex"]);
    assert_eq!(denied.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&denied.stderr).contains("linux's accepted scope"));
    let allowed = fx.cli(&["agent", "guard", "README.md", "--for", "codex"]);
    assert!(allowed.status.success());
    let own = fx.cli(&[
        "agent",
        "guard",
        "src/lib.rs",
        "--for",
        "linux",
        "--require-scope",
    ]);
    assert!(own.status.success());

    let payload = |path: &Path| {
        json!({ "cwd": fx.workspace, "tool_name": "Write", "tool_input": { "file_path": path } })
            .to_string()
    };
    let hooked = fx.cli_with_stdin(
        &["agent", "guard", "--hook", "--for", "codex"],
        Some(&payload(Path::new("src/new.rs"))),
    );
    assert_eq!(hooked.status.code(), Some(2));
    let outside = fx.cli_with_stdin(
        &["agent", "guard", "--hook", "--for", "codex"],
        Some(&payload(Path::new("/tmp/elsewhere.rs"))),
    );
    assert!(outside.status.success());
    let unparseable = fx.cli_with_stdin(&["agent", "guard", "--hook"], Some("not json"));
    assert!(unparseable.status.success());
}

#[tokio::test]
async fn typed_integrator_replies_bind_the_offer_and_refuse_superseded_attempts() {
    let fx = fixture("coordination-integrator").await;
    let about = fx.head();
    let assigned: IntegratorAssignResult = fx.json(&[
        "--json",
        "agent",
        "integrator",
        "assign",
        "--about",
        &about,
        "--candidate",
        "linux",
        "--candidate",
        "mac",
        "integrate the parser batch",
    ]);
    let first = assigned.selected.clone();
    let second = assigned.fallback_order[0].clone();

    let offered = fx.next(&first);
    let accept = offered
        .next_actions
        .iter()
        .find(|action| action.actor == first)
        .expect("selected candidate is told to accept");
    assert_eq!(accept.args["kind"], "accept");
    assert_eq!(accept.args["assignment_id"], assigned.assignment_id);

    let reply = fx.cli(&["agent", "integrator", "reply", "accept", "--for", &first]);
    assert!(
        reply.status.success(),
        "{}",
        String::from_utf8_lossy(&reply.stderr)
    );
    let _: Value = fx.json(&["--json", "agent", "integrator", "resume"]);
    let status: IntegratorStatusResult = fx.json(&["--json", "agent", "integrator", "status"]);
    assert_eq!(
        status.state,
        feanorfs_common::IntegratorAssignmentState::Accepted
    );

    let revoked = fx.cli(&[
        "agent",
        "integrator",
        "revoke",
        &assigned.assignment_id,
        "--reason",
        "rotate integrator",
    ]);
    assert!(
        revoked.status.success(),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );

    let stale = fx.cli(&[
        "agent",
        "integrator",
        "reply",
        "result",
        "--for",
        &first,
        "--assignment",
        &assigned.assignment_id,
        "--outcome",
        "done",
        "--verification",
        "passed",
        "--summary",
        "tests",
    ]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("superseded"));
    let stopped = fx.next(&first);
    assert!(stopped
        .warnings
        .iter()
        .any(|warning| warning.contains("superseded")));
    let guard = fx.cli(&["agent", "guard", "README.md", "--for", &first]);
    assert_eq!(guard.status.code(), Some(2));

    for args in [
        vec![
            "agent",
            "integrator",
            "reply",
            "accept",
            "--for",
            second.as_str(),
        ],
        vec![
            "agent",
            "integrator",
            "reply",
            "result",
            "--for",
            second.as_str(),
            "--outcome",
            "integrated",
            "--verification",
            "passed",
            "--summary",
            "cargo test",
        ],
    ] {
        let output = fx.cli(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _: Value = fx.json(&["--json", "agent", "integrator", "resume"]);
    }
    let done: IntegratorStatusResult = fx.json(&[
        "--json",
        "agent",
        "integrator",
        "status",
        &assigned.assignment_id,
    ]);
    assert_eq!(
        done.state,
        feanorfs_common::IntegratorAssignmentState::Completed
    );
}

#[tokio::test]
async fn capability_announcements_route_requests_and_integrator_rosters() {
    let fx = fixture("coordination-capabilities").await;
    let mac: feanorfs_common::CapabilityRoster = fx.json(&[
        "--json",
        "agent",
        "capabilities",
        "--for",
        "mac",
        "--set",
        "ios-build",
        "--set",
        "xcode",
    ]);
    assert!(mac.announced.is_some());
    let _: Value = fx.json(&[
        "--json",
        "agent",
        "capabilities",
        "--for",
        "linux",
        "--set",
        "rust",
    ]);

    let sent: feanorfs_common::AgentSendResult = fx.json(&[
        "--json",
        "agent",
        "send",
        "cap:ios-build",
        "--kind",
        "request",
        "--from",
        "linux",
        "Run the iOS simulator tests",
    ]);
    assert_eq!(sent.routed_to.as_deref(), Some("mac"));
    let inbox: feanorfs_common::AgentInboxResult =
        fx.json(&["--json", "agent", "inbox", "--for", "mac"]);
    assert!(inbox
        .messages
        .iter()
        .any(|message| message.message_id == sent.message_id && message.from == "linux"));

    let missing = fx.cli(&["agent", "send", "cap:gpu", "--kind", "request", "train"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no agent advertises"));

    let assigned: IntegratorAssignResult = fx.json(&[
        "--json",
        "agent",
        "integrator",
        "assign",
        "--about",
        &fx.head(),
        "--require",
        "rust",
        "integrate the Rust batch",
    ]);
    assert_eq!(assigned.selected, "linux");
    assert!(assigned.fallback_order.is_empty());

    let _: Value = fx.json(&[
        "--json",
        "agent",
        "capabilities",
        "--for",
        "ci-mac",
        "--set",
        "ios-build",
    ]);
    let ambiguous = fx.cli(&[
        "agent",
        "send",
        "cap:ios-build",
        "--kind",
        "request",
        "test",
    ]);
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("2 agents advertise"));

    let next = fx.next("linux");
    let names: Vec<&str> = next
        .roster
        .iter()
        .map(|entry| entry.agent.as_str())
        .collect();
    assert_eq!(names, vec!["ci-mac", "linux", "mac"]);
}
