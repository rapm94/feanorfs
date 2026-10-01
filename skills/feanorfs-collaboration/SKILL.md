---
name: feanorfs-collaboration
description: Coordinate coding agents across machines with FeanorFS encrypted signals, agent workspaces, and conflict resolution. Use when an agent must send or read snapshot-linked messages, spawn or land isolated agent work, resolve overlapping edits, or operate inside a FeanorFS runner child.
---

# FeanorFS multi-agent collaboration

Agents on one or many machines share one encrypted work-in-progress
workspace. Scope, routing, and authorship are advisory coordination, never
access control. FeanorFS never merges file content.

## The loop

1. **Know who you are.** Use `FEANORFS_AGENT` (set by `agent run` and runner
   children), else the name you were spawned as. Never claim another agent's
   name.
2. **Ask what to do.** Run `feanorfs agent next` (MCP: `status`). It projects
   work intent, integrator offers, conflicts, and resolution jobs into one
   lifecycle and returns `next_actions`. Each names the `actor` who owes it,
   a ready MCP `tool` + `args`, and the `cli` form. Replace `<placeholders>`
   with real values; never invent ids.
3. **Do only your actions** (`actor` is you), in order. Leave other agents'
   actions to them. `human` actions are escalations: tell the user, do not
   perform them yourself.
4. **Check before you write.** Run `feanorfs agent guard <path>…` (MCP:
   `work` with `op: guard`) before editing. `deny` (exit 2) means the path is
   inside another agent's accepted scope, your integrator attempt was
   superseded, or (with `--require-scope`) it is outside your own accepted
   scope. `warn` means a pending conflict or another agent's open proposal.
   Hosts with the guard hook installed run this before every edit.
5. **Re-run `feanorfs agent next`** after every mutation and before declaring
   work complete. While `projection_incomplete` is true, stop mutating: an
   absent item is not proof that nothing is pending.

Lifecycle stages (`items[].stage`):

```text
task:         proposed → accepted → settled → done
conflict:     conflicted → assigned → resolving → (awaiting_human) → done
integration:  assigned → resolving → (awaiting_human) → done
exits:        blocked | stopped
```

## Continuous active agents

