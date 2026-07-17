# Usage

Full reference for the FeanorFS CLI (`feanorfs`) — one binary for sync client
and blob hub (`feanorfs serve`).

## Install

macOS and Linux use one platform-aware installer:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh | sh
```

It installs the signed/notarized universal package on macOS. On Linux x86-64
or ARM64 it prefers a checksummed native `.deb` for Debian/Ubuntu or `.rpm` for
Fedora/RHEL, letting `apt`, `dnf`, or `yum` resolve the tray dependencies. The
installer verifies package identity and architecture and rejects embedded
install scripts. A checked tar bundle is retained for custom install prefixes;
older or unsupported releases fall back explicitly to the CLI-only installer.

Windows uses the checksummed, Authenticode-gated desktop installer:

```powershell
irm https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-windows-installer.ps1 | iex
```

Both paths install the CLI and tray together when a trusted desktop product is
available. After verification succeeds, an interactive desktop install opens
the tray with equal native choices to **Start Mirroring a Folder…**, **Join
Another Computer…**, or continue later. Both setup paths reuse the existing
secure tray actions without opening Terminal. Existing workspaces never
re-prompt. Root/headless sessions and
`FEANORFS_NO_LAUNCH=1` skip launch and print the CLI setup command instead.
The tray delegates its folder picker to the same `feanorfs start` engine.

## Normal setup

On the first computer, install FeanorFS, choose **Start Mirroring a Folder…**,
and select the project. On another computer, choose **Join Another
Computer…**, paste the one-time capability, and select the destination folder.
The tray delegates both flows to the same secure CLI engine.
When something is unhealthy, choose **Check System Health…**. The tray reads
only check names and statuses from `feanorfs --json doctor`, maps them to
generic human labels, and discards diagnostic messages that may contain local
workspace identifiers or endpoints. **Repair Mirroring** is explicit and
delegates to the existing `feanorfs start -- <folder>` lifecycle; it reuses the
workspace's encryption and conflict safeguards while normal synchronization
and service repair remain CLI-owned.

With no saved workspace or hub, `start` creates a private network hub in
`~/.feanorfs/hub-data`, enables native TLS and bearer authentication, installs
the hub and workspace watcher as per-user login services, performs the initial
sync, and registers the desktop tray when installed. Use **Pair Another
Computer…** in the tray to display the one-time code in a native dialog without
opening Terminal (or run `feanorfs pair`). On the other computer choose **Join
Another Computer…**, paste the code, and select the destination folder. Both
sides stay in the tray; the receiving capability crosses only bounded stdin
into the ordinary `start` path.

The terminal equivalents are:

```bash
feanorfs start ~/projects/my-app
feanorfs start fnp1-… ~/projects/my-app
```

The normal path does not require Terminal, `serve`, `scp`, `nohup`, PID files,
service commands, or credentials/capabilities in process arguments. Use `start --host` to explicitly
make this computer the private hub when a different saved hub exists.

## What FeanorFS is (and isn't)

FeanorFS is a working-directory mirror for developers who use more than one machine — think **Dropbox for the folder you are actively working in**, not for your git history. It keeps current files in sync across machines, including paths that are not in version control.

**It is not version control.** It has append-only recovery history, but no branches,
commits, or content-merge UI. Use a VCS for intentional history and collaboration.

## Dedicated server and advanced self-hosting

### Start the secure blob hub

```bash
feanorfs serve
feanorfs serve --gc-only --data-dir server-data
feanorfs serve --mdns
```

The hub listens on `0.0.0.0:3030` with native Rustls HTTPS by default. For a
private/self-hosted hub it creates a durable CA, refreshes a leaf certificate
for current network interfaces at startup, and prints an `fnh1-…` secure hub
invite. The first client consumes that capability with one `start` command.

The data directory contains:

```text
server-data/
├── auth-token     # generated hub bearer token (0600 on Unix)
├── db.sqlite      # metadata database
├── blobs/         # content-addressed ciphertext blobs
└── tls/           # private CA + refreshed server certificate (0700/0600 on Unix)
```

Back up the complete data directory for ciphertext and metadata. Separately,
create an encrypted identity bundle so a restore keeps the same private CA and
bearer token—existing clients then retain trust without receiving new invites:

```bash
# Stop the hub first; the command refuses an active data directory.
feanorfs serve recovery export ~/Backups/feanorfs-hub.recovery \
  --data-dir server-data

