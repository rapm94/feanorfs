# client

## Purpose

CLI + library crate. Owns directory scanning, format-v3 snapshot sync orchestration, local cache metadata, predictive hydration, summaries, and the watcher. Agent and history operations delegate to `feanorfs-agent-core`. It transports content without interpreting or merging it. Serializable result types are shared by library callers and `--json`.

## Ownership

- Crates: `feanorfs-client` produces binary `feanorfs` (sync + `serve` hub + agents) and library `feanorfs_client`. Agent workspace logic lives in `feanorfs-agent-core` — this crate re-exports `Runtime`, `Workspace`, and thin `agent.rs` / `conflicts.rs` wrappers.
- Modules under `client/src/`:
  - `lib.rs` — public API re-export surface. Add new public functions here; downstream Rust consumers depend on this list.
  - `api.rs` — HTTP + in-process hub transport (`Backend::Http` | `Backend::Local`). `ApiClient::from_config` / `open_for_workspace`.
  - `hub.rs` — thin re-export (`pub use feanorfs_agent_core::hub::*`); `LocalHub` lives in `feanorfs-agent-core`. The `LocalHub` type is a JSON/state wrapper with durable `hub_state.json` + blob storage, not an Axum router. `feanorfs serve` uses the `LocalHub` from agent-core directly.
  - `migrate_sqlite/` — one-time SQLite → JSON/files migration coordinator (`migrate_workspace_stores`), journal (`metadata-migration.json`), cache state machine (`cache.rs`), hub state machine (`hub.rs`), WAL/fingerprint/archive helpers (`journal.rs`). Runs automatically on every `open_client_db` / `open_api_client` call before opening the store. Produces `.migrated-v1.db` archives next to each original SQLite file.
  - `cli/serve.rs` — `feanorfs serve` and `--gc-only`.
  - `cli/history.rs` — human and JSON `log` / `undo` output.
  - `commands.rs` — sync/push/pull/hydrate/cat/status via unified `run_sync_pass`. Owns `MirrorState` with `human_label()`. Returns `Serialize`-derived structs. No `println!` — UI in `main.rs` / `cli/`.
  - `conflicts.rs` — workspace conflict detection, registry, `resolve_conflict`, join/attach divergent-path guards, placeholder corruption, post-upload create/create, case conflicts. `seed_last_synced_from_server` skips same-path hash mismatches. `conflicts history` reads `conflict_resolutions`.
  - `conflict_artifacts.rs` — shared `.original`/`.local`/`.cloud` writer for agent and workspace conflicts.
  - `cli/` — CLI helpers (`util`, `agent`, `conflicts`, `serve`, `start`, `mcp`, `events`, `workspace`, …). Keeps `main.rs` under 1k lines.
  - `fs_util.rs` — `atomic_write` (temp + rename), `file_mtime_ms`.
  - `local.rs` — `Config` (`hub_local`, `format_version`), `ClientDb`, `scan_local_directory`.
  - `agent.rs` — re-exports `feanorfs_agent_core` agent ops; CLI `--json` uses the same shapes as [docs/agent-api.md](../docs/agent-api.md).
  - `predictive.rs` — `record_access_with_recent`, `prefetch_related` (top-5 siblings, 0.95 decay). Local-only.
  - `summary.rs` — `diff_since_last_session`, `commit_session_marker`, `render_via_summary_tool`. Zero-knowledge — never ships file contents to a remote LLM.
  - `watch.rs` — debounced (500 ms) filesystem watcher that drives `do_sync` on changes. Watcher path MUST be the workspace `current_dir`, never `"."`.
  - `tray.rs` — tray dashboard aggregation (`do_tray_status`, `build_conflict_show`). Agent summary cached on disk at `.feanorfs/tray-agent-cache.json` (30 s TTL); `invalidate_agent_cache` after land/keep.
- Local runtime data lives in `.feanorfs/` (git-ignored by FeanorFS itself; never include in distributions). Live state files: `.feanorfs/local_state.json` (format version, server URL, auth) and embedded hub `hub_state.json` + `blobs/<hash>` at `.feanorfs/hub-data/`.

## Local Contracts

