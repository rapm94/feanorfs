use crate::local::validate_e2ee_key;

#[test]
fn generated_key_shape_is_accepted_for_encrypted_formats() {
    let key = feanorfs_common::generate_password().expect("generate key");
    validate_e2ee_key(&key, 2).expect("format v2 key");
    validate_e2ee_key(&key, 3).expect("format v3 key");
}

#[test]
fn encrypted_formats_reject_human_or_noncanonical_keys() {
    for key in [
        "correct horse battery staple".to_string(),
        "A".repeat(64),
        "g".repeat(64),
        "0".repeat(63),
        "0".repeat(65),
    ] {
        let error = validate_e2ee_key(&key, 3).expect_err("weak key must be rejected");
        assert!(error.to_string().contains("brute-forced offline"));
    }
}

#[test]
fn legacy_format_keeps_loading_historical_keys() {
    validate_e2ee_key("historical-human-passphrase", 1).expect("legacy key remains readable");
}

#[cfg(unix)]
#[test]
fn workspace_credentials_are_private() {
    use std::os::unix::fs::PermissionsExt as _;

    let workspace = tempfile::tempdir().unwrap();
    let config = crate::local::Config {
        server_url: "https://example.test".into(),
        workspace_id: "private".into(),
        encryption_password: Some("a".repeat(64)),
        server_password: Some("server-token".into()),
        tls_ca_pem: Some("public-ca".into()),
        format_version: 3,
        hub_local: false,
        relay: None,
    };

    crate::local::save_config(workspace.path(), &config).unwrap();

    let state = crate::workspace_layout::ensure_workspace_state(workspace.path()).unwrap();
    assert!(!workspace.path().join(".feanorfs").exists());
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let config_path = state.join("config.json");
    assert_eq!(
        std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    crate::local::save_config(workspace.path(), &config).unwrap();
    assert_eq!(
        std::fs::metadata(config_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn legacy_configs_without_tls_ca_still_decode() {
    let workspace: crate::local::Config = serde_json::from_str(
        r#"{"server_url":"http://127.0.0.1:3030","workspace_id":"legacy","encryption_password":null}"#,
    )
    .unwrap();
    assert_eq!(workspace.tls_ca_pem, None);
    assert_eq!(workspace.format_version, 1);
    assert!(!workspace.hub_local);
    assert_eq!(workspace.relay, None);

    let global: crate::local::GlobalConfig =
        serde_json::from_str(r#"{"server_url":"https://hub.example","server_password":"token"}"#)
            .unwrap();
    assert_eq!(global.tls_ca_pem, None);
    assert_eq!(global.relay, None);
}

#[test]
fn config_debug_redacts_credentials_and_capabilities() {
    let config = crate::local::Config {
        server_url: "https://hub.example".into(),
        workspace_id: "workspace".into(),
        encryption_password: Some("secret-e2ee-key".into()),
        server_password: Some("secret-bearer-token".into()),
        tls_ca_pem: Some("public-ca-body".into()),
        format_version: 3,
        hub_local: false,
        relay: Some(feanorfs_common::RelayConfig {
            url: "wss://relay.example".into(),
            route: "secret-relay-route".into(),
        }),
    };
    let rendered = format!("{config:?}");
    for secret in [
        "secret-e2ee-key",
        "secret-bearer-token",
        "public-ca-body",
        "secret-relay-route",
    ] {
        assert!(!rendered.contains(secret));
    }
    assert!(rendered.contains("<redacted>"));
}
