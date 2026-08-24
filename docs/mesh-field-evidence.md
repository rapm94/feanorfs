# Mesh Transport Field Evidence

Two-machine internet field test of the direct-P2P mesh transport
(`docs/mesh-transport.md`). Second machine: Arch-based Linux box (hostname
`omarchy`; initial LAN address `192.168.50.124`), first machine: this macOS
host.
Both ran the same uncommitted working-tree build (mesh P0–P3 + hardening).

## Topology

- Mac hosts the automatic private hub (TLS, port 3031) and owns workspace
  `fsw1-9a5a…` under an isolated `FEANORFS_HOME`.
- Box joined via `fnr1` invite copied over plain SSH (no Tailscale).
- No relay configured anywhere.

## Results

| Check | Evidence |
| --- | --- |
| Join + initial sync | `feanorfs start <fnr1>` on the box linked and synced |
| Capability dial wins | box `mesh-state.json`: `tcp.successes=2`, `last_path = 192.168.50.30:3031 (lan)` after candidate refresh |
| QUIC punch cross-machine | forced quic-only capability: `quic.successes=1`, `last_path.transport=quic`; sync completed over the bridge |
| Punch auth | Ed25519 node signature required; tampered identity rejected in unit tests; live run passed |
| Doctor mesh checks | both hosts report `ipv6_reachable` / `upnp_mapping` / `udp_punch_capable` (typed info/ok, no addresses) |
| AI-authored code over mesh | OpenCode agent (`opencode/x-preview-f-free`) authored a 5-file Vite+React app inside a spawned feanorfs agent worktree; `agent land react-dev` published it; Mac pulled `src/App.jsx` etc. across the mesh |
| Conflict surfacing | simultaneous README edits → `EditEdit` conflict with `.original/.local/.cloud` artifacts on the box |
| Conflict resolution | `conflicts keep README.md --cloud` resolved; both machines converged to identical content |
| Signal messaging | `agent send human --kind status …` from box delivered into Mac `agent inbox` |
| Secrets hygiene | E2EE key + token only in OS-store/protected files; nothing in argv/env/logs |

## Two-AI collaborative build

On 2026-08-24, one OpenCode agent ran as `alpha-mac` on macOS and one as
`beta-box` on Linux. Both used `opencode/x-preview-f-free`, worked in separate
FeanorFS agent worktrees, and coordinated through encrypted workspace signals.

| Step | Secret-free evidence |
| --- | --- |
| Work proposals | `alpha-mac` proposed `stats-panel` for `src/components/**` in message `aa399255…`; `beta-box` proposed `reset-button` for `README.md` and `src/App.jsx` in `5c2274e3…` |
| Coordinator decisions | `agent work decide` accepted both proposals in messages `e3da3916…` and `cd836537…`; current `agent work status --json` projects both tasks as `accepted` with the exact scopes above |
| Alpha authorship + land | OpenCode authored `src/components/StatsPanel.jsx`; snapshot `4115c73b…` records an `alpha-mac` land changing that path |
| Beta authorship + land | OpenCode added the reset control and collaboration note; snapshot `736e65e2…` records a `beta-box` land changing `README.md` and `src/App.jsx` |
| Cross-machine composition | `beta-box` refreshed snapshot `25a4119f…` and received `src/components/StatsPanel.jsx`; both contributions then appeared in the shared app |
| Deliberate same-file collision | Both worktrees changed `src/App.jsx`; `agent land` surfaced `EditEdit` with preserved legs (`base 785b21f9…`, `ours d07233a6…`, `theirs b1abe576…`) rather than merging content |
| Resolution | Coordinator selected the cloud leg with `conflicts keep src/App.jsx --cloud`; resolution snapshots are authored `human`, and both main workspaces now report `No paths need attention` |
| Losing-side notification | `alpha-mac` inbox contains message `6d61461a…`: the coordinator names the collision, selected leg, and requested rebasing the footer/background change on the refreshed head |
| Final convergence | After pausing both automatic watchers, coordinator published the explicit combined file once: Mac reported `Uploaded 1`, Linux reported `Downloaded 1`; `src/App.jsx` is SHA-256 `10edb36a…` on both and contains beta header, beta reset/background, and alpha footer |
| Agent cleanup | Explicit `agent refresh --replace` after publication leaves `alpha-mac` and `beta-box` in `clean` state with the merged head |
| Mesh after Wi-Fi change | Mac moved from `192.168.50.*` to `192.168.1.*`; Linux reappeared at `192.168.1.17`. A bounded Linux sync completed with no relay; secret-free `mesh-state.json` projection reports TCP `21/32` successes/attempts, QUIC `1/6`, and last path `tcp/lan` |

