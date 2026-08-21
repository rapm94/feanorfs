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

---

## Live continuous status (SDK-1 additive)

`agent status <name>` returns the frozen `AgentCheckResult` shape plus an
optional additive `live` projection, present only while a controller actively
owns the agent (`agent run` lifetime or an enabled configured runner):

```json
{
  "schema_version": 1,
  "agent": "worker",
  "active": true,
  "phase": "idle",
  "observed_head": "<64-hex>",
  "observed_tree": "<64-hex>",
  "settled_snapshot": "<64-hex>",
  "pending_local": false,
  "deferred_count": 0,
  "attention": null,
  "owner_pid": 1234,
  "owner_start_id": "…",
  "updated_at_ms": 1719500000000
}
```

- `phase` is one of `starting`, `idle`, `local_dirty`, `reconciling_local`,
  `refreshing_remote`, `offline`, `needs_attention`, `stopping`. The same
  transition/error table drives `agent run`, runner workers, `agent status`,
  events, and tests; it never grants semantic merge authority.
- `attention` is `{reason, detail}` with reasons `pending_conflicts`,
  `unsafe_path`, `corrupt_state`, `unsupported_schema`, `ownership_lost`.
- The projection is bounded, secret-free, and persisted atomically under the
  agent's private state directory (`continuous-status.json`, ≤ 64 KiB).
  Readers ignore projections written by newer schemas and treat a projection
  whose lease owner is dead as absent.
- A settled snapshot is the latest reachable snapshot carrying the settled
  tree; agents must reference it exactly in `result` signals. The runner
  rejects a result for an earlier/pre-final snapshot and accepts a correlated
  `blocked` terminal when final reconciliation is offline or needs attention.
  Signal-only heads change `observed_head` but keep `observed_tree` and the
  existing `settled_snapshot`, so publishing a terminal does not invalidate
  the exact snapshot it names.

Routine tray refreshes read the worker-published `WorkerStatusSnapshot`,
whose additive `continuous` field aggregates live/attention/offline counts
without scanning worktrees.

## Live reconciliation events (NDJSON)

`feanorfs events` emits metadata-only lifecycle records projected from the
bounded status files:

```json
{"event":"agent_reconcile_started","agent":"worker","phase":"reconciling_local"}
{"event":"agent_reconciled","agent":"worker","settled_snapshot":"<64-hex>"}
{"event":"agent_reconcile_deferred","agent":"worker","deferred_count":2}
{"event":"agent_reconcile_attention","agent":"worker","reason":"pending_conflicts"}
```

These are current CLI-only projections (like the runner surface), not frozen
SDK-1 result types. They carry no message bodies, file contents, credentials,
or endpoints. Head-change wakeups arrive through the same bounded head
observer as the watcher and runner; on old hubs the 30-second window remains
the recovery backstop.

---

## Encrypted work-intent protocol (SDK-1 additive)

Canonical types and logic live in `common/src/work_contract.rs` (`ffwork1`
profiles, bounds, canonical encode/parse, pure overlap evaluation, typed
transition rejection) and `agent-core/src/work.rs` (deterministic reducer,
private `work-state.json` projection). Adapters (CLI, FFI, TypeScript, MCP)
are thin wrappers over the same canonical implementation. The hub never
decides; there is no server route, relay metadata, or plaintext index.

### Operations

| Operation | CLI | Rust (`Workspace`) | JSON result type |
|-----------|-----|-------------------|------------------|
| Propose | `agent work propose --task <id> --path <p>… [--agent <name>] [--sequence <n>] [--causal-base <id>] [--coordinator <name>] [--concern <c>]… [--dependency <task>]… [--capability <cap>]…` | `work_propose(WorkProposeInput)` | `WorkSendResult` |
| Decide | `agent work decide <proposal-message-id> --kind accept|reject|narrow|order|accept-overlap [--reason <r>] [--path <p>]… [--concern <c>]… [--after <id>] [--overlap <json>]…` | `work_decide(WorkDecideInput)` | `WorkSendResult` |
| Amend | `agent work amend --task <id> --intent <id> [--path <p>]… [--concern <c>]… [--dependency <task>]… [--reason <r>]` | `work_amend(WorkAmendInput)` | `WorkSendResult` |
| Yield | `agent work yield --task <id> --intent <id> [--reason <r>]` | `work_yield(WorkYieldInput)` | `WorkSendResult` |
| Settle | `agent work settle --task <id> --intent <id> --inspected <snapshot> --verification passed|failed|skipped --summary <text>` | `work_settle(WorkSettleInput)` | `WorkSendResult` |
| Complete | `agent work complete --task <id> --intent <id> --outcome <text>` | `work_complete(WorkCompleteInput)` | `WorkSendResult` |
| Block | `agent work block --task <id> --intent <id> --reason <text>` | `work_block(WorkBlockInput)` | `WorkSendResult` |
| Status | `agent work status [--coordinator <name>]` | `work_status(WorkStatusInput)` | `WorkStatusResult` |

