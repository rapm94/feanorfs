# FEANORFS KNOWLEDGE BASE

**Generated:** 2026-07-17T16:40:41+02:00
**Branch:** main
**Status:** Format-v3 encrypted Merkle snapshots, native Rustls hub transport with capability-pinned private CAs and DHCP-resilient CA-bound mDNS names, authenticated fail-closed hub trust refresh, automatic credential-free private-hub and per-workspace login services, encrypted workspace recovery kits, crash-safe private-hub identity recovery and CA/token rotation, OS-backed unattended credential storage with protected-file fallback, JSON-backed embeddable SDK, safe SQLite import, PAKE-authenticated LAN/off-LAN pairing and bounded opaque inner-TLS hub relay, hardened attestable amd64/arm64 relay OCI product, cross-platform desktop tray, universal notarizable macOS `.dmg`/`.pkg`, attested native Linux `.deb`/`.rpm`/`.pkg.tar.zst`/tar products, and Authenticode-gated Windows installer-EXE release workflows (credentialed signing proof pending), five-target Node package assembly, append-only history, retained-manifest GC, MCP/events, and catch-up summary.

## Unifying Principle

**FeanorFS is dumb storage, smart transport.** FeanorFS never makes decisions about file content (no auto-merge, no summarization, no chat). Its job is to decide _what_ to transport, _when_ to transport it, and _how_ to isolate and preserve files safely. Anything requiring file-semantic understanding belongs in the consumer/agent layer.

## Layers

| Layer | Role |
|---|---|
| **Hub** (`feanorfs serve`) | Opaque blob storage plus compare-and-swap heads, format markers, and reachability manifests. The server never decrypts trees or sees format-v3 filenames. |
| **Engine** (`feanorfs_client` + `feanorfs_agent_core`) | Builds encrypted trees, reconciles snapshots, materializes working copies, and exposes CLI, Rust, C, TypeScript, MCP, and events surfaces. |
| **Tray client** (shipped) | `tray/` — cross-platform `feanorfs-tray` system-tray app. Shells the CLI for status, lifecycle, pairing, conflict/agent actions, workspace recovery, diagnostics, and release awareness. No duplicate sync, pairing, credential, or cryptography logic. |

**Defaults:**
- Prefer smart defaults over flags where practical (`feanorfs start [folder]` creates a secure private hub when no connection exists, syncs, installs automatic hub/workspace services and the desktop tray, and returns; `feanorfs stop [folder]` reversibly removes automatic sync and tray registration; `--foreground` is explicit).
- Reusing `start fnh1-… <existing-folder>` refreshes hub trust only after an HTTPS CA/token/head probe succeeds; it must preserve that folder's workspace ID, E2EE key, refs, files, and encrypted history.
- Implicit new folders receive distinct opaque `fsw1-…` workspace IDs; `--workspace` is an advanced/manual override, never a shared consumer default.
- Server auth = **token**; workspace secrecy = **encryption key** (distinct concepts in user-facing copy).
- Native TLS is the hub default. `--allow-http` is explicit reverse-proxy/development mode; never disable certificate verification in clients.
- Surface conflicts; never auto-merge file content.
- Bulk conflict choices may apply one explicitly confirmed local-or-mirror policy to every pending path, but still never merge file content.
- Self-host and hosted deployments share the same API and client binary.
- Agent-first, human-legible: every agent capability keeps a plain-files, plain-language human path (working copy stays normal files; conflicts resolved by editing + `conflicts keep`/tray). Transport/snapshot internals stay invisible to humans until needed — FeanorFS is not a VCS and grows no git-shaped porcelain.