# After restoring db.sqlite and blobs/ (or onto a replacement host):
feanorfs serve recovery import ~/Backups/feanorfs-hub.recovery \
  --data-dir server-data
```

The passphrase is read from the terminal, never argv or an environment
variable. The bundle uses Argon2id plus XChaCha20-Poly1305, is written
atomically with `0600` permissions on Unix, and contains no leaf certificate;
the next `serve` regenerates the leaf for the replacement machine. Import
validates the CA/key pair and leaves a durable startup fence if interrupted;
rerun the same import to resume. `--replace` is required to overwrite a
different existing hub identity.

After a suspected CA-key or bearer-token compromise, rotate both while the hub
is stopped. Rotation preserves `db.sqlite`, blobs, opaque heads, manifests, and
all E2EE ciphertext, but intentionally invalidates every old client connection:

```bash
feanorfs serve recovery rotate ~/Backups/feanorfs-hub-rotated.recovery \
  --data-dir server-data
```

The command generates a fresh private CA and 256-bit token, requires a new
Argon2id/XChaCha20-Poly1305 recovery bundle to be written successfully first,
requires that backup to live outside the hub data directory, then fences the
multi-file identity replacement so the same command and bundle path can resume
after interruption. Restart the hub to emit its replacement
`fnh1-…` invite. On every existing client, run:

```bash
feanorfs start fnh1-… /path/to/existing/folder
```

The client authenticates the replacement HTTPS CA/token and existing opaque
workspace head before changing connection trust. Its workspace ID, E2EE key,
files, refs, and encrypted history remain unchanged. Do not restore the old
identity after compromise; retain the new recovery bundle and its passphrase in
separate secure locations.

| Flag | Description | Default |
|---|---|---|
| `--token <TOKEN>` | Rotate/set the persisted bearer token. `--password` is an alias. | generated 64-hex token |
| `--allow-open` | Disable bearer authentication | off; development only |
| `--allow-http` | Disable native TLS | off; reverse proxy/development only |
| `--tls-cert`, `--tls-key` | Use a supplied PEM certificate chain and private key | auto-generated private CA |
| `--tls-ca` | Private CA certificate to include in hub/workspace invites | none for public CA chains |
| `--public-url` | URL embedded in the `fnh1` hub invite | first non-loopback IPv4 address |
| `--show-invite` | Expose the secret-bearing hub invite when stdout is redirected | off |
| `--port <PORT>` | Port to listen on. Use different ports for multi-instance deployments. | `3030` |
| `--data-dir <DIR>` | Data directory for SQLite DB and blob storage. Each instance should have its own. | `./server-data` |
| `--mdns` | Enable mDNS service advertisement for LAN discovery | off |
| `--relay` | Enable public opaque routes for `fnp2` pairing and inner-TLS private-hub tunnels (`--pair-relay` compatibility alias) | off |
| `--gc-only` | Run blob/tombstone GC once and exit (no HTTP listener) | off |
| `--gc-interval <SECS>` | Periodic GC while serving (`feanorfs serve` only) | off |

All flags can also be set via environment variables: `FEANORFS_TOKEN`, `FEANORFS_PORT`, `FEANORFS_DATA_DIR`.

### Internet deployment

Native TLS can use a publicly trusted certificate directly:

```bash
feanorfs serve \
  --tls-cert /etc/letsencrypt/live/sync.example.com/fullchain.pem \
  --tls-key /etc/letsencrypt/live/sync.example.com/privkey.pem \
  --public-url https://sync.example.com
