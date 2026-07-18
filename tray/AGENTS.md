# tray

## Purpose

macOS, Linux, and Windows system-tray companion for FeanorFS. Shells `feanorfs --json` for status, conflicts, and agent land — never duplicates sync logic.

## Ownership

- Crate: `feanorfs-tray` (`tray/`).
- `src/main.rs` — event loop, menu rebuild, watch child process.
- `src/feanorfs.rs` — subprocess wrappers for CLI commands.
- `src/icons.rs` — state glyphs (idle, syncing, conflict, paused, …).
- `src/password_dialog.rs` — masked native password-prompt adapter; no recovery cryptography.

## Local Contracts

- All sync state comes from `feanorfs --json tray status` (`TrayStatusResult` in `common/src/tray_contract.rs`).
- CLI discovery prefers an explicit `FEANORFS_BIN`, then a colocated binary, then the native package location (`/usr/local/bin/feanorfs` on macOS or `/usr/bin/feanorfs` on Linux), then `PATH`. This order is required for first launch from Finder/LaunchServices, whose `PATH` may omit `/usr/local/bin`.
- Status subprocess failures retain the last good state but set the error visual and surface a bounded cause, explicit file-preservation reassurance, and the native **Check System Health…** recovery path. Never collapse a known failure into generic “feanorfs failed” copy or send a normal desktop user to Terminal.
- Action subprocesses use global `--json` (`tray pause`, `conflicts keep`, `agent land`, `sync --no-watch`) so failures surface structured errors.
- Background sync normally belongs to the per-workspace OS service installed by `feanorfs start`. The tray detects that managed external watcher, stops/restarts the service around Keep/Land/Sync Now, and refuses only unmanaged terminal watchers. It may spawn a legacy `feanorfs sync` child for workspaces without an installed service.
- `StatusReady` carries `task_generation` + workspace path — stale fetches after workspace switch are ignored.
- Pause = `feanorfs tray pause` (`.feanorfs/paused`); the watch loop in `feanorfs-client` respects it. On pause CLI failure, tray re-reads `.feanorfs/paused` from disk.
- Recent workspaces live in locked, atomically replaced `~/.feanorfs/recent.json` (`start`/hidden `tray register` add entries; `stop` removes them and selects the next active workspace).
- Unavailable recent workspaces remain visible as disabled **— unavailable** entries. **Remove Unavailable Folders…** must warn about disconnected external drives, require confirmation, and shell hidden `feanorfs --json tray forget-unavailable`; the CLI alone owns the locked/atomic mutation. Cleanup removes only tray records and must not delete files, `.feanorfs`, credentials, services, hubs, or remote snapshots.
- The global per-user tray login job has no workspace working directory; deleting or moving the first registered workspace must not prevent the tray from starting.
- With no workspace, the tray stays alive and offers **Add Folder…** through `rfd`'s native picker. A verified installer may pass only the public `--first-run` hint; when no configured current/recent workspace resolves, present one native three-way choice: **Start Mirroring a Folder…**, **Join a Shared Folder…**, or **Not Now**. Route the first two into the exact existing menu actions; the choice itself receives no capability or secret. Existing workspaces, login launches, unknown arguments, and **Not Now** must not re-prompt or change state. Setup shells `feanorfs start -- <folder>` with captured output, so secrets remain hidden and the tray never duplicates onboarding or sync logic. Setup must show immediate busy feedback and a native success or actionable failure result; background status polling must not erase a failed add before the user sees it.
- Folder setup and workspace switching are mutually exclusive asynchronous actions. Their completions carry `task_generation`; stale completions are ignored, failures preserve the current workspace, and a canceled picker changes no state.
- **Stop Mirroring This Folder…** requires native confirmation, stops any tray-owned legacy watcher, and shells `feanorfs --json stop -- <folder>`. The CLI owns service removal and locked/atomic recent-state updates; the tray then adopts the next configured workspace. Files, `.feanorfs`, credentials, remote snapshots, and private hubs remain untouched.
- **Other Computers → Share Selected Folder…** presents a short LAN `fnp1` code or copies a long off-LAN `fnp2` capability without opening Terminal or showing terminal instructions. The CLI automatically reuses an opaque relay configured by `start --relay`; the tray never reads or manages the tunnel route. The CLI still owns discovery/rendezvous, clipboard presentation, SPAKE2, AEAD, and invite delivery; its hidden tray mode emits only `{event, code, expires_in_seconds}` over a bounded captured stdout pipe. Long `fnp2` values stay out of the dialog body. The capability never enters argv or logs, the tray never receives the full invite, token, route, or E2EE key, and closing the dialog terminates the pairing child and clears the copied value only if the clipboard still contains it.
- **Other Computers → Join a Shared Folder…** is the receiver-side normal path: accept the pasted `fnp1`/`fnp2` through masked native UI, choose a new or unconfigured folder, and send the capability once through bounded stdin to hidden `feanorfs tray join -- <folder>`. The CLI validates it into the existing zeroizing `PairCode` and delegates to `run_start`; the tray never parses the capability, receives the full invite, or puts a secret in argv/environment/logs.
- **Recovery → Export/Restore…** uses native file and masked-password dialogs, zeroizes the local passphrase copy, and sends it once through the CLI child's bounded stdin. Subprocess arguments contain only the action, hidden stdin marker, `--`, and user-selected paths. `feanorfs recovery` owns KDF/AEAD, kit validation, decrypted capability, initial sync, credentials, services, and recent registration; the tray never receives the decrypted invite or duplicates recovery policy.
- First-machine `feanorfs start [folder]` also provisions the private hub service before registering the tray. The tray remains only the human control surface: it never starts the hub directly, reads hub credentials, or duplicates hub/sync lifecycle logic. **Pair Another Computer…** shares; **Join Another Computer…** receives.
- **Check System Health…** shells global `--json doctor` in the current workspace but deserializes only `{ok, checks[{name,status}]}`; ignore message/action fields because they may contain workspace identifiers or endpoints. Map names to fixed generic labels. While the check runs, make the tray temporarily exclusive except for **Open Folder** and **Quit** so repair cannot race stop/sync/pair/recovery. On required failure, **Repair Mirroring** must be an explicit custom-button choice and delegate to the existing `start -- <folder>` wrapper with the untrusted path after `--`. It may run normal synchronization and reinstall services, but never duplicates diagnosis, changes encryption identity, or resolves conflicts automatically.
- **Check for Updates…** shells only `feanorfs --json update` and treats the result as advisory. Require bounded safe version strings and the exact `https://github.com/rapm94/feanorfs/releases/tag/v<latest_version>` URL before displaying it; only **Open Release Page** may launch the browser. Keep the check temporarily exclusive except for **Open Folder** and **Quit**. Never download, install, execute, or claim signature verification in the tray; platform release gates remain authoritative.
- Target-specific crate features are mandatory: GTK/AppIndicator/libxdo plus XDG portal/Wayland support on Linux, common-controls-v6 on Windows, and native AppKit on macOS. Clipboard access is text-only and clears a pairing code only when the clipboard still contains that exact code.
- The Windows tray Task Scheduler job must use `InteractiveToken` so a running process belongs to the logged-in desktop session and can display its icon; never apply that UI-only logon mode to hub or workspace workers.
- Masked recovery/passcode entry uses only platform facilities: AppleScript's hidden-answer dialog on macOS, hidden PowerShell/WinForms on Windows, and packaged `zenity` with `kdialog` fallback on Linux. File/folder/message dialogs remain on `rfd`. Dialog programs receive static script input and public copy only; secret values come back through bounded captured output and must never enter process arguments, environment variables, status strings, or logs.
- `package.metadata.dist.dist = false` keeps this native UI crate out of cargo-dist. `tray-release.yml` owns the universal signed/notarized macOS package and DMG; `desktop-release.yml` owns checksummed/attested Linux x86-64/ARM64 `.deb`/`.rpm`/`.pkg.tar.zst`/tar products and fail-closed Azure Authenticode-signed Windows x86-64 installer EXEs and bundles.
- `scripts/install.sh` is the primary Unix entry point and selects the trusted macOS package or verified native Linux `.deb`/`.rpm`/`.pkg.tar.zst` when present, with a checked tar fallback. `scripts/install.ps1` verifies the Windows bundle checksum, exact two-file payload, and both Authenticode signatures; the normal download is the signed installer EXE. A listed desktop product must never fall back after failed verification.

