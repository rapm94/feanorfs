# FEANORFS KNOWLEDGE BASE

**Status:** pre-1.0 (v0.12.x). Format-v3 encrypted Merkle snapshots over an
opaque hub; Rustls transport with capability-pinned private CAs, mesh and
relay reachability; one supervisor login service; agent coordination
(`ffmsg1` signals, `ffwork1` work intent, `ffint1` integrator assignment,
`ffres1` resolution, `ffcap1` capabilities) projected into one lifecycle by
`agent next`; cross-platform tray and native installers. Signed macOS/Windows
products wait on founder credentials ([TODO.md](TODO.md) F1).

## Unifying Principle

**FeanorFS is dumb storage, smart transport.** FeanorFS never makes decisions about file content (no auto-merge, no summarization, no chat). Its job is to decide _what_ to transport, _when_ to transport it, and _how_ to isolate and preserve files safely. Anything requiring file-semantic understanding belongs in the consumer/agent layer.

## Layers

| Layer | Role |
|---|---|
| **Hub** (`feanorfs serve`) | Opaque blob storage plus compare-and-swap heads, format markers, and reachability manifests. Never decrypts trees or sees format-v3 filenames. |
| **Engine** (`feanorfs_client` + `feanorfs_agent_core`) | Builds encrypted trees, reconciles snapshots, materializes working copies; exposes CLI, Rust, C, TypeScript, MCP, and events surfaces. |
| **Tray** (`tray/`) | Shells the CLI for status, lifecycle, pairing, conflicts, recovery, diagnostics, and update awareness. No duplicate sync, pairing, credential, or cryptography logic. |

**Defaults:**
- Prefer smart defaults over flags: `feanorfs start [folder]` creates a secure private hub when no connection exists, syncs, installs the background service and tray, and returns; `feanorfs stop [folder]` reversibly removes automatic sync and tray registration; `--foreground` is explicit.
- `start fnh1-… <existing-folder>` refreshes hub trust only after an HTTPS CA/token/head probe succeeds and preserves the folder's workspace ID, E2EE key, refs, files, and history.
- New folders get distinct opaque `fsw1-…` workspace IDs; `--workspace` is a manual override, never a shared consumer default.
- Server auth = **token**; workspace secrecy = **encryption key** (distinct in user-facing copy).
- Native TLS is the hub default. `--allow-http` is explicit reverse-proxy/development mode; never disable certificate verification in clients.
- Surface conflicts; never auto-merge file content. Bulk choices may apply one explicitly confirmed local-or-mirror policy to every pending path.
- Self-host and hosted deployments share the same API and client binary.
- Agent-first, human-legible: every agent capability keeps a plain-files, plain-language human path. FeanorFS is not a VCS and grows no git-shaped porcelain.

## Architecture

1. **Sync:** format-v3 clients diff encrypted Merkle trees against the private `last-synced` ref, stage objects under `~/.feanorfs/workspaces/<opaque-id>/`, and compare-and-swap one workspace head. Sync snapshots carry an encrypted `ffbase1` Git baseline label (read-only from `.git/HEAD`).
2. **Storage:** Blake3 ciphertext-addressed blobs; `feanorfs serve` keeps opaque heads/manifests in SQLite, the embedded `LocalHub` in lock-protected JSON.
3. **E2EE:** ChaCha20-Poly1305 with a deterministic SIV-style nonce (required for CAS stability). Format ≥2 rejects non-AEAD blobs; unmigrated v1 decrypts via legacy XOR until `feanorfs migrate`, and removing that path requires approved field evidence (`doctor --migration-report`). Clients re-hash downloaded ciphertext before decrypting.
4. **Lazy hydration:** `--lazy` writes 0-byte placeholders; `hydrate`/`cat` fetch bytes.
5. **Agents:** `agent spawn` clones a worktree with one base snapshot ref; land commits through head CAS; conflicts persist in encrypted tree entries plus private artifacts. Agent workspaces isolate data, not processes.
6. **Coordination:** signals are no-file-change snapshots; reducers project work intent, integrator assignment, and resolution; `agent next` derives one lifecycle with prefilled actions; `agent guard` checks writes; `cap:<capability>` routes requests.
7. **History:** `log` walks reachable parents; `undo` appends a two-parent snapshot; complete reachability manifests drive server and local GC.
8. **Lifecycle:** one supervisor job (`com.feanorfs.agent`) spawns the private hub, one watcher per workspace, configured runners, and the tray. Workers receive only canonical paths; keys, tokens, invites, and passphrases never appear in argv, environment, logs, or discovery.
9. **Pairing and reachability:** SPAKE2 + AEAD pairing (`fnp1` LAN, `fnp2` relay rendezvous); signed mesh candidates race ahead of mDNS; the opaque relay forwards inner TLS only.
10. **Recovery and credentials:** Argon2id + XChaCha20-Poly1305 workspace kits and hub identity bundles; OS credential stores with protected-file fallback that never spills back once migrated.
11. **Local-only data:** catch-up summaries send paths, never contents; predictive-hydration weights never leave the client; workspace state is never deleted by age, absence, or name inference (only `retire`).

