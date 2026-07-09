# FeanorFS Tray

Menu-bar companion for FeanorFS. **Shells `feanorfs --json` only** — no duplicate sync logic.

## Requirements

- Built `feanorfs` on `PATH`, or set `FEANORFS_BIN`
- At least one workspace registered via `feanorfs start` (writes `~/.feanorfs/recent.json`)

## Run

```bash
cargo build -p feanorfs-tray --release
./target/release/feanorfs-tray
```

Or point at a workspace explicitly:

```bash
FEANORFS_WORKSPACE=~/projects/my-app feanorfs-tray
```

## Features (DX-26–28)

| State | Tray icon |
|-------|-----------|
| Up to date | Green dot |
| Has changes | Blue dot |
| Syncing | Blue ring |
| Error | Red dot |
| Offline | Gray dot |
| Needs attention | Orange dot |
| Paused | Yellow dot |

- **Open Folder** — reveals the active workspace in Finder
- **Pause / Resume** — writes `.feanorfs/paused`; the background `feanorfs sync` watcher skips uploads/downloads while paused
- **Needs attention** — per-conflict submenu with plain-language labels and Keep local / cloud / both actions
- **Agents** — `N working · M need attention` with Land shortcuts
- **Switch Workspace** — recent folders from `~/.feanorfs/recent.json`

Status refreshes every 10 seconds; menu actions run on worker threads so the
menu never blocks. The tray spawns and supervises one `feanorfs sync` watcher
per active workspace (skipped when paused or when an external watcher is
already running), and stops it with SIGTERM before killing.

## CLI surface (used by the tray)

```bash
feanorfs --json tray status    # TrayStatusResult
feanorfs tray pause|resume     # TrayPauseResult with --json
feanorfs --json tray recent
feanorfs tray activate -- <path>
```

Untrusted values (paths, agent names) are always passed after `--`.
