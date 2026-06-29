# FEANORFS KNOWLEDGE BASE

**Generated:** 2026-06-23T18:15:00+02:00
**Branch:** main
**Status:** E2EE Sync + Lazy Hydration + Agent Workspaces + JSON API + Predictive Hydration + Catch-up Summary

## Unifying Principle

**FeanorFS is dumb storage, smart transport.** FeanorFS never makes decisions about file content (no auto-merge, no summarization, no chat). Its job is to decide _what_ to transport, _when_ to transport it, and _how_ to isolate and preserve files safely. Anything requiring file-semantic understanding belongs in the consumer/agent layer.

## OVERVIEW
`FeanorFS` is a developer-focused uncommitted-code synchronization tool written in Rust. It utilizes a **self-contained local-first architecture**:
1. **Metadata Synchronization**: SQLite on both sides. Server in `server-data/db.sqlite`, client cache in `.feanorfs/local_cache.db`. Client queries `/api/sync/diff` with its metadata and receives a delta.
2. **Blob Storage**: Content-addressed storage (CAS) blobs identified by Blake3 hashes on a lightweight Axum server.
3. **End-to-End Encryption (E2EE)**: Symmetric keystream from `Blake3 XOF(password, path)` with length-prefixed domain separation — cloud sees only encrypted hashes and scrambled bytes. Client re-verifies downloaded blob integrity by re-hashing the ciphertext and comparing to the expected `encrypted_hash` before decrypting, mitigating active-server-tampering attacks (substitute AEAD mitigation until ChaCha20-Poly1305 lands).
4. **On-Demand Hydration (Lazy Sync)**: `pull --lazy` creates 0-byte placeholders; actual bytes fetched via `hydrate` or `cat`.
5. **Workspace Isolation**: `agent spawn` creates copy-on-write snapshots under `.feanorfs/agents/<name>/` (hardlinks + fallback copy) and records the server's per-file view into `agent_snapshots`. Requires the workspace E2EE password so uncached files get correct base hashes. `agent commit` detects concurrent edits (base/ours/theirs) by sending the base snapshot back to `/api/sync/diff` as the "client" view — every server-side change since spawn shows up as `download_required`. Conflicts are written under `.feanorfs/conflicts/` preserving directory structure; FeanorFS does NOT merge — the consumer (human or AI agent) reconciles. Level 1 sandbox executes commands inside the agent workspace via `agent run`.
6. **Agent Library API**: Client crate is split into `lib.rs` + `main.rs`; `feanorfs_client::sync/push/pull/hydrate/cat` are callable from any Rust program. `--json` flag on the CLI emits structurally-typed results for every status-returning command.
7. **Catch-Up Summary**: `summary` command diffs current workspace against the previous session marker (stored in `last_session` table) and lists added/modified/deleted paths. `--summarize` shells out to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`) feeding it the structured result as JSON; if the binary is absent, falls back to plain path listing. Zero-knowledge is preserved by never shipping file contents to a remote LLM.
8. **Predictive Hydration**: `file_access_log` table tracks co-occurrence (path × sibling_path × weight × updated_at). After `cat`/`hydrate`, `predictive::record_access_with_recent` loads the rolling 5-path history from `last_session`, bumps the weights of recently-touched siblings, and saves the updated list back; `predictive::prefetch_related` runs after `Hydrate` to fetch top-5 co-occurring siblings in the background. Each run applies a 0.95 time-decay factor — no ML, pure weighted co-occurrence.

---

## STRUCTURE
```
feanorfs/
├── common/              # Shared data models and utilities
│   └── src/lib.rs       # FileState, SyncRequest/Response, AgentSnapshotEntry, ConcurrentEdit, AgentCommitResult, Blake3 XOF crypt_bytes and hashing
├── server/              # Pure blob storage server
│   ├── src/db.rs        # SQLite metadata DB coordinator using SQLx
│   ├── src/app.rs       # Axum routes for sync negotiation, blob uploads & downloads
│   ├── src/lib.rs       # feanorfs_server library (build_router, init_app_state)
│   └── src/main.rs      # CLI entrypoint
├── client/              # CLI terminal client + library crate
│   ├── src/lib.rs       # feanorfs_client lib export surface (sync/push/pull/hydrate/cat, types, Db, ApiClient)
│   ├── src/api.rs       # HTTP client request wrappers for blob transport
│   ├── src/commands.rs  # Push/pull/sync/hydrate/cat command implementations (Serialize'd result types)
│   ├── src/agent.rs     # Workspace Isolation: spawn/commit/list/clean, CoW snapshots, three-way conflict detection
│   ├── src/summary.rs   # Catch-up summary: diff against last_session, FEANORFS_SUMMARY_CMD shell-out with plain fallback
│   ├── src/predictive.rs # Predictive hydration: access recording + co-occurrence prefetch + time decay
│   ├── src/local.rs     # Client-side config, ignore rules, local cache DB, agent_snapshots, file_access_log, last_session tables
│   ├── src/main.rs      # CLI subcommand router with global --json flag and AgentAction subcommand
│   └── src/watch.rs     # Debounced real-time change watcher
└── server-data/         # Created by server to store file blobs and sqlite metadata (git-ignored)
```

---

## THE LOCAL CACHE DESIGN
To avoid unnecessary re-hashing of unchanged local files, the client maintains a local cache database:
1. **`local_cache.db`** (Local Cache): SQLite database created via `sqlx::SqlitePool`. Contains:
   - **`local_files`**: maps disk path modifications (`mtime`/`size` -> hashes), including `server_mtime` and `hydrated` flag.
   - **`agent_snapshots`**: per-agent `(agent_name, path)` → `(base_hash, base_size, base_mtime)` — the server's view at spawn time. `agent commit` uses this as the "base" leg of three-way concurrent-edit detection.
   - **`file_access_log`**: `(path, sibling_path, weight, updated_at)` — co-occurrence table for predictive hydration. Weights accumulate then decay by 0.95 on each prefetch pass.
   - **`last_session`**: simple `(key, value)` key-value. The `last_scan` key stores a JSON-serialized `HashMap<String, FileState>` snapshot that `summary` diffs against on next session.
2. **Double-hash and Server Mtime Tracking** in `local_files`:
   - `plaintext_hash`: Used to quickly detect disk modifications.
   - `encrypted_hash`: Stores the actual crypt hash matching the server blob key.
   - `server_mtime`: Tracks the server's official commit mtime. Reporting this to the server during diff negotiation avoids false local changes detection for unhydrated placeholders.

---

## WHERE TO LOOK

| Task / Feature | Location | Notes |
| :--- | :--- | :--- |
| FileState definition | [lib.rs](common/src/lib.rs) | Tracks relative path, Blake3 hash (encrypted), size, mtime, and deleted status. |
| Agent snapshot + conflict types | [lib.rs](common/src/lib.rs) | `AgentSnapshotEntry`, `ConcurrentEdit`, `AgentCommitResult` — the wire types `agent commit` returns. |
| Blake3 XOF E2EE | [lib.rs](common/src/lib.rs) | Symmetric XOR cipher driven by Blake3 Extendable Output Function (XOF) with length-prefixed domain separation. Also exports `is_valid_hash` for path-traversal defense. |
| Library API surface | [lib.rs](client/src/lib.rs) | `feanorfs_client::sync/push/pull/hydrate/cat` callable from any Rust program. Re-exports `ApiClient`, `ClientDb`, `Config`, types. |
| Local Cache DB + tables | [local.rs](client/src/local.rs) | Schema for `local_files`, `agent_snapshots`, `file_access_log`, `last_session`. CRUD on each. |
| Directory Scanning | [local.rs](client/src/local.rs) | Uses `ignore` WalkBuilder. Matches size and mtime. Reports cached `server_mtime` for untouched placeholders. |
| HTTP API Wrappers | [api.rs](client/src/api.rs) | Wraps `/api/sync/diff`, `/api/upload`, `/api/download/:hash`, `/api/workspaces`. Sends Bearer auth header when configured. |
| CLI Actions | [main.rs](client/src/main.rs) | Subcommand router. Global `--json` flag. Agent subcommand with Spawn/Commit/List/Clean/Run actions. |
| Sync Engine | [commands.rs](client/src/commands.rs) | Pure sync logic returning `Serialize`-derived result types (`SyncResult`, `PushResult`, etc.). No `println!` — UI-agnostic. |
| Workspace Isolation | [agent.rs](client/src/agent.rs) | `spawn_agent` (hardlink CoW + fallback copy + per-file base snapshot, requires E2EE password), `commit_agent` (three-way concurrent edit via `/api/sync/diff`), `list_agents`, `clean_agent`, `write_conflict_files`. |
| Catch-up Summary | [summary.rs](client/src/summary.rs) | `diff_since_last_session`, `commit_session_marker`, `render_via_summary_tool` (shells out to `FEANORFS_SUMMARY_CMD`, default `feanorfs-llm`, falls back to plain listing). |
| Predictive Hydration | [predictive.rs](client/src/predictive.rs) | `record_access_with_recent`, `prefetch_related` (top-5 siblings, 0.95 decay factor). Triggered from `hydrate` and `cat` CLI arms. |
| Change Watching | [watch.rs](client/src/watch.rs) | Debounced (500ms) filesystem watcher that triggers `do_sync` on changes. |

---

## CODE MAP

### Core Data Models ([common/src/lib.rs](common/src/lib.rs))
- `FileState`: Schema for paths, hashes, sizes, mtimes, and deleted states.
- `SyncRequest` & `SyncResponse`: Serialization structs for endpoint negotiation.
- `AgentSnapshotEntry`: One row of the per-agent base snapshot — `(agent_name, path, base_hash, base_size, base_mtime)`.
- `ConcurrentEdit`: Three-way triple (base/ours/theirs) emitted by `agent commit` for conflicting paths. FeanorFS does not merge — consumers reconcile.
- `AgentCommitResult`: Aggregate result of `agent commit` — `our_changes`, `their_changes`, `conflicts`.

### Server API Endpoints ([server/src/app.rs](server/src/app.rs))
- `POST /api/sync/diff`: Compares incoming client list with `files` table and returns a delta response. **Reused by `agent commit`**: the client sends the base snapshot as the "client" view, so every server-side change since spawn surfaces as `download_required`. No new endpoint needed. Deletions are propagated via `upload_required` with `deleted=true` in the `FileState` payload (handled inside the diff handler).
- `POST /api/upload?workspace_id=...`: Receives raw encrypted bytes, writes them to `server-data/blobs/<hash>`, and upserts DB metadata. If the DB upsert fails, the orphaned blob is removed from disk before returning an error (no partial state). Request body size capped at 100 MB via `DefaultBodyLimit` to prevent memory-exhaustion DoS.
- `GET /api/download/:hash`: Streams raw file contents. Rejects non-hex hashes via `is_valid_hash` to prevent path traversal.
- `GET /api/workspaces`: Lists all workspace IDs that have at least one non-deleted file.
- **Auth middleware**: If `--token` is set, all routes require `Authorization: Bearer <token>` header. `--password` accepted as alias. Token comparison uses constant-time equality to prevent timing side-channels.
- **mDNS**: Server advertises `_feanorfs._tcp.local.` on port 3030 for LAN discovery when started with `--mdns` (off by default for internet deployments).
- **Multi-instance**: `--port` and `--data-dir` flags allow running multiple isolated instances behind a reverse proxy (SaaS deployment model).

### Client Database Schema ([client/src/local.rs](client/src/local.rs))
- `local_files` table: `path` (PK), `plaintext_hash`, `encrypted_hash`, `size`, `mtime` (disk), `server_mtime` (remote), `hydrated`.
- `agent_snapshots` table: `agent_name`, `path`, `base_hash`, `base_size`, `base_mtime`. Primary key `(agent_name, path)`.
- `file_access_log` table: `path`, `sibling_path`, `weight`, `updated_at`. Primary key `(path, sibling_path)`.
- `last_session` table: `key` (PK), `value`. Currently stores `last_scan` = JSON-serialized `HashMap<String, FileState>` of the previous session.
- Global config: `~/.feanorfs/global.json` stores server URL + optional server password, cached by `feanorfs connect`.
- Workspace config: `.feanorfs/config.json` stores server URL, workspace ID, E2EE password, and optional server password.

---

## CONVENTIONS
1. **Cross-Platform Paths**: All files are tracked and uploaded using forward slashes (`/`). Always normalize path slashes using `feanorfs_common::normalize_path` before doing DB operations.
2. **No Redundant Hashing**: Check disk files against `local_cache.db` first. Rehash only if `mtime` or `size` differs.
3. **Zero-Knowledge Encryption**: Always encrypt file contents using `crypt_bytes` before calling `api.upload_file` and store the resulting `encrypted_hash` in the database.
4. **Library-First Result Types**: Commands return `Serialize`-derived structs (`SyncResult`, `PushResult`, etc.) so the `--json` flag and `feanorfs_client::` library callers see the same shape.
5. **No Auto-Merge**: `agent commit` emits three-way `ConcurrentEdit` triples under `.feanorfs/conflicts/`. Reconciliation is the consumer's job (human edits, or LLM agent invokes reconcile). FeanorFS never decides file content.
6. **Predictive Hydration is Local-Only**: `file_access_log` never leaves the client. Weights and access patterns stay in `.feanorfs/local_cache.db`.

---

## ANTI-PATTERNS (THIS PROJECT)
- **DO NOT** scan the `.feanorfs`, `.git`, or `.feanorfs/agents/` directories as part of the main workspace scan. They are hardcoded as skipped. Agents have their own scan inside their workspace dir (separate `ClientDb`).
- **DO NOT** trigger syncs on every raw filesystem change event. Filesystem saves are noisy. Debounce updates for 500ms using a channel.
- **DO NOT** download remote file bytes immediately during sync if `--lazy` is enabled. Write 0-byte placeholders instead.
- **DO NOT** add a new server endpoint when `agent commit` can reuse `/api/sync/diff` by sending the base snapshot as the client view. Reuse keeps the server dumb.
- **DO NOT** ship file contents to a remote LLM when implementing `--summarize`. The shell-out tool is fed paths and metadata only; the E2EE password and file bytes stay local.
- **DO NOT** attempt to merge concurrent edits. `agent commit` writes three files (`path.base`, `path.ours`, `path.theirs`) under `.feanorfs/conflicts/` and stops. A consumer reconciles.

---

## COMMANDS

### Workspace Commands
```bash
# Build all crates
cargo build