## OVERVIEW
`FeanorFS` is a developer-focused uncommitted-code synchronization tool written in Rust. It uses a self-contained local-first architecture:
1. **Snapshot synchronization**: Format-v3 clients compare encrypted Merkle trees against `.feanorfs/refs/last-synced`, stage blobs and tree objects, then compare-and-swap one workspace head.
2. **Blob storage**: Content-addressed storage (CAS) blobs use Blake3 ciphertext hashes. Remote `feanorfs serve` uses SQLite for opaque heads, manifests, format markers, and legacy-format metadata; the embedded LocalHub uses lock-protected JSON plus blob files.
3. **End-to-End Encryption (E2EE)**: New blobs are sealed with ChaCha20-Poly1305 AEAD (`pack_bytes`/`unpack_bytes`), key derived from `blake3(domain ‖ len-prefixed password ‖ len-prefixed path)`, deterministic SIV-style nonce (required for CAS stability). Format v2 workspaces reject non-AEAD blobs (`LegacyPolicy::Reject`); unmigrated v1 workspaces still fall back to legacy XOR on decrypt until `feanorfs migrate`. Removing compatibility requires separately approved representative field evidence. Client re-hashes downloaded ciphertext against `encrypted_hash` before decrypting.
4. **Local hub (in-process)**: `setup --local` / `hub_local` config uses agent-core `LocalHub` directly — no socket, daemon, server crate, or SQLite. Share on the network via `feanorfs serve --data-dir .feanorfs/hub-data` (invites are not portable for embedded hubs).
5. **On-Demand Hydration (Lazy Sync)**: `pull --lazy` creates 0-byte placeholders; actual bytes fetched via `hydrate` or `cat`.
6. **Workspace isolation**: `agent spawn` clones files and writes one base snapshot ref. Status and land descend only into changed subtrees. Land commits through head compare-and-swap, and conflicts survive in encrypted tree entries plus human-readable artifacts.
7. **Agent Library API**: Client crate is split into `lib.rs` + `main.rs`; `feanorfs_client::sync/push/pull/hydrate/cat` are callable from any Rust program. `--json` flag on the CLI emits structurally-typed results for every status-returning command.
8. **Catch-Up Summary**: `summary` diffs current workspace against the previous session marker stored in `local_state.json` and lists added/modified/deleted paths. `--summarize` shells out to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`) with structured JSON; if absent, it falls back to plain paths. File contents never go to a remote LLM.
9. **History and retention**: `log` walks reachable snapshot parents. `undo` records the selected tree as a new two-parent snapshot. Clients upload complete opaque reachability manifests; server and local GC retain configured snapshot closures. Server GC is serialized against publication.
10. **Migration safety**: A durable server fence excludes legacy writes from pre-reseal pull through atomic format stamp, flat-row deletion, and fence release. Client journal phases preserve old and target keys across retries.
11. **Predictive hydration**: `file_access_log` tracks local path co-occurrence and never leaves the client.
12. **Automatic lifecycle**: First-machine `feanorfs start [folder]` creates/reuses `~/.feanorfs/hub-data` and installs a credential-free private-hub login service; `--host` is the explicit override. A fresh hub prefers port 3030 and atomically persists an available fallback when occupied; existing hubs retain their endpoint. It also installs one credential-free service per workspace and registers the desktop tray on macOS, Linux, and Windows. `feanorfs stop [folder]` uninstalls that workspace service and removes its locked/atomic recent entry while preserving files, encrypted setup, credentials, remote snapshots, and the shared private hub. `doctor` verifies the complete lifecycle and emits the same secret-free named checks in human or JSON form; the tray's **Check System Health…** projects only fixed check names/statuses and offers explicit repair through the same `start` path. Workers receive only canonical data/workspace paths, read protected credentials and endpoint state in-process, and never put keys, tokens, invites, recovery passphrases, or automatic port selection in argv, environment, logs, or discovery. Path-plus-Blake3 executable identities restart jobs after same-path package upgrades, and background `start` coordinates with the managed watcher instead of racing its sync lock.
13. **LAN pairing**: `feanorfs pair` advertises a secret-free ephemeral mDNS session and delivers the automatic hub's stable CA-bound `.local` hostname. The desktop tray presents its short code on the sharing computer and offers **Join Another Computer…** on the receiver; the receiver supplies the capability through masked UI and bounded stdin into the ordinary `start` engine, never argv/environment/logs. The CLI retains all discovery and cryptography. `start fnp1-… [folder]` remains the terminal equivalent and uses SPAKE2 plus ChaCha20-Poly1305 to receive the full invite, sync, and install background service. Pairing is client-to-client; the hub stays opaque and never receives pairing or E2EE secrets.
14. **Off-LAN pairing and opaque hub relay**: `start --relay <public HTTPS URL> [folder]` persists a random 256-bit reachability route in protected workspace/global config and atomic `0600` hub-local state; its credential-free service receives only the hub data-directory path and maintains outbound WSS offers. Remote clients resolve the CA-bound hub hostname to an ephemeral loopback bridge and tunnel the existing Rustls stream, so the relay never sees bearer tokens, workspace IDs, API paths, object names, or tunneled bytes. The same stored relay makes tray/CLI pairing emit `fnp2`; its 80-bit secret and PAKE/AEAD invite remain client-side. `serve --relay` enables both bounded public routes; no default hosted relay or direct NAT traversal is claimed.
15. **Secure transport**: `feanorfs serve` uses Rustls HTTPS by default. A durable private CA under the hub data directory signs leaves containing a stable CA-derived mDNS hostname; automatic address tracking survives interface and DHCP changes without router reservations. `fnh1`/`fnr1` capabilities carry only the public certificate. Public CA chains remain supported.
16. **Local credential protection**: Secure onboarding stores E2EE keys and server tokens in macOS Keychain for signed releases, Windows Credential Manager, or Linux Secret Service and leaves only a random reference in config JSON. Unsigned macOS/source builds and unavailable platform stores use atomic `0600` config as the compatibility fallback; an already-migrated reference fails closed instead of spilling secrets back to disk.
17. **Private-hub recovery**: `serve recovery export` seals the durable hub CA and bearer token with Argon2id + XChaCha20-Poly1305. Offline import validates identity, fences partial writes across crashes, regenerates leaf certificates, and preserves client trust without placing recovery passphrases in argv or environment variables. Offline rotation writes a mandatory encrypted backup before crash-safe replacement of both CA and token, preserves opaque storage, and deliberately requires authenticated `fnh1` re-pairing on every client.
18. **Workspace recovery kit**: `recovery export|import` seals the complete portable `WorkspaceInvite` capability with Argon2id + XChaCha20-Poly1305 in an atomic private file. Import authenticates and validates before workspace/global writes, then supplies the decrypted invite in-process to the ordinary `start` path. The tray owns only native file/masked-input dialogs and a bounded stdin pipe; passphrases and decrypted capabilities never enter argv, environment variables, or logs. Kits are access backup, not blob backup, and require the hub to remain reachable.
19. **Release awareness**: `feanorfs update` performs a manual bounded HTTPS-only lookup of the official stable GitHub release, compares versions with `semver`, and validates the exact matching public tag URL. The tray presents the typed result and opens that page only after an explicit choice. Neither surface downloads, installs, or executes artifacts; signed/checksummed platform installers and release attestations remain the trust boundary.
20. **Large-file transport**: Format-v3 files above 64 MiB use path/index-bound 8 MiB ChaCha20-Poly1305 chunks and an authenticated encrypted manifest whose ciphertext hash remains the tree file identity. Requests remain below the hub's 100 MiB body bound, chunks are included in opaque reachability manifests, and streaming reconstruction verifies ciphertext hashes, AEAD, order, sizes, total length, and plaintext Blake3 before commit.

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
│   ├── src/tls.rs       # Native TLS, durable private CA, refreshed interface leaf
│   ├── src/recovery.rs  # Encrypted CA/token backup, offline crash-safe restore
│   ├── src/private_file.rs # Private directory, lock, and atomic-write helpers
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
│   ├── src/recovery.rs  # Encrypted offline workspace-capability recovery kit
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
| Sync scope & ignores | [sync-scope.md](docs/sync-scope.md) | Why we sync gitignored paths, small `DEFAULT_IGNORES`, no `.gitignore`, optional `.feanorfsignore`, and nested valid `CACHEDIR.TAG` pruning. |
| Transport (`ApiClient`) | [api.rs](client/src/api.rs) + [hub.rs](client/src/hub.rs) | HTTP or in-process hub via `Backend::Http` / `Backend::Local`. Wraps `/api/sync/diff`, upload, download, workspaces. |
| Native TLS | [tls.rs](server/src/tls.rs), [api.rs](agent-core/src/api.rs), [invite.rs](common/src/invite.rs) | Rustls server, invite-pinned private CA, system-root public TLS, and secure hub/workspace capabilities. |
| Private-hub recovery | [recovery.rs](server/src/recovery.rs), [serve.rs](client/src/cli/serve.rs) | Argon2id + XChaCha20-Poly1305 identity bundles, offline runtime lock, durable import fence, CA/key validation, and leaf regeneration. |
| Workspace recovery | [recovery.rs](client/src/recovery.rs), [recovery.rs](client/src/cli/recovery.rs) | Opaque Argon2id + XChaCha20-Poly1305 capability kits, atomic private writes, fail-before-write import, and in-process delegation to `start`. |
| CLI Actions | [main.rs](client/src/main.rs) + [cli/](client/src/cli/) | Subcommand router. Global `--json`. Agent: spawn/check/refresh/land (commit alias). Workspace: start/stop/setup/join/serve. |
| Sync Engine | [commands.rs](client/src/commands.rs) | Pure sync logic returning `Serialize`-derived result types (`SyncResult`, `PushResult`, etc.). No `println!` — UI-agnostic. |
| Workspace Isolation | [agent.rs](agent-core/src/agent.rs) | `spawn_agent`, `check_agent`, `land_agent`, `refresh_agent`, `list_agents`, `clean_agent`. Format v3 compares encrypted snapshot heads; legacy formats retain peek/diff compatibility. |
| Workspace sync conflicts | [conflicts.rs](agent-core/src/conflicts.rs), [tree_reconcile.rs](agent-core/src/tree_reconcile.rs) | Tree-based last-synced reconciliation, registry/artifacts, and `conflicts keep`. |
| Catch-up Summary | [summary.rs](client/src/summary.rs) | `diff_since_last_session`, `commit_session_marker`, `render_via_summary_tool` (shells out to `FEANORFS_SUMMARY_CMD`, default `feanorfs-llm`, falls back to plain listing). |
| Predictive Hydration | [predictive.rs](client/src/predictive.rs) | `record_access_with_recent`, `prefetch_related` (top-5 siblings, 0.95 decay factor). Triggered from `hydrate` and `cat` CLI arms. |
| Change Watching | [watch.rs](client/src/watch.rs) | Debounced (500ms) filesystem watcher that triggers `do_sync` on changes. |
| Background lifecycle | [service.rs](client/src/cli/service.rs), [hub_service.rs](client/src/cli/hub_service.rs) | Per-workspace and automatic private-hub launchd/Linux user-service/Task Scheduler lifecycle. Worker argv contains only the workspace or protected hub-data path. |
| Secure LAN pairing | [pair.rs](client/src/cli/pair.rs) | Single-use `fnp1` code, mDNS rendezvous, SPAKE2, AEAD invite delivery, key confirmation, attempt/expiry limits, and stable managed-hub endpoint delivery. |
| Off-LAN pairing rendezvous | [routes_pair_relay.rs](server/src/app/routes_pair_relay.rs), [pair.rs](client/src/cli/pair.rs) | Optional bounded public WSS PAKE/AEAD frame relay plus `fnp2`; private-hub reachability is supplied separately by the inner-TLS tunnel route. |
| Opaque inner-TLS relay | [routes_tunnel_relay.rs](server/src/app/routes_tunnel_relay.rs), [tunnel.rs](agent-core/src/tunnel.rs), [hub_service.rs](client/src/cli/hub_service.rs) | Capability-routed WebSocket byte forwarding; original hub CA/SNI and bearer authentication remain end to end. |
| CI, security, and releases | [ci.yml](.github/workflows/ci.yml), [npm-release.yml](.github/workflows/npm-release.yml), [security.yml](.github/workflows/security.yml), [release-plz.yml](.github/workflows/release-plz.yml), [release.yml](.github/workflows/release.yml), [tray-release.yml](.github/workflows/tray-release.yml), [desktop-release.yml](.github/workflows/desktop-release.yml) | Main CI verifies SDK dependency boundaries and packed Node tarballs. App tags release the CLI and optional desktop tray; npm package assembly is manual dry-run only. Cargo-dist owns its generated workflow. |

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
- **Auth middleware**: All routes require `Authorization: Bearer <token>` by default. The hub generates/persists a 64-hex token when none is supplied; `--token`/`--password` rotates it and `--allow-open` is explicit development mode. Comparison uses constant-time equality.
- **mDNS**: Server advertises `_feanorfs._tcp.local.` plus scheme and public CA fingerprint on port 3030 when started with `--mdns`. mDNS never establishes private-CA trust; use the `fnh1` capability.
- **Multi-instance**: `--port` and `--data-dir` flags allow running multiple isolated instances behind a reverse proxy (SaaS deployment model).

### Client Local State ([agent-core/src/state.rs](agent-core/src/state.rs))
- Cache entries map paths to plaintext/encrypted hashes, size, disk/server mtimes, mode, hydration, and deletion metadata.
- Access log entries store local-only co-occurrence weights and timestamps with deterministic bounds and decay.
- Conflict registry and resolution history preserve needs-attention state and explicit choices.
- Session key/value state stores the previous `last_scan` summary baseline.
- Global config: `~/.feanorfs/global.json` stores server URL, optional public hub CA, and either a random OS-credential reference or a protected-file token fallback (cached automatically by `feanorfs start`; hidden `connect` also writes it).
- Workspace config: `.feanorfs/config.json` stores server URL, workspace ID, optional public hub CA, and either a random OS-credential reference or a protected-file E2EE key/token fallback.

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
11. **Shared pre-1.0 versions**: Internal workspace path dependencies use a pre-1.0 range so release-plz can bump all `version.workspace` crates together. These crates remain unpublished; main CI validates their compatibility.

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
# Cross-platform core and native desktop tray
cargo build --workspace --locked

# Core, integration, and native tray tests
cargo test --workspace --all-features --locked
```