- All paths stored in `local_state.json` use forward slashes via `feanorfs_common::normalize_path`. Always normalize before cache lookup or mutation.
- Legacy SQLite stores (`local_cache.db`, `agents/<name>/.feanorfs/local_cache.db`, `hub-data/db.sqlite`) are migrated one time to JSON/state files on first open. Migration is idempotent: re-running is a no-op once the journal shows `Archived` for all stores.
- Avoid redundant hashing: check `local_state.json` first and re-hash only if `mtime`/`size` differs from the cached entry. For unchanged placeholders (`hydrated=false`, `size==0`), reuse the cached hashes so the sync diff remains correct without downloading bytes.
- **Migration contracts:** lock (`.feanorfs/metadata-migration.lock`, `fs2` exclusive) gates the journal; journal (`.feanorfs/metadata-migration.json`) records store key, source paths, fingerprints, phase (`Discovered → Imported → Verified → Archived`), and archive paths; archives land next to each original SQLite as `.migrated-v1.db` (+ `-wal` + `-shm`) with hash-collision guard. Resuming after interruption reads the journal and skips completed phases.
- **Sync scope:** mirror the working directory (including gitignored/untracked paths). Hard skip `.feanorfs/`, `.git/`, symlinks, and nested directories declaring a valid `CACHEDIR.TAG` (workspace-root tags are exempt). Small frozen `DEFAULT_IGNORES` plus optional `.feanorfsignore` — does NOT honor `.gitignore`. Rationale and admission criteria: [docs/sync-scope.md](../docs/sync-scope.md). Do not grow `DEFAULT_IGNORES` without meeting all three criteria there.
- Zero-knowledge: seal file bytes before transport and verify ciphertext hashes before decrypting. Format-v3 uploads file blobs through opaque object storage and commits encrypted trees through head compare-and-swap.
- Result types are `Serialize`-derived. The `--json` CLI flag and `feanorfs_client::` library callers MUST see the same shape; do not add `println!` in `commands.rs` or `agent.rs` — keep UI in `main.rs` only.
- Workspace conflicts: bare `feanorfs conflicts` or `conflicts list`; `keep`; `show [--open]`. Registry in `conflict_registry`; artifacts under `.feanorfs/conflicts/<ts>/`. `ResolveKeep::Cloud` on `edit_delete` conflicts removes the local file and uploads a tombstone when the cloud artifact is a deletion sentinel.
- `agent status` (or bare `agent`) lists agents with one-line state when online; hidden `agent list` keeps legacy JSON shape. `agent status <name>` / hidden `agent check` peek via `/api/sync/peek`.
- Agent workspaces isolate DATA, not processes — `agent run` is cwd-scoping only. Never claim sandboxing; link [docs/threat-model.md](../docs/threat-model.md) § Process isolation.
- Predictive hydration is local-only: `file_access_log` never leaves the client.
- Local-hub workspaces (`hub_local` / `start --local`): in-process transport via `LocalHub` (agent-core JSON/state at `hub_data_dir/hub_state.json` + `blobs/`). `hub.rs` is a re-export shim; the Axum router for `feanorfs serve` is in `cli/serve.rs`. Do not print portable invites; share with `feanorfs serve --data-dir .feanorfs/hub-data`.
- Sync reconciliation compares local and head trees with `.feanorfs/refs/last-synced`. The `last_synced_files` and `agent_snapshots` tables are removed in format v3. `mtime` remains cache and rollback evidence, never content identity.
- Format-v3 migration journals old key, target key, fence token, and phase in `.feanorfs/migration-v3.json`; this is distinct from SQLite import journal `.feanorfs/metadata-migration.json`. Rekey never persists the target key before reseal, parentless head CAS, server stamp, and local finalization complete. Resume the command after interruption.
- Rekey requires clean or landed agent workspaces because old-key agent base refs cannot cross the key boundary.
- Remote join: `fnr1-…` invite via `common/src/invite.rs`; `start fnr1-…` or hidden `join` (full initial sync). Hidden `attach`/`init`/`setup` configure only.
- CLI surface: visible verbs include `log` and `undo` alongside sync, workspace, agent, and conflict commands. Hidden tray commands continue to feed `feanorfs-tray`.

## Work Guidance

- New public functions go into the appropriate module and are re-exported from `client/src/lib.rs`.
- `commands::password_or_default` warns when falling back to `LEGACY_DEFAULT_PASSWORD`. Treat any codepath that needs the default as a bug.
- Summary JSON shape (`SummaryResult`) is consumed by `FEANORFS_SUMMARY_CMD` via stdin — coordinate before renaming fields.
- After ANY code change in `commands.rs` or `local.rs`, run `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.

## Verification

- `cargo test --workspace` — unit tests + `client/tests/sync_engine.rs` integration harness + `client/tests/tray_contract_snapshots.rs`.
- `cargo clippy -p feanorfs-client --all-targets -- -D warnings`.
- `cargo fmt -p feanorfs-client -- --check`.

## Child DOX Index

`migrate_sqlite/` is a module directory inside `client/src/` but has no separate AGENTS.md — it remains fully under the client crate's ownership and contract.
