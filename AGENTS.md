# FEANORFS KNOWLEDGE BASE

**Generated:** 2026-06-23T18:15:00+02:00
**Branch:** main
**Status:** E2EE sync, agent workspaces, single binary (`feanorfs` + `feanorfs serve`, in-process local hub), MCP/events, catch-up summary.

## Unifying Principle

**FeanorFS is dumb storage, smart transport.** FeanorFS never makes decisions about file content (no auto-merge, no summarization, no chat). Its job is to decide _what_ to transport, _when_ to transport it, and _how_ to isolate and preserve files safely. Anything requiring file-semantic understanding belongs in the consumer/agent layer.

## Layers

| Layer | Role |
|---|---|
| **Hub** (`feanorfs serve`) | Blob + metadata backend in the same binary as the sync client. Same router as the optional legacy `feanorfs-server` crate binary (`--port`, `--data-dir`, `--token` per instance). |
| **Engine** (`feanorfs_client` + CLI) | Sync library + CLI. Embeds the hub for local workspaces (`hub.rs`, in-process `ApiClient`) and exposes `feanorfs serve` for network hubs. `--json` is the contract for any UI shell. |
| **Tray client** (shipped) | `tray/` — `feanorfs-tray` menu-bar app. Shells `feanorfs --json tray status`, `conflicts keep`, `agent land`. No duplicate sync logic. |

**Defaults:**
- Prefer smart defaults over flags where practical (`feanorfs start`, watch after sync).
- Server auth = **token**; workspace secrecy = **encryption key** (distinct concepts in user-facing copy).
- Surface conflicts; never auto-merge file content.
- Self-host and hosted deployments share the same API and client binary.
- Agent-first, human-legible: every agent capability keeps a plain-files, plain-language human path (working copy stays normal files; conflicts resolved by editing + `conflicts keep`/tray). Transport/snapshot internals stay invisible to humans until needed — FeanorFS is not a VCS and grows no git-shaped porcelain.

