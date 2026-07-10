# FeanorFS roadmap

**Focus:** SDK distribution, legacy-crypto retirement, hosted connectivity, and deferred large-file or operating-system integration work.

Shipped foundations include format-v3 encrypted Merkle snapshots, executable intent, atomic agent land, portable conflicts, `log`/`undo`, opaque reachability manifests, retained-snapshot garbage collection, and the macOS tray.

**Strategy:** Maintain one open-source stack for self-hosted and managed deployments. Optimize for concurrent agent work and background synchronization of uncommitted files.

Shipped agent SDK surfaces: [agent-api.md](agent-api.md), `agent-core/`, `feanorfs-ffi/`, `bindings/ts/`, `examples/sdk-agent-loop.sh`, and `examples/zig-agent/`. The CI `sdk` job checks CLI, Node, and Zig paths.

## CLI vocabulary

| Operation | Primary | Hidden aliases |
|---|---|---|
| Onboard | `feanorfs start [URL\|invite\|folder]` | `setup`, `init`, `join`, `attach`, `connect` |
| Upload, download, or both | `sync --up`, `sync --down`, `sync` | `push`, `pull`, `watch` |
| History | `log`, `undo` | None |
| Agent work | `agent spawn`, `status`, `refresh`, `land` | `agent check`, `agent commit`, `agent list` |
| Conflicts | `conflicts`, `keep`, `show --open` | `conflicts list`, `conflicts open`, `conflicts history` |
| Config and key | `config`, `config --key` | `show-key` |
| Hub | `feanorfs serve` | None |
| Orchestrators | None | `events`, `mcp`, `workspaces` |

## Backlog

### P1: Tray MVP, shipped

| ID | Task |
|---|---|
| DX-26 | Menu-bar app with state icon, pause toggle, folder action, and workspace switcher |
| DX-27 | Needs-attention submenu with plain-language conflict actions |
| DX-28 | Agent presence and land shortcuts |
| P-4 | Tray JSON contracts, conflict JSON, event mirror state, and pause support |

### P2: Agent and sync polish

| ID | Status | Task |
|---|---|---|
| AG-28 | Shipped | Detect agent and folder creation of the same path as a no-base conflict |
| AG-29 | Shipped | Converge after crashes at each immutable-object and head-swap land boundary |
| AG-30 | Shipped | Surface rename versus folder-edit overlap without blocking the independent new path |
| DX-11 | Shipped | Keep non-lazy sync as default and make placeholders explicit |
| DX-14 | Shipped | Use hashes and last-synced trees for direction and conflict identity |
| DX-18 | Shipped | Preserve portable executable intent through tree and worktree round trips |
| DX-19 | Shipped | Report skipped symlinks once and never follow them |
| DX-21 | Shipped | Preserve destination bytes and clean temporary files after failed downloads |
| DX-23 | Shipped | Warn when server state regresses and constrain explicit upload restoration |
| DX-24 | Measured | Keep scanner parallelization deferred while the recorded 10,000-file profile remains 49 ms warm and 48 ms for one change |
| DX-25 | Shipped | Collapse bulk filesystem events into one debounced snapshot pass |
| DX-29 | Shipped | Prune valid nested `CACHEDIR.TAG` directories while exempting the workspace root |
| SEC-6 | Gated | Remove `LegacyPolicy` and XOR decryption after migration evidence shows no format-v1 workspaces |
| GC-7 | Shipped | Use immutable snapshot retention instead of standalone file history |

### P2: Agent SDK distribution and storage

| ID | Status | Task |
|---|---|---|
| SDK-5b | Release-ready | Assemble and verify five native `@feanorfs/agent` packages plus the facade; trusted-tag CI publishes with npm provenance after registry credentials or trusted publishers are configured. No registry publication is claimed yet. |
| SDK-7 | Shipped | `feanorfs-agent-core` uses lock-protected JSON for local cache/conflict state and embedded-hub metadata, with no SQLx or server dependency. Client-owned migration imports and archives existing workspace, agent, and embedded-hub SQLite stores. |

### P2: Hosted connect, blocked on service

| ID | Task | Notes |
|---|---|---|
| CONN-6 | Add account vault, login, client-encrypted keys, workspace-name join, and recovery kit | Requires hosted identity backend |
| CONN-7 | Add workspace rendezvous, network address discovery, NAT traversal, and relay fallback | Build when explicit URL, self-host, and LAN options no longer suffice |

Connect constraints remain fixed: one hub per workspace, no mesh, and one folder mount bound by `.feanorfs/config.json`.

### P3: Deferred

| ID | Task | Trigger |
|---|---|---|
| DX-12 | Add operating-system dataless files through File Provider, Cloud Files, or FUSE | Product demand for integrated placeholders |
| CHUNK-1..4 | Add FastCDC chunks, manifest hashes, server blob references, and reference-based GC | 100 MB cap or large re-upload complaints |

Chunking sketch: use approximately 1 MiB FastCDC chunks, keep files below 4 MiB as one blob, derive per-chunk keys, and preserve the `FileState` JSON shape.

## Suggested order

1. Configure npm trusted publishers for all six packages and let a trusted version tag perform the first registry release.
2. Remove legacy crypto only after migration evidence exists.
3. Build hosted identity and rendezvous when a hosted tier exists.
4. Add dataless files or chunking only after their product triggers occur.

## Key files for open work

| Area | Files |
|---|---|
| SDK distribution | `.github/workflows/`, `bindings/ts/`, `dist-workspace.toml` |
| SDK storage migration | `client/src/migrate_sqlite/`, `agent-core/src/local.rs`, `agent-core/src/hub.rs` |
| Crypto cleanup | `common/src/lib.rs`, `client/src/migrate.rs`, `docs/threat-model.md` |
| Hosted connect | To be designed when service work begins |