```

A TLS-terminating proxy remains supported, but disabling native TLS must be
explicit and the HTTP listener must not be exposed directly:

```bash
feanorfs serve --allow-http
caddy reverse-proxy localhost:3030
```

mDNS is off by default — it's only useful on LAN and can't cross routers.

### Multi-instance deployment (SaaS-ready)

```bash
feanorfs serve --port 3001 --data-dir /data/alice
feanorfs serve --port 3002 --data-dir /data/bob
```

Caddy or another gateway can route subdomains to private `--allow-http` ports:
```
alice.feanorfs.app { reverse_proxy localhost:3001 }
bob.feanorfs.app   { reverse_proxy localhost:3002 }
```

Each instance is fully isolated: separate SQLite DB, separate blob storage, separate auth token. This is the deployment model for the managed SaaS — same binary, no code changes needed.

### LAN deployment

For local-only setups, `serve` prints a secure `fnh1` capability containing the
URL, bearer token, and public CA certificate. Paste it into the first client:

```bash
feanorfs start fnh1-… ~/projects/my-app
```

mDNS advertises only the HTTPS endpoint and public CA fingerprint. Because mDNS
is unauthenticated, `start --lan` refuses private-CA trust-on-first-use and asks
for the `fnh1` capability. Publicly trusted HTTPS hubs can still be discovered.
Automatic private hubs advertise a CA-derived `feanorfs-….local` hostname whose
addresses track interface and DHCP changes. The capability-pinned public CA,
not mDNS, remains the trust anchor.

Log verbosity can be tuned via `RUST_LOG` (see [Environment](#environment) below).

## Client

### Visible commands

| Command | Purpose |
|---|---|
| `start` | Create, join, or resume a workspace — then sync and enable automatic background sync |
| `stop` | Stop mirroring a folder and remove it from the tray while preserving files and encrypted setup |
| `sync` | Upload/download changes; enters watch mode by default |
| `status` | Read-only diff vs the mirror |
| `hydrate` / `cat` | Materialize lazy placeholders / print a file |
| `summary` | What changed since your last session |
| `config` | Inspect workspace config (`--key` for full key + invite) |
| `recovery` | Export or restore a passphrase-encrypted workspace access kit |
| `service` | Inspect or repair automatic per-workspace background sync |
| `pair` | Share this configured workspace with another computer using a single-use LAN code |
| `doctor` | Troubleshoot lifecycle/config or collect privacy-safe migration evidence |
| `update` | Check the official stable release without downloading or installing it |
| `serve` | Run a blob hub |
| `migrate` | Upgrade legacy workspaces to format v3 encrypted snapshots |
| `agent` | Isolated agent workspaces (`status`, `spawn`, `land`, …) |
| `conflicts` | List and resolve sync conflicts (`keep`, `show`) |

Hidden aliases remain for scripts: `setup`, `init`, `join`, `attach`, `connect`, `push`, `pull`, `watch`, `show-key`, `workspaces`, `prune-ignored`, `events`, `mcp`, `agent check`, `agent commit`, `conflicts open`, `conflicts history`.

### `start` — create, join, or resume (recommended entry point)

```bash
feanorfs start                                    # resume existing workspace
feanorfs start ~/projects/my-app                  # first use: host privately; later: resume
feanorfs start --host ~/projects/my-app           # explicitly host on this computer
feanorfs start --relay https://relay.example ~/projects/my-app # private hub reachable off-LAN
feanorfs start fnh1-… ~/projects/new-app          # create from secure hub invite
feanorfs start fnh1-… ~/projects/existing-app     # refresh trust after hub identity rotation
feanorfs start https://my-server.com:3030         # create on server
feanorfs start 127.0.0.1:3030                     # unambiguous bare host:port (https:// added)
feanorfs start fnr1-…                             # join from invite
feanorfs start fnp1-… ~/projects/my-app           # secure LAN pair + join + run at login
feanorfs start fnp2-… ~/projects/my-app           # secure off-LAN pair through embedded relay URL
feanorfs start fnr1-… ~/projects/my-app           # join into folder + run at login
feanorfs start --local                            # embedded local hub
feanorfs start --local --workspace existing --encryption-key <KEY>
feanorfs start --lan                              # mDNS discovery + create
feanorfs start --no-watch                         # sync once, no watch loop
feanorfs start --foreground                       # keep watcher attached to terminal
```

| Flag / positional | Description |
|---|---|
| `target` (first positional) | Server URL, unambiguous `localhost`/IP/dotted `host:port`, `fnh1-…` hub invite, `fnp1-…` LAN or `fnp2-…` off-LAN pairing capability, `fnr1-…` workspace invite, or folder path. Use `https://` for a custom single-label server name. |
| `folder` (second positional) | Destination folder when the target is a server, pairing code, or invite (default: current directory) |
| `--workspace`, `-w` | Explicit workspace ID for advanced/manual setup; new mirrors receive an opaque random `fsw1-…` ID when omitted |
| `--encryption-key` | Manual re-link with the existing 64-character lowercase-hex recovery key (requires `--workspace`; human passphrases are rejected before configuration is written) |
| `--server-token`, `--token` | Server access token |
| `--lan` | Discover server via mDNS |
| `--local` | Embedded in-process hub (no remote server) |
| `--host` | Create/reuse this computer's secure private hub and run it automatically at login |
| `--relay <HTTPS-URL>` | Create/reuse this computer's private hub and persist an outbound opaque relay route (`FEANORFS_RELAY`) |
| `--no-watch` | Sync once and exit (no watch loop) |
| `--foreground` | Keep the watcher attached to the terminal instead of installing a user service |

