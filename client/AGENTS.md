# client

## Purpose

CLI + library crate (`feanorfs-client`: binary `feanorfs`, library `feanorfs_client`). Owns scanning, format-v3 sync orchestration, local cache metadata, predictive hydration, summaries, the watcher, the consumer lifecycle (`start`/`stop`/service/supervisor), pairing, recovery, and the MCP/events/agent CLI adapters. Agent, coordination, and history operations delegate to `feanorfs-agent-core`. It transports content without interpreting or merging it; serializable results are shared by library callers and `--json`.

## Ownership

- `lib.rs` — public re-export surface; `open_api_client` migrates metadata first, then selects direct/mDNS/relay. New public functions are re-exported here.
- `api.rs`, `hub.rs` — HTTPS/HTTP and in-process transport (re-export of agent-core `LocalHub`); private hub CAs extend normal Rustls verification.
- `commands.rs` — sync/push/pull/hydrate/cat/status via the unified sync pass; `MirrorState::human_label()`; `StatusResult` (including `git_baseline_mismatch`). No `println!`.
- `conflicts.rs`, `conflict_artifacts.rs` — workspace conflict gate, registry, `resolve_conflict` (single and bulk hold the sync lock end to end), shared `.original`/`.local`/`.cloud` writer.
- `local.rs`, `fs_util.rs`, `predictive.rs`, `summary.rs`, `recent.rs`, `backoff.rs`, `workspace_path.rs` — config/cache facade, atomic writes, local-only access weights, catch-up diff, locked recent registry, one `ExponentialBackoff`, exact `CanonicalWorkspacePath`.
- `migrate_sqlite/` — one-time SQLite → JSON migration with `metadata-migration.json` journal (`Discovered → Imported → Verified → Archived`).
- `watch.rs` — debounced (500 ms) watcher driving `do_sync`; head-observer wakeups; publishes `worker-status.json`.
- `tray.rs` — bounded tray status aggregation and the secret-free worker snapshot.
- `endpoint.rs`, `join_preflight.rs` — mesh/mDNS/relay endpoint selection; mutation-free non-empty join preview.
- `recovery.rs` + `cli/recovery.rs` — encrypted workspace-capability kits.
- `cli/` — command handlers: `start`, `service`, `supervisor/` (single background job), `hub_service`, `pair`, `serve`, `update`, `history`, `sync`, `tray`, `process_tree/` (sole process-ownership boundary), `agent`, `agent_live` (continuous controller), `agent_runner/` + `runner` (configured runners), `work`, `resolution`, `integrator` (dispatcher and candidate `reply`), `coordination` (`agent next`, `agent guard`, `agent capabilities`, `agent claim`, `agent done`, `agent coordinate`), `integrate` (MCP/skill/hook installation), `mcp`, `events`.

## Local Contracts

### Sync and state

- All stored paths use forward slashes via `normalize_path`; rehash only when mtime/size changed; unchanged placeholders reuse cached hashes.
- Sync scope: mirror the working directory including gitignored paths; hard-skip legacy `.feanorfs/`, `.feanorfsignore`, `.git/`, `.jj/`, symlinks, nested valid `CACHEDIR.TAG` trees (root tags exempt). Never create an ignore file or honor `.gitignore`; follow [docs/sync-scope.md](../docs/sync-scope.md) before growing `DEFAULT_IGNORES`.
- Format-v3 reconciliation compares trees with private `refs/last-synced`; mtime is cache and rollback evidence, never content identity. File↔directory transitions, rollback, and crash recovery go through agent-core's staged materializer under the sync lock.
- Large files use authenticated 8 MiB AEAD chunks; uploads stream one descriptor-anchored file with at most four pending chunks.
- Watcher root is the canonical workspace path (never `"."`; canonicalize so macOS `/private/var` events match). Each pass takes the sync lock and rechecks pause; failed passes arm a bounded retry; the watcher acknowledges its own publications.
- `StatusResult.git_baseline_mismatch` compares this clone's `.git/HEAD` commit with the newest `ffbase1` label; it is advisory and never fails status.
- MCP `status`/`sync_status` return a compact projection (counts and actionable paths, never the full `local_files` map); CLI `--json status` keeps the full result. Human status shows at most five symlink examples.
- Migration: `metadata-migration.json` (SQLite import) and `migration-v3.json` (rekey: old/target keys, fence token, phase, source/candidate binding) are distinct journals. Rekey never commits the target key before reseal, parentless head CAS, stamp, and local finalization; resumed runs reuse recorded IDs; older journals without bindings fail closed. Rekey requires clean or landed agents.
- Predictive hydration and summaries are local-only; summaries ship paths, never contents. `password_or_default` warning means a bug.