## OVERVIEW
`FeanorFS` is a developer-focused uncommitted-code synchronization tool written in Rust. It utilizes a **self-contained local-first architecture**:
1. **Metadata Synchronization**: SQLite on both sides. Server in `server-data/db.sqlite`, client cache in `.feanorfs/local_cache.db`. Client queries `/api/sync/diff` with its metadata and receives a delta.
2. **Blob Storage**: Content-addressed storage (CAS) blobs identified by Blake3 hashes on a lightweight Axum server.
3. **End-to-End Encryption (E2EE)**: New blobs are sealed with ChaCha20-Poly1305 AEAD (`pack_bytes`/`unpack_bytes`), key derived from `blake3(domain ‖ len-prefixed password ‖ len-prefixed path)`, deterministic SIV-style nonce (required for CAS stability). Format v2 workspaces reject non-AEAD blobs (`LegacyPolicy::Reject`); unmigrated v1 workspaces still fall back to legacy XOR on decrypt until `feanorfs migrate` — removing that path is [SEC-6](docs/roadmap.md). Client re-hashes downloaded ciphertext against `encrypted_hash` before decrypting.
4. **Local hub (in-process)**: `setup --local` / `hub_local` config uses `LocalHub` + `tower::oneshot` — no socket, no daemon. Share on the network via `feanorfs serve --data-dir .feanorfs/hub-data` (invites are not portable for embedded hubs).
5. **On-Demand Hydration (Lazy Sync)**: `pull --lazy` creates 0-byte placeholders; actual bytes fetched via `hydrate` or `cat`.
6. **Workspace Isolation**: `agent spawn` creates snapshots under `.feanorfs/agents/<name>/` (APFS clonefile / copy fallback) and records the server's per-file view into `agent_snapshots`. `agent check` previews; `agent land` applies clean work, uploads, and registers conflicts. Conflicts are written under `.feanorfs/conflicts/` as `.original`/`.local`/`.cloud`; FeanorFS does NOT merge.
7. **Agent Library API**: Client crate is split into `lib.rs` + `main.rs`; `feanorfs_client::sync/push/pull/hydrate/cat` are callable from any Rust program. `--json` flag on the CLI emits structurally-typed results for every status-returning command.
8. **Catch-Up Summary**: `summary` command diffs current workspace against the previous session marker (stored in `last_session` table) and lists added/modified/deleted paths. `--summarize` shells out to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`) feeding it the structured result as JSON; if the binary is absent, falls back to plain path listing. Zero-knowledge is preserved by never shipping file contents to a remote LLM.
9. **Predictive Hydration**: `file_access_log` table tracks co-occurrence (path × sibling_path × weight × updated_at). After `cat`/`hydrate`, `predictive::record_access_with_recent` loads the rolling 5-path history from `last_session`, bumps the weights of recently-touched siblings, and saves the updated list back; `predictive::prefetch_related` runs after `Hydrate` to fetch top-5 co-occurring siblings in the background. Each run applies a 0.95 time-decay factor — no ML, pure weighted co-occurrence.

---

## STRUCTURE
```
feanorfs/
├── common/              # Shared data models and utilities
│   └── src/lib.rs       # FileState, SyncRequest/Response, invite encode/decode, crypto
├── server/              # Pure blob storage server
│   ├── src/db.rs        # SQLite metadata DB coordinator using SQLx
│   ├── src/app.rs       # Axum routes for sync negotiation, blob uploads & downloads
│   ├── src/serve.rs     # run_http_server, GC, shared with client embed
│   ├── src/lib.rs       # feanorfs_server library (build_router, init_app_state)
│   └── src/main.rs      # CLI entrypoint
├── agent-core/          # Embeddable agent SDK (Runtime, Workspace, spawn/land/conflicts)
│   └── src/             # agent, local, hub, api, conflicts, sync_pass, …
├── feanorfs-ffi/        # C ABI (JSON strings in/out) + feanorfs.h
├── bindings/ts/         # @feanorfs/agent napi-rs Node bindings
├── client/              # CLI terminal client + library crate
│   ├── src/lib.rs       # feanorfs_client lib export surface (sync/push/pull/hydrate/cat, types, Db, ApiClient)
│   ├── src/api.rs       # HTTP + in-process ApiClient backends
│   ├── src/hub.rs       # In-process local hub (LocalHub + tower oneshot)
│   ├── src/commands.rs  # Push/pull/sync/hydrate/cat command implementations (Serialize'd result types)
│   ├── src/agent.rs     # Workspace Isolation: spawn/check/land/refresh, snapshots, three-way conflict detection
│   ├── src/conflicts.rs # Workspace sync conflict gate: last_synced_files three-way detection, conflict_registry, resolve
│   ├── src/conflict_artifacts.rs # Conflict version files (.original/.local/.cloud) + sentinel placeholders
│   ├── src/fs_util.rs   # atomic_write (temp+rename), file mtime helpers
│   ├── src/cli/         # CLI handlers (agent, conflicts, serve, start, mcp, events, workspace, …)
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
   - **`agent_snapshots`**: per-agent `(agent_name, path)` → `(base_hash, base_size, base_mtime)` — base leg for three-way diff; advanced after `agent land` from post-land main `FileState`.
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
| Agent snapshot + conflict types | [lib.rs](common/src/lib.rs) | `AgentSnapshotEntry`, `ConcurrentEdit`, `AgentLandResult` (land JSON); `AgentCommitResult` is a legacy subset alias. |
| E2EE primitives | [lib.rs](common/src/lib.rs) | `pack_bytes`/`unpack_bytes` (ChaCha20-Poly1305 AEAD, deterministic SIV-style nonce) with legacy `crypt_bytes` XOR fallback on decrypt for unmigrated v1. Format v2 rejects non-AEAD. Also exports `is_valid_hash` for path-traversal defense. |
| Library API surface | [lib.rs](client/src/lib.rs) | `feanorfs_client::sync/push/pull/hydrate/cat` callable from any Rust program. Re-exports `ApiClient`, `ClientDb`, `Config`, types. |
| Local Cache DB + tables | [local.rs](client/src/local.rs) | Schema for `local_files`, `agent_snapshots`, `file_access_log`, `last_session`, `last_synced_files`, `conflict_registry`. CRUD on each. |
| Directory Scanning | [local.rs](client/src/local.rs) | Uses `ignore` WalkBuilder. Matches size and mtime. Reports cached `server_mtime` for untouched placeholders. |
| Sync scope & ignores | [sync-scope.md](docs/sync-scope.md) | Why we sync gitignored paths, small `DEFAULT_IGNORES`, no `.gitignore`, optional `.feanorfsignore`, planned `CACHEDIR.TAG`. |
| Transport (`ApiClient`) | [api.rs](client/src/api.rs) + [hub.rs](client/src/hub.rs) | HTTP or in-process hub via `Backend::Http` / `Backend::Local`. Wraps `/api/sync/diff`, upload, download, workspaces. |
| CLI Actions | [main.rs](client/src/main.rs) + [cli/](client/src/cli/) | Subcommand router. Global `--json`. Agent: spawn/check/refresh/land (commit alias). Workspace: start/setup/join/serve. |
| Sync Engine | [commands.rs](client/src/commands.rs) | Pure sync logic returning `Serialize`-derived result types (`SyncResult`, `PushResult`, etc.). No `println!` — UI-agnostic. |
| Workspace Isolation | [agent.rs](client/src/agent.rs) | `spawn_agent`, `check_agent`, `land_agent` (`commit_agent` alias), `refresh_agent`, `list_agents`, `clean_agent`. Three-way via `/api/sync/peek` + diff. |
| Workspace sync conflicts | [conflicts.rs](client/src/conflicts.rs) | `negotiate_sync_with_conflict_gate`, `last_synced_files`, `conflict_registry`, `resolve_conflict` (`conflicts keep`), join/attach divergent-path guards. |
| Catch-up Summary | [summary.rs](client/src/summary.rs) | `diff_since_last_session`, `commit_session_marker`, `render_via_summary_tool` (shells out to `FEANORFS_SUMMARY_CMD`, default `feanorfs-llm`, falls back to plain listing). |
| Predictive Hydration | [predictive.rs](client/src/predictive.rs) | `record_access_with_recent`, `prefetch_related` (top-5 siblings, 0.95 decay factor). Triggered from `hydrate` and `cat` CLI arms. |
| Change Watching | [watch.rs](client/src/watch.rs) | Debounced (500ms) filesystem watcher that triggers `do_sync` on changes. |
| CI, security, and releases | [ci.yml](.github/workflows/ci.yml), [security.yml](.github/workflows/security.yml), [release-plz.yml](.github/workflows/release-plz.yml), [release.yml](.github/workflows/release.yml), [tray-release.yml](.github/workflows/tray-release.yml), [dist-workspace.toml](dist-workspace.toml), [SECURITY.md](SECURITY.md) | Core gates exclude the macOS-only tray on Linux/Windows; tray gates run on macOS. Release-plz runs after trusted `main` CI succeeds. Cargo-dist owns its generated workflow and packages only the `feanorfs` CLI; the post-release tray workflow verifies the tag commit, builds, checksums, attests, and uploads both macOS archives. |

