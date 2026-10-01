# common

## Purpose

`feanorfs-common`: shared wire models, canonical Merkle trees and snapshots, sync delta and three-way classification, AEAD/legacy crypto, invites, and every protocol contract (`ffmsg1`, `ffint1`, `ffwork1`, `ffres1`, `ffcap1`, `ffbase1`, coordination, tray, mesh). Pure: no filesystem, network, or SQLite.

## Ownership

- `lib.rs` — `FileState`, sync models, crypto (`pack_bytes`/`unpack_bytes`/legacy `crypt_bytes`), path and hash validation, re-exports. Every `pub` item is a cross-crate contract; wire changes need server and client releases in lockstep.
- `tree.rs`, `tree_codec.rs`, `tree_convert.rs`, `tree_diff.rs` — public snapshot types, versioned canonical bytes, flat conversion, hash-pruned diff.
- `agent_contract.rs`, `work_contract.rs`, `integrator_contract.rs`, `resolution_contract.rs` — signal envelopes and protocol profiles, reducers' input/output types, canonical fixtures.
- `coordination_contract.rs` — unified lifecycle (`CoordinationStatus`, `LifecycleStage`, `NextAction`), guard types, typed integrator replies, `ffcap1` announcements and the capability roster.
- `git_baseline.rs` — `GitBaseline` and the `ffbase1:<commit>:<branch>` label codec.
- `invite.rs`, `mesh_contract.rs`, `hub_contract.rs`, `sealed_envelope.rs`, `tray_contract.rs`, `sync_delta.rs`, `three_way.rs`.
- `release-product-state.txt` — content-only release-selection carrier maintained by `scripts/update-release-product-state.sh`; never read at runtime.
- Dependencies stay leaf-only: serialization, hashing, randomness/time/error, ChaCha20-Poly1305/Argon2, Unicode normalization.

## Local Contracts

- **Crypto:** new blobs use ChaCha20-Poly1305 with key `blake3(domain ‖ len-prefixed password ‖ len-prefixed path)`. The deterministic nonce `blake3(key ‖ len ‖ plaintext)[..12]` is load-bearing for CAS stability — never switch to random nonces (accepted leak: reverts are observable). Format ≥2 rejects non-AEAD blobs; v1 falls back to XOR only when the AEAD prefix is absent, or auth fails and the expected plaintext size matches the blob length. Length-prefix every hashed field. `LEGACY_DEFAULT_PASSWORD` use is a bug that must warn.
- **Paths and hashes:** `is_valid_hash` = exactly 64 lowercase hex (path-traversal defense). `is_safe_rel_path` accepts one NFC portable forward-slash spelling: no absolute/drive/device syntax, backslashes, empty/dot components, Windows aliases, or case-insensitive `.git`/`.jj`/`.feanorfs` components. Validate the exact path later joined or persisted.
- **Trees:** canonical ids hash sorted, length-prefixed bytes; snapshot identity excludes mtimes; decoders reject non-canonical bytes (`decode → encode` reproduces input). Non-root empty directories are invalid. `FileState.mode` is portable executable intent; FTR1 stays byte-exact and FTR2 appears only when conflict legs need distinct modes; the visible leg is `theirs`, then `ours`, then `base`. Budgets: 16 MiB object, 256 depth, 250 000 objects/outputs, two million work items, 64 MiB aggregate paths; traversal is iterative.
- **Manifests:** every manifest contains its own snapshot root; raw entries are capped before allocation.
- **Sync delta and conflicts:** `compute_sync_delta` is a read-only LWW hint (client wins mtime ties); conflict identity is hash/deletion/executable intent; converged sides never conflict; a missing local entry present in base is a deletion.
- **Signals:** agent names ≤255 bytes; bodies ≤8 KiB; envelopes ≤64 KiB checked before JSON allocation. Every profile parser (`ffmsg1`, `ffint1`, `ffwork1`, `ffres1`, `ffcap1`) accepts only canonical JSON and returns `None` otherwise, so unknown or malformed profiles stay ordinary text.
- **Integrator:** capabilities arrive canonical; completed digests need passed verification, zero remaining conflicts, and no question; `requires_human` needs exactly one bounded question.
- **Coordination:** lifecycle ordering puts `awaiting_human` first and terminal stages last; text is bounded at 512 bytes. `IntegratorReplyInput` and `GuardInput` deny unknown fields. Capabilities follow the integrator identifier rules (lowercase, ≤32 bytes, ≤64 entries, sorted, unique).
- **Git baseline:** commits are 40 or 64 lowercase hex; branches are non-empty, ≤255 bytes, without `:` or control characters; anything else parses to `None`.
- **Invites:** `fnh1` carries URL, optional token, public CA, relay metadata; `fnr1` adds workspace ID, E2EE key, and optional ignore policy. CA fields are public certificates only. Encoders enforce decoder bounds; `Debug` redacts tokens, keys, routes, CA bodies, and policies. `hub_ca_fingerprint`/`hub_mdns_hostname` derive from the exact CA bytes and are reachability hints only.
- **Tray/status projections** are bounded, secret-free, and additive; missing fields mean unknown. `HeadResponse.wait_supported` defaults false and is omitted unless a hub honored bounded waits.

## Work Guidance

- Add wire types next to related ones with `Debug, Clone, Serialize, Deserialize`; `#[must_use]` on pure helpers.
- Keep tests pure: unit tests beside the code, integration tests in `tests/`.
- New untrusted-input parsers get a property in `tests/fuzz/properties.rs` (shared with `/fuzz`) and seeds in `tests/parser_fuzz.rs`.

## Verification

- `cargo test -p feanorfs-common` (includes `tests/parser_fuzz.rs`: 3 000 deterministic mutations per seed per parser).
- `cargo clippy -p feanorfs-common --all-targets -- -D warnings`; `cargo fmt -p feanorfs-common -- --check`.
- Coverage-guided: `cargo +nightly fuzz run <tree_codec|invites|signals|aead>` from the repository root (CI: `security.yml`).

## Child DOX Index

No child directories carry separate contracts; `tests/fuzz/` holds shared fuzz properties.
