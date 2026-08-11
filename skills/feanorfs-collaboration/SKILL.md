---
name: feanorfs-collaboration
description: Coordinate FeanorFS coding agents across machines through encrypted snapshot-tied signals (feanorfs agent send / agent inbox, MCP agent_send / agent_inbox), including handling one configured local runner-child invocation. Use when an agent must request platform-specific work, report a bounded result or blocker, check incoming coordination signals, or parse a RunnerInvocation and publish its correlated terminal reply. Configure or control a persistent runner only when the user explicitly asks. Never use for file sync itself, conflict merging, or chat.
---

# FeanorFS multi-agent collaboration

Coordinate coding agents in a shared FeanorFS workspace through the encrypted
signal protocol. Treat routing and authorship as advisory. Keep signals out of
project files and Git state.

## Identify your agent

1. Use a workspace-unique agent name (for example `linux-dev`, `mac-test`, `ci1`).
2. Work inside the agent workspace via `feanorfs agent run <name> -- <command>` so `FEANORFS_AGENT` is set, or pass `--from <name>` explicitly.
3. Keep the original shared workspace root available. Inside `feanorfs agent run`, CLI signal commands (`agent inbox`, `agent send`) and MCP automatically use that control workspace. Run `agent refresh` and other FeanorFS lifecycle commands from the shared root; the isolated agent worktree itself has no project-local FeanorFS configuration.
4. Never claim an identity you are not authorized to use; attribution is advisory in this protocol.

## Check your inbox

1. Run `feanorfs agent inbox` at startup.
2. Re-check after every `feanorfs sync`, `feanorfs agent refresh`, or `feanorfs agent land`.
3. Re-check before declaring a task complete.
4. To read only new signals, pass the previous `cursor` as `--after <cursor>`; a `cursor_reset` result means older signals may have been missed — do not assume complete delivery.
5. Use `--json` (`feanorfs --json agent inbox --for <name>`) when another program consumes the result.

## Act on a request

1. Read `about_snapshot` on every request.
2. Before acting outside a configured runner-child invocation, refresh from the shared workspace root: use `feanorfs sync --no-watch` for the main worktree, or `feanorfs agent refresh <name>` for an isolated agent worktree. A runner child uses the pre-refreshed procedure below and must not reacquire its parent's runner lease.
3. Verify file context with JSON `status`/`agent status` and `feanorfs --json log`. A newer signal-only head has empty `changed_paths` and the same files, so a different head ID alone does not invalidate `about_snapshot`. Do not claim the requested snapshot when intervening file changes, deferred conflicts, local agent edits, or bounded/ambiguous history prevent you from establishing the same file tree.
4. If you test a different file tree, set the final reply's `--about` to the snapshot you actually inspected, keep the original request through `--reply-to`, and name both the requested and inspected snapshots in the body. Never associate a result with an untested snapshot.
5. Send at most one `status` update per request, and only when it adds real information.
6. Finish every accepted request with one `result` or `blocked` reply; do not
   infer exactly-once delivery from that child-side contract.
7. Reference the original request with `--reply-to <message-id>` and keep its `about_snapshot` when its file tree still applies.

## Handle a configured runner-child invocation

Follow this procedure only when an already-configured local runner starts this
process. Do not configure, start, stop, reset, or remove a persistent runner
unless the user explicitly asks for operator control.

1. Read exactly one `RunnerInvocation` JSON document from stdin until EOF.
   Require `schema_version: 1`, retain its `session_id`, and require its
   `message` to be a direct `request` to its `agent`.
2. Check that the invocation agent agrees with `FEANORFS_AGENT` when that
   variable is present. Use `FEANORFS_AGENT_DIR` as the agent worktree and
   `FEANORFS_WORKSPACE_ROOT` as the shared control root.
3. Treat the agent worktree as already refreshed by the parent runner
   immediately before launch. Do **not** call ordinary `feanorfs sync` or
   `feanorfs agent refresh`: the parent holds the runner lifetime lease, so a
   child refresh is rejected. Verify `message.about_snapshot` against JSON
   agent status/history before acting, then follow steps 3–7 of the request
   procedure above. If the requested file tree cannot be established, do not
   act; publish exactly one correlated `blocked` terminal naming the requested
   and observed snapshot. Keep the original requester, request message ID, and
   snapshot context for that reply.
4. Treat stdout and stderr as diagnostics only. Do not use either as a reply:
   the runner discards both, and a process exit does not complete the request.
5. Publish one terminal `result` or `blocked` through `feanorfs agent send`
   (or `agent_send`) from the configured agent to `message.from`, with
   `--reply-to <message-id>` and an accurate `--about` snapshot. Send an
   optional `status` only when it adds real information.