### Lifecycle, services, and transport

- `start [target] [folder]` accepts `fnh1`, `fnr1`, `fnp1`, `fnp2`, or a folder; with no connection it hosts a private hub (`--host`, `--relay <URL>` explicit). Every branch converges on link/create → initial sync → service + tray, reported as separate resumable stages; a later failure preserves identity and names the stage to rerun.
- Non-empty joins preview counts without touching setup; differing ignore policy needs confirmation (`--accept-join` for automation). New mirrors get random `fsw1-…` IDs; never a shared `default`.
- Positional targets preserve Windows drive paths; scheme-free servers only for localhost/IP/dotted hosts with an explicit port.
- `start fnh1-…` on a configured folder authenticates CA/token/head before writing config and preserves workspace ID, key, format, refs, files, history.
- E2EE keys for v2/v3 are canonical 64-char lowercase hex, validated before any write; v1 human keys stay loadable only for `migrate --rekey`.
- Credentials go to the OS store (signed macOS, Windows, Linux Secret Service) with protected-file fallback; migrated references fail closed. Worker argv/env/logs never contain keys, tokens, invites, routes, or passphrases.
- The single supervisor job owns the hub worker, workspace watchers, configured runners, and the tray (Windows keeps a separate interactive tray task); locked/atomic bounded `supervisor.json` registry; path-plus-Blake3 identity triggers reinstall; Unix process groups and Windows Job Objects tear down descendants; liveness uses exact process identity and fails closed.
- `service stop` moves the folder to the stopped set and waits (≤5 s on Unix) for its watcher and sync lock to clear; background `start` parks a managed watcher during its initial sync and restores it on failure. `--foreground` keeps a terminal watcher; `--no-watch` is one-shot. Hidden `service refresh-installation` is the installer handoff: migrate legacy jobs, re-add recent workspaces, restart the supervisor on identity change; no secrets or sync.
- `stop [folder]` removes automatic sync and the recent entry only; never files, credentials, snapshots, or hubs. `tray forget-unavailable` edits only the registry; `retire` is the only path that deletes workspace state and requires tombstone, grace, quarantine, and re-verification.
- Automatic hubs prefer port 3030, scan 3030–3130, then an OS port, persisted as `0600` `hub-data/listen-port`; worker argv stays `service hub-run <data-dir>`. Portable invites rewrite only loopback to the CA-derived `.local` name.
- Never use an accept-invalid-certificate client; unauthenticated mDNS never creates trust. Relay sync keeps the original SNI, pinned CA, and bearer auth; routes live only in `0600` `hub-data/relay.json`.
- Pairing never logs or places codes/invites in argv/env/mDNS/hub; full invites are SPAKE2-authenticated, AEAD-encrypted, key-confirmed, zeroized. `fnp2` requires WSS outside loopback tests.
- Recovery kits: Argon2id v19 (64 MiB, 3 iterations, 1 lane) + XChaCha20-Poly1305, bounded reads, atomic `0600` no-clobber writes, decrypt-and-validate before local writes, then `run_start` in-process.
- `doctor` accumulates secret-free named checks; one failure never hides later checks. `doctor --migration-report` emits only an aggregate format count and never authorizes removing legacy decryption.
- `update` is advisory: bounded HTTPS lookup of the official stable release, `semver` comparison, exact tag URL; `--apply` verifies SHA-256 of the canonical platform archive and refuses root/Homebrew/cargo installs.
- Windows Task Scheduler actions store program and arguments separately (never `schtasks /TR`); status uses the state enum, not localized text.
- File logs: `info` default, bounded 10 MiB + one rotation, private permissions, bounded lock waits; tray commands never block on the log lock.