MCP tools: `work_propose`, `work_decide`, `work_amend`, `work_yield`,
`work_settle`, `work_complete`, `work_block`, `work_status`. FFI:
`ffs_work_propose`, `ffs_work_decide`, `ffs_work_amend`, `ffs_work_yield`,
`ffs_work_settle`, `ffs_work_complete`, `ffs_work_block`, `ffs_work_status`.
TypeScript: `workPropose`, `workDecide`, `workAmend`, `workYield`,
`workSettle`, `workComplete`, `workBlock`, `workStatus`.

Send operations construct ordinary `AgentMessageInput` envelopes and publish
through the existing `ffmsg1` signal channel (`agent send`). They never mutate
local projection state: `agent work status` (observe + reducer) applies state
after reading signals since the persisted cursor. A sent proposal is never a
claim of acceptance — only an observed coordinator decision applies the
accepted scope. Human output states this explicitly.

### `WorkSendResult`

```json
{
  "message_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "about_snapshot": "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
  "task_id": "parser-impl",
  "agent": "linux-dev",
  "profile": "work_intent",
  "state": "proposed",
  "scope": {"paths": ["src/parser.rs"], "concerns": ["parser behavior"], "dependencies": []},
  "causal_refs": [],
  "overlap": [],
  "projection_incomplete": false
}
```

`state` is the state the sent profile expresses *when applied by the
reducer*; a `work_intent` result always reports `proposed` until an observed
decision changes it. `causal_refs` are the exact message ids the profile
references (causal base, intent, proposal, ordering, superseded decision).
`overlap` carries explicitly accepted overlap entries for
`accept-overlap` decisions only.

### `WorkStatusResult`

```json
{
  "cursor": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "cursor_reset": false,
  "projection_incomplete": false,
  "messages_processed": 4,
  "tasks": [
    {
      "task_id": "parser-impl",
      "state": "accepted",
      "proposals": [
        {
          "agent": "linux-dev",
          "state": "accepted",
          "sequence": 1,
          "intent_message_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          "coordinator": "human",
          "accepted_scope": {"paths": ["src/parser.rs"], "concerns": ["parser behavior"], "dependencies": []},
          "decision": {"message_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "coordinator": "human", "kind": {"kind": "accept"}},
          "accepted_overlap": [],
          "amendments": [],
          "causal_refs": [],
          "inspected_snapshot": null,
          "verification": null,
          "outcome": null,
          "reason": null,
          "source_message_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
          "updated_at_ms": 1785852000000
        }
      ]
    }
  ],
  "evidence_count": 0,
  "dropped_count": 0,
  "updated_at_ms": 1785852000000
}
```

`projection_incomplete=true` means the closure was truncated (cursor reset or
bound exhaustion): acceptance is not fully provable and the reducer never
infers it. `evidence_count` counts retained protocol evidence (losing
branches, invalid transitions, superseded decisions). `dropped_count` counts
transitions dropped by bound exhaustion. Task state derives from the
highest-priority proposal state; terminal states dominate.

### `ffwork1` profiles

Profiles travel inside ordinary `ffmsg1` bodies using the existing
`request`/`status`/`result`/`blocked` kinds:

```text
ffwork1:{"type":"work_intent","task_id":"parser-impl","agent":"linux-dev","sequence":1,"coordinator":"human","paths":["src/parser.rs","tests/parser.rs"],"concerns":["parser behavior"],"dependencies":[],"capabilities":["rust"]}
```

