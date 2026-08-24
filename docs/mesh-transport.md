# Mesh Transport Decision Record

Status: implemented (P0–P3). This file records the binding transport
decisions behind the direct-P2P mesh so future changes cannot silently
reverse them.

## D1 — Identity

One Ed25519 machine identity per profile, generated once and stored through
the existing OS credential store with an atomic `0600` private-file fallback.
The 32-byte verifying key is the public `NodeId`; it is public material and
rides inside capabilities. Secret keys never enter argv, environment
variables, logs, or the hub.

## D2 — Capability format v2

`MeshConfig` (version 2) carries one node id plus at most 16 canonical,
duplicate-free candidates. Candidate kinds are `lan`, `direct`, `mapped`,
and `reflexive`; transports are `tcp` and `quic`. Reflexive candidates
require QUIC. Old invites without a `mesh` field stay valid; new fields are
additive everywhere (`WorkspaceInvite`, `HubInvite`, workspace/global
config, pairing).

## D3 — Selection order

Endpoint selection races capability TCP candidates first, before same-machine
loopback assumptions, DNS, mDNS, or any relay. Only an authenticated
TLS + bearer probe wins a candidate; losers are abandoned. The winning path
and per-attempt counters persist locally in bounded `mesh-state.json`
(rebuildable, never authoritative).

## D4 — WAN reachability

UPnP IGD / NAT-PMP / PCP mapping runs on the hub host with a short bounded
gateway search and produces `mapped` TCP candidates; absence of a gateway is
normal and non-fatal. STUN binding (RFC 5389) against a small fixed server
list, raced from the hub's advertised UDP port, produces `reflexive` QUIC
candidates. Both fail gracefully to fewer candidates, never to errors.

## D5 — Coordinated punch over QUIC

The hub binds one QUIC endpoint on its advertised UDP port and bridges
authenticated streams to its local TLS port. A client punches by connecting
QUIC to a reflexive/mapped/LAN candidate, then authenticates by signing a
domain-separated message with its machine key before any bytes bridge.
The inner hub TLS session keeps its own CA pinning, SNI hostname, and bearer
token end-to-end; the punch is pure byte transport.

Ceiling: today any correctly signed machine identity may punch. Admission by
workspace membership is deferred until membership is queryable without new
hub endpoints; bridged traffic still terminates at the token-authenticated
hub.

## D6 — Relay demotion

WS tunnel and pair relays remain supported but become explicit legacy
fallback: they activate only when a relay was deliberately configured via
`--relay`/stored config AND mesh candidates could not connect. No default
hosted relay exists.

## D7 — Observability

Local-only attempt statistics classify failures (`timeout`, `unreachable`,
`authentication`, `stun`, `nat`) per transport. `doctor` reports machine-level
`ipv6_reachable`, `upnp_mapping`, and `udp_punch_capable` checks; the tray
worker snapshot projects the last observed reachability class
(`lan` | `direct` | `direct_mapped` | `punched` | `unreachable`). Nothing in
these projections contains addresses, tokens, or routes.
