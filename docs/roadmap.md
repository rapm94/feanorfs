# FeanorFS Roadmap

**Focus:** tray MVP (DX-26–28), then Phase C polish.  
**Freeze list** (bug fixes only until tray MVP): `predictive.rs`, `summary --summarize`, mDNS LAN discovery.

**Strategy:** One OSS stack for self-host and managed hosting — **agent loop** (concurrent agents, conflict surfacing) + **background sync** (uncommitted files across machines).

---

## CLI vocabulary

| Operation | Primary | Hidden aliases |
|---|---|---|
| Onboard | `feanorfs start [URL\|invite\|folder]` | `setup`, `init`, `join`, `attach`, `connect` (configure-only; no auto watch) |
| Upload / download / both | `sync --up` / `--down` / `sync` | `push`, `pull`, `watch` |
| Agent work | `agent spawn` / `status` / `refresh` / `land` | `agent check`, `agent commit`, `agent list` |
| Conflicts | `conflicts` (list) / `keep` / `show --open` | `conflicts list`, `conflicts open`, `conflicts history` |
| Config / key | `config` / `config --key` | `show-key` |
| Hub | `feanorfs serve` | `feanorfs-server` (legacy server-only install) |
| Orchestrators | — | `events`, `mcp`, `workspaces` |

---

## Backlog

### P1 — Tray MVP

| ID | Task |
|----|------|
| DX-26 | Menu-bar app: state icon (up to date / syncing / offline / needs attention / paused), pause toggle, open folder, workspace switcher. Shells `feanorfs --json` — no duplicate sync logic. |
| DX-27 | Needs-attention view: per conflict, plain-language keep local / cloud / both; calls `conflicts keep`. |
| DX-28 | Agent presence: "N agents working · M need attention" with land shortcuts. |
| P-4 | Invest list completion: `--json` gaps, conflicts UX polish. |

### P2 — Agent & sync polish

| ID | Task |
|----|------|
| AG-28 | Test: agent + folder both create same path (no-base-leg conflict). |
| AG-29 | Test: crash mid-land → re-run converges. |
| AG-30 | Test: agent rename vs folder edit → edit/delete conflict surfaced. |
| DX-11 | Default sync non-lazy; `--lazy` explicit with help warning. |
| DX-14 | Clock-skew regression test (hashes decide conflicts, not cross-machine mtime). |
| DX-18 | `FileState.mode` for executable bit (wire change, server+client lockstep). |
| DX-19 | Warn once per skipped symlink in `status`; document. |
| DX-21 | Verify + test disk-full mid-download (`atomic_write` path). |
| DX-23 | Server-rollback UX when server metadata regresses vs `last_synced_files`. |
| DX-24 | Profile 10k+ trees; parallelize hashing only if needed. |
| DX-25 | Bulk-touch test (branch switch → single debounced sync pass). |
| DX-29 | Skip directories containing `CACHEDIR.TAG` (complements small `DEFAULT_IGNORES`; see [sync-scope.md](sync-scope.md)). |
| SEC-6 | Remove `LegacyPolicy` + XOR decrypt when no v1 workspaces remain. |
| GC-7 | Optional server `file_history` (time machine). |

### P2 — Hosted connect (blocked on service)

| ID | Task | Notes |
|----|------|-------|
| CONN-6 | Account vault: `feanorfs login`, E2EE keys encrypted client-side on server, join by workspace name. Recovery kit at signup. | Needs hosted identity backend. |
| CONN-7 | Rendezvous/relay: workspace → address, NAT hole-punch, relay fallback. | Build when users hit the NAT wall; explicit URL / self-host / LAN suffice today. |

**Frozen connect rules:** one hub per workspace; no mesh; folder = mount point bound by `.feanorfs/config.json`.

### P3 — Deferred

| ID | Task | Trigger |
|----|------|---------|
| DX-12 | Real dataless files (File Provider / Cloud Files / FUSE). | Product need for OS-integrated placeholders. |
| CHUNK-1..4 | FastCDC chunking; manifest hash; server `blob_refs`; GC via refs. | 100 MB cap or re-upload cost complaints. |

**Chunking sketch:** ~1 MiB FastCDC chunks; files under 4 MiB stay single-blob; per-chunk keys; `FileState` shape unchanged.

---

## Suggested order

1. DX-26, DX-27 (tray MVP)
2. DX-28, P-4
3. AG-28..AG-30 (agent edge tests)
4. DX-11, DX-14, DX-23, DX-25
5. SEC-6, GC-7
6. CONN-6, CONN-7 when hosted tier exists
7. DX-12, CHUNK-* on demand

---

## Key files (for open work)

| Area | Files |
|------|-------|
| Tray (new) | TBD native shell calling `feanorfs --json` |
| Agent edge cases | `client/src/agent.rs`, `client/tests/sync_engine.rs` |
| Sync polish | `client/src/commands.rs`, `client/src/conflicts.rs`, `client/src/watch.rs` |
| Crypto cleanup | `common/src/lib.rs`, `client/src/migrate.rs` |
| Server history | `server/src/db.rs`, `server/src/gc.rs` |
| Hosted connect | TBD |
