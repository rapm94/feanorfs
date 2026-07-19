# common

## Purpose

Shared data models, canonical Merkle tree/snapshot objects, sync delta (`compute_sync_delta`), three-way conflict classification (`detect_concurrent_edits`), and crypto (`pack_bytes`/`unpack_bytes` AEAD + legacy `crypt_bytes`) used by both server and client.

## Ownership

- Crate: `feanorfs-common` (library only; no binary).
- `release-product-state.txt` is a content-only release-selection carrier maintained by `scripts/update-release-product-state.sh`; it is not compiled or read at runtime.
- Public surface: every item in `src/lib.rs` is `pub` and re-exported through downstream crates. Treat the wire types as a binding contract — changing field names or types requires server AND client releases in lockstep.
- No file system, network, or sqlite dependencies. This crate must remain leaf-only so it can be embedded in both server and client without pulling their heaviest transitive deps.

## Local Contracts

- `pack_bytes` / `unpack_bytes` — ChaCha20-Poly1305 for new blobs; format v2 and v3 workspaces reject non-AEAD blobs. Unmigrated v1 workspaces still fall back to legacy `crypt_bytes` XOR on decrypt; removal requires separately approved representative field evidence.
- Deterministic SIV-style nonce (`blake3(key ‖ len ‖ plaintext)[..12]`) is LOAD-BEARING: CAS keys and change detection require identical `(key, path, plaintext)` → identical ciphertext. Do NOT switch to random nonces. Known accepted leak: the server can observe a file reverting to a previous state.
- `compute_sync_delta` — pure LWW read-only transport hint used by server peek/diff handlers. Clients reconcile the complete server view against their last agreed state by hash; cross-machine mtime is not conflict identity.
- `detect_concurrent_edits` / `classify_conflict_kind` — shared three-way logic for agent and workspace conflicts. When ours and theirs independently reach identical hash/deletion state, they have converged and do not conflict even when mtimes differ.
- Length-prefix domain separation before each XOF input field is mandatory — never concatenate without it. `(password="ab", path="cdef")` and `(password="abc", path="def")` MUST produce different keystreams.
- `is_valid_hash(hash)` returns true iff `hash` is exactly 64 lowercase hex chars. All blob download/upload endpoints MUST reject anything else to prevent path traversal via `..` or absolute paths.
- `LEGACY_DEFAULT_PASSWORD` is an unsafe fallback preserved only for legacy compatibility. New code paths MUST surface a warning when this default is used; treat any caller relying on it as a bug.
- `tree.rs` owns public snapshot types; `tree_codec.rs` owns versioned canonical bytes; `tree_convert.rs` and `tree_diff.rs` keep flat conversion and hash-pruned traversal I/O-free.
- Canonical tree ids hash sorted, length-prefixed bytes. Snapshot identity excludes mtimes. `FileState.mode` is portable executable intent (`0` or `EXECUTABLE_MODE`) and zero stays absent from legacy JSON.
- Conflict entry `hash` identifies the leg visible in the working copy: `theirs`, then `ours`, then `base`. Tree decoding rejects any other value.
- `fnh1` hub invites carry URL, optional bearer token, optional public CA, and optional opaque-relay metadata; `fnr1` workspace invites additionally carry workspace ID, E2EE key, and an optional `.feanorfsignore` policy. The policy is encrypted whenever the capability travels through pairing or recovery; `None` remains backward-compatible with older capabilities. `RelayConfig` contains a public relay URL plus random 256-bit reachability route, never the bearer token. TLS CA fields are public certificates only—private keys never enter common wire types.
- `hub_ca_fingerprint` and `hub_mdns_hostname` derive public discovery identity from the exact serialized CA certificate. The hostname is reachability metadata only; clients still pin the full public CA from an authenticated capability.

## Work Guidance

- Add new wire types next to existing ones. Derive `Debug, Clone, Serialize, Deserialize` matching the surrounding convention. Use `#[must_use]` on pure helpers (`hash_bytes`, `normalize_path`, `crypt_bytes`, `is_valid_hash`) so silent drops surface as warnings.
- Tests live inside `src/lib.rs` under `#[cfg(test)] mod tests`, `tests/sync_models.rs`, and `tests/tree_models.rs`. Pure-property tests (determinism, roundtrip, rejection cases) belong here; do not add tests that require I/O.

## Verification

- `cargo test -p feanorfs-common` — exercises crypto, path/hash rejection, wire serde, canonical tree/snapshot roundtrips, executable intent, and changed-subtree diff bounds.
- `cargo clippy -p feanorfs-common -- -D warnings`.
- `cargo fmt -p feanorfs-common -- --check`.

## Child DOX Index

No child directories. `src/` is a flat module and `tests/` is a single integration file.
