# FEANORFS KNOWLEDGE BASE

**Generated:** 2026-07-12T18:00:00+02:00
**Branch:** main
**Status:** Format-v3 encrypted Merkle snapshots, JSON-backed embeddable SDK, safe SQLite import, five-target Node package assembly, append-only history, retained-manifest GC, tray, MCP/events, and catch-up summary.

## Unifying Principle

**FeanorFS is dumb storage, smart transport.** FeanorFS never makes decisions about file content (no auto-merge, no summarization, no chat). Its job is to decide _what_ to transport, _when_ to transport it, and _how_ to isolate and preserve files safely. Anything requiring file-semantic understanding belongs in the consumer/agent layer.

## Layers

| Layer | Role |
|---|---|
| **Hub** (`feanorfs serve`) | Opaque blob storage plus compare-and-swap heads, format markers, and reachability manifests. The server never decrypts trees or sees format-v3 filenames. |
| **Engine** (`feanorfs_client` + `feanorfs_agent_core`) | Builds encrypted trees, reconciles snapshots, materializes working copies, and exposes CLI, Rust, C, TypeScript, MCP, and events surfaces. |
| **Tray client** (shipped) | `tray/` — `feanorfs-tray` menu-bar app. Shells `feanorfs --json tray status`, `conflicts keep`, `agent land`. No duplicate sync logic. |

**Defaults:**
- Prefer smart defaults over flags where practical (`feanorfs start`, watch after sync).
- Server auth = **token**; workspace secrecy = **encryption key** (distinct concepts in user-facing copy).
- Surface conflicts; never auto-merge file content.
- Self-host and hosted deployments share the same API and client binary.
- Agent-first, human-legible: every agent capability keeps a plain-files, plain-language human path (working copy stays normal files; conflicts resolved by editing + `conflicts keep`/tray). Transport/snapshot internals stay invisible to humans until needed — FeanorFS is not a VCS and grows no git-shaped porcelain.

## OVERVIEW
`FeanorFS` is a developer-focused uncommitted-code synchronization tool written in Rust. It uses a self-contained local-first architecture:
1. **Snapshot synchronization**: Format-v3 clients compare encrypted Merkle trees against `.feanorfs/refs/last-synced`, stage blobs and tree objects, then compare-and-swap one workspace head.
2. **Blob storage**: Content-addressed storage (CAS) blobs use Blake3 ciphertext hashes. Remote `feanorfs serve` uses SQLite for opaque heads, manifests, format markers, and legacy-format metadata; the embedded LocalHub uses lock-protected JSON plus blob files.
3. **End-to-End Encryption (E2EE)**: New blobs are sealed with ChaCha20-Poly1305 AEAD (`pack_bytes`/`unpack_bytes`), key derived from `blake3(domain ‖ len-prefixed password ‖ len-prefixed path)`, deterministic SIV-style nonce (required for CAS stability). Format v2 workspaces reject non-AEAD blobs (`LegacyPolicy::Reject`); unmigrated v1 workspaces still fall back to legacy XOR on decrypt until `feanorfs migrate` — removing that path is [SEC-6](docs/roadmap.md). Client re-hashes downloaded ciphertext against `encrypted_hash` before decrypting.
4. **Local hub (in-process)**: `setup --local` / `hub_local` config uses agent-core `LocalHub` directly — no socket, daemon, server crate, or SQLite. Share on the network via `feanorfs serve --data-dir .feanorfs/hub-data` (invites are not portable for embedded hubs).
5. **On-Demand Hydration (Lazy Sync)**: `pull --lazy` creates 0-byte placeholders; actual bytes fetched via `hydrate` or `cat`.
6. **Workspace isolation**: `agent spawn` clones files and writes one base snapshot ref. Status and land descend only into changed subtrees. Land commits through head compare-and-swap, and conflicts survive in encrypted tree entries plus human-readable artifacts.
7. **Agent Library API**: Client crate is split into `lib.rs` + `main.rs`; `feanorfs_client::sync/push/pull/hydrate/cat` are callable from any Rust program. `--json` flag on the CLI emits structurally-typed results for every status-returning command.
8. **Catch-Up Summary**: `summary` diffs current workspace against the previous session marker stored in `local_state.json` and lists added/modified/deleted paths. `--summarize` shells out to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`) with structured JSON; if absent, it falls back to plain paths. File contents never go to a remote LLM.
9. **History and retention**: `log` walks reachable snapshot parents. `undo` records the selected tree as a new two-parent snapshot. Clients upload complete opaque reachability manifests; server and local GC retain configured snapshot closures. Server GC is serialized against publication.
10. **Migration safety**: A durable server fence excludes legacy writes from pre-reseal pull through atomic format stamp, flat-row deletion, and fence release. Client journal phases preserve old and target keys across retries.
11. **Predictive hydration**: `file_access_log` tracks local path co-occurrence and never leaves the client.

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
│   ├── src/hub.rs       # Thin re-export of agent-core LocalHub
│   ├── src/migrate_sqlite/ # One-time workspace/agent/embedded-hub importer
│   ├── src/commands.rs  # Push/pull/sync/hydrate/cat command implementations (Serialize'd result types)
│   ├── src/agent.rs     # Thin re-export of agent-core operations
│   ├── src/conflicts.rs # Thin re-export of agent-core conflict operations
│   ├── src/conflict_artifacts.rs # Conflict version files (.original/.local/.cloud) + sentinel placeholders
│   ├── src/fs_util.rs   # atomic_write (temp+rename), file mtime helpers
│   ├── src/cli/         # CLI handlers (agent, conflicts, serve, start, mcp, events, workspace, …)
│   ├── src/summary.rs   # Catch-up summary: diff against last_session, FEANORFS_SUMMARY_CMD shell-out with plain fallback
│   ├── src/predictive.rs # Predictive hydration: access recording + co-occurrence prefetch + time decay
│   ├── src/local.rs     # Client-side config, ignore rules, rebuildable cache, access log, session markers
│   ├── src/main.rs      # CLI subcommand router with global --json flag and AgentAction subcommand
│   └── src/watch.rs     # Debounced real-time change watcher
└── server-data/         # Created by server to store file blobs and sqlite metadata (git-ignored)
```