6. Publish `blocked` when the bounded task cannot complete. If the runner
   reports `cursor_reset`, `pending_overflow`, `ambiguous_execution`,
   `delivery_unknown`, or `preparation_failed` (local preparation failed before
   launch), stop and await explicit operator inspection/reset; do not launch a
   replacement child or replay the request yourself.

## Send a signal

```bash
feanorfs agent send <recipient> --kind <request|status|result|blocked> \
  [--about <snapshot-id>] [--reply-to <message-id>] [--from <name>] "<body>"
```

1. `kind` must be `request`, `status`, `result`, or `blocked`.
2. `--about` defaults to the current workspace head; pass it explicitly when the snapshot matters.
3. `--reply-to` must reference a real signal snapshot; use it for `result`/`blocked` replies.
4. Use `*` as recipient only for genuinely broadcast announcements.
5. Keep bodies bounded (maximum 8 KiB) and use paths and summaries, never raw logs or file contents.

## Safety rules

1. Never send credentials, recovery kits, pairing codes, `.env` values, or any secret intended for fewer than all workspace participants. Every participant can read every signal.
2. Never send `.env` contents or private keys, even though transport is end-to-end encrypted.
3. Never route work by path ownership claims; treat them as advisory.
4. Resolve file conflicts with `feanorfs conflicts` / `feanorfs conflicts keep <path> --local|--cloud`; never merge file content automatically.
5. Never fabricate delivery guarantees: reads may redeliver, and a cursor reset means possible missed signals after cursor, scan, or result bounds.
6. Never configure or control a persistent runner unless the user explicitly asks for that operator action.

## Choose activation

- Treat a signal as passive transport: it cannot start an inactive model or
  arbitrary process by itself.
- Let an external orchestrator monitor the NDJSON `events` metadata stream or
  poll `feanorfs --json agent inbox --for <name>` with a stored cursor, then
  choose whether to invoke an agent.
- Let an explicitly configured local runner invoke only its fixed local command
  for direct requests to its configured agent. Broadcasts remain readable in
  the normal inbox but do not invoke the runner child. Treat signals and this
  skill as passive instructions: neither one wakes an inactive model or process.

Read `references/protocol.md` for the exact envelope format, direct-request
admission, correlation, and links to the operator/API documentation.

## Integrator role (random assignment)

A dispatcher may randomly select one temporary **integrator** for a bounded
batch. Accept the integrator role **only** when you are the selected candidate
for the current attempt:

1. Read the `ffint1` assignment request (`feanorfs agent inbox`): verify
   `selected` names you, the `assignment_id`, `attempt`, `about_snapshot`, and
   `roster_fingerprint`. Ignore assignments where another agent is selected.
2. Refresh (`feanorfs sync --no-watch` or `feanorfs agent refresh <name>`) and
   verify the requested file tree with JSON `status`/`log` before accepting.
   A newer signal-only head with the same files does not invalidate the
   assignment.
3. Reply `--kind status` with an `ffint1` `accepted` profile
   (`feanorfs agent send <dispatcher> --kind status --about <snapshot> "ffint1:{...}"`).
4. Before **every** mutating operation, re-check for supersession: read your
   inbox for a newer `ffint1` assignment to another agent, a revocation, or a
   cursor reset; stop when your attempt is superseded.
5. Work in an isolated agent workspace. Materialize encrypted conflict legs
   read-only on your machine with `feanorfs conflicts materialize` (artifacts
   land under private global FeanorFS state; the head never changes).
6. Reconcile only when every leg is available, intent is compatible, no
   security/product/data-loss ambiguity exists, and verification passes.
   Create and verify a candidate in the agent workspace, then apply **only**
   through the explicit `feanorfs conflicts keep <path> --file <reconciled>`
   (or `--local`/`--cloud`/`--both`) operation. Never merge file content.
7. Send exactly one terminal reply: `--kind result` with an `ffint1` `result`
   profile carrying the bounded digest (outcome, verification summary,
   counts, risks, at most one decision question), or `--kind blocked` with an
   `ffint1` `blocked` profile. Keep the original request in `--reply-to` and
   name the snapshot you actually inspected.
8. Escalate (one focused question, no code) when: implementations disagree on
   product behavior; either side can lose data; security, credentials,
   cryptography, recovery, or permissions are affected; public APIs/formats
   change without authorization; required tests fail or cannot run; a cursor
   reset occurred; a leg cannot be authenticated; you authored a conflicting
   side and no neutral reviewer exists.

Dispatchers own dispatch state; you do not. Never act after supersession, and
never treat assignment as a security claim — it is advisory coordination.
