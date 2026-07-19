# Changelog

All notable changes to FeanorFS are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.7](https://github.com/rapm94/feanorfs/compare/v0.7.6...v0.7.7) - 2026-07-19

### Fixed

- Windows release checksum files now use portable LF line endings, with a
  publication-time regression check, so standard Unix `sha256sum -c` and
  `shasum -a 256 -c` verification work without rewriting release evidence.

## [0.7.6](https://github.com/rapm94/feanorfs/compare/v0.7.4...v0.7.6) - 2026-07-19

### Fixed

- The Windows product smoke now uses a verified-clean login profile so Task
  Scheduler workers resolve the same private global state as the foreground
  client, validates that projects remain free of FeanorFS metadata, and removes
  only state it proved absent before the test.
- The cross-platform tray smoke now verifies pause state through the public CLI
  and explicitly proves that pause and resume never create project-local
  metadata.

## [0.7.4](https://github.com/rapm94/feanorfs/compare/v0.7.3...v0.7.4) - 2026-07-19

### Fixed

- The macOS launchd product smoke now uses a verified-clean login account home,
  matching how background services resolve global workspace state, and removes
  only the state and launch agents it proved did not exist before the test.

## [0.7.3](https://github.com/rapm94/feanorfs/compare/v0.7.2...v0.7.3) - 2026-07-19

### Fixed

- `start` and service restarts now wait for the managed workspace worker to
  actually reach its running state before reporting success, with a bounded
  actionable failure instead of a premature tray/CLI success message.

## [0.7.2](https://github.com/rapm94/feanorfs/compare/v0.7.1...v0.7.2) - 2026-07-19

### Fixed

- Workspace rename identity now includes the directory creation time, preventing
  a deleted folder's reused filesystem inode from attaching an unrelated
  workspace state or encryption key. Filesystems without a stable creation time
  fail safe by disabling identity-based rename recovery.
- Tray availability tests use an injected workspace probe instead of requiring
  a separately installed CLI, keeping the release matrix deterministic without
  changing the tray's production CLI-backed behavior.

## [0.7.1](https://github.com/rapm94/feanorfs/compare/v0.7.0...v0.7.1) - 2026-07-19

### Added

- Zero-litter workspace state: config, credentials references, cache, refs,
  objects, manifests, agents, conflicts, locks, logs, and custom ignore rules
  now live under opaque `~/.feanorfs/workspaces/<id>/` directories.
- `feanorfs ignore [PATTERN]` lists or changes per-workspace gitignore-syntax
  rules without adding a file to the project.

### Changed

- Existing project-local state and ignore rules migrate transactionally into
  global state, including a verified cross-filesystem copy fallback,
  interrupted-migration recovery, conflict-path relocation, folder-rename
  identity tracking, and quarantine of ambiguous prior global copies.
- Agent copies now separate `worktree/` from transport `state/` under the
  global root, so agent metadata can never appear as agent-authored work.

### Fixed

- `.git/`, `.jj/`, and legacy FeanorFS metadata are hard-excluded from scans,
  watcher events, mirror state, and conflict state. Cleanup preserves every
  local byte while removing accidental encrypted copies and stale conflicts.
- macOS private hubs now register through the native DNS-SD responder, while
  clients correlate the CA-derived service instance and retain pinned-CA TLS
  hostname verification. Discovery waits through IPv6-only resolution events
  and uses Avahi's system D-Bus resolver on Linux before its pure-Rust fallback,
  remaining stable across DHCP changes without persisting a numeric address or
  changing TLS SNI.
- Local objects/manifests, recovery temporaries, logs, download staging, and
  abandoned atomic-write files have bounded retention and cleanup. Large-file
  materialization stages beside its destination to remain atomic on external
  volumes.
- Format-v3 pull-only sync never publishes a downloader's unrelated local-only
  files or manifests; it records the remote head as the last agreed state and
  keeps remaining local work pending locally.
- Cloud conflict resolution now publishes only the explicitly resolved paths
  over the current hub head, so unrelated local work is neither uploaded nor
  discarded.

### Security

- Global workspace directories are private (`0700`) and sensitive configs are
  `0600`; secrets remain in the OS credential store or protected fallback.
  Migration never silently discards divergent state.

## [0.7.0](https://github.com/rapm94/feanorfs/compare/v0.6.4...v0.7.0) - 2026-07-19

### Added

- make desktop releases product-aware
- preview non-empty joins safely

### Fixed

- publish hub blobs atomically
- make desktop recovery and folder listing reliable

## [0.6.4](https://github.com/rapm94/feanorfs/compare/v0.6.3...v0.6.4) - 2026-07-18

### Fixed

- Linux watchers ignore non-mutating access/open notifications, preventing a
  scan from triggering an endless series of zero-change scans.
- Reinstalling an updated tray service now stops the running old executable
  before replacing and starting the service definition.
- Linux service stops now wait for the watcher and sync lock to clear before
  an upgrade sync begins, removing a first-run lock race.

## [0.6.3](https://github.com/rapm94/feanorfs/compare/v0.6.2...v0.6.3) - 2026-07-18

### Added

- `feanorfs conflicts keep --all --local` resolves every pending workspace
  conflict with the current folder's versions in one bounded operation,
  records each choice, and publishes one resolution snapshot.

### Fixed

- Private hubs now publish their actual non-loopback IPv4 addresses explicitly
  and re-announce them when interfaces change. This restores discovery from
  Linux hosts while retaining DHCP resilience and CA-bound TLS names; no LAN
  address is embedded in FeanorFS configuration or release code.
- Bulk local conflict resolution preserves legacy format-v2 behavior while
  format-v3 workspaces keep encrypted snapshot publication and history.
- Workspace recovery can reach its own managed private hub when same-host mDNS
  is unavailable, but only after the local hub CA and persisted port match the
  pinned capability; TLS hostname verification and bearer authentication stay
  enabled over the loopback route.

## [0.6.2](https://github.com/rapm94/feanorfs/compare/v0.6.1...v0.6.2) - 2026-07-18

### Added

- A stable multi-folder tray switcher that shows every followed folder, keeps
  actions scoped to the selected workspace, and gives immediate native
  feedback when a folder is added.
- A fail-closed encrypted hub-transfer maintenance path that copies and
  verifies complete reachable format-v3 history, accepts only loopback HTTP,
  preserves rollback data, and resumes safely when the destination has already
  advanced from the transferred head.

### Changed

- Tray menus now stay open reliably during pointer movement, avoid unnecessary
  native menu replacement, and describe managed background sync without the
  confusing “another terminal” wording.

### Fixed

- Directory entries in `.feanorfsignore` now prune before traversal, preventing
  ignored trees such as legacy hub storage from entering snapshots.
- New-folder setup, recent-workspace selection, and migration diagnostics now
  report actionable results without hiding successful additions or naming a
  background service as an external sync owner.
- Hub transfer seeds current encrypted local objects before conflict
  continuation, tolerates bounded live-filesystem races, and never publishes a
  partial or unrelated destination history.

### Security

- Temporary compatibility hubs can bind explicitly to loopback, keeping
  plaintext maintenance endpoints off the LAN; destination objects are
  Blake3-verified before authenticated head publication and configuration
  cutover.

## [0.6.1](https://github.com/rapm94/feanorfs/compare/v0.6.0...v0.6.1) - 2026-07-18

### Fixed

- Keep the Arch clean-install smoke on the official x86-64 image while proving
  ARM64 through exact Arch package metadata/payload checks and native
  Debian/Fedora execution; Docker's official Arch image has no ARM64 manifest.

## [0.6.0](https://github.com/rapm94/feanorfs/compare/v0.5.0...v0.6.0) - 2026-07-18

### Added

- Native tray-first installers for every supported desktop family: a universal
  macOS DMG containing the exact signed package, an Inno Setup Windows EXE, and
  Arch/Manjaro `.pkg.tar.zst` packages alongside Debian `.deb` and Fedora
  `.rpm` products.
- Clean-package smoke coverage for Arch Linux and install/PATH/uninstall smoke
  coverage for the Windows installer.

### Changed

- Finder package installation and the Windows setup wizard now open the same
  public `--first-run` tray chooser used by the verified script installers.
  Start and join continue to delegate to the merged secure `feanorfs start`
  engine; installers handle no tokens, pairing capabilities, or encryption
  material.

### Security

- macOS publication now requires notarization and stapling for both the signed
  package and its DMG container. Windows publication requires Authenticode on
  the installer EXE as well as both embedded executables. Arch packages receive
  exact-payload checks, SHA-256 files, clean-container execution, and GitHub
  provenance attestations.

## [0.5.0](https://github.com/rapm94/feanorfs/compare/v0.4.0...v0.5.0) - 2026-07-18

### Added

- Seamless first-machine hosting: with no saved connection, `feanorfs start [folder]` creates a native-TLS private hub under `~/.feanorfs/hub-data`, installs a credential-free per-user hub service, syncs, installs the workspace watcher, and registers the desktop tray. `start --host` explicitly selects that path; `serve` remains the advanced foreground/server surface.
- Automatic per-workspace user services: `feanorfs start` now syncs, installs background sync at login, and returns; `service install|status|start|stop|uninstall` provides recovery controls.
- Universal macOS Installer package containing `/usr/local/bin/feanorfs` and a proper `/Applications/FeanorFS.app`; the tray registers at login and coordinates exclusive actions with managed workspace services.
- Cross-platform desktop tray for macOS, Linux, and Windows, with colocated CLI discovery, per-user login registration, native folder/dialog integration, and no duplicate sync or cryptography implementation.
- Linux x86-64/ARM64 checksummed and attested `.deb`/`.rpm` desktop packages with declared GTK/AppIndicator/libxdo/portal dependencies, application-menu launchers, exact-payload verification, and a checked tar fallback; plus fail-closed Azure Authenticode signing for the Windows x86-64 CLI/tray bundle.
- One recommended Unix installer that automatically selects the signed macOS package or verified native Linux package, delegates dependency resolution to `apt`/`dnf`/`yum`, and truthfully falls back to the tar or CLI-only path when appropriate; a PowerShell installer verifies Windows checksums, exact contents, and both executable signatures and creates a Start menu shortcut.
- Tray-first onboarding: the system tray app now stays alive without a configured workspace and offers a native **Start Mirroring a Folder…** picker that delegates to the secure `feanorfs start` flow.
- Tray-to-tray onboarding: the receiver's **Join Another Computer…** action accepts a pasted `fnp1`/`fnp2` in masked native UI, chooses a destination folder, and delegates through bounded stdin to the existing zeroizing `PairCode` and `run_start` path without Terminal.
- Reversible folder offboarding: `feanorfs stop [folder]` and the tray's confirmed **Stop Mirroring This Folder…** action uninstall automatic sync and remove the recent-workspace entry while preserving files, encrypted setup, credentials, remote snapshots, and private hubs for later resume.
- Comprehensive lifecycle diagnostics: `feanorfs doctor` now checks automatic workspace sync, tray registration, locally owned private-hub persistence, authenticated mirror reachability, E2EE, format-v3 trees, and local state; global `--json` returns stable secret-free check records for automation.
- Native tray diagnostics and repair: **Check System Health…** reads only `doctor` check names/statuses, displays generic labels without workspace identifiers or endpoints, and offers explicit **Repair Mirroring** through the existing flag-safe `start -- <folder>` lifecycle instead of requiring Terminal.
- Safe update awareness: `feanorfs update` performs a bounded HTTPS-only semantic-version check against the official stable GitHub release, rejects noncanonical metadata/URLs, and powers the tray's explicit **Check for Updates…** / **Open Release Page** flow without downloading, installing, or executing artifacts.
- Privacy-safe migration evidence: `feanorfs doctor --migration-report` reads only local workspace format versions and emits deduplicated aggregate v1/v2/v3, unsupported, and unreadable counts. It never resolves credentials, contacts a hub, or reports paths, workspace IDs, labels, endpoints, credential references, relay routes, keys, tokens, or capabilities; legacy decryption remains gated on representative field evidence.
- Recoverable tray history: unavailable workspace folders are labeled and disabled instead of failing when selected. A confirmed **Remove Unavailable Folders…** action warns about disconnected external drives and delegates one locked, atomic recent-list cleanup to the CLI without changing files, encrypted setup, credentials, services, hubs, or remote snapshots.
- Tray-first installer handoff: after every checksum, signature, architecture, and payload check succeeds, interactive macOS, Linux, and Windows desktop installs open the exact installed tray with a non-secret `--first-run` hint. An unconfigured tray immediately offers custom **Start Mirroring a Folder…**, **Join Another Computer…**, and **Not Now** buttons, routing the first two into the existing secure menu actions without receiving a capability; existing workspaces never re-prompt, and **Not Now** leaves the tray available. Root/headless sessions and `FEANORFS_NO_LAUNCH=1` remain noninteractive; CLI-only fallbacks never launch a tray. The macOS app resolves its packaged CLI at `/usr/local/bin/feanorfs` without relying on LaunchServices `PATH`.
- Opaque per-folder workspace IDs: implicit new mirrors use distinct random `fsw1-…` identifiers instead of sharing `default`; explicit `--workspace` remains available for manual linking.
- Single-use LAN pairing: `feanorfs pair` and **Pair Another Computer…** produce an `fnp1-…` code that the receiver supplies through **Join Another Computer…** or the equivalent `start`; mDNS discovery, initial sync, and background setup are automatic.
- Stable private-hub addressing: automatic hubs use a CA-derived `feanorfs-….local` hostname whose mDNS addresses follow interface and DHCP changes, removing the normal router-reservation step.
- Native Rustls HTTPS: `feanorfs serve` now creates a durable private CA by default, refreshes interface certificates, and emits an `fnh1-…` secure hub invite; `--allow-http` is explicit.
- Zero-flag hub authentication: generate and persist a private 64-hex bearer token, reuse it after restart, and rotate it with explicit `--token`.
- Secure hub/workspace capabilities carry only public CA certificates so reqwest/Rustls can verify private hubs across LAN address changes.
- Fail-closed macOS packaging: secret-free native builds are combined into universal binaries, then require Developer ID Application and Installer signatures, Apple notarization, a stapled ticket, Gatekeeper package acceptance, and published evidence before upload.
- Signed-release Keychain gate: run the Developer ID CLI against an isolated workspace, require redacted config plus a readable native credential, delete the test item, bind the result to the packaged CLI hash, and publish the smoke record with the release.
- Offline private-hub recovery: `serve recovery export|import` preserves the hub CA and bearer token in an encrypted, crash-safe bundle so restored hubs retain existing client trust.
- Offline workspace recovery: `recovery export|import` and native tray actions protect the complete portable workspace capability with Argon2id + XChaCha20-Poly1305, fail before local writes on wrong-passphrase or tamper, use atomic `0600` files, and restore through the same initial-sync/service/tray `start` path.
- Fail-closed hub trust refresh: `start fnh1-… <existing-folder>` authenticates a replacement HTTPS CA/token and existing opaque head before preserving the folder's workspace ID, E2EE key, refs, files, and encrypted history.
- Crash-safe private-hub identity rotation: `serve recovery rotate` writes a mandatory encrypted backup, replaces the stopped hub's CA and bearer token behind a durable resume fence, removes stale leaf material, and preserves all opaque storage.
- Off-LAN private-hub transport: `start --relay <URL>` persists a random 256-bit route for an owned automatic hub, whose credential-free service maintains outbound WebSocket offers. Remote clients tunnel the existing Rustls stream through a loopback bridge, preserving hub CA verification and bearer authentication end to end. `serve --relay` enables both this bounded tunnel and `fnp2` pairing; `--pair-relay` remains a compatibility alias.
- Off-LAN pairing rendezvous: `fnp2` capabilities carry a public 128-bit session ID plus the relay URL and client-only 80-bit secret; the relay forwards bounded opaque WebSocket frames while SPAKE2, AEAD, and key confirmation remain end to end. Stored private-hub relay settings are reused automatically by the tray and CLI.
- Standard `feanorfs --version` installation/support output.
- Hardened opaque-relay OCI product: trusted tags publish the existing `feanorfs serve --relay` binary for amd64/arm64 as a non-root, read-only-capable image with protected persistent identity, authenticated health checks, SBOM, and digest-bound provenance.

### Changed

- Make fresh automatic private hubs survive a local port-3030 collision by scanning a bounded stable fallback range and atomically persisting the selection; existing hubs keep their endpoint, and service arguments remain limited to the protected hub-data path.
- Make tray failure copy state the unavailable operation, reassure users when files or workspace access were preserved, and provide a concrete retry, reinstall, or `feanorfs doctor` path instead of exposing `FEANORFS_BIN`, “watcher,” or generic “feanorfs failed” messages.
- Gate native Windows desktop artifacts on a full one-command host, redacted Credential Manager persistence/reload/cleanup, Task Scheduler hub/workspace/tray, TLS, doctor, MCP, and reversible stop/resume smoke; repeat it after Authenticode verification before signed publication.
- Gate native Linux desktop publication on clean installs of the exact `.deb` and `.rpm` artifacts in digest-pinned Debian 13 and Fedora 44 containers, including an idle format-v3 encrypted one-shot workspace with private config and real snapshot objects plus tray startup against that workspace under isolated Xvfb/D-Bus sessions.
- Keep MCP `sync_status` concise: return the mirror state, local file count, actionable pending paths, conflict/offline state, rollback warning, and skipped-symlink count without serializing the complete local file map.
- Replace the low-adoption tray password-dialog dependency with a narrow platform adapter: built-in masked AppleScript and WinForms prompts on macOS/Windows, plus packaged `zenity` with `kdialog` fallback on Linux.

### Security

- Reject human passphrases, uppercase/non-hex values, and wrong-length manual E2EE keys before writing new format-v2/v3 workspace or global configuration. Generated 256-bit keys remain the canonical path; legacy format-v1 keys stay readable for migration and optional rekeying.
- Keep the automatic hub's bearer token, CA private key, workspace E2EE key, and invites out of service argv, environment variables, logs, and discovery; its supervised command receives only the protected hub data-directory path.
- Store unattended E2EE keys and server tokens in macOS Keychain for signed releases, Windows Credential Manager, or Linux Secret Service; config JSON keeps only a random reference. Unsigned macOS/source builds and unavailable stores retain the atomic private-file fallback, while migrated configs fail closed on credential-store errors.
- Protect `~/.feanorfs/recent.json` updates with a private lock and atomic replacement; malformed state now fails explicitly instead of being silently overwritten during start/stop races.
- Keep encryption keys and server tokens out of service-manager arguments, and create atomic Unix credential directories/files and their temporary replacements with `0700`/`0600` permissions.
- Protect LAN invite delivery with SPAKE2, ChaCha20-Poly1305, explicit key confirmation, three-attempt rate limiting, expiry, secret zeroization, and secret-free mDNS metadata.
- Keep mDNS outside the trust boundary: stable endpoint adoption requires normal TLS verification against the invite-pinned public CA plus a successful authenticated hub probe; forged advertisements can cause denial of service but cannot impersonate the hub.
- Hide recovery keys and capability invites from redirected output unless explicitly requested; never disable TLS certificate verification.
- Protect hub recovery bundles with Argon2id and XChaCha20-Poly1305; validate CA/key identity, require an offline runtime lock, and fence partial multi-file imports until resumed.
- Protect workspace recovery kits with Argon2id and XChaCha20-Poly1305; expose no capability metadata, refuse implicit overwrite, and keep passphrases plus decrypted invites out of argv, environment variables, and logs while the tray supplies only a bounded stdin pipe.
- Keep tray recovery passphrases confined to masked platform UI, capped captured output, zeroizing memory, and the CLI child's bounded stdin; dialog commands contain only static scripts and public labels.
- Keep receiver-side pairing capabilities out of argv, environment variables, and logs; the tray sends one bounded stdin line, and the CLI validates it before folder creation or configuration writes.
- Refuse plaintext or tokenless hub identity refreshes, and leave existing connection settings untouched when replacement TLS or authentication cannot be verified.
- Rotate compromised hub CA/token material together, require offline exclusive access and a recoverable encrypted replacement identity, and force explicit capability re-pairing on every old client.
- Keep off-LAN pairing secrets, invites, bearer tokens, and workspace metadata out of relay URLs/state/logs; require WSS outside loopback tests; bound pairing and tunnel queues, concurrency, frames, bytes, and lifetime; zeroize client pairing-capability copies; and preserve hub CA verification plus bearer authentication inside the relayed TLS stream.

### Fixed

- Wait for the LAN pairing service to be announced before showing its one-time tray/CLI code, bound automatic-hub readiness probes so slow network attempts cannot hide startup failures or outlive the user-facing deadline, preserve native Windows SQLite paths without URL parsing, reserve enough Windows main-thread stack for consumer onboarding, register Windows task executables and arguments separately so long consumer paths remain valid, use native Windows process-liveness checks for watcher and sync-lock state, document hosted-macOS multicast limitations without weakening non-hosted LAN product smoke, avoid macOS SDK-smoke broken pipes, and keep Windows product smoke state out of PowerShell's read-only automatic variables while reporting redacted lifecycle diagnostics.
- Preserve Unix parent-directory durability after atomic private writes while avoiding unsupported directory `sync_all` calls on Windows for recovery kits, automatic-hub endpoint/relay state, and private server identity files.
- Treat Windows drive paths and ordinary colon-bearing names as folders in the merged `start [target] [folder]` flow instead of misclassifying them as scheme-free servers; bare endpoints remain available for unambiguous localhost, IP, and dotted-host values with an explicit port.
- Prevent embedded local hubs from inheriting a bearer token cached for an unrelated remote hub.
- Stop `doctor` from reporting a reachable but non-persistent workspace as healthy; failures now state what was preserved and provide a concrete recovery command instead of aborting on the first connection error.
- Make macOS `service stop` unload and preserve its launchd job so `KeepAlive` cannot immediately respawn it; the tray no longer starts a shadow watcher for a stopped managed service.
- Wait for the macOS watcher process to release its sync lock after launchd unloads it, preventing package upgrades and tray-exclusive actions from racing the terminating process.
- Present tray pairing in a native, terminal-free one-time-code dialog. The CLI child retains mDNS and cryptography, emits only the expiring code and TTL through a captured pipe, keeps the code out of argv/logs, and is terminated when the dialog closes.
- Refresh and restart an automatic host's TLS leaf before LAN pairing so laptops moved between networks advertise an address covered by the server certificate.
- Replace fixed LAN-IP invites with the durable CA-bound hostname, auto-update advertised addresses, retain an IP fallback for custom loopback hubs, and recover existing numeric configurations without retrying ambiguous publication requests.
- Detect same-path binary replacements with path-plus-Blake3 service identities, then restart the managed hub, workspace watcher, and tray onto the upgraded bytes. Background `start` now coordinates with an existing managed watcher, releases its workspace handles before launching the replacement, and restores the service if the initial sync fails.
- Make an ordinary `start <existing-folder>` refresh its locally owned private-hub service after a same-path package upgrade; users do not need to rediscover or pass `--host` for lifecycle repair.
- Read the stable Windows Task Scheduler state instead of localized status text so upgrades stop active managed watchers and do not relaunch an already-running tray.
- Read the automatic private hub's actual Windows Task Scheduler state instead of treating every installed hub task as stopped, allowing `doctor` and restart decisions to distinguish running, stopped, and missing hubs correctly.
- Register the Windows tray with Task Scheduler's interactive user token so the menu is visible in the logged-in desktop session instead of allowing a background-only tray process.
- Upgrade Axum to 0.8.9, reqwest to 0.13.4, tower-http to 0.7, and add axum-server 0.8/Rustls 0.23 plus rcgen 0.14.8 for maintained native TLS.
- Upgrade the tray stack to tray-icon 0.24.1, muda 0.19.3, and tao 0.35.3; move filesystem watching to notify 8.2, AEAD to ChaCha20-Poly1305 0.11, randomness to getrandom 0.4, and interface discovery to if-addrs 0.15 while preserving Rust 1.88 support.
- Upgrade the Node SDK toolchain to napi-rs 3 (including its `create-npm-dirs` package workflow), diff rendering to diffy 0.5, and executable discovery to which 8; keep SQLx, notify, SPAKE2, Argon2, and constant-time comparison on their newest stable Rust-1.88-compatible lines, and retain tokio-tungstenite 0.29 while Axum 0.8.9 uses it so release binaries contain one WebSocket protocol stack instead of both 0.29 and 0.30.
- Refresh the Rust-1.88-compatible Linux credential stack to zbus 5.18 and its current zvariant/zbus-name patch releases.
- Exercise the expanded macOS package as a real product in CI: one-command private hosting, credential-free launchd arguments, tray startup, authenticated TLS, MCP discovery/status, and secret-protected LAN pairing readiness.
- Keep human `status` output bounded when dependency trees contain thousands of skipped symlinks; `--json` still returns the complete sorted list.

## [0.4.0](https://github.com/rapm94/feanorfs/compare/v0.3.3...v0.4.0) - 2026-07-17

### Added

- *(common)* add encrypted snapshot models

### Fixed

- *(release)* focus distribution on FeanorFS

### Other

- *(release)* defer npm publication
- document format v3 release
- harden quality and release gates ([#6](https://github.com/rapm94/feanorfs/pull/6))

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

[Unreleased]: https://github.com/rapm94/feanorfs/compare/v0.6.3...HEAD
[0.3.0]: https://github.com/rapm94/feanorfs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/rapm94/feanorfs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rapm94/feanorfs/releases/tag/v0.1.0
