//! Deterministic mutation smoke test over every untrusted-input parser.
//!
//! Runs the same properties as the libFuzzer targets in `/fuzz` on stable
//! Rust in every `cargo test`: valid seeds plus Blake3-driven bit flips,
//! byte insertions/deletions, truncations, and splices. Coverage-guided
//! fuzzing lives in CI (`.github/workflows/security.yml`).

#[path = "fuzz/properties.rs"]
mod properties;

use feanorfs_common::git_baseline::GitBaseline;
use feanorfs_common::resolution_contract::resolution_fixtures;
use feanorfs_common::work_contract::work_fixtures;
use feanorfs_common::{
    encode_agent_message, encode_capability_announcement, encode_hub_invite,
    encode_integrator_profile, encode_invite, encode_resolution_profile, flat_to_tree, pack_bytes,
    AgentMessageKind, AgentMessagePayload, FileState, HubInvite, IntegratorProfile,
    ResolutionProfile, Snapshot, WorkspaceInvite,
};
use std::collections::HashMap;

const MUTATIONS_PER_SEED: u64 = 3_000;

fn tree_seeds() -> Vec<Vec<u8>> {
    let files: HashMap<String, FileState> = ["src/main.rs", "src/lib/mod.rs", "README.md"]
        .iter()
        .map(|path| {
            (
                (*path).to_string(),
                FileState {
                    path: (*path).to_string(),
                    hash: "a".repeat(64),
                    size: 12,
                    mtime: 0,
                    deleted: false,
                    mode: 0,
                },
            )
        })
        .collect();
    let bundle = flat_to_tree(&files).unwrap();
    let mut seeds: Vec<Vec<u8>> = bundle
        .trees
        .values()
        .map(feanorfs_common::Tree::to_canonical_bytes)
        .collect();
    seeds.push(
        Snapshot {
            root: bundle.root,
            parents: vec!["b".repeat(64)],
            author: "linux".into(),
            created_at_ms: 1,
            message: Some(
                GitBaseline::new(&"c".repeat(40), Some("main"))
                    .unwrap()
                    .encode(),
            ),
        }
        .to_canonical_bytes(),
    );
    seeds
}

fn invite_seeds() -> Vec<Vec<u8>> {
    let workspace = WorkspaceInvite {
        server_url: "https://hub.example:3030".into(),
        workspace_id: "fsw1-demo".into(),
        server_token: Some("t".repeat(64)),
        encryption_key: "0".repeat(64),
        tls_ca_pem: None,
        hub_local: false,
        relay: None,
        mesh: None,
        ignore_policy: Some(String::new()),
    };
    let hub = HubInvite {
        server_url: "https://hub.example:3030".into(),
        server_token: Some("t".repeat(64)),
        tls_ca_pem: None,
        relay: None,
        mesh: None,
    };
    vec![
        encode_invite(&workspace).unwrap().into_bytes(),
        encode_hub_invite(&hub).unwrap().into_bytes(),
    ]
}

fn signal_seeds() -> Vec<Vec<u8>> {
    let message = encode_agent_message(&AgentMessagePayload {
        to: "mac-test".into(),
        kind: AgentMessageKind::Request,
        body: "Run iOS simulator tests".into(),
        about_snapshot: "a".repeat(64),
        reply_to: None,
    })
    .unwrap();
    let assignment = encode_integrator_profile(&IntegratorProfile::Accepted {
        assignment_id: "0123456789abcdef0123456789abcdef".into(),
        attempt: 0,
        about_snapshot: "a".repeat(64),
    })
    .unwrap();
    let resolution =
        encode_resolution_profile(&ResolutionProfile::Result(resolution_fixtures::result()))
            .unwrap();
    [
        message,
        assignment,
        work_fixtures::work_intent_json(),
        resolution,
        encode_capability_announcement(&["ios-build".into()]).unwrap(),
        GitBaseline::new(&"d".repeat(64), None).unwrap().encode(),
    ]
    .into_iter()
    .map(String::into_bytes)
    .collect()
}

fn aead_seeds() -> Vec<Vec<u8>> {
    vec![pack_bytes(b"fn main() {}", &"0".repeat(64), "src/main.rs").unwrap()]
}

/// Deterministic mutation stream: Blake3 XOF keyed by seed and round.
fn mutate(seed: &[u8], round: u64) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&round.to_le_bytes());
    hasher.update(seed);
    let mut rng = hasher.finalize_xof();
    let mut next = |bound: usize| -> usize {
        let mut bytes = [0u8; 8];
        rng.fill(&mut bytes);
        (u64::from_le_bytes(bytes) % bound.max(1) as u64) as usize
    };
    let mut data = seed.to_vec();
    for _ in 0..=next(4) {
        match next(5) {
            0 if !data.is_empty() => {
                let index = next(data.len());
                data[index] ^= 1 << next(8);
            }
            1 => {
                let index = next(data.len() + 1);
                data.insert(index, next(256) as u8);
            }
            2 if !data.is_empty() => {
                data.remove(next(data.len()));
            }
            3 => data.truncate(next(data.len() + 1)),
            _ => {
                let start = next(data.len() + 1);
                let copy: Vec<u8> = data[start..].iter().take(next(16)).copied().collect();
                let at = next(data.len() + 1);
                data.splice(at..at, copy);
            }
        }
    }
    data
}

fn exercise(seeds: Vec<Vec<u8>>, property: fn(&[u8])) {
    for seed in &seeds {
        property(seed);
        for round in 0..MUTATIONS_PER_SEED {
            property(&mutate(seed, round));
        }
    }
}

#[test]
fn tree_codec_survives_mutation() {
    exercise(tree_seeds(), properties::tree_codec);
}

#[test]
fn invites_survive_mutation() {
    exercise(invite_seeds(), properties::invites);
}

#[test]
fn signal_profiles_survive_mutation() {
    exercise(signal_seeds(), properties::signals);
}

#[test]
fn aead_rejects_mutated_ciphertext_without_panicking() {
    exercise(aead_seeds(), properties::aead);
}
