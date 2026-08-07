# Merkle snapshot engine review — findings and fixes

Review scope: format-v3 encrypted Merkle snapshot engine (`common/src/tree*`,
`agent-core/src/{objects,snapshot,snapshot_diff,prepared_tree,history,head,object_gc,sync_pass,large_file,upload_registry}.rs`,
`server/src/{db,gc}.rs`, embedded `LocalHub`), plus client materialization paths.
All fixes shipped in this session; acceptance evidence below.

## Critical correctness

- [x] **C1 — v3/v2 clients can never download updates over an existing file (silent revert).**
  `process_downloads`'s `stale_local` guard compared the disk mtime against `local.mtime`, which the
  scan fills from the cached *server* mtime — `0` for every tree-flattened v3 file. The guard always
  tripped, the download was skipped, `finish_sync_pass` recorded the head as last-synced anyway, and
  the stale copy was later pushed back, reverting the updater's edit. Also silently broke `undo`
  materialization and update-over-placeholder pulls.
  *Fix (`agent-core/src/sync_pass.rs`): compare the current disk mtime/size against the scan-time
  disk fingerprint (cache entry `mtime`/`size`) and treat lazy placeholders (`hydrated=false`) as
  never-stale.*
  **Evidence:** before — A pushes v2 → B pull skips ("local file changed since scan") → B sync
  re-uploads v1 → A sync downloads v1 (A's edit silently reverted). After — A pushes v2 → B pull
  shows `Downloading`, both machines converge on v2, `scripts/smoke-test.sh` "Client B: pull full
  file after A update" passes; `undo` now restores files and pull-over-placeholder replaces the
  0-byte sentinel with new content.

## Logic gaps

- [x] **G1 — Conflict legs reloaded from a head carried the visible leg's size.**
  `get_tree_state` gave all three legs `entry.size`; the tree format stores only the visible leg's
  size, so non-representative legs with different sizes failed `read_bytes` size verification and
  materialized as sentinels.
  *Fix (`objects.rs`, `large_file.rs`): visible leg keeps `entry.size`; other legs are size-unknown
  (`0`); `read_bytes`/`materialize` skip length checks only when the expected size is unknown and
  still verify content hashes for chunked files.*
- [x] **G2 — EditDelete conflicts flattened as deleted while the tree says the file is live.**
  `get_tree_state` set `deleted: theirs.is_none()`, but `insert_conflict` makes ours the visible leg
  when theirs is deleted; the visible leg is always a live blob.
  *Fix (`objects.rs`): flattened conflict files are live (`deleted: false`), matching the
  visible-leg semantics encoded by the tree format.*
- [x] **G3 — 8 MiB manifest cap hard-failed large workspaces.**
  *Fix: raised the reachability-manifest cap to 64 MiB in `server/src/app/routes_publication.rs` and
  `agent-core/src/hub.rs` (~1M objects headroom).*
- [x] **G4 — `log` changed-paths diffed only the first parent.**
  *Fix (`history.rs`): union the diffs across all parents so merge/undo snapshots report
  second-parent deltas.*
- [x] **G5 — `promote_rollback_restores` was thought dead.**
  Investigation during the fix showed it is **not** dead: it fires for format-v2 workspaces when the
  server metadata regresses to an older mtime (restored server backup, backwards clock skew) while
  the local file still matches the agreed state — covered by the
  `clock_skew_uses_hash_direction_and_warns_for_one_path_rollback` integration test. Kept as-is with
  a comment; v3 flattens mtimes to 0 on both sides so it is a deliberate no-op there (v3 rollback
  protection is hash-based via the conflict gate).
- [x] **G6 — `scripts/smoke-test.sh` could not run and was not wired into CI.**
  Its hardcoded E2EE key was not 64 lowercase hex (v3 rejects it), and no workflow referenced it.
  *Fix: valid 64-hex test key; added the non-PR `source-smoke` CI job to `ci.yml` running the full
  script (fmt/clippy/test/doc + live two-client E2E).*

## Optimizations

- [x] **O1 — Every sync re-encrypted and re-uploaded every tree object.**
  *Fix: durable per-workspace uploaded-object registry (`agent-core/src/upload_registry.rs`,
  `state/uploaded-objects`), seeded/updated only from reachability manifests the hub accepted. Tree
  and snapshot uploads skip registry-known ids; chunked uploads skip known chunks; `undo` skips
  known worktree blobs. On manifest rejection the registry is cleared and the sync pass retries once
  with forced upload of every live file.*
  **Evidence:** first push = 7 server blobs; no-op sync = 7 (no tree re-uploads); one-file change
  adds only the changed closure (+5), not all trees; deleting a server blob mid-history recovers
  automatically (manifest rejection → clear → force re-upload; local file intact). Registry is safe
  because objects reachable from the current head are always retained by hub GC (head manifests are
  never expired) and the manifest upload re-validates every referenced blob.
- [x] **O2 — Large-file reads always hit the network.**
  *Fix: `download_verified` checks the verified local object cache first
  (`objects::cached_object`), so hydrate/cat/reachability no longer re-download cached blobs —
  including each large file's chunk manifest on every sync.*
- [x] **O3 — Local object GC ran on every snapshot write.**
  *Fix (`object_gc.rs`): `prune` throttled to once per 60 s per workspace.*
- [x] **O4 — `undo` re-uploaded every local file.**
  *Fix (`history.rs`): skip blobs the hub already accepts via the registry.*
- [x] **O5 — Chunked uploads re-uploaded unchanged chunks and were sequential.**
  *Fix (`large_file.rs`): skip registry-known chunks (file cursor still advances, length still
  checked) and upload the remainder with bounded concurrency (4 in flight).*
- [ ] **O6 — Structural O(n) traversals per sync (accepted, not changed).**
  `snapshot_local_view` + `load_state` + `candidate_root` + `write` + `snapshot_reachability` each
  decrypt every tree, and `cat`/`read_bytes` buffer whole files. The flat-view design makes these
  inherent; the registry (O1) removes the dominant network cost. Fresh-hub-with-same-workspace also
  retains the pre-existing "empty remote head ⇒ remote deleted everything" semantics (mass
  delete_local) — out of scope, documented here.

## Verification

- `cargo test --workspace --exclude feanorfs-tray --all-features --locked` — 509 passed, 0 failed.
- `cargo clippy --workspace --exclude feanorfs-tray --all-targets --all-features --locked -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- `scripts/smoke-test.sh` — passes end to end (previously failed at `init` and would have caught C1).