### Agents and coordination

- Agent workspaces isolate data, not processes. `agent run` sets `FEANORFS_AGENT`, `FEANORFS_AGENT_DIR`, and absolute `FEANORFS_WORKSPACE_ROOT`; nested signal, coordination, status, and MCP commands resolve the shared root through `control_workspace_root`. After the child exits 0 with every edit settled, `agent run` finishes the agent's claimed work (`finish_quietly`); failure only leaves the claim open.
- Continuous controller: one notify watcher (500 ms debounce), guarded automatic land (`clean=false, propose=false`) and refresh (never `--replace`), fail-closed attention, bounded final flush; never merges, never activates dormant agents.
- Configured runners stay disabled until `start`; status never exposes argv, bodies, output, or credentials; `reset`/`remove` need `--discard-pending`; attention reasons (`cursor_reset`, `pending_overflow`, `ambiguous_execution`, `delivery_unknown`, `preparation_failed`) are never replayed automatically.
- `agent next` and MCP `status` are read-only projections; `agent guard` exits 2 on deny and, with `--hook`, allows on any internal error (advisory, never access control); it probes with lease-free `workspace_has_preferred_state` and logs globally, so unrelated folders never gain workspace state. `integrate --guard-hook` (deny-only) and `--auto-claim` (claiming guard plus `agent done --hook` Stop hook, 300 s timeouts) touch only FeanorFS's own entries in `.claude/settings.json`. `agent claim` exits 0 covered/accepted, 3 pending, 1 rejected; `agent done --hook` blocks stopping once on failure and never loops (`stop_hook_active`); both hooks log globally.
- Work, resolution, integrator, and capability commands are thin adapters; senders default to `FEANORFS_AGENT`, then `human`. `agent send cap:<capability>` routes through the engine roster and refuses ambiguity.
- MCP lists nine compact tools (`status`, `send`, `inbox`, op-routed `agent`, `work`, `conflicts`, `resolve`, `integrator`, `history`); after `op` is removed every call parses through its `deny_unknown_fields` contract (unknown fields → `-32602`). Legacy names stay callable; `FEANORFS_MCP_LEGACY_TOOLS=1` lists them.
- `events` is metadata-only NDJSON driven by the shared head observer; transient failures keep cursors; `agent_message_cursor_reset` precedes reset wakeups.
- Conflicts: bare `conflicts` lists; `keep --all --local|--cloud` validates every version before one scan/publication; human output caps lists at 20 and diffs at 64 KiB. Cloud deletion sentinels remove the local path.

## Work Guidance

- New public functions go into their module and are re-exported from `lib.rs`.
- Keep service commands idempotent and per-user; `start` never needs root, `nohup`, PID files, or credentials in argv.
- Automatic-hub changes test secure defaults, worker argv, port fallback and persistence, legacy refusal, TLS 401, authenticated readiness, and invite rewriting. Pairing changes test right/wrong code, expiry/attempts, URL rewrite, and real LAN discovery.
- `SummaryResult` is consumed by `FEANORFS_SUMMARY_CMD`; coordinate before renaming fields.
- After changing `commands.rs` or `local.rs`, run workspace clippy and tests.

## Verification

- `cargo test --workspace` (unit tests plus `client/tests/*`, including `coordination_cli.rs`, `git_baseline.rs`, `operation_matrix.rs`, `doc_conformance.rs`).
- `cargo clippy -p feanorfs-client --all-targets -- -D warnings`; `cargo fmt -p feanorfs-client -- --check`.
- `python3 eval/run.py eval/scenarios/<name>.json` after coordination changes.
- macOS/Windows product smokes: `scripts/smoke-macos-product.sh`, `scripts/smoke-windows-product.ps1`.

## Child DOX Index

`migrate_sqlite/` and `cli/` subdirectories are module directories inside `client/src/` without separate AGENTS.md; this file owns them.