### Starting the Blob Hub
```bash
# Same binary as the sync client (recommended)
cargo run --bin feanorfs -- serve --port 3030 --data-dir server-data
cargo run --bin feanorfs -- serve --allow-http --port 3030 --data-dir server-data --token server-secret # reverse proxy/dev only
cargo run --bin feanorfs -- serve --gc-only --data-dir server-data

# Source-only compatibility binary; not a release product
cargo run --bin feanorfs-server
```

### Client CLI Usage
```bash
# Begin: create, pair/join, or resume — then sync + automatic service
cargo run --bin feanorfs -- start ~/projects/app   # first use auto-hosts; later use resumes
cargo run --bin feanorfs -- start --host ~/projects/app # explicit first-machine host
cargo run --bin feanorfs -- start 127.0.0.1:3030 --workspace my-workspace --token "server-pass"
cargo run --bin feanorfs -- start fnr1-...
cargo run --bin feanorfs -- start fnh1-... ~/projects/app
cargo run --bin feanorfs -- pair
cargo run --bin feanorfs -- start fnp1-... ~/projects/app
cargo run --bin feanorfs -- start --local --workspace my-workspace
cargo run --bin feanorfs -- start --no-watch       # sync once after create/join
cargo run --bin feanorfs -- stop ~/projects/app    # stop automatic sync; preserve setup
cargo run --bin feanorfs -- recovery export ~/FeanorFS-recovery.fnrk
cargo run --bin feanorfs -- recovery import ~/FeanorFS-recovery.fnrk ~/projects/app-restored

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
cargo run --bin feanorfs -- conflicts keep --all --local
cargo run --bin feanorfs -- conflicts keep --all --cloud
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
  platform desktop tray products with integrity metadata, not internal crates or compatibility
  binaries already covered by `feanorfs serve`.
- Consumer onboarding is one native installer followed immediately by a tray choice between **Start Mirroring a Folder…**, **Join Another Computer…**, and **Not Now**; `feanorfs start [invite-or-server] [folder]` is the terminal equivalent. Distribution publishes a trusted macOS `.dmg` containing the signed `.pkg`, verified Linux `.deb`/`.rpm`/`.pkg.tar.zst`, and an Authenticode-verified Windows installer `.exe`, with checked script/tar fallbacks. After every artifact passes verification, an interactive install starts the tray with the public `--first-run` hint; an unconfigured tray routes the selected choice into the existing secure menu action, while an existing workspace never re-prompts. Root/headless sessions and `FEANORFS_NO_LAUNCH=1` receive the explicit CLI path instead. Older or unsupported releases report their CLI-only fallback. With no saved connection the first machine automatically provisions a secure private hub, prefers port 3030 without requiring it, and persists a safe available fallback without exposing it in service argv; background persistence and the desktop tray are automatic, while raw `serve`/service supervision remains an advanced diagnostic or dedicated-server surface.
- The cross-platform tray must remain useful before setup: its native folder picker delegates to the same `feanorfs start` path and must not duplicate sync, credential, pairing, or encryption logic.
- Consumer offboarding is reversible: `feanorfs stop [folder]` and the tray remove only automatic sync and recent registration. They preserve working files, encrypted setup, OS credentials, remote snapshots, and private hubs so `start` can resume safely.
- Normal desktop pairing stays inside the tray on both computers: show the short-lived LAN `fnp1` code or copy the long off-LAN `fnp2` capability with its TTL, paste it through masked receiver UI, choose the destination folder, and delegate through bounded stdin to the ordinary `start` engine. Automatically reuse a relay stored by `start --relay`, keep the full invite and cryptography in the CLI child, and never place either capability in argv, environment variables, or logs.
- Treat encryption and local credential handling as product requirements: never place E2EE keys or server tokens in service arguments, logs, pairing discovery metadata, or opaque hub storage.
- Normal desktop diagnostics and repair stay in the tray: project only stable `doctor` check names/statuses into generic native copy, ignore local identifiers/endpoints, require an explicit repair choice, and delegate repair to the ordinary flag-safe `start -- <folder>` lifecycle without duplicating sync, credential, encryption, or conflict policy.
- Update awareness must remain advisory until signed cross-platform elevation is proven: use the official stable release, semantic comparison, bounded HTTPS metadata, and exact canonical tag URL; require an explicit browser-open choice and never download, install, or execute update artifacts from the CLI or tray.
- Workspace recovery is a normal CLI/tray product surface: encrypt the complete portable capability with a user-held passphrase, write kits atomically/private, authenticate before local writes, and re-enter `start`. Keep passphrases and decrypted capabilities out of argv/environment/logs; never imply that an access kit backs up hub blobs.
- New format-v2/v3 setup accepts only the canonical generated 256-bit lowercase-hex E2EE key shape and must validate it before any workspace/global write. Human passphrases remain readable only in legacy format-v1 workspaces so `migrate --rekey` can replace them safely.
- Keep direct dependencies on the latest maintained stable releases compatible with the tested Rust 1.88 MSRV. Document intentional holds instead of adopting pre-releases or silently raising MSRV; SQLx 0.9 currently requires Rust 1.94, constant_time_eq 0.5 requires Rust 1.95, and tokio-tungstenite 0.30 remains held while Axum 0.8.9 selects 0.29 so releases ship one WebSocket protocol stack.
- Relay deployment must reuse `feanorfs serve --relay`, run non-root with a read-only-capable runtime, persist protected identity outside the container layer, terminate public TLS before internal HTTP, omit capability-bearing request paths from logs, and publish SBOM/provenance without claiming that a default hosted relay exists.
- Keep exactly one authoritative open-work list at root `TODO.md`, split by founder and AI ownership with dependencies and acceptance evidence. Remove shipped, speculative, trigger-only, or superseded tasks instead of preserving them as backlog history.

## Open Work

The sole authoritative open-work list is [TODO.md](TODO.md). It separates founder-owned credentials, decisions, infrastructure, and field evidence from AI-owned implementation and verification. Shipped and speculative work does not remain in the TODO.

## Child DOX Index

Direct children own durable crate or automation boundaries; subdirectories inside crates share files at the top level and do not merit separate AGENTS.md.

| Child | Purpose |
| :--- | :--- |
| [.github/](.github/AGENTS.md) | CI, security scanning, dependency automation, release orchestration, and contributor templates. |
| [common/](common/AGENTS.md) | Shared wire models, encrypted snapshot types, and AEAD/legacy crypto. Zero I/O and zero side effects; dependencies stay limited to serialization, hashing, randomness, time, error, and ChaCha20-Poly1305 primitives. |
| [server/](server/AGENTS.md) | Axum blob storage server and SQLite metadata coordinator. Pure transport — never decrypts, never inspects file content. |
| [client/](client/AGENTS.md) | CLI + library crate. Sync engine, watch, summary, predictive; agent ops delegate to agent-core. |
| [bindings/ts/](bindings/ts/) | napi-rs 3 Node bindings, five native platform packages generated with `create-npm-dirs`, deterministic assembly, and resumable npm publication scripts. |
| [feanorfs-ffi/](feanorfs-ffi/) | C ABI and generated header consumed by the Zig example. |
| [tray/](tray/) | macOS/Linux/Windows system-tray companion (`feanorfs-tray`). Shells CLI `--json`; see [tray/README.md](tray/README.md). |
| [agent-core/](agent-core/AGENTS.md) | Embeddable agent SDK: `Runtime`, `Workspace`, local hub, conflict gate. Consumed by client, FFI, and Node bindings. |
| [scripts/](scripts/AGENTS.md) | Platform installers, exact native package assembly, and executable product/release smoke tests. |
