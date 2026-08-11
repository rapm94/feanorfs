# Agent SDK JSON contract (SDK-1)

Stable wire format for `feanorfs --json agent …`, `feanorfs-agent-core`, `feanorfs-ffi`, and `@feanorfs/agent`. **Semver policy:** additive fields only in minor releases; renames or removals require a major bump. The [agent runner section](#agent-runner-cli-only-current-projection) is explicitly excluded from that SDK-1 promise: it documents a current CLI-only projection, not a frozen SDK result type.

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
| Send signal | `agent send <to> --kind <k> [--about <id>] [--reply-to <id>] [--from <name>] <body>` | `send_message(AgentMessageInput)` | `AgentSendResult` |
| Inbox | `agent inbox [--for <name>] [--after <head>] [--limit <n>]` | `inbox(AgentInboxQuery)` | `AgentInboxResult` |
| Local runner control (CLI-only) | `agent runner setup|start|stop|status|reset|remove` | — | current redacted control JSON; not an SDK-1 contract |

---

## Agent runner (CLI-only current projection)

The optional local runner's documented public projection is CLI-only today.
Its current `--json` output is a redacted projection for operators, not a
public `RunnerControlResult` SDK type and not a compatibility guarantee. See
the [operator runbook](usage.md#agent-runner) and [local delivery sequence](agent-communication.md#local-runner-delivery).

Every runner control command returns this current shape. `runner` is `null`
when no runner is configured or after `remove`:

```json
{
  "action": "status",
  "runner": {
    "configured": true,
    "enabled": false,
    "agent": "mac-test",
    "phase": "idle",
    "pending_count": 0,
    "active_message_id": null,
    "active_session_id": null,
    "active_started_at_ms": null,
    "active_spawned_at_ms": null,
    "last_terminal_kind": "result",
    "last_terminal_message_id": "<64-hex-signal-id>",
    "attention": null,
    "updated_at_ms": 1785852000000,
    "inbox_failure_count": 0
  },
  "supervisor": {
    "registered": false,
    "state": "not_installed"
  }
}
```

`action` is one of `setup`, `start`, `stop`, `status`, `reset`, or `remove`.
`phase` is `idle`, `launching`, `running`, or `needs_attention`.
`attention` is `null`, `cursor_reset`, `pending_overflow`,
`ambiguous_execution`, `delivery_unknown`, or `preparation_failed`.
`preparation_failed` means local refresh/preparation failed before a child was
launched; the pending request remains preserved for inspection and explicit
discard/reset. `last_terminal_kind` is `null`, `result`, or `blocked`.
`supervisor.state` is `not_installed`, `running`, or `stopped`. The active
ID/session/timestamps and last terminal ID are each `null` when unavailable.

This projection deliberately excludes the configured command and fixed
arguments, message bodies, child stdout/stderr, and process metadata. The
durable runtime likewise records bounded cursors, IDs, phases, and timestamps
instead of task bodies or child output.

### Runner lifecycle and recovery semantics

The runner admits only direct requests for its configured agent and runs one
fixed command at a time. A child publishes a correlated `result` or `blocked`
signal through the normal message transport; process exit is not a reply. The
runner may publish a generic correlated `blocked` fallback for known launch,
stdin, timeout, cancellation, or exit failures. If it cannot establish one
correlated terminal, it records `delivery_unknown` and stops. A cursor reset,
pending overflow, ambiguous launching/running checkpoint, or local
`preparation_failed` also stops admission. None of these states is replayed
automatically: stop the runner, inspect/repair local state, and use explicit
`reset --discard-pending` (or `remove --discard-pending`) to abandon work.

The supervisor restarts hub, watcher, and runner workers after clean exits or
crashes with bounded backoff. Restarting a runner with a persisted
`launching`/`running` checkpoint marks `ambiguous_execution` instead of
launching that request again. Child ownership is native and cross-platform:
Unix (including macOS) uses a fresh process group with bounded TERM/KILL
teardown; Windows starts children suspended, adopts and verifies a private
kill-on-close Job Object, then resumes them. Timeout, cancellation, and
direct-child exit tear down descendants.

`stop` disables admission before unregistering the runner. If supervisor
authority exists, it waits for a durable workspace-specific registry
reconciliation acknowledgement bound to the live supervisor's exact native
process identity. Fresh or idempotently disabled, unregistered setup/stop with
no supervisor authority has no possible child acknowledgement and skips that
wait. Stale registry, status, or acknowledgement authority remains fail-closed.
This is not an exactly-once
delivery guarantee: inbox reads can redeliver or reset, and an unobserved
terminal remains ambiguous.

### Child invocation and terminal reply

The configured child receives one bounded JSON document on stdin and then EOF:

```json
{
  "schema_version": 1,
  "session_id": "<32-hex-session-id>",
  "agent": "mac-test",
  "message": {
    "message_id": "<64-hex-request-id>",
    "from": "linux-dev",
    "to": "mac-test",
    "kind": "request",
    "body": "Run iOS simulator tests",
    "about_snapshot": "<64-hex-snapshot-id>",
    "reply_to": null,
    "created_at_ms": 1785852000000
  }
}
```

This is `RunnerInvocation` schema version 1. It is current CLI child input,
separate from SDK-1 wire types. The implementation bounds this input, but this
document intentionally does not freeze its internal byte limit.

The child publishes its one terminal through the existing `agent send` / MCP
`agent_send` transport using `AgentMessageInput`; it does not reply on stdout
or stderr. For example:

```json
{
  "to": "linux-dev",
  "kind": "result",
  "body": "Passed 42 tests on iPhone 16 simulator",
  "about_snapshot": "<64-hex-actually-inspected-snapshot-id>",
  "reply_to": "<64-hex-request-id>",
  "from": "mac-test"
}
```

Use `blocked` instead of `result` when appropriate. The terminal must be from
the configured agent to the requester's `from`, reference the request ID in
`reply_to`, and accurately name the inspected snapshot in `about_snapshot`.
The runner can publish a generic correlated `blocked` fallback for known
process/invocation failures; failure to establish a terminal delivery becomes
runner attention rather than a replay.

There is no runner-control MCP tool, C FFI function, TypeScript wrapper, or
`Workspace` convenience method. Existing `agent_send` surfaces carry the
child’s terminal message only.

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

### `AgentSendResult`

One signal publication. `message_id` is the immutable signal snapshot ID;
`about_snapshot` is the caller-selected context (defaults to the head observed
when sending started). Sending appends one encrypted no-file-change snapshot
whose parent is the latest head and whose root equals the latest head's root.

```json
{"message_id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","about_snapshot":"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"}
```

### `AgentMessage`

One typed signal. `kind`: `request` | `status` | `result` | `blocked`.
`reply_to`, when present, references another signal snapshot.

```json
{"message_id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","from":"linux-dev","to":"mac-test","kind":"request","body":"Run iOS simulator tests","about_snapshot":"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210","reply_to":null,"created_at_ms":1785852000000}
```

### `AgentInboxResult`

`cursor` is the workspace head observed by the read; pass it back as
`after` to read the graph delta. `cursor_reset=true` means the supplied cursor
was unreachable, the scan bound was exhausted, or the requested result limit
omitted older matches. Older signals may therefore have been missed; the
returned view is bounded. Reads may redeliver and never publish
acknowledgements.

```json
{"cursor":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","cursor_reset":false,"messages":[{"message_id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","from":"linux-dev","to":"mac-test","kind":"request","body":"Run iOS simulator tests","about_snapshot":"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210","reply_to":null,"created_at_ms":1785852000000}]}
```

Input shapes (same names across Rust/FFI/TS):

```json
{"to":"mac-test","kind":"request","body":"Run iOS simulator tests","about_snapshot":null,"reply_to":null,"from":"linux-dev"}
```

```json
{"recipient":"mac-test","after":null,"limit":50}
```

Validation: `from`/`to` are agent names (`to="*"` broadcasts); CLI and MCP
callers default the sender to `FEANORFS_AGENT`, then `human`, while embeddings
use their explicit `from` value or `human`; `body` is non-empty UTF-8 after
trimming and at most 8 KiB; `about_snapshot`/`reply_to` must be full reachable
snapshot IDs, with `reply_to` required to reference a real signal. The
envelope wire format is `ffmsg1:` plus canonical compact JSON in
`Snapshot.message`; see [agent-communication.md](agent-communication.md).

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
- `ffs_agent_send(root, input_json)` takes an `AgentMessageInput` JSON string
  and returns an `AgentSendResult` JSON string.
- `ffs_agent_inbox(root, query_json)` takes an `AgentInboxQuery` JSON string
  and returns an `AgentInboxResult` JSON string.

## TypeScript (`@feanorfs/agent`)

Typed wrappers in `api.mjs` over the native module:

- `sendMessage(root, input)` → `AgentSendResult` (input is `AgentMessageInput`).
- `inbox(root, query)` → `AgentInboxResult` (query is `AgentInboxQuery`).

Low-level JSON variants `agentSend(root, inputJson)` and
`agentInbox(root, queryJson)` are exported by the native module. Declarations
live in `contract.d.ts`; shapes match this document and
`common/src/agent_contract.rs`.

## MCP

`agent_send` and `agent_inbox` delegate to agent-core:

- `agent_send(from?, to, kind, body, about_snapshot?, reply_to?)` with
  `kind` restricted to `request | status | result | blocked` and `body`
  bounded at 8192 UTF-8 bytes (the JSON schema's `maxLength` is an additional
  character-count preflight; agent-core enforces the byte limit).
- `agent_inbox(for?, after?, limit?)` with `limit` bounded 0–1000.

Tool descriptions state that all workspace participants can read messages,
identity is advisory, and requests/results should carry exact snapshot context.
The NDJSON `events` stream emits `agent_message` wakeup records (bounded
metadata, never the body) when new signals appear. If the bounded inbox read
resets its cursor or truncates an overflow, it emits a separate metadata-only
`agent_message_cursor_reset` record before the returned wakeups:

```json
{"event":"agent_message_cursor_reset","cursor":"<observed-workspace-head>","cursor_reset":true}
```

The reset record contains no message body, ID, routing, path, or integrator
fields; consumers should treat it as evidence that older wakeups may be
missing and re-read the typed inbox as needed.

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
---

## Randomized integrator assignment (SDK-1 additive)

Canonical types and logic live in `common/src/integrator_contract.rs`
(selection, ranking, `ffint1` profiles, digest bounds) and
`agent-core/src/integrator.rs` (state machine, persistence, materialization).
Adapters (CLI, FFI, TypeScript, MCP) are thin wrappers over the same
canonical implementation.

### Operations

| Operation | CLI | Rust (`Workspace`) | JSON result type |
|-----------|-----|-------------------|------------------|
| Assign | `agent integrator assign --about <id> --candidate <name>… [--require <cap>…] [--exclude <name>…] [--exclude-author <name>…] [--ack-timeout <d>] <task>` | `integrator_assign(IntegratorAssignInput)` | `IntegratorAssignResult` |
| Status | `agent integrator status [<assignment-id>]` | `integrator_status(Option<&str>)` | `IntegratorStatusResult` |
| Revoke | `agent integrator revoke <id> --reason <summary>` | `integrator_revoke(id, reason)` | `IntegratorStatusResult` |
| Resume | `agent integrator resume [--ack-timeout <d>] [--fallback-on-blocked]` | `integrator_resume(IntegratorObserveOptions)` | `IntegratorObserveResult` |
| Materialize | `conflicts materialize [--about <id>] [--path <p>]…` | `materialize_conflicts(about, paths)` | `ConflictMaterializeResult` |

MCP tools: `integrator_assign`, `integrator_status`, `integrator_revoke`,
`integrator_resume`, `conflict_materialize`. FFI: `ffs_integrator_assign`,
`ffs_integrator_status`, `ffs_integrator_revoke`, `ffs_integrator_resume`,
`ffs_conflict_materialize`. TypeScript: `integratorAssign`, `integratorStatus`,
`integratorRevoke`, `integratorResume`, `conflictMaterialize`.

### `IntegratorAssignInput`

```json
{
  "about_snapshot": "<64-hex>",
  "candidates": [
    {"name":"mac-test","capabilities":["ios","rust"],"enabled":true,"available":true}
  ],
  "required_capabilities": ["rust"],
  "conflict_authors": ["agent-a"],
  "excluded": [],
  "task_summary": "Integrate parser implementation and tests",
  "ack_timeout_ms": 300000
}
```

Validation: names use FeanorFS agent-name rules; capabilities are lowercase
ASCII identifiers (≤ 32 bytes, ≤ 64 per list); rosters are bounded at 64
candidates; duplicate names/capabilities are rejected; `about_snapshot` must
be a full reachable format-v3 snapshot id; `task_summary` is bounded at 1024
bytes.

### `IntegratorAssignResult`

```json
{
  "assignment_id": "0123456789abcdef0123456789abcdef",
  "about_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "selected": "agent-b",
  "fallback_order": ["agent-a"],
  "neutral_integrator": true,
  "roster_fingerprint": "26a359d7aceb46c7bfa48880140bf6624163e47098d2478cb8ee43f32408d9d1",
  "attempt": 0,
  "request_message_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "state": "offered",
  "task_summary": "Integrate parser implementation and tests"
}
```

`assignment_id` is 128 bits of OS CSPRNG (32 hex); `selection_nonce` is 256
bits (64 hex). The ranking is

```text
score = BLAKE3("feanorfs-integrator-selection-v1" ‖ len(workspace_id) ‖
       workspace_id ‖ about_snapshot ‖ assignment_id ‖ selection_nonce ‖
       roster_fingerprint ‖ len(agent_name) ‖ agent_name)
```

every variable-width value length-prefixed, sorted ascending by the 32-byte
score with agent-name bytes as the collision tie-breaker. The first candidate
is selected; the rest is the immutable fallback order. `roster_fingerprint`
is the Blake3 of the canonical JSON array of the sorted final pool. The
workspace id never leaves the trusted client process. Fixed nonces are
injectable only through test/internal APIs; production always uses the OS
CSPRNG.

### `IntegratorStatusResult`

```json
{
  "assignment_id": "0123456789abcdef0123456789abcdef",
  "about_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "state": "offered",
  "selected": "agent-b",
  "attempt": 0,
  "neutral_integrator": true,
  "roster_fingerprint": "26a359d7aceb46c7bfa48880140bf6624163e47098d2478cb8ee43f32408d9d1",
  "fallback_order": ["agent-a"],
  "task_summary": "Integrate parser implementation and tests",
  "created_at_ms": 1785852000000,
  "updated_at_ms": 1785852000000,
  "attempts": [
    {"attempt":0,"selected":"agent-b","state":"offered","offered_at_ms":1785852000000,
     "request_message_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
     "terminal_message_id":null,"reason":null}
  ],
  "digest": null,
  "inbox_cursor": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
}
```

`state` transitions: `created → offered → accepted → active → completed |
blocked`; `accepted → revoked → offered(next)`; `offered → timed_out →
offered(next)`. Terminal states are `completed`, `blocked`, `requires_human`,
and `cancelled`. A timed-out pre-acceptance attempt is terminal for that
candidate only. Post-acceptance, timeout alone never activates a fallback:
the dispatcher must stop/revoke the accepted integrator or receive a
`blocked` reply first.

### `IntegratorObserveResult` (`integrator resume`)

```json
{"assignment_id":"0123456789abcdef0123456789abcdef","state":"accepted",
 "messages_processed":1,"cursor":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
 "cursor_reset":false,"action":"accepted"}
```

`cursor_reset=true` fails closed into `requires_human`; automatic mutation
stops until state is recovered. Resume never re-sends a recorded request.

### `IntegratorDigest` and bounds

```json
{
  "assignment_id": "0123456789abcdef0123456789abcdef",
  "integrator": "agent-b",
  "about_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "inspected_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "state": "completed",
  "landed_paths": 12,
  "resolved_conflicts": 3,
  "remaining_conflicts": 0,
  "verification": {"status": "passed", "summary": "84 tests passed"},
  "outcome": "Integrated parser implementation and tests.",
  "risks": [],
  "decision_required": null
}
```

Bounds: outcome ≤ 512 UTF-8 bytes; verification summary ≤ 512 bytes; risks
≤ 10 entries of 256 bytes; at most one decision question of 512 bytes; paths
are counts by default. No patch, file content, raw log, or reasoning field
exists.

### `ConflictMaterializeResult` (`conflicts materialize`)

Read-only materialization of first-class encrypted conflict entries:

```json
{
  "about_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "conflict_dir": "~/.feanorfs/workspaces/<opaque>/conflicts/materialize_1719500000000",
  "entries": [
    {"path":"src/main.rs","kind":"edit_edit","original_available":true,
     "local_available":true,"cloud_available":true,"is_binary":false,
     "already_materialized":false}
  ]
}
```

Artifacts `.original`/`.local`/`.cloud` are written only under protected
global FeanorFS state with existing deletion sentinels; a local pending row
is registered without publishing a new head; stale or already-resolved
conflicts are refused; project files and Git/Jujutsu state are untouched.

### `ffint1` profiles

Assignment/reply profiles travel inside ordinary `ffmsg1` bodies and use the
existing `request`/`status`/`result`/`blocked` kinds:

```text
ffint1:{"type":"assignment","assignment_id":"0123456789abcdef0123456789abcdef","attempt":0,"selected":"agent-b","about_snapshot":"…","roster_fingerprint":"…","neutral_integrator":true,"task":"Integrate parser implementation and tests"}
```

`accepted` (status), `result` (with a digest), and `blocked` profiles carry
the same `assignment_id`/`attempt`; terminal replies reference the original
assignment request through `reply_to`. Unknown `ffint` versions remain
ordinary signal text and cannot break typed inbox reads.
