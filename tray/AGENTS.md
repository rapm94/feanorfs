# tray

## Purpose

`feanorfs-tray`: macOS, Linux, and Windows system-tray companion. Shells `feanorfs --json` for status, lifecycle, pairing, conflicts, recovery, diagnostics, and updates; never duplicates sync, pairing, credential, or cryptography logic.

## Ownership

- `main.rs` (event loop), `model.rs` (state/view model), `menu.rs` (pure menu and action IDs), `actions.rs` (dispatch and background tasks), `dialogs.rs` (native dialogs and copy), `feanorfs.rs` (bounded `CapturedCommand` CLI adapter and pair/join state machines), `icons.rs`, `password_dialog.rs` (masked prompts only).

## Local Contracts

### CLI adapter and status

- `CapturedCommand` polls both pipes without reader threads (Unix `O_NONBLOCK`, Windows `PeekNamedPipe`), reads at most 16 × 4096 bytes per stream per turn, closes both handles on every return, and bounds post-exit draining to one second (or the earlier timeout → `Timeout`). Unix stops the process group; Windows only the direct child. Interactive join pipes do not use this contract.
- CLI discovery: `FEANORFS_BIN`, colocated binary, native package path (`/usr/local/bin/feanorfs` macOS, `/usr/bin/feanorfs` Linux), then `PATH`.
- Recurring refresh is one background `feanorfs --json tray overview`; results carry `task_generation` + workspace path so stale fetches are ignored; an unavailable recent registry keeps the last good list. Activity rows use `TrayStatusResult::activity_lines()` (last-reported counts; idle means up to date with the hub).
- Status failures keep the last good state, show the error visual with a bounded cause, file-preservation reassurance, and **Check System Health…**; never generic "feanorfs failed" copy or Terminal instructions.
- Actions use global `--json` (`tray pause`, `conflicts keep`, `agent land`, `sync --no-watch`) for structured errors.

### Lifecycle and workspaces

- Background sync belongs to the supervisor (`com.feanorfs.agent`) installed by `start`; the tray stops/restarts the managed watcher through it around Keep/Land/Sync Now, refuses unmanaged terminal watchers, and may spawn a legacy `sync` child only for unserviced workspaces. It never starts the hub or reads hub credentials.
- Pause is `feanorfs tray pause` (marker in private state); on CLI failure re-read the marker.
- Recent workspaces live in locked, atomic `~/.feanorfs/recent.json` owned by the CLI. Unavailable entries stay visible as disabled; **Remove Unavailable Folders…** warns about disconnected drives, confirms, and shells `tray forget-unavailable` (registry only).
- One tray per signed-in user: hold `~/.feanorfs/tray-instance.lock` (ignoring `FEANORFS_HOME`) for the process lifetime. Manual secondary launches exit 0 before UI; `--managed` secondaries exit retryably; an external **Quit** leaves a one-shot marker so the next managed attempt exits cleanly. `--version` exits before the lock.
- With no workspace the tray stays alive with **Add Folder…**. Only the public `--first-run` hint may trigger the one-time three-way choice (**Start Mirroring a Folder…**, **Join a Shared Folder…**, **Not Now**), routed into existing actions; existing workspaces, login launches, and **Not Now** never re-prompt. Setup shells `feanorfs start -- <folder>` with captured output, shows busy feedback and a native result, and polling must not hide a failure.
- Setup and workspace switching are mutually exclusive generation-checked tasks; failures keep the current workspace; a canceled picker changes nothing. Completion copy distinguishes pairing, initial sync, workspace registration, and tray registration, and routes retry through `start -- <folder>`.
- **Stop Mirroring This Folder…** confirms, stops any tray-owned legacy watcher, and shells `--json stop -- <folder>`; files, credentials, snapshots, and hubs stay untouched.

### Secrets and dialogs

- **Share Selected Folder…** shows the short `fnp1` code or copies the long `fnp2` capability (never in the dialog body); the hidden CLI tray mode emits only `{event, code, expires_in_seconds}`. Closing the dialog stops the child and clears the clipboard only if it still holds that value.
- **Join a Shared Folder…** takes the capability through masked UI and sends it once over bounded stdin to `tray join -- <folder>`; non-empty joins show the CLI's typed preview and answer `CONFIRM`/`CANCEL` on the same pipe (cancel is not a failure).
- **Recovery → Export/Restore…** uses native file and masked dialogs, zeroizes the passphrase copy, and sends it once over bounded stdin; argv holds only the action, stdin marker, `--`, and paths.
- Masked entry uses AppleScript (macOS), hidden PowerShell/WinForms (Windows), `zenity` with `kdialog` fallback (Linux); other dialogs use `rfd`. Secrets never enter argv, environment, status strings, or logs.

### Conflicts, resolutions, health, updates

- Bulk local/mirror conflict choices show the exact count and consequences, require strong confirmation, and delegate to one CLI bulk operation then sync; never per-path loops or merges.
- **Review Resolutions…** fetches one pending question/result through one CLI review (routine polling never fetches resolver text), preserves the question generation, records answers locally, then publishes; publish failure leaves **Send Saved Answer**. **Open Preserved Versions…** opens only the directory CLI materialize returns.
- **Check System Health…** deserializes only `{ok, checks[{name,status}]}` from `--json doctor`, maps names to fixed labels, makes the tray exclusive (except Open Folder and Quit) while running, and offers **Repair Mirroring** only as an explicit choice delegating to `start -- <folder>`.
- **Check for Updates…** shells `--json update`, requires safe versions and the exact `https://github.com/rapm94/feanorfs/releases/tag/v<latest_version>` URL, and opens the browser only on **Open Release Page**. **Install v<latest>…** delegates to `update --apply` (10-minute bound), refuses an `applied_version` that differs from the advertised one, and never verifies or unpacks archives itself.

### Platform and packaging

- Target features are mandatory: GTK/AppIndicator plus XDG portal/Wayland on Linux, common-controls-v6 on Windows, AppKit on macOS. Never enable muda's optional `libxdo` (incompatible `.so.3`/`.so.4` ABIs). Clipboard access is text-only.
- The Windows tray task uses `InteractiveToken`; never apply it to hub or workspace workers.
- `package.metadata.dist.dist = false`; `tray-release.yml` (macOS) and `desktop-release.yml` (Linux/Windows) own products. `scripts/install.sh` and `scripts/install.ps1` never fall back after failed verification of a listed desktop product.

## Work Guidance

- Do not import `feanorfs-client` or `feanorfs-agent-core`; stay a thin shell over the CLI. Set `FEANORFS_BIN` to test a non-`PATH` build.

## Verification

- `cargo build -p feanorfs-tray`; `cargo test -p feanorfs-tray`.
- CI `tray` and `desktop-tray` jobs run Rust 1.88, Clippy, tests, native release builds, and payload checks on macOS, Linux, and Windows; product smokes are listed in [scripts/AGENTS.md](../scripts/AGENTS.md).

## Child DOX Index

| Child | Scope |
|---|---|
| [assets/](assets/AGENTS.md) | Linux launcher and application-icon assets shipped by native packages and tar bundles. |