---

## CODE MAP

### Core Data Models ([common/src/lib.rs](common/src/lib.rs))
- `FileState`: Schema for paths, hashes, sizes, mtimes, and deleted states.
- `SyncRequest` & `SyncResponse`: Serialization structs for endpoint negotiation.
- `AgentSnapshotEntry`: One row of the per-agent base snapshot — `(agent_name, path, base_hash, base_size, base_mtime)`.
- `ConcurrentEdit`: Three-way triple (base/ours/theirs) emitted by `agent land` for conflicting paths. FeanorFS does not merge — consumers reconcile.
- `AgentLandResult`: Aggregate result of `agent land` — `our_changes`, `their_changes`, `conflicts`, `landed`, `message`. `AgentCommitResult` is a legacy subset (no `landed`/`message`).

### Server API Endpoints ([server/src/app.rs](server/src/app.rs))
- `POST /api/sync/diff`: Compares incoming client list with `files` table and returns a delta response. **Reused by `agent land`**: the client sends the base snapshot as the "client" view, so every server-side change since spawn surfaces as `download_required`. No new endpoint needed. Deletions are propagated via `upload_required` with `deleted=true` in the `FileState` payload (handled inside the diff handler).
- `POST /api/upload?workspace_id=...`: Receives raw encrypted bytes, writes them to `server-data/blobs/<hash>`, and upserts DB metadata. If the DB upsert fails, the orphaned blob is removed from disk before returning an error (no partial state). Request body size capped at 100 MB via `DefaultBodyLimit` to prevent memory-exhaustion DoS.
- `GET /api/download/:hash`: Streams raw file contents. Rejects non-hex hashes via `is_valid_hash` to prevent path traversal.
- `GET /api/workspaces`: Lists all workspace IDs that have at least one non-deleted file.
- **Auth middleware**: If `--token` is set, all routes require `Authorization: Bearer <token>` header. `--password` accepted as alias. Token comparison uses constant-time equality to prevent timing side-channels.
- **mDNS**: Server advertises `_feanorfs._tcp.local.` on port 3030 for LAN discovery when started with `--mdns` (off by default for internet deployments).
- **Multi-instance**: `--port` and `--data-dir` flags allow running multiple isolated instances behind a reverse proxy (SaaS deployment model).