# Run unit and integration tests
cargo test
```

### Starting the Blob Server
```bash
# Runs the Axum server on port 3030
cargo run --bin feanorfs-server
```

### Client CLI Usage
```bash
# Connect to a server (internet: explicit URL; LAN: --lan for mDNS discovery)
# If server requires auth and no --password given, prompts interactively
cargo run --bin feanorfs -- connect https://my-server.com:3030 --password "server-pass"
cargo run --bin feanorfs -- connect --lan

# Initialize a workspace (uses cached server, auto-generates E2EE key)
# Prints a ready-to-paste join command and copies key to clipboard
cargo run --bin feanorfs -- init --workspace my-workspace
cargo run --bin feanorfs -- init --workspace my-workspace --lan

# Join an existing workspace from another machine (combines connect + init)
cargo run --bin feanorfs -- join my-workspace --password "e2ee-key-from-machine-A"

# Show current connection and workspace configuration
cargo run --bin feanorfs -- config

# Check differences between local directory and server
cargo run --bin feanorfs -- status

# Upload local additions, updates, and deletes
cargo run --bin feanorfs -- push

# Download remote updates and deletes (fully hydrated)
cargo run --bin feanorfs -- pull

# Perform a lazy pull (downloads metadata only, creating placeholder files)
cargo run --bin feanorfs -- pull --lazy