The phase-B alpha model read the collision task but produced no edit, so the
coordinator applied that requested footer/background edit inside the alpha
worktree. Beta's overlapping edit remained AI-authored. The conflict and
resolution exercised real agent snapshots and signals; this record does not
misstate the coordinator-applied alpha edit as autonomous model output.

## Field findings and fixes

1. **Stale capability after network change** — candidates baked at hub start
   pointed at the old subnet; sync silently fell back to mDNS. Worked around
   by refreshing configs; product fix tracked in TODO AI-7.
2. **Bridge listener race** — client-side TCP listener could die between
   authentication and first use. Fixed: explicit QUIC idle timeout (300 s) +
   keepalive (10 s) and a self-healing accept loop that re-establishes and
   re-authenticates the session on demand.
3. **Linux DNS override gap** — reqwest ignores `resolve_to_addrs` for
   `.local` names on Linux, so the tunneled probe dialed the wrong address.
   Fixed: mesh path uses the SAN-covered candidate IP as URL host while the
   CA pin still verifies that exact IP.
4. **Installer wrote legacy MCP shape** — `feanorfs integrate` emitted
   `{type:"stdio",command:<str>,args}` which current opencode rejects
   ("Configuration is invalid"). Fixed to `{type:"local",enabled:true,
   command:[exe,"mcp"]}` with checker/uninstaller updated for both shapes.
5. **Watchers can race a manual post-conflict composition** — both managed
   watchers repeatedly restored their last agreed state while the coordinator
   assembled the explicit combined file. Stopping automatic sync on both
   hosts, publishing once, pulling once, and then restarting services produced
   deterministic convergence.
6. **Historical traversal exposed a missing hub object** — after final
   convergence, `feanorfs log` on Linux returned HTTP 404 for reachable
   snapshot object `c2274d42…`; the Mac could still traverse it from local CAS.
   Current files and sync remained healthy.

## Fixes shipped after the field test

All four flow defects were fixed and re-verified on the live two-machine
deployment (2026-08-24):

1. **Complete reachability manifests** — publication now walks the bounded
   parent-snapshot DAG, so every head manifest carries its full historical
   closure; a typed missing-blob rejection triggers exactly one repair pass
   from hash-verified local cache before one manifest retry. Live proof: a
   post-fix signal from the Mac healed the hub; Linux `log --limit 100` then
   traversed all 25 entries including the previously missing `c2274d42…`.
2. **Manual resolution holds the sync lock** — single and bulk
   `conflicts keep` serialize against the watcher at the agent-core boundary;
   tray pause is now a real quiescence barrier (bounded wait for the in-flight
   sync) and the watcher rechecks pause under the lock. Live proof: idle
   `tray pause` returns in ~0.2 s; resume restores watching.
3. **Authenticated DHCP candidate refresh** — an authenticated mDNS probe now
   replaces stale LAN candidates (mapped/reflexive preserved, dedupe + 16-cap)
   in workspace and global config. Live proof: the box's config picked up the
   Mac's new `192.168.1.16:3031` LAN candidate after the subnet move.
4. **Stale mesh-path projection** — the tray projects `unreachable` once a
   winning path ages past five minutes or the clock rolls back; routing never
   consumed `last_path`, so this closes the misleading-indicator gap.
5. **Deferred refresh visibility** — human `agent refresh` output now prints
   deferred overlapping paths and names `--replace` as the explicit discard
   path; agent conflicts after a losing keep remain intentionally sticky.

Remaining cross-NAT soak (true different-networks punch, lease cleanup,
membership-gated admission) stays open in TODO AI-7.