## Structure

```
common/        wire models, canonical trees, crypto, protocol contracts (no I/O)
server/        Axum hub: opaque blobs, SQLite heads/manifests, TLS, recovery
agent-core/    embeddable engine: snapshots, sync pass, agents, coordination, local hub
client/        `feanorfs` CLI + library: start/stop lifecycle, watcher, MCP, events
feanorfs-ffi/  C ABI (JSON in/out) + generated feanorfs.h
bindings/ts/   @feanorfs/agent napi-rs bindings
tray/          cross-platform desktop tray (shells the CLI)
test-support/  pre-main test profile isolation
eval/          multi-agent evaluation harness and scenarios
fuzz/          cargo-fuzz targets (properties in common/tests/fuzz/)
scripts/       installers, packaging, smoke tests
```

## Where to look

| Task | Location |
| :--- | :--- |
| Wire types, crypto, invites | [common/src/](common/src/) (`lib.rs`, `invite.rs`, `*_contract.rs`, `git_baseline.rs`) |
| Objects, snapshots, refs, history | [objects.rs](agent-core/src/objects.rs), [snapshot.rs](agent-core/src/snapshot.rs), [history.rs](agent-core/src/history.rs) |
| Sync pass and materialization | [sync_pass/](agent-core/src/sync_pass/), [commands.rs](client/src/commands.rs), [watch.rs](client/src/watch.rs) |
| Local state and scanning | [local/](agent-core/src/local/), [state/](agent-core/src/state/), [workspace_read.rs](agent-core/src/workspace_read.rs) |
| Workspace-state identity and retirement | [workspace_state_registry.rs](agent-core/src/workspace_state_registry.rs), [workspace_layout.rs](agent-core/src/workspace_layout.rs) |
| Agents and continuous reconciliation | [agent/](agent-core/src/agent/), [agent_live.rs](client/src/cli/agent_live.rs) |
| Signals and coordination | [messages.rs](agent-core/src/messages.rs), [work.rs](agent-core/src/work.rs), [integrator.rs](agent-core/src/integrator.rs), [resolution.rs](agent-core/src/resolution.rs), [resolution_protocol.rs](agent-core/src/resolution_protocol.rs), [coordination.rs](agent-core/src/coordination.rs) |
| Conflicts | [conflicts.rs](agent-core/src/conflicts.rs), [conflict_artifacts.rs](agent-core/src/conflict_artifacts.rs), [tree_reconcile.rs](agent-core/src/tree_reconcile.rs) |
| Transport, TLS, mesh, relay | [api.rs](agent-core/src/api.rs), [mesh/](agent-core/src/mesh/), [tunnel.rs](agent-core/src/tunnel.rs), [tls.rs](server/src/tls.rs), [endpoint.rs](client/src/endpoint.rs) |
| Lifecycle, pairing, recovery | [supervisor/](client/src/cli/supervisor/), [start.rs](client/src/cli/start.rs), [pair.rs](client/src/cli/pair.rs), [recovery.rs](client/src/recovery.rs), [server/src/recovery.rs](server/src/recovery.rs) |
| MCP, events, agent CLI | [mcp.rs](client/src/cli/mcp.rs), [events.rs](client/src/cli/events.rs), [cli/](client/src/cli/) |
| Agent JSON contract and docs | [docs/agent-api.md](docs/agent-api.md), [docs/agent-communication.md](docs/agent-communication.md), [skills/feanorfs-collaboration/](skills/feanorfs-collaboration/SKILL.md) |
| Sync scope rationale | [docs/sync-scope.md](docs/sync-scope.md) |
| Threat model | [docs/threat-model.md](docs/threat-model.md) |
| CI and releases | [.github/](.github/AGENTS.md) |