Variant tags: `work_intent`, `work_decision` (with `kind` accept | reject |
narrow | order | accept_overlap), `work_amendment`, `work_yield`,
`work_settled`, `work_completed`, `work_blocked`, `work_superseded`. All
variants are `deny_unknown_fields`; collections must be sorted and unique;
paths must be canonical portable workspace-relative paths or the supported
`dir/**` containment glob; unknown `ffwork` versions remain ordinary signal
text and cannot break typed inbox reads.

---

## Exact conflict resolution (SDK-1 additive)

Canonical types and logic live in `common/src/resolution_contract.rs`
(versioned canonical `ConflictIdentity`, the byte-exact domain-separated
Blake3 fingerprint, the bounded `ResolutionJob`, the closed
`ResolutionResult` outcomes, the bounded human escalation reasons, and the
bounded verification policy reference) and `agent-core/src/resolution.rs`
(prepare/submit/apply engine operations under the protected orchestrator
boundary). Adapters must never reimplement identity canonicalization,
fingerprinting, or result validation. The hub never merges file content; a
harness produces a candidate, and only the engine validates/publishes it.

### Operations

| Operation | CLI | Rust (`Workspace`) | JSON result type |
|-----------|-----|-------------------|------------------|
| Prepare | `agent resolution prepare <path> --reason exhausted\|violated --detail <text>` | `resolution_prepare(path, PreventionReason)` | `ResolutionJob` |
| Status | `agent resolution status [<job-id>]` | `resolution_status(Option<&str>)` | `ResolutionStatusProjection` |
| Submit | `agent resolution submit <job-id> --result <file-or->` | `resolution_submit(job_id, ResolutionResult)` | `ResolutionResult` |
| Apply | `agent resolution apply <job-id>` | `resolution_apply(job_id)` | `ResolutionApplyOutcome` |
| Materialize | `agent resolution materialize <job-id>` | `resolution_materialize_legs(job_id)` | `[{"role","path"}]` |
| Put | `agent resolution put <job-id> <file>` | `resolution_put_candidate(job_id, bytes)` | `CandidateDescriptor` |
| Answer | `agent resolution answer <job-id> --defer\|--keep-unresolved\|--candidate <file>` | `resolution_answer(HumanResolutionAnswer)` | `HumanResolutionAnswer` |
| Defer | `agent resolution defer <job-id>` | `resolution_defer(job_id)` | `null` |
| Protocol status | `agent resolution protocol-status [--rebuild]` | `resolution_protocol_status(rebuild)` | `ResolutionProtocolStatus` |
| Assign | `agent resolution assign <job-id>` | `resolution_assign(job_id)` | `{"message_id"}` |
| Reply | `agent resolution reply <job-id>` | `resolution_reply(job_id)` | `{"message_id"}` |
| Revoke | `agent resolution revoke <job-id> [--superseded]` | `resolution_revoke(job_id, superseded)` | `{"message_id"}` |
| Publish answer | `agent resolution publish-answer <job-id> --defer\|--keep-unresolved\|--candidate <file>` | `resolution_publish_answer(&HumanResolutionAnswer)` | `{"message_id"}` |

MCP tools: `resolution_prepare`, `resolution_status`, `resolution_submit`,
`resolution_apply`, `resolution_materialize`, `resolution_put`,
`resolution_answer`, `resolution_defer`, `resolution_protocol_status`,
`resolution_assign`, `resolution_reply`, `resolution_revoke`,
`resolution_publish_answer`. FFI: `ffs_resolution_prepare`,
`ffs_resolution_status`, `ffs_resolution_submit`, `ffs_resolution_apply`,
`ffs_resolution_materialize`, `ffs_resolution_put`,
`ffs_resolution_answer`, `ffs_resolution_defer`,
`ffs_resolution_protocol_status`, `ffs_resolution_assign`,
`ffs_resolution_reply`, `ffs_resolution_revoke`,
`ffs_resolution_publish_answer`. TypeScript:
`resolutionPrepare`, `resolutionStatus`, `resolutionSubmit`,
`resolutionApply`, `resolutionMaterialize`, `resolutionPut`,
`resolutionAnswer`, `resolutionDefer`, `resolutionProtocolStatus`,
`resolutionAssign`, `resolutionReply`, `resolutionRevoke`,
`resolutionPublishAnswer`. NDJSON events: `resolution_prepared`,
`resolution_submitted`, `resolution_applied`, `resolution_revoked`,
`resolution_assigned`, `resolution_result_received`,
`resolution_human_answered` (metadata-only ids/state/counts wakeups; never
paths or bodies).

