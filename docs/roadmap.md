# FeanorFS Roadmap

**Focus:** Merkle snapshot engine (MERK-1..7) and Phase C polish.  
Shipped tray MVP: `tray/` (`feanorfs-tray`), `feanorfs tray status|pause|resume|recent`, conflict `show --json`, `TrayStatusResult` contract in `common/src/tray_contract.rs`.
**Freeze list** (bug fixes only until MERK-1): `predictive.rs`, `summary --summarize`, mDNS LAN discovery.

**Strategy:** One OSS stack for self-host and managed hosting — **agent loop** (concurrent agents, conflict surfacing) + **background sync** (uncommitted files across machines).

Shipped agent SDK: [docs/agent-api.md](agent-api.md), `agent-core/`, `feanorfs-ffi/`, `bindings/ts/`, `examples/sdk-agent-loop.sh`, `examples/zig-agent/`. CI job `sdk` smoke-tests CLI, Node, and Zig paths.

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

### P1 — Tray MVP (shipped)

| ID | Task |
|----|------|
| DX-26 | Menu-bar app (`feanorfs-tray`): state icon, pause toggle, open folder, workspace switcher. Shells `feanorfs --json`. |
| DX-27 | Needs-attention submenu: plain-language labels, `conflicts keep` actions. |
| DX-28 | Agent presence line + Land shortcuts. |
| P-4 | `TrayStatusResult`, `conflicts show --json`, `ConflictKeepResult`, events `mirror_state` snake_case, pause file + watch respect. |

### P2 — Agent & sync polish

| ID | Task |
|----|------|
| AG-28 | Test: agent + folder both create same path (no-base-leg conflict). |
| AG-29 | Test: crash mid-land → re-run converges. Write now against the current engine; becomes a MERK-4 acceptance test. |
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
| GC-7 | Optional server `file_history` (time machine). Subsumed by MERK-6 (retention over immutable snapshots) — close when MERK-6 lands. |

### P2 — SDK follow-ups (post-tray unless embedder demand pulls forward)

| ID | Task |
|----|------|
| SDK-5b | Prebuilt `@feanorfs/agent` npm binaries (cargo-dist targets); publish from CI. |
| SDK-7 | Drop SQLite from the agent SDK dependency tree. Staged: **(a)** `agent_snapshots` → JSON/bincode under `.feanorfs/agents/<name>/`; **(b)** client cache to atomically-written serialized map (or redb); **(c)** pluggable hub metadata (in-memory/JSON for embedded hub, SQLite for `feanorfs serve`). Acceptance: `feanorfs-agent-core` without `sqlx`; FFI static lib links cleanly; smaller napi module; migration for existing `local_cache.db`. **Note:** (a) and the `last_synced_files` half of (b) are subsumed by MERK-4/MERK-5 — re-scope against the MERK track before starting. |

### P2 — Merkle snapshot engine (MERK) — post-tray flagship

**Goal:** replace the flat `path → FileState` model (client `last_synced_files` / `agent_snapshots` rows, server `files` table) with immutable Merkle-tree snapshots stored in the existing encrypted CAS, adopting two ideas from Jujutsu: **every workspace state is a snapshot** and **conflicts are first-class data, not blockers**. What it buys the agent SDK: O(1) spawn, O(changed-subtree) status/land, atomic land (one head swap — no torn per-file upserts), free history/undo, and conflicts that survive restarts and travel across machines. For humans it adds exactly one new power — undo — with zero new ceremony: the working copy stays plain files, git remains their version control, and the snapshot machinery stays invisible until they ask for `log`/`undo` or the tray says "needs attention".

**Primary consumer:** agent harnesses (opencode, Cursor CLI, custom loops) driving FeanorFS via `feanorfs --json`, `feanorfs-ffi`, or `@feanorfs/agent`. Tasks below are written to be executable by an autonomous coding agent: self-contained motivation, exact file anchors, machine-verifiable acceptance.

**Binding invariants — read before starting any MERK task:**

