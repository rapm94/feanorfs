# client

## Purpose

CLI + library crate. Owns the local cache DB, directory scanner, sync engine, agent workspace isolation, predictive hydration, catch-up summary, and the real-time watcher. Exposes both a CLI binary (`feanorfs`) and a Rust library (`feanorfs_client`) so external Rust programs can drive sync imperatively. Never makes decisions about file content — it only decides what to transport, when, and how. The library result types (`SyncResult`, `PushResult`, `PullResult`, `HydrateResult`, `CatResult`, `StatusResult`, plus the re-exported `AgentCommitResult`, `ConcurrentEdit`, `FileState`) are the contract for the `--json` flag and library callers.

## Ownership

- Crates: `feanorfs-client` produces binary `feanorfs` (sync + `serve` hub + agents) and library `feanorfs_client`.
- Modules under `client/src/`:
  - `lib.rs` — public API re-export surface. Add new public functions here; downstream Rust consumers depend on this list.
  - `api.rs` — HTTP + in-process hub transport (`Backend::Http` | `Backend::Local`). `ApiClient::from_config` / `open_for_workspace`.
  - `hub.rs` — embedded local hub: `LocalHub` Axum router + `tower::ServiceExt::oneshot`.
  - `cli/serve.rs` — `feanorfs serve` and `--gc-only`.
  - `commands.rs` — sync/push/pull/hydrate/cat/status via unified `run_sync_pass`. Owns `MirrorState` with `human_label()`. Returns `Serialize`-derived structs. No `println!` — UI in `main.rs` / `cli/`.
  - `conflicts.rs` — workspace conflict detection, registry, `resolve_conflict`, join/attach divergent-path guards, placeholder corruption, post-upload create/create, case conflicts. `seed_last_synced_from_server` skips same-path hash mismatches. `conflicts history` reads `conflict_resolutions`.
  - `conflict_artifacts.rs` — shared `.original`/`.local`/`.cloud` writer for agent and workspace conflicts.
  - `cli/` — CLI helpers (`util`, `agent`, `conflicts`, `serve`, `start`, `mcp`, `events`, `workspace`, …). Keeps `main.rs` under 1k lines.
  - `fs_util.rs` — `atomic_write` (temp + rename), `file_mtime_ms`.
  - `local.rs` — `Config` (`hub_local`, `format_version`), `ClientDb`, `scan_local_directory`.
  - `agent.rs` — workspace isolation via three-way diff. After `land`, snapshot base advances from post-land main `FileState`. `land_agent(..., propose)` optional diff3 artifacts. Spawn holds `SyncLock` during snapshot walk.
  - `predictive.rs` — `record_access_with_recent`, `prefetch_related` (top-5 siblings, 0.95 decay). Local-only.
  - `summary.rs` — `diff_since_last_session`, `commit_session_marker`, `render_via_summary_tool`. Zero-knowledge — never ships file contents to a remote LLM.
  - `watch.rs` — debounced (500 ms) filesystem watcher that drives `do_sync` on changes. Watcher path MUST be the workspace `current_dir`, never `"."`.
- Local runtime data lives in `.feanorfs/` (git-ignored by FeanorFS itself; never include in distributions).

## Local Contracts

- All paths stored in `local_cache.db` use forward slashes via `feanorfs_common::normalize_path`. Always normalize BEFORE any DB operation.
- Avoid redundant hashing: check `local_cache.db` first and re-hash only if `mtime`/`size` differs from the cached entry. For unchanged placeholders (`hydrated=false`, `size==0`), reuse the cached hashes so the sync diff remains correct without downloading bytes.
- **Sync scope:** mirror the working directory (including gitignored/untracked paths). Hard skip `.feanorfs/`, `.git/`. Small frozen `DEFAULT_IGNORES` plus optional `.feanorfsignore` — does NOT honor `.gitignore`. Rationale and admission criteria: [docs/sync-scope.md](../docs/sync-scope.md). Do not grow `DEFAULT_IGNORES` without meeting all three criteria there.
- Zero-knowledge: always `pack_bytes` plaintext BEFORE calling `api.upload_file` and store the resulting `encrypted_hash` in the cache. `unpack_bytes` handles ChaCha20-Poly1305 blobs and legacy XOR on unmigrated v1 workspaces. On download, re-hash ciphertext before decrypting.
- Result types are `Serialize`-derived. The `--json` CLI flag and `feanorfs_client::` library callers MUST see the same shape; do not add `println!` in `commands.rs` or `agent.rs` — keep UI in `main.rs` only.
- Workspace conflicts: bare `feanorfs conflicts` or `conflicts list`; `keep`; `show [--open]`. Registry in `conflict_registry`; artifacts under `.feanorfs/conflicts/<ts>/`.
- `agent status` (or bare `agent`) lists agents with one-line state when online; hidden `agent list` keeps legacy JSON shape. `agent status <name>` / hidden `agent check` peek via `/api/sync/peek`.
- Agent workspaces isolate DATA, not processes — `agent run` is cwd-scoping only. Never claim sandboxing; link [docs/threat-model.md](../docs/threat-model.md) § Process isolation.
- Predictive hydration is local-only: `file_access_log` never leaves the client.
- Local-hub workspaces (`hub_local` / `start --local`): in-process transport via `hub.rs`. Do not print portable invites; share with `feanorfs serve --data-dir .feanorfs/hub-data`.
- Remote join: `fnr1-…` invite via `common/src/invite.rs`; `start fnr1-…` or hidden `join` (full initial sync). Hidden `attach`/`init`/`setup` configure only.
- CLI surface: visible verbs are `start`, `sync`, `status`, `hydrate`, `cat`, `summary`, `config`, `doctor`, `serve`, `migrate`, `agent`, `conflicts`. Onboarding is `feanorfs start` only.

## Work Guidance

- New public functions go into the appropriate module and are re-exported from `client/src/lib.rs`.
- `commands::password_or_default` warns when falling back to `LEGACY_DEFAULT_PASSWORD`. Treat any codepath that needs the default as a bug.
- Summary JSON shape (`SummaryResult`) is consumed by `FEANORFS_SUMMARY_CMD` via stdin — coordinate before renaming fields.
- After ANY code change in `commands.rs` or `local.rs`, run `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.

## Verification

- `cargo test --workspace` — unit tests + `client/tests/sync_engine.rs` integration harness (96 tests).
- `cargo clippy -p feanorfs-client --all-targets -- -D warnings`.
- `cargo fmt -p feanorfs-client -- --check`.

## Child DOX Index

No child directories. `src/` modules are file-level, not dir-level boundaries.
