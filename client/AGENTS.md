# client

## Purpose

CLI + library crate. Owns the local cache DB, directory scanner, sync engine, agent workspace isolation, predictive hydration, catch-up summary, and the real-time watcher. Exposes both a CLI binary (`feanorfs`) and a Rust library (`feanorfs_client`) so external Rust programs can drive sync imperatively. Never makes decisions about file content — it only decides what to transport, when, and how. The library result types (`SyncResult`, `PushResult`, `PullResult`, `HydrateResult`, `CatResult`, `StatusResult`, plus the re-exported `AgentCommitResult`, `ConcurrentEdit`, `FileState`) are the contract for the `--json` flag and library callers.

## Ownership

- Crates: `feanorfs-client` produces binary `feanorfs` and library `feanorfs_client` (both defined in `client/Cargo.toml`).
- Modules under `client/src/`:
  - `lib.rs` — public API re-export surface. Add new public functions here; downstream Rust consumers depend on this list.
  - `api.rs` — HTTP request wrappers. Wraps `/api/sync/peek` (alias `/api/sync/diff`), `/api/upload`, `/api/download/:hash`, `/api/workspaces`. Adds Bearer auth when configured.
  - `commands.rs` — sync/push/pull/hydrate/cat/status via unified `run_sync_pass`. Returns `Serialize`-derived structs. No `println!` — UI in `main.rs` / `cli/`.
  - `conflicts.rs` — workspace conflict detection, registry, and `resolve_conflict`. Uses `negotiate_sync_with_conflict_gate` before apply.
  - `conflict_artifacts.rs` — shared base/ours/theirs file writer for agent and workspace conflicts (`<feanorfs-sentinel:` placeholders).
  - `cli/` — CLI helpers (`util`, `agent`, `conflicts` command handlers). Keeps `main.rs` under 1k lines.
  - `local.rs` — `Config`, `ClientDb` (includes `last_synced_files`, `conflict_registry`), `scan_local_directory`.
  - `agent.rs` — workspace isolation via `feanorfs_common::detect_concurrent_edits`. Agent conflicts under `.feanorfs/conflicts/<ts>_<name>/`.
  - `predictive.rs` — `record_access_with_recent`, `prefetch_related` (top-5 siblings, 0.95 time-decay factor). Local-only; `file_access_log` never leaves the client.
  - `summary.rs` — `diff_since_last_session`, `commit_session_marker`, `render_via_summary_tool` (shells out to `FEANORFS_SUMMARY_CMD` default `feanorfs-llm`; falls back to plain listing). Zero-knowledge — never ships file contents to a remote LLM.
  - `watch.rs` — debounced (500 ms) filesystem watcher that drives `do_sync` on changes. The watcher path MUST be the workspace `current_dir`, never `"."`, so origin-agnostic invocations observe the correct directory.
- Local runtime data lives in `.feanorfs/` (git-ignored by FeanorFS itself; never include in distributions).

## Local Contracts

- All paths stored in `local_cache.db` use forward slashes via `feanorfs_common::normalize_path`. Always normalize BEFORE any DB operation.
- Avoid redundant hashing: check `local_cache.db` first and re-hash only if `mtime`/`size` differs from the cached entry. For unchanged placeholders (`hydrated=false`, `size==0`), reuse the cached hashes so the sync diff remains correct without downloading bytes.
- Scanner does NOT honor `.gitignore`, `.ignore`, or global gitignore. All files are synced. If exclusion is needed in the future, add a `.feanorfsignore` or explicit denylist — do NOT re-enable gitignore inheritance silently.
- Zero-knowledge: always `pack_bytes` plaintext BEFORE calling `api.upload_file` and store the resulting `encrypted_hash` in the cache. `unpack_bytes` handles ChaCha20-Poly1305 blobs and legacy XOR. On download, re-hash ciphertext before decrypting.
- Result types are `Serialize`-derived. The `--json` CLI flag and `feanorfs_client::` library callers MUST see the same shape; do not add `println!` in `commands.rs` or `agent.rs` — keep UI in `main.rs` only.
- Workspace conflicts: `conflicts list|resolve`. Registry in `conflict_registry`; artifacts under `.feanorfs/conflicts/<ts>/` with `manifest.json`. Resolving one path removes only that path's artifacts; batch dir removed when last pending row clears.
- `agent commit` uses `/api/sync/peek` with base snapshot as client view. FeanorFS does NOT merge — consumer reconciles.
- Predictive hydration is local-only: `file_access_log` weights and access patterns stay in `.feanorfs/local_cache.db` and MUST NEVER be uploaded or shipped off-device.
- When `--summarize` shells out, only paths and metadata cross the pipe. The E2EE password and file bytes stay local.

## Work Guidance

- New public functions go into the appropriate module and are re-exported from `client/src/lib.rs`. Library consumers expect them there.
- `commands::password_or_default` warns when falling back to `LEGACY_DEFAULT_PASSWORD`. Treat any codepath that needs the default as a bug — surface a clear user-visible error instead, when feasible.
- Summary JSON shape (`SummaryResult`) is consumed by `FEANORFS_SUMMARY_CMD` via stdin. Changing field names is a breaking change for that pipeline — coordinate with downstream LLM tooling before renaming.
- After ANY code change in `commands.rs` or `local.rs`, run `cargo clippy -p feanorfs-client --all-targets -- -D warnings` and the existing test suite.
- Tests so far cover: `agent::validate_name` (path traversal and identifier cases), `main::truncate_password_for_display` (length boundary, multibyte, 12/13 char edge cases), `common::*` (crypt roundtrips, domain separation, hash bytes, normalize, generate_password, is_valid_hash), `summary::diff_since_last_session`, `watch::event_paths_warrant_sync`, and `client/tests/sync_engine.rs` (push/pull/sync roundtrip, lazy placeholders, agent concurrent-edit detection). Predictive prefetch currently has no dedicated tests — treat as a gap when adding behavior there.

## Verification

- `cargo test --workspace` — runs all crate unit tests plus `common/tests/sync_models.rs` integration tests and `client/tests/sync_engine.rs` in-process server harness (56 tests across 7 suites; bin tests cover CLI-only helpers).
- `cargo clippy -p feanorfs-client --all-targets -- -D warnings`.
- `cargo fmt -p feanorfs-client -- --check`.
- For E2E smoke: spin up `feanorfs-server`, then two `test-client-a/`/`test-client-b/` fixtures and run `feanorfs sync`/`feanorfs agent spawn`/`feanorfs agent commit`. Fixtures live in workspace root but are NOT tracked in git.

## Child DOX Index

No child directories. `src/` modules are file-level, not dir-level boundaries. Individual `.rs` files do not warrant their own AGENTS.md — this file already maps the module responsibilities.