| # | Invariant |
|---|---|
| I1 | Server stays dumb. Tree/snapshot objects are opaque encrypted blobs indistinguishable from file blobs. The only new server state is an opaque per-workspace head string with compare-and-swap. Never teach the server to parse trees. |
| I2 | E2EE covers structure. Tree objects (filenames, layout, conflict marks) are sealed with `pack_bytes` before upload. Deterministic SIV nonce is required so identical subtrees dedupe in CAS. |
| I3 | No auto-merge, ever. A conflict is *represented* (base/ours/theirs hashes in a tree entry), never resolved by FeanorFS. `conflicts keep` records the resolution as a new snapshot. |
| I4 | JSON contract frozen. Existing structs in `feanorfs_common::agent_contract` and shapes in [agent-api.md](agent-api.md) keep their exact field sets; changes are additive fields or new result structs only. `client/tests/contract_snapshots.rs` must pass unchanged. |
| I5 | No new SQLite (aligns SDK-7). Object cache is plain files under `.feanorfs/objects/`; refs are atomically-written files (`fs_util::atomic_write`). |
| I6 | Crash-safety by construction. All objects immutable; the head ref is the single mutation point; every op must converge when re-run after a crash at any point. |
| I7 | Agent-first, human-legible. The working copy stays plain files — humans inspect, edit, and delete with normal tools; FeanorFS never rewrites a working file with markers or placeholders it didn't already use. Every SDK op keeps a human twin: CLI human output or a tray plain-language action. Conflict resolution for humans stays "edit the file, then `conflicts keep` / tray button". |
| I8 | Not a VCS. Git remains the human's version control. No staging, no branches, no required messages, no snapshot ids a human must type in full. User-facing porcelain grows by exactly two commands (`log`, `undo`) — reject git-shaped feature creep (rebase, cherry-pick, …) even if the model makes it easy. |
| I9 | Bounded bloat. A pass that changes nothing writes zero objects. The local `.feanorfs/objects/` cache obeys the same retention as server GC v2. No new dependencies in any MERK task. |

#### MERK-1 — Tree + snapshot object model (`feanorfs_common`)

Pure data, zero I/O (respect `common/AGENTS.md`: no new heavy deps, no side effects).

- New module `common/src/tree.rs`:
  - `TreeEntry { name, kind, hash, size }` with `kind ∈ {File, Dir, Conflict}`. `Conflict` carries `base/ours/theirs: Option<hash>` (Options cover add/add and edit/delete shapes).
  - `Tree { entries: Vec<TreeEntry> }` — entries sorted by name, canonical byte encoding (length-prefixed fields, no map-ordering ambiguity). Same logical tree ⇒ same bytes ⇒ same hash on every platform.
  - `Snapshot { root, parents: Vec<hash>, author, created_at_ms, message: Option<String> }`, same canonical-encoding rules. An agent's snapshot has `parents = [base, head]` on land — a merge node.
- Converters: flat `HashMap<String, FileState>` → nested trees (split on `/`; inputs already `normalize_path`d) and back. Bottom-up construction so parents can reference child hashes.
- `diff_trees(a, b, fetch)` descending only into subtrees whose hashes differ; `fetch` is a closure so `common` stays I/O-free.
- Property tests: flat↔tree round-trip identity; hash stable under entry insertion order; one-file change in a 10k-path map touches only the ancestor chain of subtrees.

Acceptance: `cargo test -p feanorfs-common` green; no new dependencies; public items documented.

#### MERK-2 — Encrypted object store over the existing CAS (depends: MERK-1)

- New `agent-core/src/objects.rs`: `ObjectStore` sealing tree/snapshot bytes with `pack_bytes(bytes, password, "feanorfs:obj:v1")` — the fixed synthetic path domain-separates object keys from file-blob keys; SIV nonce keeps dedup.
- Object id = blake3 of the **ciphertext**, the same convention as `FileState.hash` — the server cannot tell a tree from a file blob (I1). Upload via existing `POST /api/upload`, fetch via `GET /api/download/:hash`, re-hash ciphertext before `unpack_bytes` (same rule as file blobs). **No new server endpoints in this task.**
- Local write-through cache at `.feanorfs/objects/<hash>` using `fs_util::atomic_write` (I5); reads hit cache first.

Acceptance: client A writes a snapshot chain; client B (fresh dir, same key) resolves head → root → subtrees → file states. A test asserts stored blobs are AEAD-sealed (no plaintext structure). `cargo test -p feanorfs-agent-core` green.

#### MERK-3 — Per-workspace head ref with compare-and-swap (depends: MERK-2)

