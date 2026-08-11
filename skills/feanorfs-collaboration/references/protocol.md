# FeanorFS agent signal protocol reference

Encrypted, low-volume coordination signals for coding agents sharing one FeanorFS
workspace. Signals are format-v3 snapshots with no file-tree changes; the hub
never sees plaintext routing, bodies, or workspace metadata.

For local activation rather than signal transport, use the
[Agent runner runbook](../../../docs/usage.md#agent-runner),
[local delivery sequence](../../../docs/agent-communication.md#local-runner-delivery),
and [CLI runner projection](../../../docs/agent-api.md#agent-runner-cli-only-current-projection).

## Envelope

A signal lives in `Snapshot.message` as an exact versioned discriminator
followed by canonical compact JSON:

```text
ffmsg1:{"to":"mac-test","kind":"request","body":"Run iOS simulator tests","about_snapshot":"<64-hex>","reply_to":null}
```

Fields derived from the enclosing snapshot are not duplicated in the payload:

- `message_id` = the signal snapshot ID (immutable);
- `from` = `Snapshot.author`;
- `created_at_ms` = `Snapshot.created_at_ms`.

The snapshot object, author, message, tree, and paths are encrypted. The hub
observes only ordinary ciphertext objects, sizes, manifests, head changes, and
timing.

## Message kinds

| Kind | Meaning | Expected follow-up |
|---|---|---|
| `request` | Ask another agent to perform bounded work against a snapshot | optional `status`; exactly one correlated `result` or `blocked` terminal |
| `status` | Short progress update | none |
| `result` | Final bounded outcome | requester consumes it |
| `blocked` | Final explanation of why the request cannot complete | requester decides next action |

FeanorFS validates the enum and transports the body; it never interprets
success, failure, task ownership, paths, or content semantics.

## Validation

- `from`/`to` are FeanorFS agent names; `to="*"` is the only broadcast form.
- Sender defaults to `FEANORFS_AGENT`, then `human`, unless an embedding
  supplies an explicit validated sender.
- `body` is non-empty UTF-8 after trimming, at most 8 KiB.
- `about_snapshot` defaults to the head observed when sending starts and must
  be a full reachable snapshot ID.
- `reply_to`, when present, must be a full reachable `ffmsg1` signal snapshot
  ID.
- Signal-only descendants retain their parent's file-tree root, so a newer head
  ID alone does not mean the requested files changed. A reply about a genuinely
  different inspected file tree uses that actual snapshot as `about_snapshot`,
  links the original request with `reply_to`, and names the requested and
  inspected snapshots in its body. It must not imply that an untested snapshot
  was verified.
- Unknown snapshot messages remain ordinary history messages; malformed
  `ffmsg1` payloads are ignored by typed inbox reads but stay visible in raw
  history.

## Send semantics

Sending is append-only through the workspace-head compare-and-swap. Every CAS
retry reloads the latest head and reuses that head's tree root, so a retry can
never roll back visible files. `about_snapshot` stays fixed at the caller's
choice even if the head advances concurrently. A successful send returns only
after the encrypted snapshot object, reachability manifest, and head swap
succeed.

## Inbox semantics

- Read-only and redelivery-safe; reading never publishes acknowledgements.
- Returns only messages addressed to the recipient or broadcast to `*`.
- `cursor` is the workspace head observed by the read; pass it back as `after`
  to read the graph delta (reachable snapshots not reachable from the cursor).
- Results are deduplicated by signal snapshot ID; ordering is display-only.
- Traversal scans at most 10 000 snapshots per call; an unreachable cursor,
  exhausted scan, or result-limit overflow sets `cursor_reset=true` and
  returns a bounded recent view — the caller may have missed older signals.

## Configured local runner

This is the short delivery rule, not a second lifecycle or JSON reference.

- Admit only a direct `request` whose recipient exactly equals the configured
  agent. Exclude broadcasts, nonrequests, duplicates, and completed request
  IDs from runner execution; normal `agent inbox` reads still return broadcasts.
- Invoke only the operator-configured fixed local command, one request at a
  time. Require one terminal `result` or `blocked` to the requester with
  `reply_to` set to the request ID and `about_snapshot` set to the snapshot
  actually inspected; `status` is optional. This child contract is not an
  exactly-once transport guarantee.
- Observe that correlated terminal before completion. For known child or
  invocation failures, attempt a generic correlated `blocked` fallback.
- Stop for attention on `cursor_reset`, `pending_overflow`,
  `ambiguous_execution`, `delivery_unknown`, or `preparation_failed`.
  `preparation_failed` means local refresh/preparation failed before launch;
  preserve the pending request for inspection and explicit discard/reset. Do
  not replay any attention state; follow the linked operator runbook.
- Own the complete child tree: Unix/macOS uses a fresh process group with
  bounded TERM/KILL teardown; Windows creates children suspended, adopts and
  verifies a private kill-on-close Job Object, then resumes them. Timeout,
  cancellation, and direct-child exit tear down descendants. A supervised
  stop waits for a durable workspace-specific registry acknowledgement bound
  to the supervisor's exact native process identity when authority exists; a
  fresh disabled/unregistered stop with no supervisor authority skips an
  impossible acknowledgement, while stale authority fails closed.

## CLI

```text
feanorfs agent send <recipient> --kind <request|status|result|blocked> [options] <body>
  --about <snapshot-id>      Snapshot the request/result concerns; defaults to current head
  --reply-to <message-id>    Signal snapshot being answered
  --from <agent-name>        Explicit sender; otherwise FEANORFS_AGENT or human

feanorfs agent inbox [options]
  --for <agent-name>         Recipient; defaults to FEANORFS_AGENT or human
  --after <head-id>          Previous inbox cursor
  --limit <n>                Bounded result count; default 50, maximum 1000
```

Global `--json` emits `AgentSendResult` and `AgentInboxResult`.

## MCP

- `agent_send(from?, to, kind, body, about_snapshot?, reply_to?)`
- `agent_inbox(for?, after?, limit?)`

Tool descriptions explain that all workspace participants can read messages,
identity is advisory, and requests/results should carry exact snapshot context.

## Events

The NDJSON `events` stream emits one `agent_message` wakeup record per new
signal with `message_id`, `from`, `to`, `kind`, and `about_snapshot` — never
the body. Normal bounded delivery may redeliver and is deduplicated through a
bounded in-process ID cache. When cursor reset or bounded overflow may have
missed older wakeups, the stream first emits this metadata-only record before
the bounded wakeups returned by that poll:

```json
{"event":"agent_message_cursor_reset","cursor":"<observed-workspace-head>","cursor_reset":true}
```

On this event, do not infer complete delivery. Immediately reread the typed
inbox without the stale `--after` cursor, reconcile the bounded recent view,
and replace the stored cursor with the inbox result. An authorized
orchestrator also calls `agent inbox` for each ordinary wakeup's typed message.

## Safety

- Routing is not an access-control boundary; sender attribution is not
  cryptographically signed in v1.
- Never send credentials, recovery kits, pairing codes, `.env` values, or
  secrets intended for fewer than all workspace participants.
- Signals are coordination checkpoints, not chat, token streams, or build logs.
- Signals are passive transport; use an external events/polling orchestrator
  or the explicitly configured local runner described above for activation.
  Neither a signal nor this reference wakes an inactive model or process.

## `ffint1` integrator profiles

Assignment coordination reuses the four `ffmsg1` kinds; the body carries a
versioned `ffint1:` profile. Assignment = `request`; acceptance = `status`;
terminal outcome = `result` (digest) or `blocked`.

```text
ffint1:{"type":"assignment","assignment_id":"<32-hex>","attempt":0,"selected":"agent-b","about_snapshot":"<64-hex>","roster_fingerprint":"<64-hex>","neutral_integrator":true,"task":"Integrate parser implementation and tests"}
```

```text
ffint1:{"type":"accepted","assignment_id":"<32-hex>","attempt":0,"about_snapshot":"<64-hex>"}
```

```text
ffint1:{"type":"result","assignment_id":"<32-hex>","attempt":0,"about_snapshot":"<64-hex>","digest":{"assignment_id":"<32-hex>","integrator":"agent-b","about_snapshot":"<64-hex>","inspected_snapshot":"<64-hex>","state":"completed","landed_paths":12,"resolved_conflicts":3,"remaining_conflicts":0,"verification":{"status":"passed","summary":"84 tests passed"},"outcome":"Integrated parser implementation and tests.","risks":[],"decision_required":null}}
```

```text
ffint1:{"type":"blocked","assignment_id":"<32-hex>","attempt":0,"about_snapshot":"<64-hex>","reason":"Missing iOS toolchain"}
```

Selection is `BLAKE3("feanorfs-integrator-selection-v1" ‖ len(workspace_id) ‖
workspace_id ‖ about_snapshot ‖ assignment_id ‖ selection_nonce ‖
roster_fingerprint ‖ len(agent_name) ‖ agent_name)` (every variable-width
value length-prefixed; ascending 32-byte score, agent-name tie-break).
`roster_fingerprint` = Blake3 of the canonical JSON array of the sorted final
pool. Only the dispatcher draws; terminal replies reference the assignment
request via `reply_to`. Unknown `ffint` versions remain ordinary message text.