---

## THE LOCAL CACHE DESIGN
To avoid unnecessary re-hashing, the client stores schema-versioned state in `.feanorfs/local_state.json`, protected by a separate advisory lock and atomic replacement:
1. **State maps:** cache entries, pending conflicts, conflict resolution history, session markers, and bounded predictive access weights.
2. **Legacy import:** client-owned migration reads WAL-visible `local_cache.db` rows for the workspace and each agent, verifies semantic equality after JSON import, then archives SQLite files as `.migrated-v1.db`.
3. **Double-hash and Server Mtime Tracking** in cache entries:
   - `plaintext_hash`: Used to quickly detect disk modifications.
   - `encrypted_hash`: Stores the actual crypt hash matching the server blob key.
   - `server_mtime`: Tracks the server's official commit mtime for cache/order and rollback evidence. Conflict identity and final sync direction use hashes relative to the last agreed state, not cross-machine clocks.

---

## WHERE TO LOOK

| Task / Feature | Location | Notes |
| :--- | :--- | :--- |
| FileState definition | [lib.rs](common/src/lib.rs) | Tracks relative path, Blake3 hash (encrypted), size, mtime, and deleted status. |
| Agent snapshot + conflict types | [lib.rs](common/src/lib.rs) | `AgentSnapshotEntry`, `ConcurrentEdit`, `AgentLandResult` (land JSON); `AgentCommitResult` is a legacy subset alias. |
| E2EE primitives | [lib.rs](common/src/lib.rs) | `pack_bytes`/`unpack_bytes` (ChaCha20-Poly1305 AEAD, deterministic SIV-style nonce) with legacy `crypt_bytes` XOR fallback on decrypt for unmigrated v1. Format v2 rejects non-AEAD. Also exports `is_valid_hash` for path-traversal defense. |
| Library API surface | [lib.rs](client/src/lib.rs) | `feanorfs_client::sync/push/pull/hydrate/cat` callable from any Rust program. Re-exports `ApiClient`, `ClientDb`, `Config`, types. |
| Local state + migration | [local.rs](agent-core/src/local.rs), [migrate_sqlite/](client/src/migrate_sqlite/) | Lock-protected JSON cache/conflict/session/access state plus one-time legacy SQLite import. Snapshot authority lives in refs and encrypted objects. |
| Encrypted objects + snapshots | [objects.rs](agent-core/src/objects.rs), [snapshot.rs](agent-core/src/snapshot.rs) | Immutable encrypted tree/snapshot CAS, refs, manifests, and head publication. |
| History | [history.rs](agent-core/src/history.rs) | Reachable DAG log, short-ID resolution, append-only undo, and worktree materialization. |
| Directory Scanning | [local.rs](client/src/local.rs) | Uses `ignore` WalkBuilder. Matches size and mtime. Reports cached `server_mtime` for untouched placeholders. |
| Sync scope & ignores | [sync-scope.md](docs/sync-scope.md) | Why we sync gitignored paths, small `DEFAULT_IGNORES`, no `.gitignore`, optional `.feanorfsignore`, planned `CACHEDIR.TAG`. |
| Transport (`ApiClient`) | [api.rs](client/src/api.rs) + [hub.rs](client/src/hub.rs) | HTTP or in-process hub via `Backend::Http` / `Backend::Local`. Wraps `/api/sync/diff`, upload, download, workspaces. |
| CLI Actions | [main.rs](client/src/main.rs) + [cli/](client/src/cli/) | Subcommand router. Global `--json`. Agent: spawn/check/refresh/land (commit alias). Workspace: start/setup/join/serve. |
| Sync Engine | [commands.rs](client/src/commands.rs) | Pure sync logic returning `Serialize`-derived result types (`SyncResult`, `PushResult`, etc.). No `println!` — UI-agnostic. |
| Workspace Isolation | [agent.rs](agent-core/src/agent.rs) | `spawn_agent`, `check_agent`, `land_agent`, `refresh_agent`, `list_agents`, `clean_agent`. Format v3 compares encrypted snapshot heads; legacy formats retain peek/diff compatibility. |
| Workspace sync conflicts | [conflicts.rs](agent-core/src/conflicts.rs), [tree_reconcile.rs](agent-core/src/tree_reconcile.rs) | Tree-based last-synced reconciliation, registry/artifacts, and `conflicts keep`. |
| Catch-up Summary | [summary.rs](client/src/summary.rs) | `diff_since_last_session`, `commit_session_marker`, `render_via_summary_tool` (shells out to `FEANORFS_SUMMARY_CMD`, default `feanorfs-llm`, falls back to plain listing). |
| Predictive Hydration | [predictive.rs](client/src/predictive.rs) | `record_access_with_recent`, `prefetch_related` (top-5 siblings, 0.95 decay factor). Triggered from `hydrate` and `cat` CLI arms. |
| Change Watching | [watch.rs](client/src/watch.rs) | Debounced (500ms) filesystem watcher that triggers `do_sync` on changes. |
| CI, security, and releases | [ci.yml](.github/workflows/ci.yml), [npm-release.yml](.github/workflows/npm-release.yml), [security.yml](.github/workflows/security.yml), [release-plz.yml](.github/workflows/release-plz.yml), [release.yml](.github/workflows/release.yml), [tray-release.yml](.github/workflows/tray-release.yml) | Main CI verifies SDK dependency boundaries and packed Node tarballs. App tags release the CLI and optional tray; npm package assembly is manual dry-run only. Cargo-dist owns its generated workflow. |

