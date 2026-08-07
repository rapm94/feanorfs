---
name: feanorfs-collaboration
description: Coordinate FeanorFS coding agents across machines through encrypted snapshot-tied signals (feanorfs agent send / agent inbox, MCP agent_send / agent_inbox). Use when a FeanorFS agent must request platform-specific work, report a bounded result or blocker, or check for incoming coordination signals in a shared encrypted workspace. Never use for file sync itself, conflict merging, or chat.
---

# FeanorFS multi-agent collaboration

Coordinate coding agents in a shared FeanorFS workspace through the encrypted signal protocol. Signals are versioned envelopes stored in ordinary snapshot history: they never become project files, never dirty Git, and stay opaque to the hub. All workspace participants can read all signals; treat routing and authorship as advisory, never as cryptographic guarantees.

## Identify your agent

1. Use a workspace-unique agent name (for example `linux-dev`, `mac-test`, `ci1`).
2. Work inside the agent workspace via `feanorfs agent run <name> -- <command>` so `FEANORFS_AGENT` is set, or pass `--from <name>` explicitly.
3. Keep the original shared workspace root available. Run `agent inbox`, `agent send`, `agent refresh`, and other FeanorFS lifecycle commands from that root; the isolated agent worktree itself has no project-local FeanorFS configuration.
4. Never claim an identity you are not authorized to use; attribution is advisory in this protocol.

## Check your inbox

1. Run `feanorfs agent inbox` at startup.
2. Re-check after every `feanorfs sync`, `feanorfs agent refresh`, or `feanorfs agent land`.
3. Re-check before declaring a task complete.
4. To read only new signals, pass the previous `cursor` as `--after <cursor>`; a `cursor_reset` result means older signals may have been missed — do not assume complete delivery.
5. Use `--json` (`feanorfs --json agent inbox --for <name>`) when another program consumes the result.

## Act on a request

1. Read `about_snapshot` on every request.
2. Before acting, refresh from the shared workspace root: use `feanorfs sync --no-watch` for the main worktree, or `feanorfs agent refresh <name>` for an isolated agent worktree.
3. Verify file context with JSON `status`/`agent status` and `feanorfs --json log`. A newer signal-only head has empty `changed_paths` and the same files, so a different head ID alone does not invalidate `about_snapshot`. Do not claim the requested snapshot when intervening file changes, deferred conflicts, local agent edits, or bounded/ambiguous history prevent you from establishing the same file tree.
4. If you test a different file tree, set the final reply's `--about` to the snapshot you actually inspected, keep the original request through `--reply-to`, and name both the requested and inspected snapshots in the body. Never associate a result with an untested snapshot.
5. Send at most one `status` update per request, and only when it adds real information.
6. Finish every accepted request with exactly one `result` or `blocked` reply.
7. Reference the original request with `--reply-to <message-id>` and keep its `about_snapshot` when its file tree still applies.

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
6. Do not attempt to wake another model through a signal; an active orchestrator must monitor events or poll the inbox.

## Orchestrators

- Long-lived orchestrators monitor the NDJSON `events` stream for `agent_message` wakeup records (bounded metadata, no body), then invoke the target agent with this skill loaded.
- Alternatively, poll `feanorfs --json agent inbox --for <name>` with a stored cursor.
- A signal cannot start an inactive model; only an already-running process can respond.

See `references/protocol.md` for the exact envelope format, message kinds, and CLI/MCP contracts.
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