Prepare requires a real current conflict in the workspace head and a typed
prevention-exhausted/violated reason; legacy unfingerprinted conflicts are
refused. Prepare and submit never mutate the worktree, conflict registry,
artifacts, or head. **Submit never applies**; apply is the only operation
that publishes, revalidating every identity field and the candidate
descriptor immediately before a single CAS (a lost CAS restarts complete
validation). Every apply result that is not `published` leaves the current
conflict untouched.

### `ResolutionJob`

```json
{
  "schema_version": 1,
  "job_id": "fedcba9876543210fedcba9876543210",
  "task_id": "parser-impl",
  "assignment_id": "0123456789abcdef0123456789abcdef",
  "attempt": 0,
  "workspace_id": "fixture-workspace",
  "owner": "agent-b",
  "conflict": {
    "schema_version": 1,
    "workspace_id": "fixture-workspace",
    "current_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "about_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "tree_root": "4444444444444444444444444444444444444444444444444444444444444444",
    "path": "src/main.rs",
    "base": {"present": true, "deleted": false, "hash": "1111111111111111111111111111111111111111111111111111111111111111", "size": 120, "mode": 0},
    "ours": {"present": true, "deleted": false, "hash": "2222222222222222222222222222222222222222222222222222222222222222", "size": 121, "mode": 0},
    "theirs": {"present": true, "deleted": false, "hash": "3333333333333333333333333333333333333333333333333333333333333333", "size": 122, "mode": 0},
    "kind": "edit_edit",
    "task_id": "parser-impl",
    "intent_message_ids": ["5555555555555555555555555555555555555555555555555555555555555555", "6666666666666666666666666666666666666666666666666666666666666666"],
    "assignment_id": "0123456789abcdef0123456789abcdef",
    "attempt": 0,
    "designated_owner": "agent-b",
    "verification_policy": "feanorfs-inline-verify-v1"
  },
  "conflict_fingerprint": "6b2f68617bf943514b164d5d85c92437bb92ded7405b436631ea569cf1239553",
  "current_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "about_snapshot": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "tree_root": "4444444444444444444444444444444444444444444444444444444444444444",
  "accepted_intents": ["5555555555555555555555555555555555555555555555555555555555555555", "6666666666666666666666666666666666666666666666666666666666666666"],
  "causal_refs": ["7777777777777777777777777777777777777777777777777777777777777777"],
  "artifacts": [
    {"role": "original", "path": "conflicts/1/src/main.rs.original"},
    {"role": "local", "path": "conflicts/1/src/main.rs.local"},
    {"role": "cloud", "path": "conflicts/1/src/main.rs.cloud"}
  ],
  "candidate_destination": {"path": "orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/candidate-0.bin", "create_new": true},
  "allowed_output_paths": ["src/main.rs"],
  "verification": {"policy_id": "feanorfs-inline-verify-v1", "command_config_ref": "feanorfs-resolver-inline-config-v1", "timeout_ms": 600000, "freshness_required": true},
  "prevention": {"type": "exhausted", "detail": "no bounded prevention path remains for this conflict"},
  "last_resort_reason": "no bounded prevention path remains for this conflict",
  "designation": {
    "method": "causal_eligible",
    "eligible": ["agent-b"],
    "ranked": [],
    "reasoning": "agent-b authored the causally older accepted intent",
    "attempt": 0
  }
}
```

`designation` is engine-computed evidence persisted in the immutable job:
the designated owner must appear in `eligible` and the selection is auditable
from transitive causal ancestry (or, only for a documented tie or
unavailable-owner fallback, from a deterministic `ffint1` ranking whose
`nonce` and `roster_fingerprint` are recorded too).

`conflict_fingerprint` is the byte-exact Blake3 fingerprint of `conflict`
(including the automatic-resolution block); adapters must never recompute it
differently. `candidate_destination` is engine-owned and create-new: the
harness creates the immutable candidate there and never overwrites it.

### `ResolutionResult`