## Conventions

1. **Portable paths:** track forward-slash paths normalized with `feanorfs_common::normalize_path`; validate the exact path you later join or persist.
2. **No redundant hashing:** consult `local_state.json`; rehash only when mtime or size changed.
3. **Descriptor-anchored reads:** never reopen scanned workspace content by pathname; traverse with `openat`/`O_NOFOLLOW` from a retained root descriptor (checked portable fallbacks elsewhere).
4. **Zero knowledge:** seal file bytes by path and objects under the object domain before upload; format-v3 hub metadata contains no filenames.
5. **Library-first results:** commands return `Serialize` structs shared by `--json` and library callers; no `println!` in engine code.
6. **No auto-merge:** conflicts produce `.original`/`.local`/`.cloud` artifacts in private state; consumers resolve with `conflicts keep`.
7. **Data isolation ≠ sandbox:** never claim process sandboxing; link [docs/threat-model.md](docs/threat-model.md).
8. **Sync scope:** mirror disk contents including gitignored paths; hard-skip `.git/`, `.jj/`, legacy metadata, symlinks, nested valid `CACHEDIR.TAG` trees; keep `DEFAULT_IGNORES` small and frozen; custom rules live in global state via `feanorfs ignore`. Never write `.git`; read only `HEAD` and the ref it names.
9. **One operation, every surface:** new agent operations land on Rust, CLI, C FFI, napi/JS/d.ts, MCP, docs, and skill together and join `client/tests/operation_matrix.rs`.
10. **CI/CD:** pin actions to SHAs, keep permissions least-privilege, validate with actionlint/zizmor; never hand-edit cargo-dist's generated `release.yml`.
11. **Changelog:** root `CHANGELOG.md` only (`changelog_path = "./CHANGELOG.md"`).
12. **Shared pre-1.0 versions:** internal path dependencies use a pre-1.0 range so release-plz bumps every `version.workspace` crate together.
13. **Test isolation:** every state-capable test executable links `feanorfs-test-support`; tests never mutate HOME/FEANORFS_HOME after startup.

## Anti-patterns

- **DO NOT** scan `.git`, `.jj`, legacy metadata, or global state as project content.
- **DO NOT** sync on every raw filesystem event; debounce 500 ms.
- **DO NOT** download bytes during a `--lazy` sync.
- **DO NOT** add a hub endpoint when objects, manifests, and head CAS already express the operation.
- **DO NOT** send file contents or the E2EE key to a remote LLM (`--summarize` gets paths and metadata only).
- **DO NOT** merge concurrent edits.
- **DO NOT** honor `.gitignore` or grow `DEFAULT_IGNORES` into a framework denylist ([docs/sync-scope.md](docs/sync-scope.md)).

## Commands

```bash
cargo build --workspace --locked
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 eval/run.py eval/scenarios/overlap.json        # multi-agent eval (needs target/debug/feanorfs)

cargo run --bin feanorfs -- start ~/projects/app        # create/resume + background sync
cargo run --bin feanorfs -- serve --allow-http --port 3030 --data-dir server-data --token dev  # dev hub
cargo run --bin feanorfs -- --json agent next           # coordination lifecycle
cargo run --bin feanorfs -- mcp                         # MCP server (compact tools)
```

Full CLI reference: [docs/usage.md](docs/usage.md).

# DOX framework

- DOX is highly performant AGENTS.md hierarchy installed here
- Agent must follow DOX instructions across any edits

## Core Contract

- AGENTS.md files are binding work contracts for their subtrees
- Work products, source materials, instructions, records, assets, and durable docs must stay understandable from the nearest applicable AGENTS.md plus every parent AGENTS.md above it

## Read Before Editing

1. Read the root AGENTS.md
2. Identify every file or folder you expect to touch
3. Walk from the repository root to each target path
4. Read every AGENTS.md found along each route
5. If a parent AGENTS.md lists a child AGENTS.md whose scope contains the path, read that child and continue from there
6. Use the nearest AGENTS.md as the local contract and parent docs for repo-wide rules
7. If docs conflict, the closer doc controls local work details, but no child doc may weaken DOX

Do not rely on memory. Re-read the applicable DOX chain in the current session before editing.

## Update After Editing

Every meaningful change requires a DOX pass before the task is done.

Update the closest owning AGENTS.md when a change affects:

- purpose, scope, ownership, or responsibilities
- durable structure, contracts, workflows, or operating rules
- required inputs, outputs, permissions, constraints, side effects, or artifacts
- user preferences about behavior, communication, process, organization, or quality
- AGENTS.md creation, deletion, move, rename, or index contents

Update parent docs when parent-level structure, ownership, workflow, or child index changes. Update child docs when parent changes alter local rules. Remove stale or contradictory text immediately. Small edits that do not change behavior or contracts may leave docs unchanged, but the DOX pass still must happen.

## Hierarchy

- Root AGENTS.md is the DOX rail: project-wide instructions, global preferences, durable workflow rules, and the top-level Child DOX Index
- Child AGENTS.md files own domain-specific instructions and their own Child DOX Index
- Each parent explains what its direct children cover and what stays owned by the parent
- The closer a doc is to the work, the more specific and practical it must be

## Child Doc Shape

- Create a child AGENTS.md when a folder becomes a durable boundary with its own purpose, rules, responsibilities, workflow, materials, or quality standards
- Work Guidance must reflect the current standards of the project or user instructions; if there are no specific standards or instructions yet, leave it empty
- Verification must reflect an existing check; if no verification framework exists yet, leave it empty and update it when one exists

Default section order:
- Purpose
- Ownership
- Local Contracts
- Work Guidance
- Verification
- Child DOX Index

## Style

- Keep docs concise, current, and operational
- Document stable contracts, not diary entries
- Put broad rules in parent docs and concrete details in child docs
- Prefer direct bullets with explicit names
- Do not duplicate rules across many files unless each scope needs a local version
- Delete stale notes instead of explaining history
- Trim obvious statements, repeated rules, misplaced detail, and warnings for risks that no longer exist

## Closeout

1. Re-check changed paths against the DOX chain
2. Update nearest owning docs and any affected parents or children
3. Refresh every affected Child DOX Index
4. Remove stale or contradictory text
5. Run existing verification when relevant
6. Report any docs intentionally left unchanged and why

## User Preferences

When the user requests a durable behavior change, record it here or in the relevant child AGENTS.md

- CI/CD should favor mainstream tooling, immutable action pins, least privilege, cross-platform coverage, release provenance, and enforced quality gates over minimal workflow setup.
- Keep pull-request CI lean: require fast Linux quality gates, then run
  expensive cross-platform, release, SDK, tray, and CodeQL checks on `main`
  before release.
- Keep GitHub Releases product-focused: ship the `feanorfs` binary and optional
  platform desktop tray products with integrity metadata, not internal crates or compatibility
  binaries already covered by `feanorfs serve`.
