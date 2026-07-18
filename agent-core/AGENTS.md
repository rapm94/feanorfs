# agent-core

## Purpose

Embeddable Rust SDK for snapshot sync and agent workspace isolation. Owns encrypted objects, snapshot heads, log/undo, spawn/status/refresh/land/clean, and conflict resolution over in-process or HTTP transport. No CLI, watcher, summary, or predictive hydration. Consumers include `feanorfs-client`, `feanorfs-ffi`, and `feanorfs-agent-node`.

## Ownership

- Crate: `feanorfs-agent-core` (`agent-core/`).
- Public blocking API: [`Runtime`](src/lib.rs), [`Workspace`](src/lib.rs), [`SpawnOptions`](src/lib.rs), [`LandOptions`](src/lib.rs), and [`RefreshOptions`](src/agent.rs).
- Internal modules:
  - `agent.rs` + `agent/` — thin facade plus three-way diff, spawn, land phases, refresh, proposals, and focused tests.
  - `conflicts.rs` / `conflict_artifacts.rs` — workspace conflict gate and artifact layout.
  - `local.rs` + `local/` — thin local-state facade with config, cache, conflicts, access log, workspace walking, scanning, and focused tests.
  - `state.rs` + `state/` — schema-versioned `LocalStateV1`, lock-protected `DurableState`, and focused model/persistence tests.
  - `api.rs` / `hub.rs` + `hub/` — HTTPS/HTTP and in-process `ApiClient`; private hub CA certificates extend normal reqwest/Rustls trust without accepting invalid certificates. Embedded routes operate directly against `HubDb` and blob files without importing `feanorfs_server`.
  - `tunnel.rs` — opaque relay transport. A remote client binds an ephemeral loopback bridge but retains the hub hostname for TLS SNI/verification; an owned hub maintains outbound offers and forwards only the existing Rustls byte stream.
  - `hub_state.rs` + `hub_state/` — `HubDb`, `HubStateV1`, workspace metadata, heads, manifests, migration fences, and migration projection.
  - `sync_pass.rs` — minimal sync pass used before spawn when `no_sync=false`.
  - `objects.rs` / `prepared_tree.rs` / `snapshot.rs` / `snapshot_diff.rs` — encrypted immutable objects, refs, and hash-pruned traversal.
  - `history.rs` — reachable history and append-only undo.
  - `tree_reconcile.rs` — last-synced tree reconciliation for sync conflict gating.
  - `object_gc.rs` — local object-cache pruning from retained manifests and refs.
  - `paths.rs` — `.feanorfs/agents`, conflicts dir, name validation (breaks agent↔conflicts cycle).
  - `ctx.rs`, `crypto.rs`, `fs_util.rs`, `lock.rs` — shared helpers.

Wire types and semver JSON contract live in `feanorfs_common::agent_contract` — see [docs/agent-api.md](../docs/agent-api.md).

## Local Contracts

- Blocking facade: `Runtime::new()` owns a multi-thread Tokio runtime; all public methods use `block_on`.
- JSON shapes returned to FFI/Node/CLI `--json` MUST match `docs/agent-api.md`; snapshot tests in `client/tests/contract_snapshots.rs`.
- Tray JSON shapes live in `feanorfs_common::tray_contract` with fixtures + snapshots in `client/tests/tray_contract_snapshots.rs`.
- `ResolveKeep::Cloud` on `edit_delete` conflicts: when the cloud artifact is the deletion sentinel, remove the local file and upload a tombstone (`is_cloud_deleted_sentinel` in `conflict_artifacts.rs`).
- Agent workspaces isolate data, not processes — never claim sandboxing.
- Each agent base is one atomic `.feanorfs/base-snapshot` ref. Per-path `agent_snapshots` rows are forbidden.
- Land uploads immutable blobs and objects before compare-and-swap. The head swap is the commit point; worktree and legacy projections happen afterward.
- Format-v3 conflict identity and last-synced state come from trees and refs, never `last_synced_files` rows.
- Bulk local conflict resolution validates every path before mutation, uploads
  the selected working-copy versions, publishes one resolution snapshot, and
  updates the registry plus resolution history in one durable-state commit.
  Format-v2 retains the same flat-server-view projection as single-path
  resolution.
