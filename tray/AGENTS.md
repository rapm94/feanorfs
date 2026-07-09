# tray

## Purpose

macOS menu-bar companion for FeanorFS. Shells `feanorfs --json` for status, conflicts, and agent land — never duplicates sync logic.

## Ownership

- Crate: `feanorfs-tray` (`tray/`).
- `src/main.rs` — event loop, menu rebuild, watch child process.
- `src/feanorfs.rs` — subprocess wrappers for CLI commands.
- `src/icons.rs` — state glyphs (idle, syncing, conflict, paused, …).

## Local Contracts

- All sync state comes from `feanorfs --json tray status` (`TrayStatusResult` in `common/src/tray_contract.rs`).
- Action subprocesses use global `--json` (`tray pause`, `conflicts keep`, `agent land`, `sync --no-watch`) so failures surface structured errors.
- Background sync = spawned `feanorfs sync` child in the active workspace directory. Stop the owned watcher before Keep/Land/Sync Now; refuse when an external `feanorfs sync` is already watching (`watching: true`, no tray-owned child).
- `StatusReady` carries `task_generation` + workspace path — stale fetches after workspace switch are ignored.
- Pause = `feanorfs tray pause` (`.feanorfs/paused`); the watch loop in `feanorfs-client` respects it. On pause CLI failure, tray re-reads `.feanorfs/paused` from disk.
- Recent workspaces live in `~/.feanorfs/recent.json` (written by `feanorfs start` and `feanorfs tray register`).

## Work Guidance

- Do not import `feanorfs-client` or `feanorfs-agent-core` — stay a thin shell over the CLI binary.
- Set `FEANORFS_BIN` when testing against a non-`PATH` build.

## Verification

- `cargo build -p feanorfs-tray`
- CI job `tray` on `macos-latest` (CLI smoke)
- Release workflow `build-tray-artifacts` packages attested arm64 and x86_64 macOS archives before creating the GitHub Release.
- Manual: `feanorfs-tray` with `FEANORFS_WORKSPACE` set

## Child DOX Index

No child directories.