# Perform a bidirectional sync (pull + push)
cargo run --bin feanorfs -- sync

# Perform a lazy bidirectional sync
cargo run --bin feanorfs -- sync --lazy

# Perform a single sync pass without entering watch mode (for scripts/CI)
cargo run --bin feanorfs -- sync --no-watch

# Download and decrypt a specific placeholder file
cargo run --bin feanorfs -- hydrate src/main.rs

# Download and decrypt all unhydrated placeholder files
cargo run --bin feanorfs -- hydrate

# Print a file's contents, automatically hydrating it if it is a placeholder
cargo run --bin feanorfs -- cat src/main.rs

# Start real-time watch and sync loop
cargo run --bin feanorfs -- watch

# List all active workspaces on the server
cargo run --bin feanorfs -- workspaces
```

### Agent Workspace & Sandbox Commands
```bash
# Spawn an isolated agent workspace (CoW snapshot via hardlinks, server snapshot recorded)
cargo run --bin feanorfs -- agent spawn ci1

# Diff agent workspace against the base snapshot
# Emits clean-our / clean-their / conflicts; writes base/ours/theirs files under .feanorfs/conflicts/
cargo run --bin feanorfs -- agent commit ci1

# List all agent workspaces
cargo run --bin feanorfs -- agent list

# Remove an agent workspace and its snapshot rows
cargo run --bin feanorfs -- agent clean ci1

