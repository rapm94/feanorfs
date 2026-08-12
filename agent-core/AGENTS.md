# agent-core

## Purpose

Embeddable Rust SDK for snapshot sync and agent workspace isolation. Owns encrypted objects, snapshot heads, log/undo, spawn/status/refresh/land/clean, unattended runner lifecycle state, conflict resolution, encrypted agent signals, and randomized integrator assignment over in-process or HTTP transport. No CLI, watcher, summary, or predictive hydration. Consumers include `feanorfs-client`, `feanorfs-ffi`, and `feanorfs-agent-node`.

## Ownership

- Crate: `feanorfs-agent-core` (`agent-core/`).
- Public blocking API: [`Runtime`](src/lib.rs), [`Workspace`](src/lib.rs), [`SpawnOptions`](src/lib.rs), [`LandOptions`](src/lib.rs), and [`RefreshOptions`](src/agent.rs).
- Internal modules:
  - `agent.rs` + `agent/` — thin facade plus three-way diff, spawn, land phases, refresh, runner lifecycle state/leases, proposals, and focused tests.
  - `conflicts.rs` / `conflict_artifacts.rs` — workspace conflict gate and artifact layout.
  - `local.rs` + `local/` — thin local-state facade with config, cache, conflicts, access log, workspace walking, scanning, and focused tests.
  - `state.rs` + `state/` — schema-versioned `LocalStateV1`, lock-protected `DurableState`, and focused model/persistence tests.
  - `api.rs` / `hub.rs` + `hub/` — HTTPS/HTTP and in-process `ApiClient`; private hub CA certificates extend normal reqwest/Rustls trust without accepting invalid certificates. Embedded routes operate directly against `HubDb` and blob files without importing `feanorfs_server`.
  - `tunnel.rs` — opaque relay transport. A remote client binds an ephemeral loopback bridge but retains the hub hostname for TLS SNI/verification; an owned hub maintains outbound offers and forwards only the existing Rustls byte stream.
  - `hub_state.rs` + `hub_state/` — `HubDb`, `HubStateV1`, workspace metadata, heads, manifests, migration fences, and migration projection.
  - `sync_pass.rs` — sync orchestration plus fail-closed structural planning and the same-filesystem staged materializer. Verified downloads activate through rollback backups, one atomic cache update, and a bounded crash-recovery journal; untracked/symlink content is never removed.
  - `large_file.rs` — format-v3 authenticated chunk manifests and fixed-size encrypted chunks over the existing opaque CAS, including streaming materialization and retained-manifest reachability.
  - `objects.rs` / `prepared_tree.rs` / `snapshot.rs` / `snapshot_diff.rs` — encrypted immutable objects, refs, linear-time tree preparation, bounded object reads, and budgeted iterative/hash-pruned traversal.
  - `history.rs` — bounded reachable history and append-only undo; target bytes are authenticated and staged before its head CAS.
  - `messages.rs` — encrypted agent signals: `send_message` (no-file-change snapshots with fresh-root CAS retry) and `inbox` (reachability-delta traversal with cursors, 10k scan bound); wire types in `feanorfs_common::agent_contract`.
  - `integrator.rs` — randomized integrator assignment: neutral-only ranked pools when possible, dispatcher state machine, `orchestrator/integrator-state.json` crash-safe persistence, exact request-snapshot cursors, recovery adoption of a published-but-unrecorded offer, context-bound and causally staged `ffint1` replies, persisted pre-acceptance timeout/fallback, revocation, cursor-reset fail-closed, and bounded read-only cross-machine conflict materialization. Canonical types/ranking in `feanorfs_common::integrator_contract`; `lock.rs::DispatcherLock` enforces the single-dispatcher invariant.
  - `tree_reconcile.rs` — last-synced tree reconciliation for sync conflict gating.
  - `object_gc.rs` — local object-cache pruning from retained manifests and refs (throttled).
  - `upload_registry.rs` — bounded durable per-workspace copy of the latest accepted reachability closure, used to skip redundant uploads without accumulating all historical objects.
  - `paths.rs` — owning-workspace agent worktree/state paths, conflicts dir, and name validation (breaks agent↔conflicts cycle).
  - `workspace_read.rs` — descriptor-anchored workspace reads. Unix traversal retains the root directory descriptor and opens every component with `openat`/`O_NOFOLLOW`; portable checked fallbacks reject aliases, symlinks, and non-regular files.
  - `ctx.rs`, `crypto.rs`, `fs_util.rs`, `lock.rs` — shared helpers.

Wire types and semver JSON contract live in `feanorfs_common::agent_contract` — see [docs/agent-api.md](../docs/agent-api.md).

## Local Contracts

