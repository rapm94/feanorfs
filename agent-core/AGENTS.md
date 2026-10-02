# agent-core

## Purpose

Embeddable Rust engine (`feanorfs-agent-core`) for snapshot sync, agent workspaces, and coordination over in-process or HTTP transport: encrypted objects and heads, log/undo, spawn/status/refresh/land/clean, continuous reconciliation, runner state, signals, work intent, integrator assignment, conflict resolution, capability routing, and the unified coordination view. No CLI, watcher, summary, or predictive hydration. Consumers: `feanorfs-client`, `feanorfs-ffi`, `feanorfs-agent-node`.

## Ownership

- Public blocking API: `Runtime`, `Workspace` (one method per operation), `SpawnOptions`, `LandOptions`, `RefreshOptions`. Wire types live in `feanorfs_common`; JSON shapes follow [docs/agent-api.md](../docs/agent-api.md).
- Modules:
  - `objects.rs`, `prepared_tree.rs`, `snapshot.rs`, `snapshot_diff.rs`, `traversal.rs`, `history.rs`, `object_gc.rs`, `upload_registry.rs` — encrypted CAS objects, refs, bounded traversal, log/undo, GC, latest accepted reachability closure.
  - `sync_pass/` — sync orchestration, negotiation, verified downloads, staged materializer with rollback and crash journal.
  - `large_file.rs` — 8 MiB AEAD chunks plus authenticated manifests.
  - `local/`, `state/`, `workspace_read.rs`, `workspace_layout.rs`, `workspace_state_registry.rs` — `ClientDb` over `local_state.json`, scanning, descriptor-anchored reads, workspace-state identity, leases, and retirement.
  - `agent/` — spawn, land, refresh, proposals, continuous leases (`continuous.rs`), runner lifecycle (`runner/`).
  - `conflicts.rs`, `conflict_artifacts.rs`, `tree_reconcile.rs` — conflict gate, artifacts, identity sidecars.
  - `messages.rs`, `signal_index.rs` — `ffmsg1` send/inbox (resolves `cap:<capability>` recipients), cached walked snapshots (never authority).
  - `work.rs` — `ffwork1` reducer and `orchestrator/work-state.json` projection.
  - `integrator.rs` — `ffint1` dispatcher state machine, offers seen by candidates, typed candidate replies, owner designation, conflict materialization.
  - `resolution.rs`, `resolution_protocol.rs` — exact-fingerprint jobs, guarded publication, `ffres1` reducer.
  - `coordination.rs` — pure lifecycle derivation (`agent next`, `--wait`), edit guard, capability roster and `ffcap1` announcements, `agent_identity`.
  - `claim.rs` — one-call protocol: `claim_scope` (propose + wait), `finish_work` (wait for landing, settle + complete), `coordinate_pass` (auto-accept non-overlapping scope); pure policies `claim_covered` and `auto_decisions`.
  - `git_baseline.rs` — read-only `.git/HEAD` baseline and `ffbase1` mismatch detection.
  - `api.rs`, `hub.rs` + `hub/`, `hub_state/`, `tunnel.rs`, `mesh/`, `head.rs` — transports, embedded hub, opaque relay, mesh dialing/NAT/STUN/QUIC, bounded head waits.
  - `ctx.rs`, `crypto.rs`, `fs_util.rs`, `durable.rs`, `lock.rs`, `paths.rs` — shared helpers; path helpers live in `paths.rs` to avoid agent ↔ conflicts cycles.

## Local Contracts

### Storage, sync, and state

- `Runtime::new()` owns a multi-thread Tokio runtime; public methods `block_on` and stay valid inside existing Tokio contexts.
- Land uploads blobs/objects and fsyncs landed files before head CAS; the CAS is the commit point; worktree projection follows through the rollback-capable materializer and recovers from its journal.
- Published snapshots upload every referenced blob before their manifest; only an HTTP 412 missing-blob response clears the upload registry for one repair pass.
- Downloads never clobber touched files, ancestors, deletions, or placeholders: revalidate after staging, authenticate and fsync replacements, publish through no-follow handles, keep rollback backups, commit cache changes once. Recovery never deletes changed, untracked, or symlink content.
- Workspace and worktree reads are descriptor-anchored (`openat`/`O_NOFOLLOW` on Unix, checked fallbacks elsewhere); small uploads must reproduce the scanned hash before any network write.
- Object, reachability, prepared-tree, diff, and history work share bounded object/work/output/path budgets; all response and object reads are length-bounded before allocation.
- `ClientDb`: exclusive `local_state.lock` → reload → mutate → atomic commit → parent sync; missing state after construction is corruption; input capped at 128 MiB; canonical serialization streams without full clones; unknown schemas are rejected; legacy `local_cache.db` requires `feanorfs migrate`.
- Access log: ≤10 000 entries, weights finite and ≥0.001, deterministic eviction.
- `atomic_write_visible`/`_durable` write a temp file in the destination directory, sync, rename, and clean up on failure; `_durable` also syncs the parent.
- Sync-lock ownership is the kernel lock, never the diagnostic PID or age; a held lock whose PID is unreadable (Windows locks are mandatory) counts as active.
- Exclusive workspace-state leases retry contention for at most 500 ms (a child forked by another thread briefly inherits descriptors), then fail closed.
- `SyncCtx::state_dir` resolves once per context and never caches globally; identity mismatches, duplicate identity matches, and same-path folder replacement fail closed. Workspace state is retired only through explicit tombstone → grace → quarantine → re-verified deletion.
- Config writes are atomic; keys/tokens go to the OS credential store with a protected-file fallback and never spill back after migration. `ApiClient::new_with_tls_resolved` may change address lookup but keeps SNI, name verification, and the pinned CA.
- Rekey publishes a parentless root and retries only the recorded candidate from the recorded source head.
- `LocalHub` caches by canonical data dir plus token, serializes metadata through a locked atomic `hub_state.json`, and matches server bounds (100 MiB body, 64 MiB manifest, valid-hash paths).
- Relay routes are 256-bit lowercase hex, WSS outside loopback tests, never logged or placed in argv; inner TLS is never terminated at the relay.
- Sync snapshots carry `ffbase1:<commit>:<branch>` from `read_git_baseline(ctx.base)`, which reads only `HEAD` and the ref it names (validated `refs/` path, loose or packed). Never write `.git`; a missing or unborn baseline is `None`.