- Consumer onboarding is one native installer followed immediately by a tray choice between **Start Mirroring a Folder…**, **Join Another Computer…**, and **Not Now**; `feanorfs start [invite-or-server] [folder]` is the terminal equivalent. Distribution publishes a trusted macOS `.dmg` containing the signed `.pkg`, verified Linux `.deb`/`.rpm`/`.pkg.tar.zst`, and an Authenticode-verified Windows installer `.exe`, with checked script/tar fallbacks. After every artifact passes verification, an interactive install starts the tray with the public `--first-run` hint; an unconfigured tray routes the selected choice into the existing secure menu action, while an existing workspace never re-prompts. Root/headless sessions and `FEANORFS_NO_LAUNCH=1` receive the explicit CLI path instead. Older or unsupported releases report their CLI-only fallback. With no saved connection the first machine automatically provisions a secure private hub, prefers port 3030 without requiring it, and persists a safe available fallback without exposing it in service argv; background persistence and the desktop tray are automatic, while raw `serve`/service supervision remains an advanced diagnostic or dedicated-server surface.
- The cross-platform tray must remain useful before setup: its native folder picker delegates to the same `feanorfs start` path and must not duplicate sync, credential, pairing, or encryption logic.
- Consumer offboarding is reversible: `feanorfs stop [folder]` and the tray remove only automatic sync and recent registration. They preserve working files, encrypted setup, OS credentials, remote snapshots, and private hubs so `start` can resume safely.
- Normal desktop pairing stays inside the tray on both computers: show the short-lived LAN `fnp1` code or copy the long off-LAN `fnp2` capability with its TTL, paste it through masked receiver UI, choose the destination folder, and delegate through bounded stdin to the ordinary `start` engine. Automatically reuse a relay stored by `start --relay`, keep the full invite and cryptography in the CLI child, and never place either capability in argv, environment variables, or logs.
- Treat encryption and local credential handling as product requirements: never place E2EE keys or server tokens in service arguments, logs, pairing discovery metadata, or opaque hub storage.
- Normal desktop diagnostics and repair stay in the tray: project only stable `doctor` check names/statuses into generic native copy, ignore local identifiers/endpoints, require an explicit repair choice, and delegate repair to the ordinary flag-safe `start -- <folder>` lifecycle without duplicating sync, credential, encryption, or conflict policy.
- Update awareness stays opt-in and never automatic: the official stable release, semantic comparison, bounded HTTPS metadata, exact canonical tag URL, and the browser-open choice remain the check path. An explicit `update --apply` (CLI flag or tray Install action) may additionally download only the canonical platform archive for the running target from that same release, verify its published SHA-256, replace the running binary in place (Windows keeps a `.old-<pid>` backup), and refresh supervised services; root/Homebrew/cargo-managed installs are refused. Signature verification becomes mandatory once cross-platform signing (F1) lands; silent or scheduled installs stay prohibited.
- Workspace recovery is a normal CLI/tray product surface: encrypt the complete portable capability with a user-held passphrase, write kits atomically/private, authenticate before local writes, and re-enter `start`. Keep passphrases and decrypted capabilities out of argv/environment/logs; never imply that an access kit backs up hub blobs.
- New format-v2/v3 setup accepts only the canonical generated 256-bit lowercase-hex E2EE key shape and must validate it before any workspace/global write. Human passphrases remain readable only in legacy format-v1 workspaces so `migrate --rekey` can replace them safely.
- Keep direct dependencies on the latest maintained stable releases compatible with the tested Rust 1.88 MSRV. Document intentional holds instead of adopting pre-releases or silently raising MSRV; SQLx 0.9 currently requires Rust 1.94, constant_time_eq 0.5 requires Rust 1.95, and tokio-tungstenite 0.30 remains held while Axum 0.8.9 selects 0.29 so releases ship one WebSocket protocol stack.
- Relay deployment must reuse `feanorfs serve --relay`, run non-root with a read-only-capable runtime, persist protected identity outside the container layer, terminate public TLS before internal HTTP, omit capability-bearing request paths from logs, and publish SBOM/provenance without claiming that a default hosted relay exists.
- Keep exactly one authoritative open-work list at root `TODO.md`, split by founder and AI ownership with dependencies and acceptance evidence. Remove shipped, speculative, trigger-only, or superseded tasks instead of preserving them as backlog history.

## Open Work

The sole authoritative open-work list is [TODO.md](TODO.md). It separates founder-owned credentials, decisions, infrastructure, and field evidence from AI-owned implementation and verification. Shipped and speculative work does not remain in the TODO.

## Child DOX Index

Direct children own durable crate or automation boundaries; subdirectories inside crates share files at the top level and do not merit separate AGENTS.md.

| Child | Purpose |
| :--- | :--- |
| [.github/](.github/AGENTS.md) | CI, security scanning, fuzzing, agent eval gate, dependency automation, and release orchestration. |
| [common/](common/AGENTS.md) | Shared wire models, canonical trees, crypto, and protocol contracts. Zero I/O. |
| [server/](server/AGENTS.md) | Axum blob hub and SQLite metadata. Pure transport — never decrypts or inspects content. |
| [agent-core/](agent-core/AGENTS.md) | Embeddable engine: snapshots, sync, agents, coordination, local hub. |
| [client/](client/AGENTS.md) | `feanorfs` CLI + library: lifecycle, watcher, MCP, events. |
| [bindings/ts/](bindings/ts/AGENTS.md) | napi-rs Node bindings and platform package assembly. |
| [feanorfs-ffi/](feanorfs-ffi/AGENTS.md) | C ABI with bounded JSON inputs and the generated header. |
| [tray/](tray/AGENTS.md) | Cross-platform desktop tray that shells the CLI. |
| [scripts/](scripts/AGENTS.md) | Installers, native packaging, and product/release smoke tests. |
| [test-support/](test-support/AGENTS.md) | Dev-only process-wide test profile isolation. |

`eval/` (multi-agent evaluation harness; see [eval/README.md](eval/README.md)) and `fuzz/` (cargo-fuzz targets over `common/tests/fuzz/properties.rs`) are small tool directories documented by their READMEs and file headers; they carry no separate AGENTS.md.