Hidden `init` / `setup` / `attach` / `join` remain for scripts: they configure only. Use `feanorfs start` for the full onboarding flow: initial sync, automatic service, and the desktop tray when installed.

The automatic private hub prefers port 3030. On a fresh profile, if another
application already owns that port, `start` scans a bounded stable fallback
range, selects an available port, and persists it in the protected hub data directory; pairing, mDNS, the tray,
diagnostics, and login restarts reuse that endpoint automatically. Existing
hubs retain their historical port. Explicit `feanorfs serve --port` behavior
is unchanged.

Each implicitly created folder receives its own opaque random workspace ID.
This prevents a second `feanorfs start ~/another-folder` from accidentally
joining the first folder's mirror. Pair codes and full invites preserve the
existing workspace ID; `--workspace` is reserved for explicit/manual setups.

When the destination folder is already configured, an `fnh1-…` target refreshes
only its hub URL, bearer token, and TLS trust. The command requires HTTPS and an
authenticated token, verifies the replacement CA and existing opaque workspace
head before persisting anything, then preserves the workspace ID, E2EE key,
local files, Merkle refs, and encrypted history. If authentication fails, the
old connection remains intact. This is the client re-pairing path after an
intentional hub identity rotation; offline server-side rotation remains an
advanced operation that must preserve the hub database and blobs.

By default, `start` installs one credential-free service command per workspace.
The service receives only the canonical folder path and reads the protected
workspace configuration itself. Signed macOS releases use Keychain; Windows
uses Credential Manager and Linux uses Secret Service when available. JSON
then contains only a random credential reference. Unsigned macOS/source builds
or unavailable stores retain the atomic private-file fallback (`0600` on Unix).
Set `FEANORFS_CREDENTIAL_STORE=file` only for an intentional headless fallback.
Services use launchd on macOS, the native user service manager on Linux, and
Task Scheduler on Windows.

The first-machine path also installs one global private-hub login service. Its
command contains only the protected hub data-directory path. It generates or
reads the bearer token and TLS material inside that directory, runs periodic
retained-snapshot GC, and never receives credentials, capabilities, or recovery
passphrases through argv or the environment. The host workspace uses loopback;
exported invites use the managed hub's stable CA-bound `.local` hostname after
verifying that the public CA matches the durable CA on disk. Existing numeric
LAN configurations migrate only after an authenticated, CA-verified probe.

### `stop` — stop mirroring without deleting the folder

```bash
feanorfs stop                       # current folder
feanorfs stop ~/projects/my-app     # explicit folder
feanorfs --json stop ~/projects/my-app
```

`stop` removes the workspace's automatic sync service and tray registration,
then selects the next recent workspace in the tray. It preserves working files,
`.feanorfs` encrypted setup, OS-stored credentials, remote encrypted snapshots,
and any private hub that other folders or computers may still use. Open the
folder and run `feanorfs start` to resume. If an unmanaged `feanorfs sync`
process is running in a terminal, `stop` refuses until that process exits rather
than claiming that mirroring stopped while it is still active.

If a listed folder was moved, deleted, or is on a disconnected drive, the tray
shows it as unavailable and disables switching to it. **Remove Unavailable
Folders…** asks for confirmation—so a temporarily disconnected drive can be
reconnected instead—then removes only those tray-list records. It does not
delete files, encrypted setup, credentials, services, hub data, or remote
snapshots.

### `recovery` — encrypted workspace access backup and restore

From a mirrored folder:

```bash
feanorfs recovery export ~/FeanorFS-recovery.fnrk
feanorfs recovery export --replace ~/FeanorFS-recovery.fnrk
```

Export prompts twice for a passphrase of at least 12 characters. The resulting
versioned kit contains the full portable workspace capability—server location,
workspace ID, bearer token when present, E2EE key, public private-hub CA, and
optional opaque relay route—only as authenticated ciphertext. Argon2id (64 MiB,
three iterations, one lane) derives a key for XChaCha20-Poly1305. Unix writes
are atomic and `0600`; an existing destination is refused unless `--replace`
is explicit.