### Agents, conflicts, and coordination

- Agent workspaces isolate data, not processes; never claim sandboxing. Each agent base is one atomic `base-snapshot` ref. Agent spawn/status/land build the base `SyncCtx` from the loaded `Config`, never the legacy fallback constructor.
- Conflict identity is hash/deletion/executable-intent based; mtime never decides content changes. Executable intent is preserved per leg (FTR2 only when needed). `ResolveKeep::Cloud` on a deletion sentinel removes the local file and uploads a tombstone. Bulk resolution validates every artifact before mutation and commits once.
- Continuous agents: lease per (workspace, agent); guarded land (`clean=false, propose=false`) and refresh (never `--replace`); bounded `continuous-status.json`; activation is explicit (`agent run` or an enabled runner); manual land/refresh is refused while an owner is active.
- `ffmsg1` names ≤255 bytes, bodies ≤8 KiB, envelopes ≤64 KiB. Integrator replies must match candidate, dispatcher, kind, request, assignment/attempt, and snapshot; `integrator_reply` binds all of them from the observed offer and refuses superseded, terminal, out-of-order, or scan-truncated replies.
- Resolution publication embeds its broadcast status in the same CAS as the resolved tree; a prepared job may cross at most 64 single-parent signal-only snapshots with the same tree root; leg materialization reuses only byte-identical regular files.
- Work, integrator, resolution, and capability senders default to `FEANORFS_AGENT`, then `human`.
- `coordination_status` reads every projection once and fails soft: a source error becomes a warning plus `projection_incomplete`. `derive_coordination` and `evaluate_guard` stay pure and unit-tested. Items ≤64, actions ≤8, warnings ≤16, text ≤512 bytes.
- `claim_scope` never sends a signal for covered paths and reuses an identical pending proposal; `finish_work` settles only with a snapshot proven landed (live controller idle, no unlanded changes) and defaults verification to `skipped`; `coordinate_pass` only accepts (never rejects or narrows) proposals addressed to its identity that overlap no other agent's live scope.
- Capability roster = newest `ffcap1` announcement per sender plus work-intent capabilities. `cap:` routing resolves to exactly one agent or errors; `integrator_assign` with no candidates uses the roster.
- Keep this crate free of `clap`, `notify`, and `tracing-subscriber`. New agent-facing operations land here first and on every surface in the operation matrix.

## Work Guidance

- Unit and integration test roots link `feanorfs-test-support` once; never mutate HOME in tests.
- Prefer extending existing reducers and projections over new persisted state; derived views (like `coordination.rs`) compute on read.

## Verification

- `cargo test -p feanorfs-agent-core`, `cargo test -p feanorfs-ffi`, `cargo test -p feanorfs-client --test contract_snapshots --test tray_contract_snapshots --test coordination_cli`.
- Opt-in profiles: `cargo test -p feanorfs-agent-core --release -- --ignored --nocapture scan_profile_10k` (and `local_state_serialization_profile_100k`, `state::tests::persistence::local_state_persistence_profile_100k`).

## Child DOX Index

| Child | Purpose |
| :--- | :--- |
| [`src/agent/`](src/agent/AGENTS.md) | Agent diff, spawn, land, refresh, runner lifecycle, proposals. |
| [`src/hub/`](src/hub/AGENTS.md) | Embedded hub dispatch and routes. |
| [`src/hub_state/`](src/hub_state/AGENTS.md) | JSON hub persistence, blobs, SQLite projection. |
| [`src/local/`](src/local/AGENTS.md) | Config, `ClientDb` operations, walking, scanning. |
| [`src/state/`](src/state/AGENTS.md) | Durable local-state persistence and schema tests. |
