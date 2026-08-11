# common

## Purpose

Shared data models, canonical Merkle tree/snapshot objects, sync delta (`compute_sync_delta`), three-way conflict classification (`detect_concurrent_edits`), the canonical integrator-assignment contract (`integrator_contract.rs`: eligibility, auditable ranking, `ffint1` profiles, digest bounds), and crypto (`pack_bytes`/`unpack_bytes` AEAD + legacy `crypt_bytes`) used by both server and client.

## Ownership

- Crate: `feanorfs-common` (library only; no binary).
- `release-product-state.txt` is a content-only release-selection carrier maintained by `scripts/update-release-product-state.sh`; it is not compiled or read at runtime.
- `integrator_contract.rs` — canonical randomized-integrator contract (candidates, strict canonical capabilities, neutral-only selection/fallback whenever a neutral candidate exists, length-prefixed Blake3 ranking, context-bound `ffint1` encode/parse, bounded paths, and internally consistent terminal digests). Pure and testable; reused verbatim by CLI, SDK, FFI, TS, and MCP.
- `tray_contract.rs` — bounded secret-free desktop projections. `TrayOverviewResult` additively wraps the stable `TrayStatusResult` plus an optional recent-workspace registry so one refresh needs one CLI process; fixtures are snapshot-tested by the client.
- Public surface: every item in `src/lib.rs` is `pub` and re-exported through downstream crates. Treat the wire types as a binding contract — changing field names or types requires server AND client releases in lockstep.
- No file system, network, or sqlite dependencies. Dependencies stay leaf-only: serialization, hashing, randomness/time/error, ChaCha20-Poly1305, and Unicode normalization for canonical portable paths. This crate must remain embeddable in server and client without their heavy transitive stacks.

## Local Contracts

- `pack_bytes` / `unpack_bytes` — ChaCha20-Poly1305 for new blobs; format v2 and v3 workspaces reject non-AEAD blobs. Unmigrated v1 workspaces fall back to legacy `crypt_bytes` XOR when the AEAD prefix is absent or authentication fails, because valid legacy ciphertext has a 1/256 prefix collision; removal requires separately approved representative field evidence.
- Deterministic SIV-style nonce (`blake3(key ‖ len ‖ plaintext)[..12]`) is LOAD-BEARING: CAS keys and change detection require identical `(key, path, plaintext)` → identical ciphertext. Do NOT switch to random nonces. Known accepted leak: the server can observe a file reverting to a previous state.
- `compute_sync_delta` — pure LWW read-only transport hint used by server peek/diff handlers. Ignore unsafe paths from either side. On equal mtimes, client state is the deterministic tie-break across content, deletion, and executable intent so peers cannot remain divergent. Clients reconcile the complete server view against their last agreed state by hash; cross-machine mtime is not conflict identity.
- `detect_concurrent_edits` / `classify_conflict_kind` — shared three-way logic for agent and workspace conflicts. Absence of a local entry that existed in the base is a deletion, not “unchanged.” When ours and theirs independently reach identical hash/deletion/executable-intent state, they have converged and do not conflict even when mtimes differ.
- `ffmsg1` agent names are portable single components capped at 255 UTF-8 bytes; signal bodies are capped at 8 KiB and the complete canonical envelope at 64 KiB. Parsers reject the total size before JSON allocation.
- Integrator capabilities must arrive already canonical (no padding or duplicates). A completed digest requires passed verification, zero remaining conflicts, and no decision question; `requires_human` requires exactly one bounded question. Materialization paths are unique canonical portable paths capped at 256 entries/4 KiB each.
- Length-prefix domain separation before each XOF input field is mandatory — never concatenate without it. `(password="ab", path="cdef")` and `(password="abc", path="def")` MUST produce different keystreams.
- `is_valid_hash(hash)` returns true iff `hash` is exactly 64 lowercase hex chars. All blob download/upload endpoints MUST reject anything else to prevent path traversal via `..` or absolute paths.
- `is_safe_rel_path(path)` accepts only one NFC-normalized portable forward-slash spelling: no absolute/drive/device syntax, backslashes, empty/dot components, Windows aliases, or case-insensitive `.git`/`.jj`/`.feanorfs` components. Callers must validate the exact path they later join or persist; normalization is not validation.
- `canonical_manifest_hashes(snapshot_id, manifest)` validates and canonicalizes one format-v3 reachability closure; `canonical_manifest_hash_list` applies the same root/entry rules to an already-split list without joining it first. Every manifest must contain its own snapshot root; empty/rootless manifests are invalid, and raw entries are capped at `MANIFEST_MAX_ENTRIES` before owned-hash allocation.
- `LEGACY_DEFAULT_PASSWORD` is an unsafe fallback preserved only for legacy compatibility. New code paths MUST surface a warning when this default is used; treat any caller relying on it as a bug.
- `tree.rs` owns public snapshot types; `tree_codec.rs` owns versioned canonical bytes; `tree_convert.rs` and `tree_diff.rs` keep flat conversion and hash-pruned traversal I/O-free.
- Canonical tree ids hash sorted, length-prefixed bytes. Snapshot identity excludes mtimes. Flat map keys and conflict-leg paths must exactly equal their embedded canonical paths. Directory entries have zero size/mode, and non-root empty directories are rejected because the flat worktree model cannot represent their identity. `FileState.mode` is portable executable intent (`0` or `EXECUTABLE_MODE`) and zero stays absent from legacy JSON. Ordinary trees and zero-mode conflicts retain byte-exact `FTR1`; `FTR2` is emitted only when a conflict must preserve authoritative base/ours/theirs executable modes.
- Conflict entry `hash` and top-level mode identify the leg visible in the working copy: `theirs`, then `ours`, then `base`. Tree decoding rejects mismatched visible legs, modes on absent legs, invalid portable modes, and noncanonical zero-mode `FTR2` objects.
- Canonical conversion/diff operations enforce the shared 16 MiB object, 256-level depth, 250,000-object/output, two-million-work-item, and 64 MiB aggregate-path budgets. Traversals are iterative and apply the path ceiling both cumulatively to every constructed path and simultaneously to queued directory prefixes. Reused content-addressed subtrees remain valid outside the active ancestry; bound expansion rather than banning DAG deduplication.
- `fnh1` hub invites carry URL, optional bearer token, optional public CA, and optional opaque-relay metadata; `fnr1` workspace invites additionally carry workspace ID, E2EE key, and an optional global ignore policy. The policy is encrypted whenever the capability travels through pairing or recovery; `None` remains backward-compatible with older capabilities. `RelayConfig` contains a public relay URL plus random 256-bit reachability route, never the bearer token. TLS CA fields are public certificates only—private keys never enter common wire types. Invite encoders enforce their decoder's total token bound, hex decoding rejects arbitrary UTF-8 without byte-slicing panics, and `Debug` output redacts bearer tokens, E2EE keys, relay routes, CA bodies, and ignore-policy contents.
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
