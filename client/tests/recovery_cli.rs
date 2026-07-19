mod support;

use feanorfs_client::{do_sync, ClientDb, Config};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const PASSPHRASE: &str = "integration recovery passphrase";
const E2EE_KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn run_recovery_cli(cwd: &Path, home: &Path, args: &[&std::ffi::OsStr], pass: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_feanorfs"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("FEANORFS_HOME", home.join(".feanorfs"))
        .env("FEANORFS_CREDENTIAL_STORE", "file")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(pass.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_kit_restores_through_real_start_without_secret_arguments() {
    let server = support::spawn_test_server().await;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let source = root.path().join("source");
    let restored = root.path().join("restored");
    let wrong_destination = root.path().join("wrong");
    let tampered_destination = root.path().join("tampered-destination");
    let kit = root.path().join("workspace.fnrk");
    let tampered_kit = root.path().join("tampered.fnrk");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("hello.txt"), b"encrypted recovery payload").unwrap();

    let config = Config {
        server_url: server.url.clone(),
        workspace_id: "fsw1-recovery-integration".into(),
        encryption_password: Some(E2EE_KEY.into()),
        server_password: None,
        tls_ca_pem: None,
        format_version: 3,
        hub_local: false,
        relay: None,
    };
    let state = home
        .join(".feanorfs/workspaces")
        .join(feanorfs_agent_core::workspace_state_id(&source).unwrap());
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("config.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    let db = ClientDb::new(&state).await.unwrap();
    do_sync(
        &server.api,
        &db,
        &source,
        &config.workspace_id,
        config.encryption_password.as_deref(),
        false,
    )
    .await
    .unwrap();

    let export_args = [
        std::ffi::OsStr::new("recovery"),
        std::ffi::OsStr::new("export"),
        std::ffi::OsStr::new("--passphrase-stdin"),
        std::ffi::OsStr::new("--"),
        kit.as_os_str(),
    ];
    let exported = run_recovery_cli(&source, &home, &export_args, PASSPHRASE);
    assert!(
        exported.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let kit_bytes = std::fs::read(&kit).unwrap();
    for secret in [
        config.server_url.as_str(),
        config.workspace_id.as_str(),
        E2EE_KEY,
    ] {
        assert!(!kit_bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&kit).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let wrong_args = [
        std::ffi::OsStr::new("recovery"),
        std::ffi::OsStr::new("import"),
        std::ffi::OsStr::new("--passphrase-stdin"),
        std::ffi::OsStr::new("--no-watch"),
        std::ffi::OsStr::new("--"),
        kit.as_os_str(),
        wrong_destination.as_os_str(),
    ];
    let wrong = run_recovery_cli(&home, &home, &wrong_args, "different valid passphrase");
    assert!(!wrong.status.success());
    assert!(!wrong_destination.exists());

    let mut envelope: serde_json::Value = serde_json::from_slice(&kit_bytes).unwrap();
    let nonce = envelope["nonce"].as_str().unwrap();
    let replacement = if nonce.starts_with('A') { 'B' } else { 'A' };
    envelope["nonce"] = format!("{replacement}{}", &nonce[1..]).into();
    std::fs::write(&tampered_kit, serde_json::to_vec(&envelope).unwrap()).unwrap();
    let tampered_args = [
        std::ffi::OsStr::new("recovery"),
        std::ffi::OsStr::new("import"),
        std::ffi::OsStr::new("--passphrase-stdin"),
        std::ffi::OsStr::new("--no-watch"),
        std::ffi::OsStr::new("--"),
        tampered_kit.as_os_str(),
        tampered_destination.as_os_str(),
    ];
    let tampered = run_recovery_cli(&home, &home, &tampered_args, PASSPHRASE);
    assert!(!tampered.status.success());
    assert!(!tampered_destination.exists());

    let import_args = [
        std::ffi::OsStr::new("recovery"),
        std::ffi::OsStr::new("import"),
        std::ffi::OsStr::new("--passphrase-stdin"),
        std::ffi::OsStr::new("--no-watch"),
        std::ffi::OsStr::new("--"),
        kit.as_os_str(),
        restored.as_os_str(),
    ];
    let imported = run_recovery_cli(&home, &home, &import_args, PASSPHRASE);
    assert!(
        imported.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert_eq!(
        std::fs::read(restored.join("hello.txt")).unwrap(),
        b"encrypted recovery payload"
    );
    let restored_state = home
        .join(".feanorfs/workspaces")
        .join(feanorfs_agent_core::workspace_state_id(&restored).unwrap());
    let restored_config: Config =
        serde_json::from_slice(&std::fs::read(restored_state.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(restored_config.workspace_id, config.workspace_id);
    assert_eq!(
        restored_config.encryption_password.as_deref(),
        Some(E2EE_KEY)
    );
}