```json
{
  "schema_version": 1,
  "outcome": "candidate_ready",
  "job_id": "fedcba9876543210fedcba9876543210",
  "assignment_id": "0123456789abcdef0123456789abcdef",
  "attempt": 0,
  "owner": "agent-b",
  "conflict_fingerprint": "6b2f68617bf943514b164d5d85c92437bb92ded7405b436631ea569cf1239553",
  "candidate": {"path": "orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/candidate-0.bin", "hash": "<64-hex-plaintext-hash>", "size": 18, "mode": 0, "deleted": false},
  "verification": {"status": "passed", "summary": "fixture verification passed", "policy_version": 0, "input_hashes": [], "checks": []},
  "diagnostics": [],
  "question": null,
  "human_reason": null,
  "question_generation": 0,
  "safe_options": []
}
```

`verification` records actual fixed-policy evidence produced by the engine
when it executes the inline verification policy — never a caller assertion
plus a submission timestamp (the fixture example carries the default empty
evidence block; real submissions fill `policy_id`/`input_hashes`/
`output_hash`/`checks`). A `requires_human` result carries
`question_generation` and at least one typed safe option
(`defer` or `keep_unresolved`).

`outcome` is a closed set: `candidate_ready`, `no_change_required`, `blocked`,
`requires_human`, `failed`, `stale`. A `candidate_ready` result requires a
candidate descriptor and passed verification; a `requires_human` result
carries exactly one bounded `question` and one typed `human_reason`
(`semantic_ambiguity`, `unavoidable_data_loss`,
`missing_or_auth_failed_leg`, `security_compatibility_boundary_change`,
`required_verification_unavailable`, `indeterminate_ownership`,
`bounded_resolver_exhaustion`, `unsupported_size_safety_bound`,
`explicit_product_decision`). Offline conditions, first timeouts,
signal-only heads, stale candidates, and ordinary lost CAS are never human
reasons.

### `ResolutionStatusProjection`

Metadata-only ids/state/counts projection (never paths, identities, or
bodies), sorted by `created_at_ms`:

```json
{"schema_version": 1, "jobs": [{"job_id": "fedcba9876543210fedcba9876543210", "assignment_id": "0123456789abcdef0123456789abcdef", "attempt": 0, "owner": "agent-b", "conflict_fingerprint": "6b2f68617bf943514b164d5d85c92437bb92ded7405b436631ea569cf1239553", "assignment_state": "active", "outcome": "candidate_ready", "question_generation": 0, "created_at_ms": 1785852000000, "verified_at_ms": 1785852005000}]}
```

`assignment_state` is `active`, `revoked`, `superseded`, or `completed`.
`question_generation` is the monotonic per-fingerprint generation of the
escalation the job carries (0 when no question was ever recorded); every
human answer must reference the exact generation.

### `ResolutionApplyOutcome`

```json
{"outcome": "published", "head": "<new-workspace-head>"}
```

```json
{"outcome": "stale", "kind": "head_changed", "diagnostics": ["workspace head changed since preparation (expected …, found …)"]}
```

`kind` is a closed stale set: `head_changed`, `conflict_missing`,
`legs_changed`, `identity_mismatch`, `assignment_revoked`,
`verification_expired`, `candidate_missing`, `candidate_hash_mismatch`,
`candidate_size_mismatch`, `candidate_mode_mismatch`,
`candidate_path_mismatch`, `candidate_symlink`. Any stale outcome leaves the
current conflict and its evidence untouched.

### `resolution materialize`

