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
| Agent path | `agent run <name> -- …` | `agent_path(name)` | absolute global worktree path |
| Preview | `agent status <name>` | `status(name)` | `AgentCheckResult` |
| Refresh | `agent refresh <name> [--replace]` | `refresh(name)` | `AgentRefreshResult` |
| Land | `agent land <name>` | `land(name, opts)` | `AgentLandResult` |
| Clean | `agent clean <name>` | `clean(name)` | `AgentCleanResult` |
| Resolve | `conflicts keep <path> …` | `resolve(path, keep, file?)` | exit 0 / FFI `-1` / TS throw |
| History | `log [--limit N]` | `log(limit)` | `LogResult` |
| Undo | `undo <snapshot_id>` | `undo(snapshot_id)` | `UndoResult` |

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
  "message": "Landed 1 path; 1 needs attention.",
  "snapshot_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

### `AgentCommitResult` (legacy alias)

Subset of `AgentLandResult` without `landed` / `message`. It remains exported
from `feanorfs_common` for older library callers and is **not** emitted by
`--json` or SDK bindings. Prefer `AgentLandResult` in new code; no removal is
scheduled.

### `AgentRefreshResult`

```json
{"agent_name":"ci1","refreshed":["README.md"],"deferred":["doc.txt"]}
```

### `AgentCleanResult`

```json
{"cleaned":"ci1"}
```

### `LogResult`

`entries` starts at the current workspace head and walks reachable parents. `changed_paths` compares each snapshot with its first parent.

```json
{"entries":[{"snapshot_id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","parents":[],"author":"you","created_at_ms":1719500000000,"message":"land","changed_paths":["src/main.rs"]}]}
```

### `UndoResult`

Undo accepts a reachable full ID or an unambiguous prefix of at least eight hexadecimal characters. It appends a snapshot instead of moving or deleting history.

```json
{"snapshot_id":"abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789","restored_snapshot_id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","changed_paths":["src/main.rs"]}
```

### `FileState`

```json
{"path":"src/main.rs","hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","size":4096,"mtime":1719500000000,"deleted":false,"mode":1}
```

### `ConcurrentEdit`

Emitted inside `conflicts[]` on land/check when paths overlap:

```json
{
  "path": "src/main.rs",
  "base": { "path": "…", "hash": "…", "size": 0, "mtime": 0, "deleted": false },
  "ours": { "…": "…" },
  "theirs": { "…": "…" },
  "original_file": "~/.feanorfs/workspaces/<id>/conflicts/<ts>/src/main.rs.original",
  "local_file": "~/.feanorfs/workspaces/<id>/conflicts/<ts>/src/main.rs.local",
  "cloud_file": "~/.feanorfs/workspaces/<id>/conflicts/<ts>/src/main.rs.cloud",
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

Under `~/.feanorfs/workspaces/<id>/conflicts/<unix_ms>/`:

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
- `ffs_agent_path(root, name)` returns the existing agent's absolute global
  worktree path without requiring callers to know FeanorFS's private layout.
- `ffs_conflicts_keep`: **0 = success**, **-1 = error**.
- `ffs_log(root, limit)` returns `LogResult` JSON.
- `ffs_undo(root, snapshot_id)` returns `UndoResult` JSON.

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