## Work Guidance

- Do not import `feanorfs-client` or `feanorfs-agent-core` — stay a thin shell over the CLI binary.
- Set `FEANORFS_BIN` when testing against a non-`PATH` build.

## Verification

- `cargo build -p feanorfs-tray`
- CI jobs `tray` and `desktop-tray` enforce Rust 1.88, Clippy, tests, native release builds, and payload checks across macOS, Linux, and Windows. macOS additionally runs full product smoke coverage for automatic hosting, services, tray, TLS, MCP, encrypted workspace recovery, actual bounded-stdin tray-to-tray join, and off-LAN pairing readiness.
- `tray-release.yml` triggers on `v*` tag push, waits for cargo-dist to publish
  the GitHub Release, verifies the tag resolves to the tagged commit, builds
  both architectures without secrets, then signs, Installer-signs, notarizes,
  staples, Gatekeeper-checks, attests, and uploads one universal package plus
  public verification evidence.
- Manual: `feanorfs-tray` with `FEANORFS_WORKSPACE` set
- Linux release proof: build on native x86-64/ARM64 with GTK/AppIndicator/libxdo, verify exact `.deb`/`.rpm`/`.pkg.tar.zst`/tar payloads and dependency metadata, require complete `ldd`, install/run both architectures in Debian/Fedora, and install/run x86-64 in official Arch. Arch ARM64 has metadata/payload proof plus native Debian/Fedora execution because the official Arch container is x86-64-only.
- Windows release proof: native CI executes the complete Task Scheduler host/workspace/tray lifecycle, doctor, MCP, and stop/resume; the privileged release reruns that product smoke after Azure Authenticode verification. Unsigned binaries must never be published as the desktop product.

## Child DOX Index

| Child | Scope |
|---|---|
| [assets/](assets/AGENTS.md) | Platform-neutral Linux launcher and application-icon assets shipped by native packages and verified tar bundles. |
