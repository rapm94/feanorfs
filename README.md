# FeanorFS

[![CI](https://github.com/rapm94/feanorfs/actions/workflows/ci.yml/badge.svg)](https://github.com/rapm94/feanorfs/actions/workflows/ci.yml)
[![Security](https://github.com/rapm94/feanorfs/actions/workflows/security.yml/badge.svg)](https://github.com/rapm94/feanorfs/actions/workflows/security.yml)
[![Release](https://github.com/rapm94/feanorfs/actions/workflows/release.yml/badge.svg)](https://github.com/rapm94/feanorfs/actions/workflows/release.yml)

> **Dropbox for your uncommitted code** — end-to-end encrypted, self-host or managed.

FeanorFS synchronizes your working directory through encrypted, content-addressed Merkle snapshots. The server sees opaque hashes, ciphertext sizes, object counts, and access timing, but format-v3 filenames and file contents never leave your machine in plaintext.

It is designed for one specific situation: you write code on more than one machine and want your uncommitted work-in-progress to follow you without thinking about it — like Dropbox, but for the files you have not committed yet. Run `feanorfs start` once and it keeps your working files mirrored across machines. Self-host the server or use a managed instance — same open-source stack either way.

## Scope

FeanorFS mirrors the current contents of a working directory to a blob server. It is **not version control**: no staging, branches, tags, rebase, or merge UI. Its bounded snapshot log supports transport recovery and undo. Use a version control system for project history.

It syncs files on disk (including gitignored/untracked paths — often the point), skips `.git/` and `.feanorfs/`, and blocks common artifact trees (`target/`, `node_modules/`, …) by default. It does not read `.gitignore`. See [docs/sync-scope.md](docs/sync-scope.md).

## Features

**Agent isolation:** run multiple coding agents on one folder without trampling each other. FeanorFS detects overlaps and tells you when two agents — or you and an agent — touched the same file.

**Background sync (Dropbox-like):** your uncommitted work follows you between machines. `feanorfs start` performs the first sync, installs a per-user background service, and returns; sync restarts automatically at login.

- **Zero-knowledge E2EE** — ChaCha20-Poly1305 protects file bytes, filenames, tree layout, conflicts, and snapshots in format v3. Run `feanorfs migrate` on older workspaces.
- **Content-addressed storage** — Blake3-hashed blobs with deduplication and upload integrity checks.
- **Default ignores** — small built-in denylist for high-churn artifacts; optional `.feanorfsignore` for edge cases (does not honor `.gitignore`).
- **One verb onboarding** — on the first computer, `feanorfs start [folder]` creates a secure private hub; on another computer it securely pairs on-LAN (`fnp1-…`), through an off-LAN rendezvous (`fnp2-…`), or joins (`fnr1-…`). The same command syncs, installs automatic background services, and returns.
- **Reversible folder lifecycle** — `feanorfs stop [folder]`, or **Stop Mirroring This Folder…** in the tray, removes automatic sync and the tray entry while preserving ordinary files and encrypted setup for a later `start`.
- **Recoverable workspace list** — moved, deleted, or disconnected folders are labeled unavailable instead of failing when clicked; the tray can explicitly remove only those list entries after warning about offline external drives.
- **Truthful diagnostics and repair** — `feanorfs doctor` verifies encryption, format-v3 trees, automatic workspace/hub services, tray registration, authenticated mirror reachability, and local state; `--json` exposes the same non-secret checks to automation. **Check System Health…** presents redacted native results in the tray and offers explicit repair through the ordinary secure `start` lifecycle instead of sending desktop users to Terminal.
- **Safe release awareness** — `feanorfs update` and the tray's **Check for Updates…** compare the installed semantic version with GitHub's official stable release through a bounded HTTPS-only request. Results must point to the exact matching `github.com/rapm94/feanorfs` tag; FeanorFS never downloads, installs, or executes update code automatically.
- **Native lifecycle** — the private hub and each workspace restart at login through launchd on macOS, the available user service manager on Linux, or Task Scheduler on Windows; no `nohup`, PID files, or manual reboot setup.
- **Native credential protection** — unattended keys and tokens live in macOS Keychain for signed releases, Windows Credential Manager, or Linux Secret Service; config JSON keeps only a random reference, with a private-file fallback for unsigned macOS/source builds or unavailable stores.
- **Native secure transport** — the automatic private hub and advanced `feanorfs serve` path use Rustls HTTPS, a durable private CA, generated bearer authentication, and normal certificate verification. Fresh automatic hubs prefer port 3030 but persist a different available port when it is occupied; their stable CA-bound `.local` name also survives DHCP changes without router reservations.
- **Encrypted hub recovery** — an offline `serve recovery export` bundle preserves a private hub's CA and bearer token using Argon2id + XChaCha20-Poly1305; crash-safe restore keeps existing clients trusted.
- **Encrypted workspace recovery** — `feanorfs recovery export` stores the complete workspace capability in a private `0600` Argon2id + XChaCha20-Poly1305 kit. `recovery import` authenticates it before creating local state, then enters the same `start` flow; the tray provides native Export/Restore actions without putting the passphrase or decrypted capability in argv, environment variables, or logs.
- **Desktop tray** — macOS, Linux, and Windows product installers include the same thin tray; `start` registers it at login while native workspace services remain the only sync owners.
- **Secure LAN pairing** — `feanorfs pair` creates a single-use `fnp1-…` code; **Pair Another Computer…** presents it on the sharing machine and **Join Another Computer…** accepts it on the receiver without Terminal. SPAKE2 authenticates the code and ChaCha20-Poly1305 protects the full invite without putting keys on the hub or in mDNS; `feanorfs start <code> [folder]` remains the CLI equivalent.
- **Secure off-LAN private hubs** — `feanorfs start --relay https://… [folder]` gives an owned private hub an outbound-only opaque route; the same stored relay makes **Pair Another Computer…** emit `fnp2-…`. Pairing remains SPAKE2/AEAD protected, while synchronization crosses the relay as the hub's existing Rustls byte stream. The relay never sees the bearer token, workspace ID, object paths, or file ciphertext inside TLS.
- **Deployable opaque relay** — trusted tags publish a non-root amd64/arm64 OCI image with a read-only runtime, protected persistent identity, authenticated health check, SBOM, and provenance. It runs the same `feanorfs serve --relay` binary behind an operator-owned TLS proxy; see [deploy-relay.md](docs/deploy-relay.md).
- **Single binary** — install `feanorfs` once; `start` owns the normal client/hub lifecycle, `serve` remains the advanced foreground/server path, and `start --local` keeps the non-portable in-process mode.
- **Agent loop** — `spawn` → `status` → `refresh` → `land` → `conflicts keep`. Data isolation, not process sandboxing.
- **Conflict surfacing** — `.original`/`.local`/`.cloud` triples; bare `feanorfs conflicts` lists pending paths.
- **Operational history** — `feanorfs log` inspects reachable snapshots and `feanorfs undo` records a restored tree without rewriting history.
- **Crash-safe migration** — durable client journal and server write fence make format-v3 migration and rekey resumable.
- **Lazy hydration**, **local cache**, **catch-up summary**, **library + `--json` API**.
- **Orchestrator surfaces** — hidden `events` (NDJSON) and `mcp` (MCP protocol + tools).
- **Server GC** — `feanorfs serve --gc-only`; periodic `--gc-interval` while serving.

## Architecture

One binary (`feanorfs`) hides the normal sync-client and private-hub lifecycle behind `start`. `feanorfs serve` remains available for dedicated servers and custom deployments. Local workspaces can use a non-portable in-process hub (`start --local`).

```
┌─────────────────────────────────────────────────────────────┐
│  feanorfs (single install)                                  │
│  ┌─────────────┐    native TLS       ┌──────────────────┐  │
│  │ sync client │ ──────────────────▶ │ managed private │  │
│  │ start/sync  │                    │ hub + SQLite    │  │
│  └─────────────┘                    └──────────────────┘  │
│       │ .feanorfs/local_state.json                          │
│       └── local mode: hub_state.json + blobs/               │
└───────┼─────────────────────────────────────────────────────┘
        │ encrypted objects + compare-and-swap head
        ▼
   (remote hub or embedded LocalHub)
```

For the security analysis, see [docs/threat-model.md](docs/threat-model.md). Architecture details live in the `AGENTS.md` files at the repo root and in each crate.

## Installation

One binary covers sync and the self-hosted hub. Install `feanorfs`; run
`feanorfs serve` for server-only deployments.

### Recommended installer (no Rust required)

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh | sh
```

This is the single Unix install entry point. On macOS it selects the signed,
notarized universal CLI/tray package. On Linux x86-64 and ARM64 it selects a
verified native `.deb` on Debian/Ubuntu or `.rpm` on Fedora/RHEL so the system
package manager installs the tray's desktop dependencies automatically. A
checksummed tar bundle remains the custom-prefix fallback. Older or unsupported
releases fall back to the attested cargo-dist CLI installer and say explicitly
that the tray was not installed.

Windows PowerShell:

```powershell
irm https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-windows-installer.ps1 | iex
```

The Windows installer accepts only the checksummed two-binary desktop bundle
and requires valid Authenticode signatures on both executables. Windows release
packaging fails closed unless Azure Artifact Signing is configured.

### Pre-built binaries (cargo-binstall)

```bash
cargo install cargo-binstall
cargo binstall feanorfs-client   # installs `feanorfs`
```

### macOS with menu-bar app (recommended)

The next credentialed release will publish one universal, Apple-signed
installer at:

[FeanorFS for macOS (.pkg)](https://github.com/rapm94/feanorfs/releases/latest/download/FeanorFS-macOS.pkg)

The package workflow is implemented but its first real Developer ID release is
still gated on Apple credentials. The v0.4.0 tray ZIPs are preview artifacts:
they are ad-hoc signed and Gatekeeper rejects them. Build from source until a
release includes `FeanorFS-macOS.pkg`, its notarization JSON, and verification
evidence, including the signed-build Keychain smoke. Once published, the recommended installer above selects it
automatically. The package-specific Terminal installer remains available and
verifies that same artifact:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-macos-installer.sh | sh
```

The universal package installs `/usr/local/bin/feanorfs` and
`/Applications/FeanorFS.app`. It is Developer ID Application-signed, Developer
ID Installer-signed, notarized, stapled for offline verification, checksummed,
Keychain-smoked, and attested. When no workspace exists, the verified
interactive installer opens the menu-bar app with equal native choices to
start mirroring, join another computer, or continue later. Start/join delegate
to the existing secure tray actions and ultimately `feanorfs start`, which
registers the tray and workspace sync at login. Existing workspaces do not
re-prompt. Root/headless installs and `FEANORFS_NO_LAUNCH=1` leave launch
explicit.

### Linux and Windows desktop tray

Tagged releases build native Linux x86-64/ARM64 `.deb` and `.rpm` packages,
portable tar fallbacks, and a Windows x86-64 desktop bundle. Linux packages are
checksummed and GitHub-attested; their metadata declares GTK 3, Ayatana
AppIndicator 3, libxdo, and XDG desktop portal dependencies, and the installer
rejects unexpected package names, architectures, or install scripts. Before
publication, the exact packages must install cleanly on Debian 13 and Fedora 44,
the CLI must create an idle format-v3 encrypted workspace with private config
and real snapshot objects, and the tray must survive native GTK startup against
that workspace under an isolated Xvfb/D-Bus session. Windows
binaries are Azure Authenticode-signed, checksummed, attested, and
signature-checked again by the installer. Native Windows CI runs the colocated
CLI and tray through first-machine hosting, Task Scheduler persistence, TLS,
redacted Credential Manager storage and unattended reload, doctor, MCP, cleanup,
and reversible stop/resume; the signed release repeats that smoke after
Authenticode verification. The first credentialed Windows
release remains proof pending; there is no unsigned release fallback.

All release artifacts also carry [GitHub Artifact Attestations](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations/using-artifact-attestations-to-establish-provenance-for-builds). To verify before running: `gh attestation verify <artifact> --repo rapm94/feanorfs`. See [SECURITY.md](SECURITY.md#verifying-release-artifacts) for Apple signature, notarization, checksum, and build-from-source verification.

Trusted tags also publish the self-hostable opaque relay image at
`ghcr.io/rapm94/feanorfs-relay:<version>`. It is separate from desktop release
assets and is verified by digest-bound OCI provenance. Deployment instructions
and the required TLS/logging boundaries are in
[docs/deploy-relay.md](docs/deploy-relay.md).

### Node agent SDK

`@feanorfs/agent` has release-ready package assembly for macOS x64/ARM64,
Linux GNU x64/ARM64, and Windows x64. The first provenance-backed npm release is
owned by [F4 and AI-4](TODO.md#f4-enable-the-first-public-node-sdk-release);
application release tags currently ship the CLI and optional tray only. Build
the SDK from `bindings/ts/` or install local tarballs until that gate closes.

CLI-only release installer (advanced):

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-client-installer.sh | sh
```

### From source

```bash
cargo install --path client --bin feanorfs
```

### Build from repository

```bash
git clone https://github.com/rapm94/feanorfs.git
cd feanorfs
cargo build --release
# Binary: target/release/feanorfs  (includes `serve` subcommand)
```

## Quick start

### First computer

```bash
cd /path/to/your/project
feanorfs start
```

Or open the system tray app and choose **Start Mirroring a Folder…**. The native
folder picker invokes the same `feanorfs start` path; the tray contains no
separate sync, credential, or encryption implementation.

That one command creates or reuses `~/.feanorfs/hub-data`, enables native TLS
and bearer authentication, installs the private hub and workspace watcher at
login, performs the initial sync, and registers the desktop tray when installed.
Hub credentials and E2EE keys never appear in service arguments or logs.
Each new folder receives a distinct opaque workspace ID, so adding another
folder cannot silently attach it to the first folder's mirror.

Choose **Pair Another Computer…** in the tray, or run `feanorfs pair`.

### Other computer

Open FeanorFS and choose **Join Another Computer…**, paste the one-time code,
and choose the local destination folder. The receiver tray passes the code over
bounded stdin to the same `start` engine; it never puts the capability in
process arguments, environment variables, or logs.

The terminal equivalent remains:

```bash
feanorfs start fnp1-... ~/projects/my-app
```

The single-use code securely delivers the encrypted workspace capability,
runs the initial sync, and installs background sync. No manual `scp`, `nohup`,
PID file, or reboot setup is part of the normal flow.

Useful variants:

```bash
# Explicitly make this computer the host even when another hub is cached
feanorfs start --host ~/projects/my-app

# Publicly trusted HTTPS also works directly
feanorfs start https://my-server.com --workspace my-workspace --token "server-secret"

# Full fnr1 invite remains available for non-LAN/manual transfer
feanorfs start fnr1-... ~/projects/my-app

# Make this private hub reachable off-LAN, then pair normally from the tray
feanorfs start --relay https://relay.example ~/projects/my-app
feanorfs pair --relay https://relay.example

# After an intentional hub identity rotation, authenticate the replacement
# CA/token while preserving this folder's workspace and E2EE key
feanorfs start fnh1-... ~/projects/my-app

# Local-only embedded hub
feanorfs start --local --workspace my-workspace

# Resume an existing workspace (sync + background service)
feanorfs start
feanorfs start ~/other/project   # resume/create in another folder

# Terminal-only development mode
feanorfs start --foreground
```

Inspect config: `feanorfs config` · full key + invite: `feanorfs config --key`

To stop mirroring without deleting the folder, encrypted setup, remote snapshot,
or private hub, choose **Stop Mirroring This Folder…** in the tray or run:

```bash
feanorfs stop ~/projects/my-app
```

Open the folder and run `feanorfs start` whenever you want to resume.

To keep an offline way back into a workspace, export a passphrase-encrypted kit
from that folder. Restore feeds the authenticated capability through the same
initial sync, credential protection, background-service, and tray path as
`start`:

```bash
cd ~/projects/my-app
feanorfs recovery export ~/FeanorFS-recovery.fnrk
feanorfs recovery import ~/FeanorFS-recovery.fnrk ~/projects/my-app-restored
```

The desktop tray exposes the same flow under **Recovery**. Keep the kit and its
passphrase separately. The kit holds access capability, not file blobs: the hub
must still be reachable. For a self-hosted private hub, also keep the separate
advanced hub-identity recovery bundle and a backup of the opaque hub data.

Dedicated servers, reverse proxies, custom ports/data directories, and hub
recovery remain under the advanced `feanorfs serve` surface; see
[docs/usage.md](docs/usage.md).

An `fnh1` invite used with an already configured folder is a trust refresh, not
a new workspace. FeanorFS verifies HTTPS, the replacement CA, bearer token, and
existing opaque workspace head before changing local settings. A failed probe
leaves the previous connection untouched; the workspace ID, E2EE key, local
files, Merkle refs, and encrypted history are never replaced by this flow.
For a compromised private-hub identity, the matching offline operator command
is `feanorfs serve recovery rotate <new-recovery-bundle> --data-dir <hub-data>`.
It preserves encrypted storage, rotates both CA and token, and requires every
client to authenticate the replacement `fnh1` invite.

For an owned private hub, `feanorfs start --relay https://… [folder]` persists a
random 256-bit route and restarts the existing credential-free hub service with
outbound tunnel workers. The tray then reuses that relay automatically and
copies a single-use `fnp2` capability. SPAKE2, AEAD invite encryption, and key
confirmation remain between the clients. After pairing, the second computer
uses the same relay for the hub's inner Rustls connection, so CA verification
and bearer authentication are unchanged.

A relay can observe IP addresses, timing, the public route/session IDs, and
opaque frame sizes or deny service. It cannot read the pairing secret, invite,
hub token, workspace ID, API paths, or tunneled bytes. `feanorfs serve --relay`
provides the self-hostable relay, but no public default is deployed yet; direct
NAT traversal also remains future work.

### Sync

```bash
feanorfs status                    # read-only diff
feanorfs sync --no-watch           # one-shot bidirectional sync
feanorfs sync                      # sync + watch (default)
feanorfs sync --up --no-watch      # upload only
feanorfs sync --down --lazy        # lazy download
feanorfs hydrate src/main.rs       # materialize a placeholder
feanorfs cat src/main.rs           # print (auto-hydrates)
feanorfs summary                   # what changed since last session
feanorfs log                       # recent snapshot transitions
feanorfs undo snapshot_id          # append a restored snapshot
```

See [docs/usage.md](docs/usage.md) for the full CLI reference. Agent loop demo: [scripts/demo-agent-loop.sh](scripts/demo-agent-loop.sh).

## Security

FeanorFS provides end-to-end encryption using ChaCha20-Poly1305 (AEAD) with per-file keys derived from your encryption key and the file path. The server is zero-knowledge: it cannot read your file contents.

**E2EE is always on.** Every workspace has an encryption password — if you don't provide one, a 64-character CSPRNG-generated key is created automatically. The same E2EE password must be used on all machines sharing a workspace.

**Server authentication** is required by default. `feanorfs serve` generates a
64-hex bearer token, stores it as `data-dir/auth-token` with private
permissions, and reuses it after restart. `--token`/`--password` explicitly
rotates it; `--allow-open` is development-only. Native Rustls HTTPS is also the
default. Private hubs create a stable CA and transmit only its public
certificate through `fnh1`/`fnr1` invites. Public deployments may provide an
ordinary certificate chain with `--tls-cert` and `--tls-key`.

**LAN pairing** is client-to-client and never handled by the hub. mDNS exposes
only protocol version, a public session tag, address, and ephemeral port. A
80-bit single-use code (60 bits remain secret after its public rendezvous tag)
runs through SPAKE2; the resulting key encrypts and
confirms the invite—including its public hub CA—with ChaCha20-Poly1305.
Pairing accepts at most three connections and expires after five minutes by
default. It is LAN-only; subsequent hub traffic uses the stable hostname
derived from the public hub CA and remains independently authenticated by TLS.
Unauthenticated mDNS may be blocked or spoofed for denial of service, but it
cannot replace the invite-pinned CA or bearer token.

**Important limitations** (see [docs/threat-model.md](docs/threat-model.md) for the full analysis):

- Format-v2 and format-v3 workspaces reject non-AEAD blobs. Unmigrated format-v1 workspaces still accept legacy XOR on decrypt. Run `feanorfs migrate`; removal remains gated by [representative field evidence](TODO.md#ai-5-retire-legacy-xor-only-after-field-evidence).
- New encrypted workspaces accept only the generated 64-character lowercase-hex recovery-key shape. Manual human passphrases are rejected before configuration is written because the content-key derivation is not a password-stretching KDF. Historical format-v1 workspaces remain readable for migration; use `feanorfs migrate --rekey` when their key was human-chosen.
- Format-v3 servers do not store file paths. They can still observe ciphertext sizes, object counts, hash equality, retention, and access timing. Legacy formats expose path metadata.
- `--allow-http` disables native TLS and exposes bearer tokens to the network unless a correctly configured TLS reverse proxy or VPN encloses that connection.
- Unattended credentials are stored in the native OS credential store when available. Signed macOS releases use Keychain; unsigned macOS/source builds deliberately use the atomic private-file fallback to avoid code-identity prompts after rebuilds. `.feanorfs/config.json` and `~/.feanorfs/global.json` otherwise contain random references. Treat the logged-in user account as part of the trust boundary.

To report a security vulnerability, see [SECURITY.md](SECURITY.md).

## Configuration

The client stores its configuration in `.feanorfs/config.json`:

```json
{
  "server_url": "https://localhost:3030",
  "workspace_id": "my-workspace",
  "credential_store": "os",
  "credential_id": "fsc1-random-non-secret-reference",
  "tls_ca_pem": "optional public private-hub CA certificate"
}
```

The global server connection is cached in `~/.feanorfs/global.json`:

```json
{
  "server_url": "https://feanorfs-0123456789abcdef.local:3030",
  "credential_store": "os",
  "credential_id": "fsc1-random-non-secret-reference",
  "tls_ca_pem": "optional public private-hub CA certificate"
}
```

All files in the workspace directory are synced, including hidden files and paths git would ignore. `.feanorfs/` and `.git/` are always skipped; common build trees are skipped by default. Details: [docs/sync-scope.md](docs/sync-scope.md).

## Development

```bash
# Build all crates
cargo build

# Run tests
cargo test

# Lint
cargo clippy --all-targets -- -D warnings

# Format check
cargo fmt --check

# License/advisory audit (requires cargo-deny)
cargo install cargo-deny
cargo deny check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow.

## Remaining work

The only authoritative open-work list is [TODO.md](TODO.md). It separates the
founder's credentials, infrastructure, decisions, and field-evidence tasks from
the AI's implementation and verification tasks. Completed and speculative work
is intentionally excluded.

## Project structure

```
feanorfs/
├── common/     # Shared data models + crypto
├── server/     # Hub library (embedded in `feanorfs serve`; optional `feanorfs-server` binary)
├── agent-core/ # SQLite-free embeddable agent SDK + JSON LocalHub
├── bindings/ts/ # @feanorfs/agent napi-rs bindings and native package assembly
├── client/     # `feanorfs` binary: sync, agents, hub, MCP/events
└── docs/       # Threat model, usage, deployment, and API documentation
```

## License

[MIT](LICENSE) © 2026 Raul Puigbó
