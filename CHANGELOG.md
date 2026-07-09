# Changelog

All notable changes to FeanorFS are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-07-09

### Added

- **Tray MVP (`feanorfs-tray`):** macOS menu-bar companion shelling `feanorfs --json` — state icon, pause/resume, open folder, workspace switcher, conflict keep actions, and agent land shortcuts.
- **Tray CLI:** hidden `feanorfs tray status|pause|resume|recent|activate` commands and `TrayStatusResult` contract in `common/src/tray_contract.rs`.
- **Agent SDK:** embeddable `feanorfs-agent-core` crate with C ABI (`feanorfs-ffi`) and Node.js bindings (`@feanorfs/agent`).
- **Release attestations:** GitHub Artifact Attestations on release archives/installers.

### Changed

- Agent and conflict logic moved from client into `agent-core`; client delegates to the SDK.
- `feanorfs start` registers the workspace in `~/.feanorfs/recent.json` for the tray switcher.
- Tracing warnings route to stderr so `--json` stdout stays clean.

### Fixed

- Sync lock check ignores the current process pid (fixes false "syncing" in tray status).
- Watch loop skips sync while paused and refreshes `watch.pid` on poll so long-running watchers are not marked stale.
- P-4 JSON gaps: `conflicts show --json`, `ConflictKeepResult`, events `mirror_state` snake_case.
- Server: streamed downloads, WAL pragmas, route hardening.

## [0.2.0] - 2026-07-05

### Added

- **Single binary:** install `feanorfs` only — sync client and blob hub (`feanorfs serve`) in one executable. `feanorfs-server` remains an optional legacy server-only release artifact.
- **Agent loop (complete):** `agent check`, `agent land`, `agent refresh`. `agent commit` remains a `land` alias.
- **`conflicts keep`** — resolve with `local`, `cloud`, `both`, or `--file <reconciled>` (`conflicts resolve` aliased).
- **Conflict artifacts** use `.original` / `.local` / `.cloud` suffixes (legacy `.base`/`.ours`/`.theirs` still readable).
- **`feanorfs start`** — one-flow connect + mirror + sync + watch.
- **`feanorfs migrate`** — pull, re-seal AEAD blobs, bump workspace to format v2.
- **`feanorfs events`** — NDJSON event stream for orchestrators.
- **`feanorfs mcp`** — MCP tool server for orchestrators.
- **`conflicts show` / `conflicts open`** — terminal diff and editor compare handoff.
- **Default ignores** — built-in denylist (`target/`, `node_modules/`, …) plus `.feanorfsignore`.
- **Process sync lock** — `.feanorfs/sync.lock` serializes concurrent sync/land passes.
- **Land lock** — `.feanorfs/land.lock` serializes concurrent agent lands.
- **Server GC** — `feanorfs serve --gc-only` and `--gc-interval` periodic blob/tombstone cleanup.
- **Security:** `format_version` in config; format v2 rejects legacy XOR blobs (`LegacyPolicy::Reject`); 64-hex encryption key enforced on v2 workspaces.
- **Spawn:** pre-sync honest base, placeholder refusal, APFS `reflink` with copy fallback, `--no-sync` / `--replace` flags.
- **Placeholders:** lazy placeholders written read-only; hydrate clears the bit.
- **Unicode:** paths normalized to NFC before DB/server.
- **`FEANORFS_AGENT` / `FEANORFS_AGENT_DIR`** env vars on `agent run`.
- **Sync scope docs** — [docs/sync-scope.md](docs/sync-scope.md) records ignore policy and admission criteria.

### Changed

- **CLI consolidation:** `start` absorbs create/join/resume (URL or `fnr1-…` positional). `config --key` replaces `show-key`. `agent status` merges list + check. Bare `feanorfs agent` and `feanorfs conflicts` default to list. `conflicts show --open` replaces `open`. Removed `conflicts resolve`.
- Hidden compatibility aliases: `setup`, `init`, `join`, `attach`, `connect`, `push`, `pull`, `watch`, `show-key`, `workspaces`, `events`, `mcp`, `agent check`, `agent commit`, `conflicts open`, `conflicts history`.
- `init`/`setup`/`attach` preserve legacy semantics (setup-only / link-only, no auto-sync+watch). `start` accepts folder paths, bare `host:port`, and re-link via invite or `--encryption-key`.
- New workspaces created via `start` / hidden `setup` write `format_version: 2`.
- AEAD decrypt failure message: "wrong encryption key for this workspace".
- Agent spawn runs a full sync first and refuses when the folder has pending conflicts.

### Fixed

- **release-plz:** workspace path dependencies now include version requirements so `cargo package` manifest verification succeeds.

### Security

- Format v2 workspaces hard-fail on non-AEAD blob decrypt (closes downgrade path when migrated). Run `feanorfs migrate` on existing v1 workspaces.

## [0.1.0] - 2026-06-23

### Added

- Initial release of FeanorFS, a developer-focused zero-knowledge filesystem sync tool.
- **Client CLI** (`feanorfs`) with subcommands: `init`, `status`, `push`, `pull`, `sync`, `hydrate`, `cat`, `watch`.
- **Server** (`feanorfs-server`) — Axum-based blob storage server with SQLite metadata coordination.
- **End-to-end encryption** via Blake3 XOF symmetric XOR keystream (legacy; superseded by AEAD for new blobs).
- **Content-addressed storage**, **local cache**, **lazy hydration**, **real-time watch**.
- **Agent workspaces**, **library API**, **`--json` output**, **catch-up summary**, **predictive hydration**.

[Unreleased]: https://github.com/rapm94/feanorfs/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/rapm94/feanorfs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/rapm94/feanorfs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rapm94/feanorfs/releases/tag/v0.1.0
