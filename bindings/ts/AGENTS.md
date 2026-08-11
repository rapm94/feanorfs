# Node bindings

## Purpose

Expose the FeanorFS agent SDK as the five-target `@feanorfs/agent` napi-rs package while keeping the JavaScript layer typed, thin, bounded, and deterministic.

## Ownership

- `src/lib.rs` owns the napi-rs adapter; `api.mjs` owns JSON parsing into the public async façade.
- `contract.d.ts` is the hand-owned public contract. `index.js` and `index.d.ts` are napi-rs generated.
- `scripts/assemble-packages.mjs` owns facade/platform version convergence, native artifact architecture checks, hashes, and deterministic package assembly.
- `npm/` holds generated platform package metadata; native `.node` files and `npm/artifacts.json` are ignored build products.

## Local Contracts

- Run all synchronous core SDK work under `spawn_blocking`; do not duplicate sync, conflict, credential, or cryptographic behavior in JavaScript.
- Reject raw JSON above 1 MiB before Serde allocation, use shared strict request types, and require explicit `all: true` rather than allowing malformed conflict subsets to broaden to all paths.
- Agent names and paths use the central agent-core validators. Never return an actionable path through a lossy conversion.
- `npm run assemble-metadata` synchronizes the workspace version into the facade, lockfile, generated loader checks, and all five platform manifests.
- Full assembly accepts exactly the configured five same-source artifacts and verifies each file's Mach-O/ELF/PE format and CPU architecture before hashing or packing. Wrong-target placeholders must fail closed. A host-only build verifies metadata and runs local tests; it is never presented as five-target package proof.
- Node tests set a private `FEANORFS_HOME` and file credential store before any stateful native or CLI call.

## Work Guidance

- Regenerate native loader/declarations with `npm run build`; do not hand-maintain napi-rs boilerplate.
- Update `contract.d.ts`, adapter runtime validation, and tests together for public schema changes.

## Verification

- `npm run build`
- `npm test`
- `npm run verify-metadata`
- `npm run verify-packages` after all five genuine target artifacts are present.
- `cargo clippy -p feanorfs-agent-node --all-targets --locked -- -D warnings`

## Child DOX Index

No child directories require separate contracts.