- `undo` acquires the sync lock and appends a two-parent snapshot that retains both previous head and pre-operation worktree state.
- Sync-lock stale detection uses native process-liveness checks on Unix and Windows. Never treat every Windows PID as dead: that can break a live worker's lock and misreport tray watcher state.
- Server-published snapshots must upload every referenced file blob before their reachability manifest. Working-copy refs may use local-only manifests until they become publishable state.
- Rekey publishes a parentless root because old-key snapshot parents are intentionally unreadable under the new key.
- Sync and agent conflict identity is hash/deletion based. Cross-machine mtime can indicate a possible server rollback, but never decides whether content changed.
- `atomic_write` owns a collision-safe temp file under `.feanorfs/tmp/`, flushes and syncs it before rename, and removes it on every failed path. Destination bytes and cache state remain untouched after a failed write.
- Workspace walkers never follow symlinks and prune nested directories with a valid `CACHEDIR.TAG`; a workspace-root tag is deliberately exempt to prevent accidental mass deletion.
- `LocalHub::open` caches by canonical data-dir path plus auth token so a token change always opens a fresh instance. Metadata mutations are serialized through `hub_state.json` with `fs2` exclusive lock and `AtomicWriteFile::commit`. Blobs remain in `blobs/<hash>`. 100 MiB body limit, 8 MiB manifest limit, and valid-hash path-traversal defense are enforced. Server SQLite code is untouched.
- Agent spawn, status, and land build their base-workspace `SyncCtx` from the loaded workspace `Config`; never replace that with the fallback constructor, which intentionally defaults to legacy format 2 when no config exists.
- `ClientDb` stores its cache, conflict registry, conflict resolution history, session keys, and access log at `.feanorfs/local_state.json`, serialized as a schema-versioned BTreeMap-based JSON document. Construction acquires an exclusive lock on `.feanorfs/local_state.lock` before checking or initializing state — two racing first-opens cannot both see a missing file and overwrite data. After construction, reads and writes treat a missing state file as corruption. Every mutable operation follows lock exclusive → reload → mutate → `AtomicWriteFile::commit` → parent directory sync. Pre-commit failures preserve prior bytes; post-commit directory-sync failures return committed-but-durability-uncertain and treat the new state as authoritative. Malformed JSON and unknown future schema versions are rejected by `ClientDb::new`. Directory scans use `bulk_upsert_cache_entries` for a single commit per scan. A legacy `local_cache.db` without `local_state.json` returns `run 'feanorfs migrate' from the workspace root` without mutation.
- Access log is deterministically bounded: max 10 000 entries, minimum absolute weight 0.001. `record_access_pair` rejects non-finite `weight_delta`. After insertion, update, or decay, entries below the threshold are pruned; when over the cap, entries are evicted by ascending weight, ascending `updated_at`, then path/sibling keys. `from_json` validates all loaded weights are finite.
- Workspace/global config writes are atomic. Secure onboarding stores keys/tokens in macOS Keychain for signed releases, Windows Credential Manager, or Linux Secret Service and writes only a random `fsc1` reference to JSON. Unsigned macOS/source builds and unavailable stores fall back to Unix `0700`/`0600` protected files; an existing OS-backed config fails closed and never spills secrets back to JSON. Background services resolve credentials in-process and never receive them in argv, environment variables, or logs. Optional `tls_ca_pem` is public trust material delivered by a secure capability and persisted beside the endpoint.
- `ApiClient::new_with_tls_resolved` may override address lookup for a hostname, but it must preserve the URL hostname as TLS SNI/name verification and retain the pinned CA. It exists for CA-correlated mDNS reachability, never certificate bypass.
- Relay routes are exactly 256-bit lowercase hex and relay URLs require WSS outside loopback tests. The readiness Ping/Pong must complete before reading the local TLS ClientHello. Never log the route or put it in worker argv; never terminate inner TLS at the relay.

## Work Guidance

- Keep this crate free of `clap`, `notify`, and `tracing-subscriber`.
- New agent-facing operations go here first; `feanorfs-client` re-exports thin wrappers.
- Path helpers belong in `paths.rs` — do not reintroduce `agent` ↔ `conflicts` module cycles.

## Verification

- `cargo test -p feanorfs-agent-core`
- `cargo test -p feanorfs-ffi` (C ABI smoke)
- `cargo test -p feanorfs-client contract_snapshots`
- `cargo test -p feanorfs-client tray_contract_snapshots`
- `cargo test -p feanorfs-agent-core --release -- --ignored --nocapture scan_profile_10k` — opt-in 10k scanner profile; normal suites skip it.

## Child DOX Index

| Child | Purpose |
| :--- | :--- |
| [`src/agent/`](src/agent/AGENTS.md) | Agent diff, spawn, land phases, refresh, proposal generation, and validation tests. |
| [`src/hub/`](src/hub/AGENTS.md) | Embedded hub request dispatch, HTTP helpers, and route groups. |
| [`src/hub_state/`](src/hub_state/AGENTS.md) | JSON hub persistence, blob storage, and SQLite migration projection. |
| [`src/local/`](src/local/AGENTS.md) | Local configuration, JSON-backed `ClientDb` operations, workspace walking/scanning, and focused tests. |
| [`src/state/`](src/state/AGENTS.md) | Local-state durable persistence and focused schema/atomicity tests. |
