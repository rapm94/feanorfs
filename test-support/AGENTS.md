# test-support

## Purpose

Provide process-wide, cross-platform isolation for FeanorFS test executables before libtest, Tokio, or application code can resolve user state.

## Ownership

- `src/lib.rs` — pre-main temporary home/profile setup, process-exit cleanup, and the link-anchor macro used by state-capable test targets.

## Local Contracts

- Test executables receive distinct OS-temporary `HOME`, `USERPROFILE`, and `FEANORFS_HOME` roots plus file-backed credential policy before test threads start.
- Isolation is one root per process so parallel tests and inherited subprocesses share a stable profile without mutating environment variables after startup.
- This crate is dev-only and `publish = false`; production binaries must never depend on or link it.
- Keep the temporary directory alive for the entire process and remove it best-effort at normal process shutdown.

## Work Guidance

- Link through `feanorfs_test_support::isolate_test_process!()` once in every state-capable unit or integration test crate.
- Tests that need a different child home set environment only on `std::process::Command`; never mutate the parent test process.

## Verification

- `cargo test -p feanorfs-test-support --locked`
- Run affected crate suites with the real profile directory count captured before and after; it must not change.

## Child DOX Index

No child DOX files.
