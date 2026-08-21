# Plan 005: Add a lifecycle e2e smoke test for the published Node SDK surface

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 51d2ba8..HEAD -- bindings/ts/`
> If bindings changed since 51d2ba8, re-read `bindings/ts/AGENTS.md` and the
> test directory listing before proceeding.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: tests
- **Planned at**: commit `51d2ba8`, 2026-08-21

## Why this matters

`@feanorfs/agent` ships to npm as a facade plus five native platform
packages, yet its own test suite is thin on lifecycle behavior: existing
tests (`bindings/ts/test/loader-errors.mjs`, `loader-version.mjs`,
`loop.mjs`, `parity.mjs`) cover loader mechanics and JSON parity, but no test
drives the real native binding through an actual workspace lifecycle (create
→ spawn agent → write → land → status → conflict path). Rust-side contract
snapshots prove JSON shapes, so the residual risk concentrates exactly there:
napi marshalling and the async façade behaving end-to-end. One lifecycle
smoke test closes that gap on every host where a local `.node` artifact can
be built (CI builds darwin/linux/windows artifacts already).

## Current state

- `bindings/ts/AGENTS.md` contracts that bind this work (quote-exact rules):
  - "Node tests set a private `FEANORFS_HOME` and file credential store
    before any stateful native or CLI call."
  - "Run all synchronous core SDK work under `spawn_blocking`; do not
    duplicate sync, conflict, credential, or cryptographic behavior in
    JavaScript."
  - "`api.mjs` owns JSON parsing into the public async façade";
    `contract.d.ts` is the hand-owned public contract.
  - Verification commands: `npm run build`, `npm test`,
    `cargo clippy -p feanorfs-agent-node --all-targets --locked -- -D warnings`.
- Existing tests live in `bindings/ts/test/*.mjs`, plain Node scripts run by
  `npm test` (see `bindings/ts/package.json` scripts section).
- The embedded/local-hub path exists for zero-network testing: the CLI
  supports `start --local --workspace <name>` (in-process LocalHub; see root
  AGENTS.md), and the Node façade exposes workspace operations through
  `api.mjs`. Inspect `contract.d.ts` for exact exported names before writing
  the test — it is authoritative.
- A host artifact may or may not exist in the working tree
  (`feanorfs-agent-node.darwin-arm64.node` present at plan time). Tests must
  SKIP cleanly with a clear message when no host artifact is available,
  because CI hosts without prebuilt artifacts run `npm test` too — check how
  `test/loop.mjs` handles this first and copy its approach.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Build binding | `npm run build` (in `bindings/ts/`) | exit 0, fresh `.node` + generated files |
| Full JS tests | `npm test` | all pass incl. new file |
| Metadata check | `npm run verify-metadata` | exit 0 |
| Rust side | `cargo clippy -p feanorfs-agent-node --all-targets --locked -- -D warnings` | exit 0 |

## Scope

**In scope** (only these files may be created/modified):
- `bindings/ts/test/lifecycle.mjs` (new)
- `bindings/ts/package.json` ONLY if `npm test` uses an explicit file list
  rather than globbing `test/*.mjs` (check first; prefer no change)

**Out of scope** (do NOT touch):
- `src/lib.rs` (napi adapter) — if a needed operation is missing from the
  façade, STOP and report instead of extending the native surface.
- `api.mjs`, `contract.d.ts`, generated `index.js`/`index.d.ts`.
- `scripts/assemble-packages.mjs`.

## Git workflow

- Branch: `advisor/005-node-sdk-lifecycle-smoke`
- Commit style: conventional commits, e.g. `test(bindings): add native lifecycle smoke`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Read the façade contract

Read `bindings/ts/contract.d.ts` and one existing test (start with
`test/loop.mjs`) to learn: exported class/function names, how FEANORFS_HOME
is isolated, how tests skip without a host artifact, and the spawn_blocking
pattern used for sync calls.

**Verify**: you can state (in the commit message body) the exact façade calls
the test will make and how the skip guard works.

### Step 2: Write `test/lifecycle.mjs`

Scenario, using only existing façade operations against a LOCAL workspace
(`--local` equivalent exposed by the façade; never network):

1. Create private temp `FEANORFS_HOME` + workspace dir (same helper style as
   existing tests).
2. Initialize a local workspace via the façade.
3. Spawn agent `<name>`; write a file into the agent worktree through normal
   fs; land it; assert status reflects landed changes (typed result fields).
4. Create a conflicting edit on the base workspace for the same path;
   refresh; assert a pending conflict appears in typed results.
5. Resolve with keep-local (conflict resolution op from the façade); assert
   conflict cleared and content matches.
6. Clean up temp dirs in a `finally` block.

Rules from AGENTS.md that are graded: private FEANORFS_HOME set BEFORE any
stateful call; all blocking SDK calls wrapped per the established pattern;
no duplicated sync/conflict logic in JS — call the façade, assert on typed
results only; bounded assertions on concrete fields (no snapshot-the-world).

**Verify**: `node --test test/lifecycle.mjs` or the repo's runner equivalent
(copy however `npm test` invokes other tests) → passes locally when a host
artifact exists; skips cleanly when it does not.

### Step 3: Wire into `npm test` if needed

If `package.json` lists test files explicitly, add `test/lifecycle.mjs`;
if it globs, change nothing.

**Verify**: `npm test` → includes the new scenario in output; full suite green.

### Step 4: Regenerate-and-test sanity pass

Run `npm run build && npm test && npm run verify-metadata` in order.

**Verify**: all three exit 0; generated files unchanged by your commit
(`git status` shows only intended paths).

## Test plan

- The deliverable IS a test. Its own acceptance is Step 2's verify line plus
  the full-suite gate in Step 4.

## Done criteria

- [ ] `bindings/ts/test/lifecycle.mjs` exists and exercises init → spawn →
      write → land → conflict → resolve through the public façade only
- [ ] Private `FEANORFS_HOME` isolation present before any stateful call
- [ ] Clean skip when no host-native artifact is available
- [ ] `npm test` green (or green-with-skip) and
      `cargo clippy -p feanorfs-agent-node --all-targets --locked -- -D warnings`
      exits 0
- [ ] No files outside scope modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back if:

- The façade does not expose any operation the scenario needs (workspace
  init, spawn, land, refresh, conflicts list/keep). Report which ones are
  missing — do NOT extend the napi surface yourself.
- `npm run build` fails for environment reasons (missing toolchain targets);
  record the error, attempt only the documented fix paths.
- Existing tests have NO skip-without-artifact pattern to copy — propose the
  guard design in your report before inventing one.

## Maintenance notes

- When the five-platform release pipeline changes assembly, this smoke runs
  unchanged — it depends only on the façade and a host artifact.
- Reviewers should check the test asserts typed fields (contract.d.ts names),
  never raw JSON string matching, and that cleanup cannot leak
  FEANORFS_HOME state between tests.