Materializes the authenticated base/ours/theirs legs of one job into the
engine-owned job directory (create-new, no-follow, fsync'd) so a designated
machine can reconstruct the conflict context by ID and fingerprint.
Read-only: never changes the worktree, conflict registry, artifacts, or
head. JSON out is an array of `{role, path}` with absolute paths:

```json
[{"role": "original", "path": "<state-root>/orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/legs/original"}, {"role": "local", "path": "<state-root>/orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/legs/local"}, {"role": "cloud", "path": "<state-root>/orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/legs/cloud"}]
```

`role` is `original`, `local`, or `cloud`. Adapter `put` inputs are bounded:
the FFI/napi base64 parameter is adapter-bound (1 MiB input) and the engine
rejects any candidate over 64 MiB plaintext.

### `resolution put`

Writes the immutable engine-owned candidate file for one job from a bounded
local file (create-new, no-follow, fsync'd) and returns its plaintext
descriptor. Allowed while the job is active and carries no candidate-bearing
result:

```json
{"path": "orchestrator/resolution/jobs/fedcba9876543210fedcba9876543210/candidate-0.bin", "hash": "<64-hex-plaintext-hash>", "size": 18, "mode": 0, "deleted": false}
```

The descriptor is a `CandidateDescriptor`; the engine re-validates the
descriptor against the immutable file on every later operation. FFI:
`ffs_resolution_put(root, job_id, base64_json)` takes base64 bytes (a JSON
string document or bare base64); napi `resolutionPut(root, jobId, base64)`
takes base64 directly; MCP `resolution_put` takes `{job_id, base64}`.

### `resolution answer` and `resolution publish-answer`

Both commands build one typed `HumanResolutionAnswer` bound to the exact
current escalation. Every identity field (job, assignment, attempt,
fingerprint, and the exact `question_generation`) is read from the bounded
`resolution status` projection — the caller never supplies them, so stale
answers are impossible by construction (the engine re-validates the full
binding, including the generation). `--candidate <file>` reads the file
bounded (64 MiB cap) and records the engine-owned candidate via
`put_resolution_candidate` first.

```json
{"schema_version": 1, "job_id": "fedcba9876543210fedcba9876543210", "assignment_id": "0123456789abcdef0123456789abcdef", "attempt": 0, "conflict_fingerprint": "6b2f68617bf943514b164d5d85c92437bb92ded7405b436631ea569cf1239553", "question_generation": 1, "chosen_option": "defer", "candidate": null, "verification": null}
```

`chosen_option` is `defer`, `keep_unresolved`, or `submit_candidate`; a
`submit_candidate` answer carries the engine-validated candidate descriptor
and (for the local `answer` op) verification evidence produced by the
engine's inline verification path. `answer` records the terminal local state
or a `candidate_ready` result without any publication; `publish-answer`
validates the answer (`validate_human_resolution_answer`) and sends it as an
`ffres1` profile — the local store is never mutated by publication, and a
published `submit_candidate` answer carries an explicit `Unknown`
verification status unless the caller supplies real evidence. FFI:
`ffs_resolution_answer(root, answer_json)` /
`ffs_resolution_publish_answer(root, answer_json)` take the full
`HumanResolutionAnswer` JSON (the engine validates the binding); napi and
MCP mirror those shapes.

### `resolution defer`

Records the terminal `Deferred` state for one assignment without any
publication; the conflict is preserved for later manual action. JSON out is
`null`.

### `resolution protocol-status`

Observes the encrypted signal stream through the deterministic `ffres1`
reducer and returns the bounded metadata-only projection (ids/state/counts
only; never paths or bodies). `--rebuild` resets the cursor and re-observes
the bounded window:

```json
{"schema_version": 1, "cursor": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "projection_incomplete": false, "entries": [{"conflict_fingerprint": "6b2f68617bf943514b164d5d85c92437bb92ded7405b436631ea569cf1239553", "job_id": "fedcba9876543210fedcba9876543210", "assignment_id": "0123456789abcdef0123456789abcdef", "attempt": 0, "owner": "agent-b", "state": "result_received", "question_generation": 1, "outcome": "requires_human", "question": "Which leg represents the intended parser behavior?"}]}
```

`state` is `assigned`, `result_received`, `human_answered`, or `revoked`.
NDJSON wakeups derived from this projection diff are `resolution_assigned`,
`resolution_result_received`, `resolution_human_answered`, and
`resolution_revoked` (metadata-only ids/state/counts; never question text).

### `resolution assign`, `reply`, `revoke`

Publish the `ffres1` assignment (complete immutable job), result, or
revoke/supersede profile for one local job. Each returns the bounded
message id:

```json
{"message_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
```

`revoke --superseded` marks the assignment superseded rather than revoked.
FFI: `ffs_resolution_assign(root, job_id, flags_json)` /
`ffs_resolution_reply(root, job_id, flags_json)` /
`ffs_resolution_revoke(root, job_id, flags_json)` accept `null`, `{}`, or
`{"superseded": bool}`.
