# FeanorFS Tray

Desktop system-tray companion for FeanorFS on macOS, Linux, and Windows.
**Shells `feanorfs --json` only** — no duplicate sync logic.

## Requirements

- Built `feanorfs` on `PATH`, or set `FEANORFS_BIN`
- No workspace is required for first launch; configured workspaces are discovered through `~/.feanorfs/recent.json`
- Native Linux packages install `zenity` for masked recovery prompts; source or portable installations need a system `zenity` or `kdialog` installation

## Install

The recommended installers ship the CLI and tray together. On macOS this is a
universal background-only `FeanorFS.app`, Developer ID Application/Installer
signed, notarized, stapled, checksummed, attested, and Keychain-smoked:

[FeanorFS for macOS (.pkg)](https://github.com/rapm94/feanorfs/releases/latest/download/FeanorFS-macOS.pkg)

The package workflow is implemented, but the first credentialed release is
pending. The v0.4.0 tray ZIPs are ad-hoc-signed preview artifacts and are not a
substitute for the notarized package; build from source until the package and
its public verification evidence appear on a release.

The main Unix installer selects that package on macOS. On Linux x86-64/ARM64
it selects the matching verified `.deb` on Debian/Ubuntu or `.rpm` on
Fedora/RHEL, allowing the system package manager to install desktop dependencies
automatically; a checksummed tar bundle remains the custom-prefix fallback:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh | sh
```

The package-specific installer remains available and downloads and verifies
the same artifact:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-macos-installer.sh | sh
```

Windows uses the signed desktop installer:

```powershell
irm https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-windows-installer.ps1 | iex
```

Linux verifies checksums, package identity, architecture, absence of install
scripts, exact payloads, and native dependency metadata. The portable fallback
also checks exact archive contents and runtime linkage. Windows checks the
bundle checksum, exact contents, and Authenticode signature of both
executables. Windows release packaging has no unsigned fallback.

Run `feanorfs start [invite-or-server] [folder]` once, or stay entirely in the
tray. On the first computer it
also provisions the secure private hub at login; on every computer it performs
the initial sync, installs per-workspace automatic sync, and registers the tray
at login. Use **Pair Another Computer…** on the sharing computer and **Join
Another Computer…** on the receiver; no Terminal is required.

## Run

```bash
cargo build -p feanorfs-tray --release
./target/release/feanorfs-tray
```

Or point at a workspace explicitly:

```bash
FEANORFS_WORKSPACE=~/projects/my-app feanorfs-tray
```

## Features (DX-26–28, DX-36–40)

| State | Tray icon |
|-------|-----------|
| Up to date | Green dot |
| Has changes | Blue dot |
| Syncing | Blue ring |
| Error | Red dot |
| Offline | Gray dot |
| Needs attention | Orange dot |
| Paused | Yellow dot |

- **Mirrored Folders** — always-visible folder selector backed by `~/.feanorfs/recent.json`; it lists every followed folder, marks the active folder with a native check, keeps unavailable folders visible but disabled, and immediately scopes status and actions to a new selection
- **Open Selected Folder** — reveals the active folder in the platform file manager
- **Add Folder…** — opens the native folder picker, shows immediate setup activity, delegates to `feanorfs start`, and always ends with a native success or actionable failure dialog; every new folder receives a distinct opaque workspace ID, while existing encrypted setup is preserved and resumed rather than silently replaced
- **Stop Mirroring This Folder…** — asks for confirmation, delegates to `feanorfs stop`, removes automatic sync and the tray entry, and preserves files, encrypted setup, credentials, remote snapshots, and private hubs for a later resume
- **Other Computers → Share Selected Folder…** — shows and copies a one-time sharing code without terminal instructions; the CLI child retains discovery, tunnel configuration, rendezvous, and cryptography, the copied value is cleared when the dialog closes if it is still on the clipboard, and the tray never receives encryption keys, tokens, routes, or the full invite
- **Other Computers → Join a Shared Folder…** — receiver-side masked paste plus native folder picker; sends the one-time code only through bounded stdin to hidden `feanorfs tray join`, which validates and delegates to the ordinary `start` engine without secret argv/environment/logs
- **Recovery → Export Encrypted Recovery Kit… / Restore From Recovery Kit…** — uses native file dialogs plus the operating system's masked password UI (AppleScript, WinForms, or packaged `zenity`/`kdialog`), sends the passphrase only through a bounded stdin pipe, and delegates encryption, validation, initial sync, protected credentials, and service setup to `feanorfs recovery`; the tray never receives the decrypted workspace capability
- **Check System Health…** — runs `feanorfs --json doctor`, retains only check names/statuses, shows generic native results, and offers explicit **Repair Mirroring** through `feanorfs start -- <folder>`; diagnostic details, sync, credentials, encryption, and conflict policy stay in the CLI
- **Check for Updates…** — delegates the HTTPS/semantic/canonical-release validation to `feanorfs --json update`, repeats the exact official tag-page check, and opens that page only after an explicit click; the tray never downloads, installs, or executes update code
- **Pause / Resume** — writes `.feanorfs/paused`; the background `feanorfs sync` watcher skips uploads/downloads while paused
- **Needs attention** — per-conflict submenu with plain-language labels and Keep local / cloud / both actions
- **Agents** — `N working · M need attention` with Land shortcuts

Status refreshes every 10 seconds; menu actions run on worker threads so the
menu never blocks, and unchanged refreshes do not replace or close an open
native menu. Normally the OS-managed per-workspace service owns sync and is
not described as a terminal process.
The tray stops and restarts that service around exclusive actions. For legacy
workspaces without a service it can still supervise one `feanorfs sync` child;
unmanaged terminal watchers are left untouched.

## CLI surface (used by the tray)

```bash
feanorfs --json tray status    # TrayStatusResult
feanorfs tray pause|resume     # TrayPauseResult with --json
feanorfs --json tray recent
feanorfs tray activate -- <path>
feanorfs tray join -- <path>    # pairing capability on bounded stdin
feanorfs --json doctor          # redacted tray projection uses names/statuses only
feanorfs --json update          # typed official stable-release status
feanorfs --json stop -- <path>
feanorfs recovery export --replace --passphrase-stdin -- <kit-path>
feanorfs recovery import --passphrase-stdin -- <kit-path> <folder>
```

The recovery stdin flag is hidden and reserved for the bundled tray. Untrusted
values (paths, agent names) are always passed after `--`; passphrases never
enter arguments or environment variables.
