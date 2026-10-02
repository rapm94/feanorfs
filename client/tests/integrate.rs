use std::process::Command;
use tempfile::tempdir;

feanorfs_test_support::isolate_test_process!();

fn run_cli(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_feanorfs"));
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("execute feanorfs binary")
}

#[test]
fn shipped_skill_and_protocol_match_repository_sources() {
    let skill_source = include_str!("../../skills/feanorfs-collaboration/SKILL.md");
    let protocol_source =
        include_str!("../../skills/feanorfs-collaboration/references/protocol.md");
    let openai_source = include_str!("../../skills/feanorfs-collaboration/agents/openai.yaml");

    let temp = tempdir().unwrap();
    let project = temp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();

    let output = run_cli(
        &[
            "integrate",
            "install",
            "--project",
            project.to_str().unwrap(),
        ],
        &[],
    );
    assert!(output.status.success());

    let installed_skill = std::fs::read_to_string(
        project
            .join(".cursor")
            .join("skills")
            .join("feanorfs-collaboration")
            .join("SKILL.md"),
    )
    .unwrap();
    let installed_protocol = std::fs::read_to_string(
        project
            .join(".cursor")
            .join("skills")
            .join("feanorfs-collaboration")
            .join("references")
            .join("protocol.md"),
    )
    .unwrap();
    let installed_openai = std::fs::read_to_string(
        project
            .join(".cursor")
            .join("skills")
            .join("feanorfs-collaboration")
            .join("agents")
            .join("openai.yaml"),
    )
    .unwrap();

    assert_eq!(installed_skill, skill_source);
    assert_eq!(installed_protocol, protocol_source);
    assert_eq!(installed_openai, openai_source);
}

#[test]
fn unsupported_host_fails_without_guessing() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = run_cli(
        &["integrate", "--host", "unknown-agent-xyz"],
        &[("FEANORFS_HOME", home.to_str().unwrap())],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unsupported host 'unknown-agent-xyz'"),
        "expected unsupported host error, got: {stderr}"
    );
}

#[test]
fn install_and_status_json_roundtrip() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::create_dir_all(home.join(".config").join("opencode")).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();

    let output = run_cli(
        &["--json", "integrate", "status"],
        &[("FEANORFS_HOME", home.to_str().unwrap())],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let status_json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hosts = status_json["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 5);

    let output = run_cli(
        &["--json", "integrate", "install"],
        &[("FEANORFS_HOME", home.to_str().unwrap())],
    );
    assert!(output.status.success());
    let install_stdout = String::from_utf8_lossy(&output.stdout);
    let install_json: serde_json::Value = serde_json::from_str(&install_stdout).unwrap();
    assert!(install_json["installed"].as_array().unwrap().len() >= 5);

    let output = run_cli(
        &["--json", "integrate", "status"],
        &[("FEANORFS_HOME", home.to_str().unwrap())],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let status_after: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    for host in status_after["hosts"].as_array().unwrap() {
        assert_eq!(
            host["status"].as_str().unwrap(),
            "configured",
            "host {} should be configured",
            host["name"]
        );
        assert!(host["mcp_registered"].as_bool().unwrap());
        assert!(host["skill_installed"].as_bool().unwrap());
    }

    let output = run_cli(
        &["--json", "integrate", "uninstall"],
        &[("FEANORFS_HOME", home.to_str().unwrap())],
    );
    assert!(output.status.success());

    let output = run_cli(
        &["--json", "integrate", "status"],
        &[("FEANORFS_HOME", home.to_str().unwrap())],
    );
    let status_uninstalled: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    for host in status_uninstalled["hosts"].as_array().unwrap() {
        assert!(!host["mcp_registered"].as_bool().unwrap());
        assert!(!host["skill_installed"].as_bool().unwrap());
    }
}

#[test]
fn project_scope_creates_local_files() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("my-project");
    std::fs::create_dir_all(&project).unwrap();

    let output = run_cli(
        &[
            "--json",
            "integrate",
            "install",
            "--project",
            project.to_str().unwrap(),
        ],
        &[],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["hosts"].as_array().unwrap().len(), 5);

    assert!(project.join(".cursor").join("mcp.json").exists());
    assert!(project.join(".claude").join("mcp.json").exists());
    assert!(project.join("opencode.json").exists());
    assert!(project.join(".codex").join("config.json").exists());
    assert!(project.join(".gemini").join("mcp.json").exists());

    assert!(project
        .join(".cursor")
        .join("skills")
        .join("feanorfs-collaboration")
        .join("SKILL.md")
        .exists());
    assert!(project
        .join(".agents")
        .join("skills")
        .join("feanorfs-collaboration")
        .join("SKILL.md")
        .exists());
    assert!(!project
        .join(".codex")
        .join("skills")
        .join("feanorfs-collaboration")
        .exists());
}

#[test]
fn codex_auto_claim_hooks_install_and_uninstall_in_project_scope() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("proj");
    std::fs::create_dir_all(project.join(".codex")).unwrap();
    let hooks_path = project.join(".codex").join("hooks.json");
    let read = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap()
    };
    let project_arg = project.to_str().unwrap();

    let installed = run_cli(
        &[
            "integrate",
            "install",
            "--host",
            "codex",
            "--project",
            project_arg,
            "--auto-claim",
        ],
        &[],
    );
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let hooks = read();
    let guard = &hooks["hooks"]["PreToolUse"][0];
    assert_eq!(guard["matcher"], "Edit|Write|MultiEdit|NotebookEdit");
    assert!(guard["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("agent guard --hook --claim"));
    assert!(hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("agent done --hook"));

    let removed = run_cli(
        &[
            "integrate",
            "uninstall",
            "--host",
            "codex",
            "--project",
            project_arg,
        ],
        &[],
    );
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let hooks = read();
    assert!(hooks["hooks"]["PreToolUse"].as_array().unwrap().is_empty());
    assert!(hooks["hooks"]["Stop"].as_array().unwrap().is_empty());
}
