use super::*;
use crate::{MeshCandidate, MeshCandidateKind, MeshTransport, NodeId};

#[test]
fn invite_roundtrip() {
    let inv = WorkspaceInvite {
        server_url: "http://127.0.0.1:3030".into(),
        workspace_id: "demo".into(),
        server_token: None,
        encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        tls_ca_pem: None,
        hub_local: false,
        relay: None,
        mesh: None,
        ignore_policy: Some("target/\n".into()),
    };
    let enc = encode_invite(&inv).unwrap();
    assert!(enc.starts_with(INVITE_PREFIX));
    assert_eq!(decode_invite(&enc).unwrap(), inv);
}

#[test]
fn mesh_invite_roundtrip_preserves_typed_candidates() {
    let mesh = MeshConfig::new(
        NodeId::from_public_key([7_u8; 32]),
        vec![
            MeshCandidate::new(
                MeshTransport::Tcp,
                MeshCandidateKind::Direct,
                "[2001:db8::7]:3030".parse().unwrap(),
            )
            .unwrap(),
            MeshCandidate::new(
                MeshTransport::Quic,
                MeshCandidateKind::Reflexive,
                "198.51.100.7:3030".parse().unwrap(),
            )
            .unwrap(),
        ],
    )
    .unwrap();
    let invite = WorkspaceInvite {
        server_url: "https://feanorfs.example:3030".into(),
        workspace_id: "mesh".into(),
        server_token: Some("token".into()),
        encryption_key: "a".repeat(64),
        tls_ca_pem: Some("public-ca".into()),
        hub_local: false,
        relay: None,
        mesh: Some(mesh.clone()),
        ignore_policy: None,
    };

    let decoded = decode_invite(&encode_invite(&invite).unwrap()).unwrap();

    assert_eq!(decoded.mesh, Some(mesh));
}

#[test]
fn decode_invite_rejects_oversized() {
    let giant = format!("fnr1-{}", "aa".repeat(5000));
    assert!(decode_invite(&giant).is_err());
}

#[test]
fn invite_decoders_reject_non_ascii_hex_without_panicking() {
    assert!(decode_invite("fnr1-aéa").is_err());
    assert!(decode_hub_invite("fnh1-💥").is_err());
}

#[test]
fn invite_encoders_never_emit_tokens_the_decoders_reject_as_oversized() {
    let workspace = WorkspaceInvite {
        server_url: "https://hub.example".into(),
        workspace_id: "workspace".into(),
        server_token: None,
        encryption_key: "a".repeat(64),
        tls_ca_pem: None,
        hub_local: false,
        relay: None,
        mesh: None,
        ignore_policy: Some("x".repeat(MAX_WORKSPACE_INVITE_BYTES)),
    };
    assert!(encode_invite(&workspace).is_err());

    let hub = HubInvite {
        server_url: "https://hub.example".into(),
        server_token: None,
        tls_ca_pem: Some("x".repeat(MAX_HUB_INVITE_BYTES)),
        relay: None,
        mesh: None,
    };
    assert!(encode_hub_invite(&hub).is_err());
}

#[test]
fn invite_debug_redacts_capability_secrets() {
    let invite = WorkspaceInvite {
        server_url: "https://hub.example".into(),
        workspace_id: "workspace".into(),
        server_token: Some("super-secret-token".into()),
        encryption_key: "super-secret-key".into(),
        tls_ca_pem: Some("public-ca-but-large".into()),
        hub_local: false,
        relay: Some(RelayConfig {
            url: "wss://relay.example".into(),
            route: "super-secret-route".into(),
        }),
        mesh: None,
        ignore_policy: Some("secret-project-pattern".into()),
    };
    let rendered = format!("{invite:?}");
    for secret in [
        "super-secret-token",
        "super-secret-key",
        "super-secret-route",
        "secret-project-pattern",
        "public-ca-but-large",
    ] {
        assert!(!rendered.contains(secret));
    }
    assert!(rendered.contains("<redacted>"));
}

#[test]
fn invite_preserves_hub_local_flag() {
    let inv = WorkspaceInvite {
        server_url: "feanorfs+local://hub".into(),
        workspace_id: "local".into(),
        server_token: None,
        encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        tls_ca_pem: None,
        hub_local: true,
        relay: None,
        mesh: None,
        ignore_policy: None,
    };
    let enc = encode_invite(&inv).unwrap();
    assert!(decode_invite(&enc).unwrap().hub_local);
}

#[test]
fn hub_invite_roundtrip_preserves_tls_ca() {
    let invite = HubInvite {
        server_url: "https://192.168.1.13:3030".into(),
        server_token: Some("token".into()),
        tls_ca_pem: Some("-----BEGIN CERTIFICATE-----\npublic\n-----END CERTIFICATE-----\n".into()),
        relay: Some(RelayConfig {
            url: "wss://relay.example".into(),
            route: "a".repeat(64),
        }),
        mesh: None,
    };
    let encoded = encode_hub_invite(&invite).unwrap();
    assert!(looks_like_hub_invite(&encoded));
    assert_eq!(decode_hub_invite(&encoded).unwrap(), invite);
}

#[test]
fn legacy_invites_without_tls_fields_still_decode() {
    let workspace_json =
        br#"{"server_url":"http://127.0.0.1:3030","workspace_id":"legacy","encryption_key":"key"}"#;
    let workspace =
        decode_invite(&format!("{INVITE_PREFIX}{}", hex_encode(workspace_json))).unwrap();
    assert_eq!(workspace.workspace_id, "legacy");
    assert_eq!(workspace.tls_ca_pem, None);
    assert!(!workspace.hub_local);
    assert_eq!(workspace.relay, None);
    assert_eq!(workspace.mesh, None);
    assert_eq!(workspace.ignore_policy, None);

    let hub_json = br#"{"server_url":"https://hub.example","server_token":"token"}"#;
    let hub = decode_hub_invite(&format!("{HUB_INVITE_PREFIX}{}", hex_encode(hub_json))).unwrap();
    assert_eq!(hub.server_token.as_deref(), Some("token"));
    assert_eq!(hub.tls_ca_pem, None);
    assert_eq!(hub.relay, None);
    assert_eq!(hub.mesh, None);
}

#[test]
fn hub_mdns_identity_is_stable_and_ca_specific() {
    let first = "-----BEGIN CERTIFICATE-----\nfirst\n-----END CERTIFICATE-----\n";
    let second = "-----BEGIN CERTIFICATE-----\nsecond\n-----END CERTIFICATE-----\n";

    assert_eq!(hub_ca_fingerprint(first), hub_ca_fingerprint(first));
    assert_eq!(
        hub_mdns_hostname(first),
        format!("feanorfs-{}.local", hub_ca_fingerprint(first))
    );
    assert_ne!(hub_mdns_hostname(first), hub_mdns_hostname(second));
}
