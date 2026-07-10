# hub

## Purpose

Own embedded LocalHub request dispatch, response construction, authentication checks, and grouped route behavior.

## Ownership

- `http.rs` — response, query, format-compatibility, and migration-fence helpers.
- `routes_legacy.rs` — flat sync/upload compatibility routes.
- `routes_objects.rs` — object download, manifests, and snapshot-head routes.
- `routes_workspace.rs` — format and migration lifecycle routes.

## Local Contracts

- Keep auth and migration-token comparisons constant-time.
- Hold shared publication lock from fence check through upload/manifest/head mutation; migration start and format stamp take exclusive lock.
- Enforce 100 MiB request and 8 MiB manifest limits.
- Validate hashes and safe paths before writing blobs.
- Preserve referenced blobs after committed-but-durability-uncertain metadata writes.

## Work Guidance

- Keep `../hub.rs` limited to lifecycle, cache, auth, and dispatch.

## Verification

- `cargo test -p feanorfs-agent-core --test hub_tests --locked`
- `cargo test -p feanorfs-client --test local_hub_parity --locked`

## Child DOX Index

No child DOX files.