---

## CODE MAP

### Core Data Models ([common/src/lib.rs](common/src/lib.rs))
- `FileState`: Schema for paths, hashes, sizes, mtimes, and deleted states.
- `SyncRequest` & `SyncResponse`: Serialization structs for endpoint negotiation.
- `Tree`, `TreeEntry`, and `Snapshot`: canonical encrypted workspace structure and append-only parent graph.
- `ConcurrentEdit`: Three-way triple (base/ours/theirs) emitted by `agent land` for conflicting paths. FeanorFS does not merge — consumers reconcile.
- `AgentLandResult`: Aggregate result of `agent land` — `our_changes`, `their_changes`, `conflicts`, `landed`, `message`. `AgentCommitResult` is a legacy subset (no `landed`/`message`).

### Server API Endpoints ([server/src/app.rs](server/src/app.rs))
- `POST /api/sync/diff`: Legacy format-v1/v2 metadata compatibility path.
- `GET/PUT /api/head`: Read and compare-and-swap the opaque format-v3 snapshot head.
- `POST /api/manifest`: Store a validated opaque reachability manifest for retention GC.
- `POST /api/workspace/format`: Stamp format v3 and delete that workspace's flat metadata rows.
- `POST /api/upload?workspace_id=...`: Receives raw encrypted bytes, writes them to `server-data/blobs/<hash>`, and upserts DB metadata. If the DB upsert fails, the orphaned blob is removed from disk before returning an error (no partial state). Request body size capped at 100 MB via `DefaultBodyLimit` to prevent memory-exhaustion DoS.
- `GET /api/download/:hash`: Streams raw file contents. Rejects non-hex hashes via `is_valid_hash` to prevent path traversal.
- `GET /api/workspaces`: Lists all workspace IDs that have at least one non-deleted file.
- **Auth middleware**: If `--token` is set, all routes require `Authorization: Bearer <token>` header. `--password` accepted as alias. Token comparison uses constant-time equality to prevent timing side-channels.
- **mDNS**: Server advertises `_feanorfs._tcp.local.` on port 3030 for LAN discovery when started with `--mdns` (off by default for internet deployments).
- **Multi-instance**: `--port` and `--data-dir` flags allow running multiple isolated instances behind a reverse proxy (SaaS deployment model).

### Client Local State ([agent-core/src/state.rs](agent-core/src/state.rs))
- Cache entries map paths to plaintext/encrypted hashes, size, disk/server mtimes, mode, hydration, and deletion metadata.
- Access log entries store local-only co-occurrence weights and timestamps with deterministic bounds and decay.
- Conflict registry and resolution history preserve needs-attention state and explicit choices.
- Session key/value state stores the previous `last_scan` summary baseline.
- Global config: `~/.feanorfs/global.json` stores server URL + optional server password (cached automatically by `feanorfs start <URL>`; hidden `connect` also writes it).
- Workspace config: `.feanorfs/config.json` stores server URL, workspace ID, E2EE password, and optional server password.