On another computer:

```bash
feanorfs recovery import ~/FeanorFS-recovery.fnrk ~/projects/my-app
```

Import prompts once, decrypts and validates in-process, and makes no workspace
or global configuration write when the passphrase is wrong, the kit is
modified, or its E2EE key is invalid. A successful import delegates to the
ordinary `start` path: initial sync, protected credential storage, automatic
workspace service, and tray registration all remain unified. `--no-watch` and
`--foreground` retain their normal advanced meanings.

The tray's **Recovery → Export Encrypted Recovery Kit…** and **Restore From
Recovery Kit…** actions use native file and masked-passphrase dialogs. The
passphrase crosses only a bounded local stdin pipe to the CLI child; neither it
nor the decrypted capability appears in process arguments, environment
variables, or logs. The tray owns no cryptography.

A workspace kit is an access backup, not a copy of file blobs. The hub must
still be reachable and retain the encrypted snapshot. A self-hosted deployment
also needs an opaque hub-data backup and, separately, `feanorfs serve recovery
export` for its private CA and bearer-token identity. Losing the workspace-kit
passphrase is unrecoverable without another configured client or invite.

### `service` — automatic sync lifecycle

```bash
feanorfs service status [folder]
feanorfs service install [folder]
feanorfs service stop [folder]
feanorfs service start [folder]
feanorfs service uninstall [folder]
```

These are recovery and diagnostics commands; normal setup and offboarding use
`feanorfs start` and `feanorfs stop`. Advanced `service uninstall` removes
automatic startup without changing tray registration or deleting the workspace,
encryption key, or mirrored files.

**Machine A — create and host privately:**
```bash
feanorfs start ~/projects/my-project --workspace my-project
# → fnr1-… invite + encryption key on clipboard
```

**Machine B — join:**
```bash
feanorfs start fnr1-...
```

Interactive setup prints a `fnr1-…` workspace invite. Redirected output hides
recovery keys and capability invites; use explicit `config --key` to export
them. Local-hub workspaces (`--local`) are not portable via invite—share the
embedded data through `feanorfs serve --data-dir .feanorfs/hub-data` first.

### `pair` — add another computer on this LAN

On an already configured and synchronized computer:

```bash
feanorfs pair                    # five-minute, single-use code
feanorfs pair --expires 120      # 30–900 seconds
```

The command copies an `fnp1-…` code and waits. On the other computer, choose
**Join Another Computer…** in the tray, paste the code, and choose a folder.
The terminal equivalent is:

```bash
feanorfs start fnp1-… ~/projects/my-app
```

The second command discovers the matching ephemeral session over mDNS,
authenticates the code with SPAKE2, decrypts the full `fnr1` invite (including
its public hub CA) with ChaCha20-Poly1305, performs the initial sync, installs background sync, and
returns. mDNS never contains the pairing secret, server token, E2EE key,
workspace ID, or hub URL. A code works once, permits at most three online
attempts, and cannot cross routed networks; configure an owned private hub with
`start --relay`, use `pair --relay` as the combined configure-and-pair shortcut,
or transfer a full invite outside one LAN.

When this computer owns the automatic private hub, the pairing payload uses its
stable CA-bound `.local` hostname. The hub advertises current interface
addresses automatically, so moving a laptop or receiving a new DHCP lease does
not change the invite, CA, or bearer token. Custom loopback hubs retain a
connection-local IP fallback.

### Off-LAN pairing and private-hub transport

A public relay enables both bounded pairing rendezvous and opaque inner-TLS
tunnels. It can run independently from the hub:

```bash
feanorfs serve --relay --public-url https://relay.example.com
```

On a computer that owns an automatic private hub, configure it through the same
consumer entry point:

```bash
feanorfs start --relay https://relay.example.com ~/projects/my-app
```

This generates a random 256-bit route, stores it in protected workspace/global
config plus atomic `0600` hub-local state, and
restarts the credential-free hub login service. The hub keeps four outbound WSS
offers available. **Pair Another Computer…** and `feanorfs pair` then reuse the
stored relay automatically; `pair --relay <URL>` remains a one-step way to
configure it while opening a pairing session.

The command and tray emit an `fnp2-…` capability. It contains a public 128-bit
session ID, relay URL, and an 80-bit single-use secret; the long value stays in
the clipboard in the tray instead of filling the native dialog. On the other
computer the ordinary entry point is unchanged:

```bash
feanorfs start fnp2-… ~/projects/my-app
```

Both clients connect outbound over WSS. The relay keeps at most 1,024 pending
offers for fifteen minutes, forwards at most eight 16-KiB binary frames during a
30-second exchange, and stores no payload. It sees only the public session ID,
network metadata, timing, and opaque frame sizes. SPAKE2 prevents an offline
test of the pairing secret; ChaCha20-Poly1305 and explicit key confirmation
protect the full invite.

For synchronization, the remote client binds an ephemeral loopback bridge while
retaining the private hub's CA-bound hostname as TLS SNI. The relay forwards
only binary chunks from that inner Rustls stream. It allows at most 4,096
pending host offers, eight per route, 1,024 active tunnels, 64-KiB frames,
16 GiB per connection, and 24 hours per connection. Readiness uses WebSocket
control frames before the TLS ClientHello is consumed, so a startup race retries
without weakening TLS.

The tunnel route is an unguessable reachability capability, not hub
authentication. A relay or anyone who learns it can deny service and observe
IP addresses, timing, connection counts, and sizes. They still cannot complete
the inner TLS/bearer handshake, read or modify accepted traffic, or learn API
paths, workspace IDs, object names, tokens, or file ciphertext. There is no
deployed default relay yet, and direct NAT traversal remains separate work.
For the non-root, read-only-capable OCI deployment, reverse-proxy logging
constraints, and provenance verification, see
[Deploy the opaque relay](deploy-relay.md).

### `config` — show configuration

```bash
feanorfs config
feanorfs config --key    # full E2EE key + invite, copied to clipboard
```

Prints global connection (`~/.feanorfs/global.json`) and workspace config (`.feanorfs/config.json`). Default output truncates the key; `--key` shows the full value.

### `doctor` — diagnose connection issues

```bash
feanorfs doctor
feanorfs --json doctor
feanorfs doctor --migration-report
feanorfs --json doctor --migration-report
```

Desktop users can run the same lifecycle checks from **Check System Health…**
in the tray. Healthy and warning results are read-only. When a required check
fails, the dialog can explicitly retry the existing secure `start` lifecycle;
FeanorFS never resolves conflicts automatically.

Runs non-destructive health checks for workspace/global config, E2EE,
format-v3 encrypted Merkle snapshots, the automatic workspace service, tray
registration, authenticated mirror reachability, local sync state, and—when
this computer owns the private hub—the hub login service. It therefore catches
a workspace that is reachable but no longer syncing automatically, or a hub
that works only because a manual process happens to be running.

Human output states what failed, what was preserved, and the next recovery
command. `--json` returns `{ "ok": boolean, "checks": [...] }`; each check has
a stable `name`, `status` (`ok`, `info`, `warning`, or `failure`), `message`, and
optional `action`. No key, bearer token, invite, or recovery secret is emitted.