- Blocking facade: `Runtime::new()` owns a multi-thread Tokio runtime; all public methods use `block_on`. Calls and final runtime drop remain valid inside current- or multi-thread Tokio contexts by moving nested blocking work/drop to a scoped ordinary thread.
- Agent names are portable single path components capped at 255 UTF-8 bytes. `ffmsg1` validation reuses the common contract (8-KiB body, 64-KiB total canonical envelope) before publication or parsing.
- Integrator reply envelopes must match the selected candidate, dispatcher recipient, kind, original request, assignment/attempt, and about snapshot. Result digests additionally bind the selected integrator and a reachable inspected snapshot. Inbox batches apply acceptance before terminal replies.
- Unit and integration test crate roots link `feanorfs-test-support` once. Its pre-main process profile replaces test-local HOME mutation; subprocesses inherit it and parallel tests never change profile environment variables.
- JSON shapes returned to FFI/Node/CLI `--json` MUST match `docs/agent-api.md`; snapshot tests in `client/tests/contract_snapshots.rs`.
- Tray JSON shapes live in `feanorfs_common::tray_contract` with fixtures + snapshots in `client/tests/tray_contract_snapshots.rs`.
- `ResolveKeep::Cloud` on `edit_delete` conflicts: when the cloud artifact is the deletion sentinel, remove the local file and upload a tombstone (`is_cloud_deleted_sentinel` in `conflict_artifacts.rs`).
- A missing local leg classified as `delete_edit` uses the `deleted-locally` artifact sentinel; never describe an actual local deletion as “no local changes.”
- Agent workspaces isolate data, not processes — never claim sandboxing.
- Each agent base is one atomic private-state `base-snapshot` ref. Per-path `agent_snapshots` rows are forbidden.
- Land uploads immutable blobs and objects and prefetches/fsyncs every clean landed file before compare-and-swap. The head swap is the commit point; worktree and legacy projections happen afterward through the rollback-capable materializer. A post-CAS interruption is recovered idempotently from its journal or by the existing committed-land retry path.
- Format-v3 conflict identity and last-synced state come from trees and refs, never `last_synced_files` rows.
- Bulk local or cloud conflict resolution validates every selected artifact before mutation, materializes the explicit policy (including cloud deletions), publishes one resolution snapshot, and updates the registry plus resolution history in one durable-state commit. Format-v2 retains the same flat-server-view projection as single-path resolution.
- `undo` acquires the sync lock, validates the complete target projection, authenticates and fsyncs target bytes before CAS, then appends a two-parent snapshot that retains both previous head and pre-operation worktree state.
- Sync-lock stale detection uses native process-liveness checks on Unix and Windows. Never treat every Windows PID as dead: that can break a live worker's lock and misreport tray watcher state. A lock owned by a live process is never broken by the age cap (24 h floor as a PID-reuse guard), so long-running syncs are not broken out; same-pid re-acquire refreshes the lock timestamp.
- Server-published snapshots must upload every referenced file blob before their reachability manifest. Working-copy refs may use local-only manifests until they become publishable state.
- `SyncCtx::state_dir` resolves and caches the workspace-state path once per operation context. It holds the per-context mutex through first resolution, caches only success, and never uses a process-global path cache; a fresh context after relocation re-runs identity lookup and updates `location`. Agent-worktree contexts explicitly pin `agents/<name>/state/runtime` instead, so they never create another top-level workspace registration. Preferred path-hash slots are accepted only when their stored filesystem identity matches; moved lookup is bounded and rejects duplicate identity matches, and same-path folder replacement fails closed instead of inheriting credentials/state.
- Format-v3 files above 64 MiB use deterministic 8 MiB AEAD chunks plus an authenticated, path-bound encrypted manifest. The file's tree hash names the manifest ciphertext; every chunk is ciphertext-hash verified, index/path-bound during decryption, and included in server reachability before publication. Files above the former 100 MiB body limit therefore never create an oversized request. Format-v1/v2 reports at most five exact examples and requires migration instead of attempting an oversized upload.
- Rekey publishes a parentless root because old-key snapshot parents are intentionally unreadable under the new key.
- Sync and agent conflict identity is hash/deletion/executable-intent based. Cross-machine mtime can indicate a possible server rollback, but never decides whether content changed.
- Workspace and agent-worktree content reads are descriptor anchored. On Unix, scanners, uploads, large-file hashing/streaming, spawn copies, undo, and conflict-local choices retain a no-follow root and traverse every component with `openat`; bytes and before/after metadata come from one opened regular-file descriptor. Small uploads must reproduce the scanned encrypted hash before any network write. Large uploads plan and stream the same descriptor, authenticate even registry-known chunks, and retain at most four pending encrypted chunks. Portable checked fallbacks reject noncanonical aliases, reserved/device spellings, symlinks, and non-regular files before opening.
- Downloads guard against clobbering any touched file, ancestor, deletion, or lazy placeholder by revalidating the scan-time state after staging. A canonical target is checked before mutation; every replacement is authenticated/fsynced first, worktree/stage directory entries are fsynced in commit order, Unix publication traverses already-open no-follow directory handles and applies mode/fsync through the published file descriptor (other platforms fail closed on checked ancestors), staged hard links remain until cache commit for inode-exact crash recovery, originals move to same-filesystem rollback backups, transaction-created directories are rolled back only when proven empty, and cache deletes/upserts commit once after all deterministic renames and mode changes. `.feanorfs-tmp-materialize-*` journals recover `preparing`, `activating`, or `activated` crashes without deleting changed/untracked/symlink content; journal-less empty preparations are removable and unreadable new-only preparations are quarantined outside the active recovery namespace. `upload_registry.rs` may skip objects only from the latest bounded reachability closure the hub accepted. Only an HTTP 412 missing-blob response proves that registry stale and may clear it for a forced reupload; other failures preserve it.
- Format-v3 conflict trees preserve executable intent independently for base/ours/theirs. Zero-mode conflicts remain byte-exact FTR1; executable conflict metadata uses FTR2, and artifacts, integrator legs, single/bulk keep policies, and resolved snapshots apply the selected authoritative mode.
- Object, reachability, prepared-tree, snapshot-diff, and history processing share bounded object/work/output/path budgets. HTTP and embedded success/error responses plus object reads are length-bounded before body growth; cache entries are metadata-checked and read through a bounded stream.
- `atomic_write` owns a collision-safe temp file under the private global workspace `tmp/`, flushes and syncs it before rename, and removes it on every failed path. Destination bytes and cache state remain untouched after a failed write.
- Workspace walkers never follow symlinks and prune nested directories with a valid `CACHEDIR.TAG`; a workspace-root tag is deliberately exempt to prevent accidental mass deletion.
- `LocalHub::open` caches by canonical data-dir path plus auth token so a token change always opens a fresh instance. Metadata mutations are serialized through `hub_state.json` with `fs2` exclusive lock and `AtomicWriteFile::commit`. Blobs remain in `blobs/<hash>`. 100 MiB body and 64 MiB manifest limits, root-bound immutable manifest closures, unconditional manifested-head publication, and valid-hash path-traversal defense are enforced in parity with the server. Server SQLite code is untouched.
- Agent spawn, status, and land build their base-workspace `SyncCtx` from the loaded workspace `Config`; never replace that with the fallback constructor, which intentionally defaults to legacy format 2 when no config exists.
- `ClientDb` stores its cache, conflict registry, conflict resolution history, session keys, and access log in the private global workspace `local_state.json`, serialized as a schema-versioned BTreeMap-based JSON document. Canonical serialization borrows the large maps and sorts only vectors of references; durable commits stream this view through a bounded writer directly into the `AtomicWriteFile` temporary file, never restoring a full state clone or allocating a complete intermediate JSON string. Construction acquires an exclusive lock on the sibling `local_state.lock` before checking or initializing state — two racing first-opens cannot both see a missing file and overwrite data. After construction, reads and writes treat a missing state file as corruption. Every mutable operation follows lock exclusive → reload → mutate → `AtomicWriteFile::commit` → parent directory sync. Pre-commit failures, including size-limit or serialization errors, preserve prior bytes; post-commit directory-sync failures return committed-but-durability-uncertain and treat the new state as authoritative. Input is capped at 128 MiB before read/parse, collection cardinalities are bounded, and schema probing avoids a duplicate generic JSON tree. Malformed JSON and unknown future schema versions are rejected by `ClientDb::new`. Directory scans use `bulk_upsert_cache_entries` for a single commit per scan. A legacy project-local `local_cache.db` without `local_state.json` returns `run 'feanorfs migrate' from the workspace root` without mutation.
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
- `cargo test -p feanorfs-agent-core --release local_state_serialization_profile_100k -- --ignored --nocapture` — opt-in 100k-entry serialization profile; normal suites skip it.
- `cargo test -p feanorfs-agent-core --release 'state::tests::persistence::local_state_persistence_profile_100k' --locked -- --ignored --nocapture --exact` — opt-in streaming persistence profile; normal suites skip it.

## Child DOX Index

| Child | Purpose |
| :--- | :--- |
| [`src/agent/`](src/agent/AGENTS.md) | Agent diff, spawn, land phases, refresh, runner lifecycle, proposal generation, and validation tests. |
| [`src/hub/`](src/hub/AGENTS.md) | Embedded hub request dispatch, HTTP helpers, and route groups. |
| [`src/hub_state/`](src/hub_state/AGENTS.md) | JSON hub persistence, blob storage, and SQLite migration projection. |
| [`src/local/`](src/local/AGENTS.md) | Local configuration, JSON-backed `ClientDb` operations, workspace walking/scanning, and focused tests. |
| [`src/state/`](src/state/AGENTS.md) | Local-state durable persistence and focused schema/atomicity tests. |
