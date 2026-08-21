# Plan 003: Split the protocol-layer god files into submodule directories without changing any public path

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 51d2ba8..HEAD -- agent-core/src/resolution.rs agent-core/src/integrator.rs agent-core/src/work.rs common/src/work_contract.rs`
> On any drift in these four files, compare against the live code; treat
> structural surprises as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M (per file; total L if all four at once — do them one commit each)
- **Risk**: LOW
- **Depends on**: none (safe to run before or after plans 001/002)
- **Category**: tech-debt
- **Planned at**: commit `51d2ba8`, 2026-08-21

## Why this matters

The newest protocol modules have grown past review-friendly size:
`agent-core/src/resolution.rs` is 4069 lines, `agent-core/src/integrator.rs`
3172, `agent-core/src/work.rs` 3011, `common/src/work_contract.rs` 2414.
These are the most actively evolved, most security-sensitive modules in the
repo (conflict resolution publication, dispatcher state machine, work-intent
reducer). Large single files concentrate merge conflicts and make review
diffs noisy. The repo already has an established, proven remedy: convert a
big module file into a same-named directory with a thin `mod.rs` that keeps
every existing path working through re-exports. `sync_pass`, `supervisor/`,
`agent_runner/`, and `process_tree/` were all split this way ("re-exported
paths unchanged" per AGENTS.md). This plan applies the same mechanical
treatment.

## Current state

- Targets and sizes (`wc -l`):
  - `agent-core/src/resolution.rs` — 4069 lines. Fingerprinted conflict
    identity, resolution jobs/candidates, guarded publication.
  - `agent-core/src/integrator.rs` — 3172 lines. Dispatcher state machine +
    store + ranking.
  - `agent-core/src/work.rs` — 3011 lines. Deterministic ffwork1 reducer.
  - `common/src/work_contract.rs` — 2414 lines. Canonical wire types +
    overlap evaluation (pure data crate).
- The convention exemplar to copy exactly —
  `agent-core/src/sync_pass/mod.rs:1-27`:

    ```rust
    //! Sync-pass orchestration, the upload/delete side, and the typed outcome.

    // ... original use statements ...

    mod download;
    mod materialize;
    mod negotiate;
    mod rollback;

    pub(crate) use self::download::prefetch_downloads;
    pub use self::download::process_downloads;
    use self::materialize::portable_mode;
    pub(crate) use self::negotiate::preflight_download_projection;
    use self::negotiate::{validate_cross_direction_structure, validate_final_candidate};
    use self::rollback::recover_materialization_stages;
    ```

  Rule of the house style: child modules are declared with plain `mod x;`,
  items are pulled back into the parent namespace with explicit `use`
  statements so ALL external paths (`feanorfs_agent_core::resolution::X`,
  `feanorfs_common::work_contract::Y`) keep resolving with zero caller
  changes. Tests move into a `tests` submodule file (see
  `client/src/cli/supervisor/tests/` for the directory form).
- Module docs: each target file starts with doc comments / imports; keep the
  module-level `//!` docs in the new `mod.rs`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Build | `cargo build --workspace --locked` | exit 0 |
| Crate tests | `cargo test -p feanorfs-agent-core --locked` | all pass |
| Common tests | `cargo test -p feanorfs-common --locked` | all pass |
| Contract snapshots | `cargo test -p feanorfs-client contract_snapshots --locked` | pass |
| Lint | `cargo clippy --workspace --all-targets --locked -- -D warnings` | exit 0 |

## Scope

**In scope** (only these files/directories may be created or modified):
- `agent-core/src/resolution.rs` → becomes `agent-core/src/resolution/`
  (`mod.rs` + topic submodule files + `tests.rs` or `tests/`)
- `agent-core/src/integrator.rs` → same treatment
- `agent-core/src/work.rs` → same treatment
- `common/src/work_contract.rs` → same treatment

**Out of scope** (do NOT touch):
- `feanorfs-ffi/src/lib.rs` (2577 lines) — C ABI boundary; different
  constraints, deserves its own plan if ever needed.
- Any item's visibility (`pub`, `pub(crate)`) — if a split seems to require
  widening visibility, that is a STOP condition, not an invitation.
- Any logic, ordering, or naming inside moved code — byte-identical moves
  only (plus the `mod`/`use` scaffolding).

## Git workflow

- Branch: `advisor/003-protocol-file-splits`
- One commit per file split, e.g.
  `refactor(agent-core): split resolution.rs into resolution/ submodules`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

Repeat this exact procedure for each target, one file per commit:

### Step N.1: Choose topic boundaries by reading the file's own section comments

Read the whole target file first. Group code into coherent child modules
(e.g. for `resolution.rs`: contract types vs job store vs guarded
publication vs protocol helpers; tests almost always become their own
`tests` submodule). Aim for 4–7 children; do not force symmetry.

**Verify**: no command — record the planned grouping in the commit message body.

### Step N.2: Move code verbatim

Create `<name>/mod.rs` containing: the original `//!` module docs, the
original `use` block, `mod` declarations, and re-export `use` lines matching
the sync_pass exemplar. Move each group verbatim into its child file. Fix
nothing else — no renames, no signature changes, no import "cleanups"
beyond what the compiler demands after the move.

**Verify**: `cargo build --workspace --locked` → exit 0.

### Step N.3: Prove paths unchanged

Run the full gates for the owning crate plus contract snapshots. External
callers must compile untouched.

**Verify**: `cargo test --workspace --all-features --locked` → pass, and
`git diff --stat` shows changes only under the one target module directory.

## Test plan

- No new tests. All existing tests must pass unmodified (they are the proof
  the mechanical move preserved behavior).
- `rg -n 'mod resolution|mod integrator|mod work' agent-core/src/lib.rs common/src/lib.rs`
  → module declarations still point at the same logical module names.

## Done criteria

- [ ] Each of the four targets lives in a directory whose `mod.rs` is <300
      lines and re-exports preserve every pre-split public path
      (`cargo build --workspace --locked` from a clean tree proves it)
- [ ] `wc -l` on every new child file < ~1200 lines
- [ ] `cargo test --workspace --all-features --locked` exits 0
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` exits 0
- [ ] Four commits, one per module; no other paths modified
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back if:

- Splitting any file appears to require changing an item's visibility or a
  public path — the module boundaries are wrong; propose a different
  grouping instead of widening visibility.
- Any test fails after a purely verbatim move — that means hidden coupling
  (e.g. `include!`, macro-generated references); report which test.
- The drift check shows someone refactored these files since 51d2ba8 —
  re-read live code and confirm the grouping still makes sense before
  touching anything.

## Maintenance notes

- Future protocol work should add new functionality to the relevant child
  module, never grow `mod.rs` again.
- Reviewers should verify moves were verbatim: prefer reviewing with
  `git diff --find-renames --find-copies-harder` and spot-checking that
  non-scaffolding hunks are pure relocation.