`--migration-report` is a separate, offline evidence mode for the gated
[legacy-format retirement task](../TODO.md#ai-5-retire-legacy-xor-only-after-field-evidence). It reads only
`format_version` from the current and recent workspace config files,
deduplicates folders, and reports aggregate v1/v2/v3, unsupported, and
unreadable-or-missing counts. It does not open OS credentials, contact a hub,
or emit paths, labels, workspace IDs, endpoints, credential references, relay
routes, keys, tokens, or capabilities. A profile is marked ready only when it
contains at least one workspace, the recent-workspace registry is readable,
and every entry is a readable supported v2/v3 config. This local result is
field evidence, not permission by itself to remove legacy decryption. JSON
includes `"report_version": 1` so collectors can reject incompatible schemas.

### `update` — check the official stable release

```bash
feanorfs update
feanorfs --json update
```

The command makes one bounded, eight-second, HTTPS-only, no-redirect request to GitHub's
official latest-release API. It caps the response at 64 KiB, parses versions
with the standard `semver` crate, rejects drafts, prereleases, malformed tags,
and every release URL except the exact matching
`github.com/rapm94/feanorfs/releases/tag/v…` page. JSON returns `status`
(`up_to_date`, `update_available`, or `development_build`), current/latest
versions, and the validated public release URL.

The tray's **Check for Updates…** delegates to this command and repeats the
exact URL check before offering **Open Release Page**. Neither surface
downloads, installs, or executes an artifact. Package signatures, checksums,
attestations, and the platform installers remain the release trust boundary.

### `status` — show local vs. remote differences

```bash
feanorfs status
```

Read-only scan + server diff. Does not modify files.

### `sync` — upload, download, watch

```bash
feanorfs sync [--lazy] [--no-watch]
feanorfs sync --up [--no-watch]      # upload only
feanorfs sync --down [--lazy]        # download only
```

| Flag | Description |
|---|---|
| `--up` / `--down` | One direction only (replaces hidden `push` / `pull`) |
| `--lazy` | Metadata only; 0-byte placeholders |
| `--no-watch` | Single pass and exit (scripts/CI) |

By default, `sync` enters real-time watch after the initial pass. Debounced 500 ms.

### `hydrate` / `cat`

```bash
feanorfs hydrate [PATH]     # materialize lazy placeholder(s)
feanorfs cat <PATH>         # print file (auto-hydrates)
```

### `summary` — session catch-up diff

```bash
feanorfs summary [--summarize] [--no-remember]
```

| Flag | Description |
|---|---|
| `--summarize` | Pipe paths/metadata to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`) |
| `--no-remember` | Do not update the session baseline |

`FEANORFS_SUMMARY_CMD` must be a path to an executable file (e.g. `/usr/local/bin/feanorfs-llm`). It is invoked directly — not interpreted by a shell — so arguments like `--model` must be wrapped in a shell script.

### Upgrade to format v3 with `migrate`

```bash
feanorfs migrate [--rekey]
```

Re-seals legacy blobs with authenticated encryption, creates the initial snapshot tree, sets the workspace head, and bumps `format_version` to 3. A durable migration journal and server fence make retries safe after interruption. Run the same command again to resume. Use `--rekey` for a historical human-chosen key; it creates a canonical 256-bit replacement. Land or clean agent workspaces before using `--rekey`.

### Inspect and restore snapshots with `log` and `undo`

```bash
feanorfs log [--limit 20]
feanorfs undo snapshot_id
```

`log` prints the short ID, age, author, changed-path count, and message. Add `--json` for full IDs and structured paths. `undo` accepts a reachable full ID or an unambiguous prefix with at least eight characters, then records the restored tree as a new snapshot.

### Global flags

| Flag | Description |
|---|---|
| `--json` | Structured JSON. `status` includes `mirror_state` for tray clients. |

### `agent` — isolated workspace copies

```bash
feanorfs agent                    # list agents (one-line state when online)
feanorfs agent status [NAME]      # list all, or preview one agent
feanorfs agent spawn <NAME> [--no-sync] [--replace]
feanorfs agent refresh <NAME> [--replace]
feanorfs agent land <NAME> [--clean] [--propose]
feanorfs agent clean <NAME>
feanorfs agent run <NAME> -- <COMMAND> [ARGS...]
```

| Subcommand | Description |
|---|---|
| `status` | List all agents (enriched when server reachable; names-only offline). Hidden `agent list` returns legacy JSON `{"agents": ["name"]}`. |
| `spawn` | APFS clonefile/copy snapshot with server base hashes |
| `refresh` | Pull cloud changes the agent hasn't touched. `--replace` discards agent-local edits after preserving them as a parent snapshot. |
| `land` | Apply clean work, upload, register conflicts |
| `clean` | Remove agent dir and snapshot rows |
| `run` | Run a command in the agent dir — not a sandbox |

**Isolation caveat:** data isolation only — see [threat-model.md](threat-model.md).

### `conflicts` — list, compare, resolve

```bash
feanorfs conflicts                              # list pending paths
feanorfs conflicts keep <PATH> --local|--cloud|--both
feanorfs conflicts keep <PATH> --file <RECONCILED>
feanorfs conflicts show <PATH> [--open]
```

Version files use `.original`/`.local`/`.cloud` suffixes.

| Subcommand | Description |
|---|---|
| (bare) / `list` | Pending paths blocked from sync |
| `keep` | Resolve by keeping local, cloud, both, or a reconciled file |
| `show` | Unified diff; `--open` launches editor compare |

### Orchestrator surfaces (hidden)

```bash
feanorfs events    # NDJSON: sync_state, folder_changed, conflict_risk, …
feanorfs mcp       # MCP protocol + tools (agent_*, conflicts_*, sync_status, workspace_log, workspace_undo)
```

File contents never leave the machine on either surface. MCP `sync_status`
returns counts plus actionable pending paths instead of the complete local file
map, keeping routine agent checks small even in large workspaces.

## Examples

### First-time setup across two machines

```bash
# Machine A: private hub, sync watcher, and tray all persist at login
machine-a$ feanorfs start /path/to/project --workspace proj

# Machine A: create a single-use LAN pairing code
machine-a$ feanorfs pair

# Machine B: pair, join, and enable background sync
machine-b$ feanorfs start fnp1-... ~/projects/proj
machine-b$ cd ~/projects/proj
machine-b$ feanorfs status            # mirror is up to date
```

### Continuous sync while working

```bash
feanorfs start          # resume + automatic background sync
vim src/lib.rs          # auto-syncs within 500ms of saving
```

### Check what changed before syncing

```bash
feanorfs status
# Output:
#   Local changes not yet on the mirror (run 'feanorfs sync --up'):
#     [modify/add] src/lib.rs
#   Changes on other machines to download (run 'feanorfs sync --down'):
#     [download]   README.md (2.3 KB)
```

## Synced files

FeanorFS syncs the **current contents** of the workspace folder — including hidden files and paths that git would ignore (`.env`, local config, scratch WIP). That is intentional: those files are often exactly what you want on another machine.

**Always skipped:** `.feanorfs/` (client state), `.git/` (VCS metadata).

**Default artifact ignores:** `target/`, `node_modules/`, `.venv/`, `__pycache__/`, `dist/`, `build/`, `.next/`, `.cache/`, plus editor swap files and `.DS_Store`. These are high-churn build/install trees — syncing them would hammer the watcher on every compile, not just slow the first sync.

**`.gitignore` is not read.** FeanorFS is not a git companion; workspaces need not be repos.

**`.feanorfsignore` is optional** — gitignore syntax for project-specific exclusions (custom `out/`, `vendor/`, etc.). Most projects never need it; do not copy your entire `.gitignore` (that would exclude the files FeanorFS is for).

Full rationale: [sync-scope.md](sync-scope.md).

## Agent loop and reconciliation (orchestrators)

FeanorFS isolates **files**, not processes. An agent workspace is a normal folder under `.feanorfs/agents/<name>/` — point any coding agent's cwd at it. `agent run` sets `FEANORFS_AGENT` and `FEANORFS_AGENT_DIR` on the child; it does **not** sandbox the process — absolute-path writes can escape the agent dir (see [threat-model.md](threat-model.md)).

### Loop

```bash
feanorfs sync --no-watch          # optional: ensure folder is current
feanorfs agent spawn ci1
feanorfs agent run ci1 -- cargo test
feanorfs agent status ci1           # read-only preview
feanorfs agent refresh ci1          # pull cloud changes agent hasn't touched
feanorfs agent land ci1             # apply clean work; register needs-attention
feanorfs conflicts
feanorfs conflicts keep <path> --local | --cloud | --both | --file <reconciled>
```

### Reconciliation protocol (LLM harnesses)

1. `feanorfs --json agent land <name>` — read `conflicts[]` with `original_file`, `local_file`, `cloud_file`, `hint`.
2. Optional: `feanorfs agent land <name> --propose` writes `<path>.proposed` via diff3 (text only; never auto-applied).
3. Reconcile locally — human or agent writes a candidate file.
4. **Verify** in a spawned agent workspace: `agent spawn verify` → copy candidate → `agent run verify -- <tests>`.
5. Only a green run earns `feanorfs conflicts keep <path> --file <candidate>`.

**Tiered policy:** clean `--propose` → reconciler agent → human escalation (binary files, sensitivity-list paths, or N failed verifications).

Resolution history: `feanorfs conflicts history --json` (records method, resolver from `FEANORFS_AGENT` or `human`).

### Demo script

```bash
./scripts/demo-agent-loop.sh
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Application error (config not found, network failure, IO error, etc.) |

## Environment

The server respects `RUST_LOG` for controlling log verbosity. The client writes trace-level logs to `.feanorfs/feanorfs.log` regardless of `RUST_LOG`.

```bash
# Server: enable debug logging for the server crate and tower-http
RUST_LOG=feanorfs_server=debug,tower_http=debug feanorfs serve

# Server: silence everything except warnings
RUST_LOG=warn feanorfs serve
```

If `RUST_LOG` is unset, the server defaults to `feanorfs_server=info,tower_http=info`.

All other client configuration is stored in `.feanorfs/config.json`.