### Client Database Schema ([client/src/local.rs](client/src/local.rs))
- `local_files` table: `path` (PK), `plaintext_hash`, `encrypted_hash`, `size`, `mtime` (disk), `server_mtime` (remote), `hydrated`, `deleted_at`.
- `agent_snapshots` table: `agent_name`, `path`, `base_hash`, `base_size`, `base_mtime`. Primary key `(agent_name, path)`.
- `file_access_log` table: `path`, `sibling_path`, `weight`, `updated_at`. Primary key `(path, sibling_path)`.
- `last_synced_files` table: per-path last-agreed state (`path`, `hash`, `size`, `mtime`, `deleted`) — the base leg for workspace conflict detection.
- `conflict_registry` table: pending needs-attention paths (`path`, `kind`, `conflict_dir`, `opened_at`, `status`) — blocks LWW sync until resolved.
- `last_session` table: `key` (PK), `value`. Currently stores `last_scan` = JSON-serialized `HashMap<String, FileState>` of the previous session.
- Global config: `~/.feanorfs/global.json` stores server URL + optional server password (cached automatically by `feanorfs start <URL>`; hidden `connect` also writes it).
- Workspace config: `.feanorfs/config.json` stores server URL, workspace ID, E2EE password, and optional server password.

---

## CONVENTIONS
1. **Cross-Platform Paths**: All files are tracked and uploaded using forward slashes (`/`). Always normalize path slashes using `feanorfs_common::normalize_path` before doing DB operations.
2. **No Redundant Hashing**: Check disk files against `local_cache.db` first. Rehash only if `mtime` or `size` differs.
3. **Zero-Knowledge Encryption**: Always encrypt file contents using `pack_bytes` (AEAD) before calling `api.upload_file` and store the resulting `encrypted_hash` in the database. `crypt_bytes` is decrypt-only legacy fallback — never use it for new uploads.
4. **Library-First Result Types**: Commands return `Serialize`-derived structs (`SyncResult`, `PushResult`, etc.) so the `--json` flag and `feanorfs_client::` library callers see the same shape.
5. **No Auto-Merge**: `agent land` emits three-way `ConcurrentEdit` triples under `.feanorfs/conflicts/` (`.original`/`.local`/`.cloud`). Reconciliation is the consumer's job via `conflicts keep`.
6. **Predictive Hydration is Local-Only**: `file_access_log` never leaves the client. Weights and access patterns stay in `.feanorfs/local_cache.db`.
7. **Data Isolation ≠ Sandbox**: agent workspaces isolate files, not processes. Never claim sandboxing in code or copy; link the "Process isolation" section of [docs/threat-model.md](docs/threat-model.md) instead.
8. **Sync scope**: mirror disk contents (including gitignored paths); hard skip `.feanorfs/` and `.git/`; small frozen `DEFAULT_IGNORES` only — see [docs/sync-scope.md](docs/sync-scope.md). Do not honor `.gitignore` or expand defaults into a framework-specific denylist.
9. **CI/CD ownership**: Pin repository-owned actions to immutable SHAs, keep permissions least-privilege, and validate workflows with actionlint/zizmor. Never hand-edit cargo-dist's generated `.github/workflows/release.yml`; change `dist-workspace.toml` and regenerate it.
10. **Release changelog ownership**: Root `CHANGELOG.md` is canonical. Release-plz must use `changelog_path = "./CHANGELOG.md"`; do not create crate-local changelogs.