# Run a command inside the agent workspace (Level 1 process isolation)
# Example: `cargo test` runs in .feanorfs/agents/ci1/
cargo run --bin feanorfs -- agent run ci1 -- cargo test
```

### Catch-Up & Predictive Commands
```bash
# Show which files changed since the last session marker (plain path listing by default)
cargo run --bin feanorfs -- summary

# Shell out to FEANORFS_SUMMARY_CMD (default: feanorfs-llm) feeding it the structured diff as JSON
cargo run --bin feanorfs -- summary --summarize

# Also persist the current state as the next session's "previous" snapshot
cargo run --bin feanorfs -- summary --commit

# Predictive hydration is automatic: `cat`/`hydrate` record access patterns and
# `prefetch_related` fetches the top-5 co-occurring siblings in the background.
# No explicit command needed.
```

### JSON Output & Library API
```bash
# Global --json flag emits machine-readable structs for Status/Push/Pull/Sync/Hydrate/Cat/Summary/Agent
cargo run --bin feanorfs -- --json status
cargo run --bin feanorfs -- --json agent commit ci1
```

```rust
// Library crate usage (feanorfs-client)
use feanorfs_client::{ApiClient, ClientDb, Config, sync};

let config = Config { /* ... */ };
let db = ClientDb::new(".feanorfs").await?;
let api = ApiClient::new(&config.server_url, config.server_password.as_deref());
let result = sync(&api, &db, std::path::Path::new("."), &config.workspace_id,
                  config.encryption_password.as_deref(), /* lazy */ false).await?;