- `server/src/db.rs`: new table `heads(workspace_id TEXT PRIMARY KEY, snapshot_id TEXT NOT NULL, updated_at)`.
- `server/src/app.rs`: two routes — justified exception to the "reuse `/api/sync/diff`" anti-pattern because CAS on a ref cannot be expressed as a diff; the value stays opaque so the server stays dumb (I1):
  - `GET /api/head?workspace_id=…` → `{ "snapshot_id": string|null }`
  - `PUT /api/head` body `{ workspace_id, expected: string|null, new: string }` → 200 on swap, 409 + current value on mismatch. Existing bearer-token middleware applies unchanged.
- Embedded hub (`agent-core/src/hub.rs`) exposes the same routes via the shared router; `ApiClient` (`agent-core/src/api.rs`) gets `get_head`/`swap_head` for both backends.
- Dual-write phase: snapshot-writing clients keep updating the `files` table through the existing diff/upload flow so old clients and today's GC keep working until MERK-7.

Acceptance: race test — two landers, exactly one 200; loser receives 409 with the winner's id and re-lands cleanly on retry. Existing server tests green; `/api/sync/diff` behavior unchanged.

#### MERK-4 — Agent engine on snapshots: spawn/status/land swap (depends: MERK-3)

The heart of the track, in `agent-core/src/agent.rs`:

- `spawn_agent`: record **one base snapshot id** per agent (ref file under `.feanorfs/agents/`, I5) instead of N `agent_snapshots` rows — subsumes SDK-7(a). Clonefile materialization unchanged.
- `check_agent`/`land_agent`: three-way at tree level — base tree vs agent-workdir tree vs current-head tree, descending only into differing subtrees.
- Land algorithm: build agent tree → upload missing file blobs + tree objects → write snapshot `{root, parents: [base, head]}` → CAS-swap head → on 409, refetch head, recompute three-way, retry (bounded).
- Conflicts (I3, I7): when base≠ours≠theirs the new root records a `Conflict` entry; the working copy and `.feanorfs/conflicts/` artifacts (`.original/.local/.cloud`) stay exactly as today — the conflicted working file is never rewritten with markers, and the human path is unchanged (edit, then `conflicts keep` or the tray button). `AgentLandResult`/`ConcurrentEdit` shapes unchanged plus an additive `snapshot_id` field. Conflicts now survive restarts and are visible from other machines because they live in the tree.
- `conflicts keep` (`agent-core/src/conflicts.rs`): resolution = new snapshot replacing the `Conflict` entry with the chosen blob (I3, I6).
- Crash-safety (I6): objects immutable, head swap last ⇒ AG-29's crash-mid-land scenario converges by construction.

Acceptance: all existing `client/tests/sync_engine.rs` + `contract_snapshots` tests pass unchanged. New tests: (a) 10k-file workspace, touch 1 file, land reads O(tree-depth) objects, not O(n) rows — assert via an object-store read counter; (b) concurrent-land race converges; (c) a conflict entry round-trips through a second client; (d) kill mid-land at three injection points, re-run converges.

#### MERK-5 — Working-copy-as-snapshot + conflict gate on trees (depends: MERK-4)

- Every mutating op (sync pass in `agent-core/src/sync_pass.rs` and `client/src/commands.rs` sync/push/pull, spawn, land, refresh, resolve) first snapshots the current workdir: build tree, write snapshot with parent = previous local head, advance local ref `.feanorfs/refs/workspace` (atomic file, I5). Nothing a user or agent ever had on disk becomes unreachable.
- Watcher integration: snapshot on the debounced (500 ms) sync pass — never per raw fs event (existing anti-pattern).
- No-change passes are free (I9): if the new root hash equals the current local head's root, write no snapshot and no objects.
- Replace `last_synced_files` (the conflict-gate base leg in `agent-core/src/local.rs` / `conflicts.rs`) with the last-synced **snapshot id**; `negotiate_sync_with_conflict_gate` becomes a tree diff. Subsumes the `last_synced_files` half of SDK-7(b).
- `local_files` remains what it truly is: a rebuildable mtime/size→hash cache.

Acceptance: `refresh --replace` and `conflicts keep` are undoable — a test restores pre-op state via the parent snapshot. Conflict-gate tests pass with the table gone. Bulk-touch (DX-25 scenario: branch switch) produces exactly one snapshot. An idle watch loop writes zero new objects.

#### MERK-6 — History surface: log / undo / GC v2 (depends: MERK-5)

