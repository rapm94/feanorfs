# Agent SDK JSON contract (SDK-1)

Stable wire format for `feanorfs --json agent …`, `feanorfs-agent-core`, `feanorfs-ffi`, and `@feanorfs/agent`. **Semver policy:** additive fields only in minor releases; renames or removals require a major bump.

Canonical fixtures live in `common/src/agent_contract.rs`. Snapshot tests in `client/tests/contract_snapshots.rs` fail when serialized shapes drift.

---

## Operations

| Operation | CLI | Rust (`Workspace`) | JSON result type |
|-----------|-----|-------------------|------------------|
| List agents | `agent status` | `list()` + CLI-only status enrichment | `AgentListOfflineResult` (SDK), `AgentListResult` (CLI enriched) |
| List agents (legacy) | hidden `agent list` | — | always `AgentListOfflineResult` (plain names, even when online) |
| Spawn | `agent spawn <name>` | `spawn(name, opts)` | `SpawnResult` |
| Preview | `agent status <name>` | `status(name)` | `AgentCheckResult` |
| Refresh | `agent refresh <name>` | `refresh(name)` | `AgentRefreshResult` |
| Land | `agent land <name>` | `land(name, opts)` | `AgentLandResult` |
| Clean | `agent clean <name>` | `clean(name)` | `AgentCleanResult` |
| Resolve | `conflicts keep <path> …` | `resolve(path, keep, file?)` | exit 0 / FFI `-1` / TS throw |

---

## Types

### `SpawnResult`

```json
{"agent":"ci1","files_copied":12}
```

### `AgentListResult` (online, CLI `agent status` only)

```json
{"agents":[{"name":"ci1","state":"2 change(s)"},{"name":"ci2","state":"clean"}]}
```

`state` is a human summary: `"clean"`, `"N change(s)"`, `"N conflict(s)"`, or `"(offline)"`.

### `AgentListOfflineResult`

Plain name list — returned by SDK embeddings (`feanorfs-agent-core`, `feanorfs-ffi`, `@feanorfs/agent`) and by the CLI when the hub is unreachable. Also returned by hidden `agent list` even when online.

```json
{"agents":["ci1","ci2"]}
```

### `AgentCheckResult`

```json
{
  "agent_name": "ci1",
  "our_changes": [{"path":"doc.txt","hash":"…","size":42,"mtime":1719500000000,"deleted":false}],
  "their_changes": [],
  "conflicts": [],
  "conflict_risk": ["notes.md"]
}
```

### `AgentLandResult`

Primary land result type for `--json agent land` and all SDK embeddings.

```json
{
  "agent_name": "ci1",
  "our_changes": [],
  "their_changes": [],
  "conflicts": [],
  "landed": [{"path":"doc.txt","action":"applied"}],
  "message": "Landed 1 path; 1 needs attention."
}
```

### `AgentCommitResult` (legacy alias)

Subset of `AgentLandResult` without `landed` / `message`. Still exported from `feanorfs_common` for older library callers; **not** emitted by `--json` or SDK bindings. Prefer `AgentLandResult` in new code. Removal planned at next major.

### `AgentRefreshResult`

```json
{"agent_name":"ci1","refreshed":["README.md"],"deferred":["doc.txt"]}
```

### `AgentCleanResult`

```json
{"cleaned":"ci1"}
```

### `FileState`

```json
{"path":"src/main.rs","hash":"<64 hex>","size":4096,"mtime":1719500000000,"deleted":false}
```

### `ConcurrentEdit`

Emitted inside `conflicts[]` on land/check when paths overlap:

```json
{
  "path": "src/main.rs",
  "base": { "path": "…", "hash": "…", "size": 0, "mtime": 0, "deleted": false },
  "ours": { "…": "…" },
  "theirs": { "…": "…" },
  "original_file": ".feanorfs/conflicts/<ts>/src/main.rs.original",
  "local_file": ".feanorfs/conflicts/<ts>/src/main.rs.local",
  "cloud_file": ".feanorfs/conflicts/<ts>/src/main.rs.cloud",
  "kind": "edit_edit",
  "local_available": true,
  "cloud_available": true,
  "is_binary": false,
  "hint": "both sides edited since spawn"
}
```

`kind`: `edit_edit` | `edit_delete` | `delete_edit`.

Optional when `--propose`: `proposed_file`, `proposal_clean`.

---

## Conflict artifact layout

Under `.feanorfs/conflicts/<unix_ms>/`:

| File | Role |
|------|------|
| `<path>.original` | Base at spawn (three-way leg) |
| `<path>.local` | Agent workspace version |
| `<path>.cloud` | Server version at land |
| `<path>.proposed` | Diff3 proposal (`land --propose`; never auto-applied) |

Sentinel placeholders mark delete/create conflicts when a leg is absent.

---

## Embeddings

| Language | Crate / package | Transport |
|----------|-----------------|-----------|
| Rust | `feanorfs-agent-core` | Native types + `Runtime` / `Workspace` |
| C / Zig | `feanorfs-ffi` | UTF-8 JSON strings (`feanorfs.h`) |
| TypeScript | `@feanorfs/agent` | napi-rs async native module; typed API in `api.mjs` |

Each FFI / Node call opens the workspace fresh (pass `root` every time; no handle API yet).

See `examples/sdk-agent-loop.sh` (CLI driver) and `examples/zig-agent/` (C ABI).

---

## FFI conventions (`feanorfs-ffi`)

Thread model:

- `ffs_last_error()` is **per-thread**. Errors from one thread are invisible to another.
- Returned `char*` values (including from `ffs_last_error`) must be freed with `ffs_string_free`.
- JSON-returning functions: **NULL = error** (read `ffs_last_error` on the same thread).
- `ffs_conflicts_keep`: **0 = success**, **-1 = error**.

`keep` values for `ffs_conflicts_keep(root, path, keep, file_path)`:

| `keep` | Meaning | `file_path` |
|--------|---------|-------------|
| 0 | keep local | ignored (NULL ok) |
| 1 | keep cloud | ignored |
| 2 | keep both | ignored |
| 3 | keep reconciled file | **required** (UTF-8 path) |

Call `ffs_runtime_init()` once before any other `ffs_*` function.

Panics inside Rust are caught and reported as `"internal panic"` via `ffs_last_error`.

Generated header: `feanorfs-ffi/feanorfs.h` (regenerated on build when signatures change).
