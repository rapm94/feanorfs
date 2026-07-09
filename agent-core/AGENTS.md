# agent-core

## Purpose

Embeddable Rust SDK for FeanorFS agent workspace isolation. Owns spawn / status / refresh / land / clean / conflict resolve over the local cache DB and in-process or HTTP hub transport. No CLI, watcher, summary, or predictive hydration — consumers include `feanorfs-client`, `feanorfs-ffi`, and `feanorfs-agent-node`.

## Ownership

- Crate: `feanorfs-agent-core` (`agent-core/`).
- Public blocking API: [`Runtime`](src/lib.rs), [`Workspace`](src/lib.rs), [`SpawnOptions`](src/lib.rs), [`LandOptions`](src/lib.rs).
- Internal modules:
  - `agent.rs` — three-way diff, spawn/land/refresh/clean.
  - `conflicts.rs` / `conflict_artifacts.rs` — workspace conflict gate and artifact layout.
  - `local.rs` — `Config`, `ClientDb`, directory scan.
  - `api.rs` / `hub.rs` — HTTP and in-process `ApiClient`.
  - `sync_pass.rs` — minimal sync pass used before spawn when `no_sync=false`.
  - `paths.rs` — `.feanorfs/agents`, conflicts dir, name validation (breaks agent↔conflicts cycle).
  - `ctx.rs`, `crypto.rs`, `fs_util.rs`, `lock.rs` — shared helpers.

Wire types and semver JSON contract live in `feanorfs_common::agent_contract` — see [docs/agent-api.md](../docs/agent-api.md).

## Local Contracts

- Blocking facade: `Runtime::new()` owns a multi-thread Tokio runtime; all public methods use `block_on`.
- JSON shapes returned to FFI/Node/CLI `--json` MUST match `docs/agent-api.md`; snapshot tests in `client/tests/contract_snapshots.rs`.
- Tray JSON shapes live in `feanorfs_common::tray_contract` with fixtures + snapshots in `client/tests/tray_contract_snapshots.rs`.
- `ResolveKeep::Cloud` on `edit_delete` conflicts: when the cloud artifact is the deletion sentinel, remove the local file and upload a tombstone (`is_cloud_deleted_sentinel` in `conflict_artifacts.rs`).
- Agent workspaces isolate data, not processes — never claim sandboxing.
- Spawn with an empty main tree records zero snapshot rows; land still works for paths the agent adds after spawn (greenfield workflows).

## Work Guidance

- Keep this crate free of `clap`, `notify`, and `tracing-subscriber`.
- New agent-facing operations go here first; `feanorfs-client` re-exports thin wrappers.
- Path helpers belong in `paths.rs` — do not reintroduce `agent` ↔ `conflicts` module cycles.

## Verification

- `cargo test -p feanorfs-agent-core`
- `cargo test -p feanorfs-ffi` (C ABI smoke)
- `cargo test -p feanorfs-client contract_snapshots`
- `cargo test -p feanorfs-client tray_contract_snapshots`

## Child DOX Index

No child directories.