Under `feanorfs agent run` (or an enabled runner's child) your worktree is
reconciled continuously:

1. Never run `feanorfs sync`, `push`, `pull`, `agent land`, or
   `agent refresh` yourself; the live controller lands saved changes after
   each quiet burst and refreshes untouched paths.
2. Report verification only against a settled snapshot: read
   `feanorfs --json agent status <name>` and use `live.settled_snapshot` as
   the `--about` of a `result`. Capture it before your checks and re-read it
   after; if it changed, retest or reply `blocked`. Never claim a snapshot you
   did not inspect.
3. Stop on `needs_attention`, `cursor_reset`, or `ambiguous_execution` and
   wait for explicit resolution.
4. On exit FeanorFS makes one bounded final attempt; offline work is kept.

## Commands

| Need | Command |
|---|---|
| What next | `feanorfs agent next [--for <name>]` |
| Safe to edit? | `feanorfs agent guard <path>… [--require-scope]` |
| Read signals | `feanorfs agent inbox [--for <name>] [--after <cursor>]` |
| Send a signal | `feanorfs agent send <to> --kind <kind> [--about <snapshot>] [--reply-to <id>] "<body>"` |
| Who can do what | `feanorfs agent capabilities` (announce yours with `--set <cap>`) |
| Route by capability | `feanorfs agent send cap:ios-build --kind request "<body>"` |
| Claim scope | `feanorfs agent work propose --task <id> --sequence <n> --path <p>…` |
| Decide (coordinator) | `feanorfs agent work decide <proposal-id> --kind accept` |
| Change scope | `feanorfs agent work amend` |
| Hand scope back | `feanorfs agent work yield` |
| Verified | `feanorfs agent work settle --inspected <snapshot>` |
| Done / stuck | `feanorfs agent work complete`, `feanorfs agent work block` |
| Scope projection | `feanorfs agent work status` |
| Integrator candidate | `feanorfs agent integrator reply accept`, then `feanorfs agent integrator reply result` (or `blocked`) |
| Conflicts | `feanorfs conflicts`, `feanorfs conflicts keep <path> --local\|--cloud\|--both\|--file <f>` |
| Read-only conflict legs | `feanorfs conflicts materialize` |
| Resolver job | `feanorfs agent resolution prepare <path> --reason exhausted --detail <why>` |
| Resolver steps | `feanorfs agent resolution materialize <job>`, `feanorfs agent resolution put <job> <file>`, `feanorfs agent resolution submit <job> --result <file>`, `feanorfs agent resolution apply <job>` |
| Job status | `feanorfs agent resolution status [<job>]` |
| Human answers | `feanorfs agent resolution answer <job>`, `feanorfs agent resolution publish-answer <job>`, `feanorfs agent resolution defer <job>` |
| Cross-machine jobs | `feanorfs agent resolution assign <job>`, `feanorfs agent resolution reply <job>`, `feanorfs agent resolution revoke <job>`, `feanorfs agent resolution protocol-status` |
| Agent worktrees | `feanorfs agent spawn <name>`, `feanorfs agent status <name>`, `feanorfs agent refresh <name>`, `feanorfs agent land <name>` |

Add `--json` (`feanorfs --json agent next`) when a program reads the output.
`references/protocol.md` holds the envelope format, the per-protocol rules
behind these commands, and the integrator and resolution procedures.

## Route work to the machine that can do it

Machines differ: only a Mac builds iOS, only a GPU box trains. Announce what
your machine can do once (`feanorfs agent capabilities --set ios-build`, or
`agent run <name> --capability ios-build -- …`). Send work that needs a
capability to `cap:<capability>`; FeanorFS delivers it to the one agent that
advertises it and refuses to guess when several do. For a fair choice among
several capable agents, use `agent integrator assign --require <cap>`.

## Requests and replies

1. Read `about_snapshot` on every request and verify you can establish that
   file tree (`feanorfs --json log`; a newer signal-only head has the same
   files). If you test a different tree, reply `--about` the snapshot you
   inspected and name both in the body.
2. Send at most one `status` per request, then exactly one `result` or
   `blocked` with `--reply-to <request-id>`. Include changed paths, the
   checks you ran and their outcomes (`passed`, `failed`, `not run`), and
   remaining limits.
3. Integrator replies go through `agent integrator reply`: the engine binds
   every protocol id and refuses superseded attempts, so never hand-write
   protocol JSON.

## Configured runner child

Only when an already-configured local runner started this process:

1. Read one `RunnerInvocation` JSON document from stdin. Require
   `schema_version: 1`; its `message` must be a direct `request` to its
   `agent`, which must agree with `FEANORFS_AGENT`.
2. The parent already refreshed `FEANORFS_AGENT_DIR`; do not sync or refresh
   (the parent holds the runner lease). `FEANORFS_WORKSPACE_ROOT` is the
   shared control root.
3. stdout/stderr are diagnostics only. Publish one terminal `result` or
   `blocked` with `feanorfs agent send` to `message.from` with
   `--reply-to <message-id>` and an accurate `--about`.
4. On `cursor_reset`, `pending_overflow`, `ambiguous_execution`,
   `delivery_unknown`, or `preparation_failed`, stop for operator
   inspection; never replay the request yourself.

Never configure, start, stop, reset, or remove a persistent runner unless the
user explicitly asks.

## Safety

1. Every workspace participant can read every signal. Never send credentials,
   recovery kits, pairing codes, `.env` values, or private keys.
2. Keep bodies bounded (maximum 8 KiB): paths and summaries, never raw logs
   or file contents.
3. Overlapping edits become explicit conflicts; record a choice with
   `feanorfs conflicts keep` after editing. Never merge content silently.
4. Reads may redeliver; a cursor reset means older signals may be missed.
5. Signals are passive: they never wake an inactive model or process.

## Escalate with one question

Ask the human exactly one focused question (no code) when implementations
disagree on product behavior, either side can lose data, security,
credentials, cryptography, recovery, or permissions are affected, public APIs
or formats would change without authorization, required verification fails
or cannot run, a conflict leg cannot be authenticated, or you authored one
side and no neutral reviewer exists. Offline hubs, first timeouts, stale
candidates, and lost compare-and-swap races are retries, not escalations.