---

## CONVENTIONS
1. **Cross-Platform Paths**: All files are tracked and uploaded using forward slashes (`/`). Normalize with `feanorfs_common::normalize_path` before cache or database operations.
2. **No Redundant Hashing**: Check `local_state.json` first. Rehash only if `mtime` or `size` differs.
3. **Zero-knowledge encryption**: Seal file bytes by path and tree/snapshot objects under the fixed object domain before upload. Format-v3 server metadata contains no filenames.
4. **Library-First Result Types**: Commands return `Serialize`-derived structs (`SyncResult`, `PushResult`, etc.) so the `--json` flag and `feanorfs_client::` library callers see the same shape.
5. **No Auto-Merge**: `agent land` emits three-way `ConcurrentEdit` triples under `.feanorfs/conflicts/` (`.original`/`.local`/`.cloud`). Reconciliation is the consumer's job via `conflicts keep`.
6. **Predictive Hydration is Local-Only**: access weights never leave the client. They stay in `.feanorfs/local_state.json`.
7. **Data Isolation ≠ Sandbox**: agent workspaces isolate files, not processes. Never claim sandboxing in code or copy; link the "Process isolation" section of [docs/threat-model.md](docs/threat-model.md) instead.
8. **Sync scope**: mirror disk contents (including gitignored paths); hard skip `.feanorfs/`, `.git/`, symlinks, and nested valid `CACHEDIR.TAG` trees; small frozen `DEFAULT_IGNORES` only — see [docs/sync-scope.md](docs/sync-scope.md). Do not honor `.gitignore` or expand defaults into a framework-specific denylist.
9. **CI/CD ownership**: Pin repository-owned actions to immutable SHAs, keep permissions least-privilege, and validate workflows with actionlint/zizmor. Never hand-edit cargo-dist's generated `.github/workflows/release.yml`; change `dist-workspace.toml` and regenerate it.
10. **Release changelog ownership**: Root `CHANGELOG.md` is canonical. Release-plz must use `changelog_path = "./CHANGELOG.md"`; do not create crate-local changelogs.

---

## ANTI-PATTERNS (THIS PROJECT)
- **DO NOT** scan the `.feanorfs`, `.git`, or `.feanorfs/agents/` directories as part of the main workspace scan. They are hardcoded as skipped. Agents have their own scan inside their workspace dir (separate `ClientDb`).
- **DO NOT** trigger syncs on every raw filesystem change event. Filesystem saves are noisy. Debounce updates for 500ms using a channel.
- **DO NOT** download remote file bytes immediately during sync if `--lazy` is enabled. Write 0-byte placeholders instead.
- **DO NOT** add a new server endpoint when encrypted objects, manifests, and head compare-and-swap already express the operation. Keep the server dumb.
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

Prioritized backlog: [docs/roadmap.md](docs/roadmap.md). **Shipped:** format-v3 snapshots, JSON-backed agent SDK, safe SQLite import, executable intent, history/undo, retained GC, tray, and P2 sync polish. **Release-ready:** five-target Node package assembly; registry publication is intentionally deferred while app tags ship only the CLI and tray. **Gated:** SEC-6 waits for v1 migration evidence.

## Child DOX Index

Direct children own durable crate or automation boundaries; subdirectories inside crates share files at the top level and do not merit separate AGENTS.md.

| Child | Purpose |
| :--- | :--- |
| [.github/](.github/AGENTS.md) | CI, security scanning, dependency automation, release orchestration, and contributor templates. |
| [common/](common/AGENTS.md) | Shared data models and Blake3 XOF encryption primitives. Zero I/O, zero side effects — depends only on `blake3`, `getrandom`, `chrono`, and `serde`/`serde_json`. |
| [server/](server/AGENTS.md) | Axum blob storage server and SQLite metadata coordinator. Pure transport — never decrypts, never inspects file content. |
| [client/](client/AGENTS.md) | CLI + library crate. Sync engine, watch, summary, predictive; agent ops delegate to agent-core. |
| [bindings/ts/](bindings/ts/) | napi-rs Node bindings, five native platform packages, deterministic assembly, and resumable npm publication scripts. |
| [feanorfs-ffi/](feanorfs-ffi/) | C ABI and generated header consumed by the Zig example. |
| [tray/](tray/) | macOS menu-bar companion (`feanorfs-tray`). Shells CLI `--json`; see [tray/README.md](tray/README.md). |
| [agent-core/](agent-core/AGENTS.md) | Embeddable agent SDK: `Runtime`, `Workspace`, local hub, conflict gate. Consumed by client, FFI, and Node bindings. |