- New `Workspace` ops in agent-core: `log(limit)` (walk parents from head) and `undo(snapshot_id)` (materialize that root into the workdir, recorded as a **new** snapshot on top — history is never rewritten).
- Surfaces, all additive (I4): CLI `feanorfs log` / `feanorfs undo <id>` with `LogResult`/`UndoResult` structs in `feanorfs_common::agent_contract` + [agent-api.md](agent-api.md); FFI `ffs_log`/`ffs_undo` (header regen + smoke tests); TS async `log()`/`undo()` + `contract.d.ts`; MCP tool + `events` stream gain snapshot ids so orchestrators can key off state transitions.
- Human output (I7, I8): `log` prints one line per snapshot — short id (8 chars), relative age, author (`you` / agent name), files-changed summary; `undo` accepts short ids and prints what it will restore in plain language. Full hashes and structured detail live behind `--json`. These two commands are the **entire** human porcelain for the snapshot engine.
- GC v2 — the server cannot walk encrypted trees (I1), so: on land/sync the client uploads a **reachability manifest** (opaque newline-delimited blob-hash list for the snapshot's closure, stored beside the head). Server GC = delete blobs not in the union of manifests of retained snapshots; retention = N days or last K snapshots per workspace (config). The local `.feanorfs/objects/` cache is pruned by the same retention (I9). Subsumes GC-7 — history becomes a retention property, not a feature.

Acceptance: extend `examples/sdk-agent-loop.sh`: spawn → agent edits → land → `log` shows the land → `undo` restores pre-land state → `log` shows the undo — all through `--json`/SDK surfaces, no human-output parsing. GC test: unreachable blobs deleted, everything reachable from retained snapshots survives. Contract snapshot tests updated additively.

#### MERK-7 — Migration, cutover, docs (depends: MERK-6)

- `feanorfs migrate` grows a snapshot-engine stage: read the server `files` view, build the initial tree + snapshot locally, upload objects, set head, stamp format v3 in `.feanorfs/config.json` and on the hub.
- Cutover: remove the MERK-3 dual-write; drop `agent_snapshots` and `last_synced_files` tables; keep `local_files` cache only. A v2 client against a v3 head must fail loudly with an actionable error.
- Docs pass (DOX): [agent-api.md](agent-api.md) (new ops + additive fields), [threat-model.md](threat-model.md) — structure/filenames now encrypted at rest on the hub; blob sizes, counts, and access timing remain visible (state this honestly) — root/`agent-core`/`client`/`server` AGENTS.md, and this roadmap (delete the MERK section, fold survivors).

Acceptance: a v2 workspace migrates in place and round-trips; `cargo test --workspace` and the CI `sdk` job green on both OSes; docs updated in the same PR.

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

1. MERK-1 → MERK-7 in order (snapshot engine; subsumes SDK-7a and GC-7; re-scope the SDK-7 remainder after MERK-5). SDK-5b when publishing npm.
2. AG-28..AG-30 (agent edge tests)
3. DX-11, DX-14, DX-23, DX-25
4. SEC-6
5. CONN-6, CONN-7 when hosted tier exists
6. DX-12, CHUNK-* on demand

---

## Key files (for open work)

| Area | Files |
|------|-------|
| Tray (shipped) | `tray/src/main.rs`, `tray/README.md`, `client/src/cli/tray.rs`, `client/src/tray.rs`, `common/src/tray_contract.rs` |
| Merkle engine (MERK) | `common/src/tree.rs` (new), `agent-core/src/objects.rs` (new), `agent-core/src/agent.rs`, `agent-core/src/conflicts.rs`, `agent-core/src/sync_pass.rs`, `server/src/{db,app}.rs`, `client/src/cli/{agent,conflicts,events,mcp}.rs` |
| SDK storage (SDK-7) | `agent-core/src/local.rs`, `client/src/local.rs`, `server/src/db.rs`, `agent-core/src/hub.rs` |
| Agent edge cases | `agent-core/src/agent.rs`, `client/tests/sync_engine.rs` |
| Sync polish | `client/src/commands.rs`, `client/src/conflicts.rs`, `client/src/watch.rs` |
| Crypto cleanup | `common/src/lib.rs`, `client/src/migrate.rs` |
| Server history | `server/src/db.rs`, `server/src/gc.rs` |
| Hosted connect | TBD |