```


# DOX framework

- DOX is highly performant AGENTS.md hierarchy installed here
- Agent must follow DOX instructions across any edits

## Core Contract

- AGENTS.md files are binding work contracts for their subtrees
- Work products, source materials, instructions, records, assets, and durable docs must stay understandable from the nearest applicable AGENTS.md plus every parent AGENTS.md above it

## Read Before Editing

1. Read the root AGENTS.md
2. Identify every file or folder you expect to touch
3. Walk from the repository root to each target path
4. Read every AGENTS.md found along each route
5. If a parent AGENTS.md lists a child AGENTS.md whose scope contains the path, read that child and continue from there
6. Use the nearest AGENTS.md as the local contract and parent docs for repo-wide rules
7. If docs conflict, the closer doc controls local work details, but no child doc may weaken DOX

Do not rely on memory. Re-read the applicable DOX chain in the current session before editing.

## Update After Editing

Every meaningful change requires a DOX pass before the task is done.

Update the closest owning AGENTS.md when a change affects:

- purpose, scope, ownership, or responsibilities
- durable structure, contracts, workflows, or operating rules
- required inputs, outputs, permissions, constraints, side effects, or artifacts
- user preferences about behavior, communication, process, organization, or quality
- AGENTS.md creation, deletion, move, rename, or index contents

Update parent docs when parent-level structure, ownership, workflow, or child index changes. Update child docs when parent changes alter local rules. Remove stale or contradictory text immediately. Small edits that do not change behavior or contracts may leave docs unchanged, but the DOX pass still must happen.

## Hierarchy

- Root AGENTS.md is the DOX rail: project-wide instructions, global preferences, durable workflow rules, and the top-level Child DOX Index
- Child AGENTS.md files own domain-specific instructions and their own Child DOX Index
- Each parent explains what its direct children cover and what stays owned by the parent
- The closer a doc is to the work, the more specific and practical it must be

## Child Doc Shape

- Create a child AGENTS.md when a folder becomes a durable boundary with its own purpose, rules, responsibilities, workflow, materials, or quality standards
- Work Guidance must reflect the current standards of the project or user instructions; if there are no specific standards or instructions yet, leave it empty
- Verification must reflect an existing check; if no verification framework exists yet, leave it empty and update it when one exists

Default section order:
- Purpose
- Ownership
- Local Contracts
- Work Guidance
- Verification
- Child DOX Index

## Style

- Keep docs concise, current, and operational
- Document stable contracts, not diary entries
- Put broad rules in parent docs and concrete details in child docs
- Prefer direct bullets with explicit names
- Do not duplicate rules across many files unless each scope needs a local version
- Delete stale notes instead of explaining history
- Trim obvious statements, repeated rules, misplaced detail, and warnings for risks that no longer exist

## Closeout

1. Re-check changed paths against the DOX chain
2. Update nearest owning docs and any affected parents or children
3. Refresh every affected Child DOX Index
4. Remove stale or contradictory text
5. Run existing verification when relevant
6. Report any docs intentionally left unchanged and why

## User Preferences

When the user requests a durable behavior change, record it here or in the relevant child AGENTS.md

## Child DOX Index

Each direct child owns a crate boundary; subdirectories inside crates share files at the top level and do not merit separate AGENTS.md.

| Child | Purpose |
| :--- | :--- |
| [common/](common/AGENTS.md) | Shared data models and Blake3 XOF encryption primitives. Zero I/O, zero side effects — depends only on `blake3`, `getrandom`, `chrono`, and `serde`/`serde_json`. |
| [server/](server/AGENTS.md) | Axum blob storage server and SQLite metadata coordinator. Pure transport — never decrypts, never inspects file content. |
| [client/](client/AGENTS.md) | CLI + library crate. Owns local cache, sync engine, agent workspaces, predictive hydration, catch-up summary, watch loop. |
