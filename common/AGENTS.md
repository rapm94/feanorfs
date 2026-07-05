# common

## Purpose

Shared data models, sync delta (`compute_sync_delta`), three-way conflict classification (`detect_concurrent_edits`), and crypto (`pack_bytes`/`unpack_bytes` AEAD + legacy `crypt_bytes`) used by both server and client.

## Ownership

- Crate: `feanorfs-common` (library only; no binary).
- Public surface: every item in `src/lib.rs` is `pub` and re-exported through downstream crates. Treat the wire types as a binding contract — changing field names or types requires server AND client releases in lockstep.
- No file system, network, or sqlite dependencies. This crate must remain leaf-only so it can be embedded in both server and client without pulling their heaviest transitive deps.

## Local Contracts

- `pack_bytes` / `unpack_bytes` — ChaCha20-Poly1305 for new blobs; format v2 workspaces reject non-AEAD blobs. Unmigrated v1 workspaces still fall back to legacy `crypt_bytes` XOR on decrypt — removal tracked as [SEC-6](../docs/roadmap.md).
- Deterministic SIV-style nonce (`blake3(key ‖ len ‖ plaintext)[..12]`) is LOAD-BEARING: CAS keys and change detection require identical `(key, path, plaintext)` → identical ciphertext. Do NOT switch to random nonces. Known accepted leak: the server can observe a file reverting to a previous state.
- `compute_sync_delta` — pure LWW read-only delta (used by server peek/diff handlers).
- `detect_concurrent_edits` / `classify_conflict_kind` — shared three-way logic for agent and workspace conflicts.
- Length-prefix domain separation before each XOF input field is mandatory — never concatenate without it. `(password="ab", path="cdef")` and `(password="abc", path="def")` MUST produce different keystreams.
- `is_valid_hash(hash)` returns true iff `hash` is exactly 64 lowercase hex chars. All blob download/upload endpoints MUST reject anything else to prevent path traversal via `..` or absolute paths.
- `LEGACY_DEFAULT_PASSWORD` is an unsafe fallback preserved only for legacy compatibility. New code paths MUST surface a warning when this default is used; treat any caller relying on it as a bug.

## Work Guidance

- Add new wire types next to existing ones. Derive `Debug, Clone, Serialize, Deserialize` matching the surrounding convention. Use `#[must_use]` on pure helpers (`hash_bytes`, `normalize_path`, `crypt_bytes`, `is_valid_hash`) so silent drops surface as warnings.
- Tests live inside `src/lib.rs` under `#[cfg(test)] mod tests` and in `tests/sync_models.rs` (integration). Pure-property tests (determinism, roundtrip, rejection cases) belong here; do not add tests that require I/O.

## Verification

- `cargo test -p feanorfs-common` — exercises crypt_bytes roundtrips, domain separation, `is_valid_hash` rejections, `normalize_path`, `FileState` serde, `generate_password` properties.
- `cargo clippy -p feanorfs-common -- -D warnings`.
- `cargo fmt -p feanorfs-common -- --check`.

## Child DOX Index

No child directories. `src/` is a flat module and `tests/` is a single integration file.