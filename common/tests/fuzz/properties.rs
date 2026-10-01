//! Parser properties shared by the stable mutation smoke test
//! (`tests/parser_fuzz.rs`) and the libFuzzer targets in `/fuzz`.
//!
//! Every input here is untrusted: decrypted objects and signals written by
//! other participants, pasted capabilities, and hub-supplied ciphertext. A
//! parser must never panic, and an accepted input must be canonical: encoding
//! the parsed value reproduces the exact input.

// Each fuzz target uses one property.
#![allow(dead_code)]

use feanorfs_common::git_baseline::GitBaseline;
use feanorfs_common::{
    decode_hub_invite, decode_invite, encode_agent_message, encode_capability_announcement,
    encode_hub_invite, encode_integrator_profile, encode_invite, encode_resolution_profile,
    encode_work_profile, parse_agent_message, parse_capability_announcement,
    parse_integrator_profile, parse_resolution_profile, parse_work_profile,
    unpack_bytes_with_policy, LegacyPolicy, Snapshot, Tree,
};

/// Canonical tree and snapshot object bytes.
pub fn tree_codec(data: &[u8]) {
    if let Ok(tree) = Tree::from_canonical_bytes(data) {
        assert_eq!(
            tree.to_canonical_bytes(),
            data,
            "tree decode is not canonical"
        );
    }
    if let Ok(snapshot) = Snapshot::from_canonical_bytes(data) {
        assert_eq!(
            snapshot.to_canonical_bytes(),
            data,
            "snapshot decode is not canonical"
        );
    }
}

/// `fnr1`/`fnh1` capabilities pasted by users.
pub fn invites(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(invite) = decode_invite(text) {
        let again = decode_invite(&encode_invite(&invite).expect("decoded invite re-encodes"));
        assert_eq!(
            again.ok(),
            Some(invite),
            "workspace invite does not roundtrip"
        );
    }
    if let Ok(invite) = decode_hub_invite(text) {
        let again =
            decode_hub_invite(&encode_hub_invite(&invite).expect("decoded invite re-encodes"));
        assert_eq!(again.ok(), Some(invite), "hub invite does not roundtrip");
    }
}

/// Signal envelopes and every profile carried inside them.
pub fn signals(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(payload) = parse_agent_message(text) {
        assert_eq!(encode_agent_message(&payload).ok().as_deref(), Some(text));
    }
    if let Some(profile) = parse_integrator_profile(text) {
        assert_eq!(
            encode_integrator_profile(&profile).ok().as_deref(),
            Some(text)
        );
    }
    if let Some(profile) = parse_work_profile(text) {
        assert_eq!(encode_work_profile(&profile).ok().as_deref(), Some(text));
    }
    if let Some(profile) = parse_resolution_profile(text) {
        assert_eq!(
            encode_resolution_profile(&profile).ok().as_deref(),
            Some(text)
        );
    }
    if let Some(capabilities) = parse_capability_announcement(text) {
        assert_eq!(
            encode_capability_announcement(&capabilities)
                .ok()
                .as_deref(),
            Some(text)
        );
    }
    if let Some(baseline) = GitBaseline::parse(text) {
        assert_eq!(baseline.encode(), text);
    }
}

/// Hub-supplied ciphertext must fail cleanly, never panic.
pub fn aead(data: &[u8]) {
    let key = "0".repeat(64);
    let _ = unpack_bytes_with_policy(data, &key, "src/main.rs", LegacyPolicy::Reject);
}
