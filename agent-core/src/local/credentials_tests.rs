use super::*;

#[test]
fn redacted_config_contains_only_a_non_secret_reference() {
    let mut value = serde_json::json!({
        "server_url": "https://hub.example",
        "encryption_password": "secret-key",
        "server_password": "secret-token",
        "node_signing_key": "secret-node-key"
    });
    redact_and_mark(&mut value, "fsc1-public-id").unwrap();
    let object = value.as_object().unwrap();
    assert!(!object.contains_key("encryption_password"));
    assert!(!object.contains_key("server_password"));
    assert!(!object.contains_key("node_signing_key"));
    assert_eq!(object["credential_store"], "os");
    assert_eq!(object["credential_id"], "fsc1-public-id");
}

#[test]
fn malformed_markers_fail_closed() {
    assert!(marker(r#"{"credential_store":"os"}"#).is_err());
    assert!(marker(r#"{"credential_store":"future","credential_id":"x"}"#).is_err());
}