---

## ANTI-PATTERNS (THIS PROJECT)
- **DO NOT** scan the `.feanorfs`, `.git`, or `.feanorfs/agents/` directories as part of the main workspace scan. They are hardcoded as skipped. Agents have their own scan inside their workspace dir (separate `ClientDb`).
- **DO NOT** trigger syncs on every raw filesystem change event. Filesystem saves are noisy. Debounce updates for 500ms using a channel.
- **DO NOT** download remote file bytes immediately during sync if `--lazy` is enabled. Write 0-byte placeholders instead.
- **DO NOT** add a new server endpoint when `agent land` can reuse `/api/sync/diff` by sending the base snapshot as the client view. Reuse keeps the server dumb.
- **DO NOT** ship file contents to a remote LLM when implementing `--summarize`. The shell-out tool is fed paths and metadata only; the E2EE password and file bytes stay local.
- **DO NOT** attempt to merge concurrent edits. `agent land` writes `.original`/`.local`/`.cloud` under `.feanorfs/conflicts/` and stops. A consumer reconciles with `conflicts keep`.
- **DO NOT** honor `.gitignore` or grow `DEFAULT_IGNORES` into a per-framework cache list. Follow [docs/sync-scope.md](docs/sync-scope.md) admission criteria; use `.feanorfsignore` for project-specific exclusions.

---

## COMMANDS

### Workspace Commands
```bash
# Cross-platform core (tray is macOS-only)
cargo build --workspace --exclude feanorfs-tray --locked

# Core unit and integration tests
cargo test --workspace --exclude feanorfs-tray --all-features --locked

# macOS tray checks
cargo test -p feanorfs-tray --locked
```

### Starting the Blob Hub
```bash
# Same binary as the sync client (recommended)
cargo run --bin feanorfs -- serve --port 3030 --data-dir server-data
cargo run --bin feanorfs -- serve --gc-only --data-dir server-data

# Source-only compatibility binary; not a release product
cargo run --bin feanorfs-server
```

### Client CLI Usage
```bash
# Begin: create, join, or resume — then sync + watch
cargo run --bin feanorfs -- start ~/projects/app   # folder-as-target
cargo run --bin feanorfs -- start 127.0.0.1:3030 --workspace my-workspace --token "server-pass"
cargo run --bin feanorfs -- start fnr1-...
cargo run --bin feanorfs -- start --local --workspace my-workspace
cargo run --bin feanorfs -- start --no-watch       # sync once after create/join

# Hidden script aliases (configure only — no auto watch)
cargo run --bin feanorfs -- setup --workspace my-workspace https://my-server.com:3030
cargo run --bin feanorfs -- init 127.0.0.1:3030 --workspace my-workspace
cargo run --bin feanorfs -- attach my-workspace --encryption-key <KEY> --server-url https://my-server.com:3030

# Inspect
cargo run --bin feanorfs -- config
cargo run --bin feanorfs -- config --key
cargo run --bin feanorfs -- doctor

# Sync
cargo run --bin feanorfs -- status
cargo run --bin feanorfs -- sync --no-watch
cargo run --bin feanorfs -- sync --up --no-watch
cargo run --bin feanorfs -- sync --down --lazy --no-watch
cargo run --bin feanorfs -- hydrate src/main.rs
cargo run --bin feanorfs -- cat src/main.rs
```

### Agent Workspace Commands
```bash
cargo run --bin feanorfs -- agent                    # list agents
cargo run --bin feanorfs -- agent status ci1         # preview one agent
cargo run --bin feanorfs -- agent spawn ci1
cargo run --bin feanorfs -- agent land ci1
cargo run --bin feanorfs -- agent run ci1 -- cargo test
```

### Conflict Commands
```bash
cargo run --bin feanorfs -- conflicts
cargo run --bin feanorfs -- conflicts keep src/main.rs --local
cargo run --bin feanorfs -- conflicts show src/main.rs --open
```

### Catch-Up & Predictive Commands
```bash
# Show which files changed since the last session marker (plain path listing by default)
cargo run --bin feanorfs -- summary

# Shell out to FEANORFS_SUMMARY_CMD (default: feanorfs-llm) feeding it the structured diff as JSON
cargo run --bin feanorfs -- summary --summarize

# Session baseline is updated by default; pass --no-remember to skip `cat`/`hydrate` record access patterns and
# `prefetch_related` fetches the top-5 co-occurring siblings in the background.
# No explicit command needed.
```

### JSON Output & Library API
```bash
# Global --json flag emits machine-readable structs for Status/Push/Pull/Sync/Hydrate/Cat/Summary/Agent
cargo run --bin feanorfs -- --json status
cargo run --bin feanorfs -- --json agent land ci1
```

```rust
// Library crate usage (feanorfs-client)
use feanorfs_client::{ApiClient, ClientDb, Config, sync};

let config = Config { /* ... */ };
let db = ClientDb::new(".feanorfs").await?;
let api = ApiClient::from_config(std::path::Path::new("."), &config).await?;
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

- CI/CD should favor mainstream tooling, immutable action pins, least privilege, cross-platform coverage, release provenance, and enforced quality gates over minimal workflow setup.
- Keep pull-request CI lean: require fast Linux quality gates, then run
  expensive cross-platform, release, SDK, tray, and CodeQL checks on `main`
  before release.
- Keep GitHub Releases product-focused: ship the `feanorfs` binary and optional
  macOS tray with integrity metadata, not internal crates or compatibility
  binaries already covered by `feanorfs serve`.

## Planning

Prioritized backlog: [docs/roadmap.md](docs/roadmap.md). **Active:** Merkle snapshot engine (MERK-1..7). **Shipped:** tray MVP (`feanorfs-tray`, `feanorfs tray status`). **Freeze list** (bug fixes only until MERK-1): `predictive.rs`, `summary --summarize`, mDNS LAN discovery.

## Child DOX Index

Direct children own durable crate or automation boundaries; subdirectories inside crates share files at the top level and do not merit separate AGENTS.md.

| Child | Purpose |
| :--- | :--- |
| [.github/](.github/AGENTS.md) | CI, security scanning, dependency automation, release orchestration, and contributor templates. |
| [common/](common/AGENTS.md) | Shared data models and Blake3 XOF encryption primitives. Zero I/O, zero side effects — depends only on `blake3`, `getrandom`, `chrono`, and `serde`/`serde_json`. |
| [server/](server/AGENTS.md) | Axum blob storage server and SQLite metadata coordinator. Pure transport — never decrypts, never inspects file content. |
| [client/](client/AGENTS.md) | CLI + library crate. Sync engine, watch, summary, predictive; agent ops delegate to agent-core. |
| [tray/](tray/) | macOS menu-bar companion (`feanorfs-tray`). Shells CLI `--json`; see [tray/README.md](tray/README.md). |
| [agent-core/](agent-core/AGENTS.md) | Embeddable agent SDK: `Runtime`, `Workspace`, local hub, conflict gate. Consumed by client, FFI, and Node bindings. |
