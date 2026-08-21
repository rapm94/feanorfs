# Threat Model

This document provides a detailed security analysis of FeanorFS. For the policy on reporting vulnerabilities, see [../SECURITY.md](../SECURITY.md).

## System model

```
┌─────────────────────────────────────────────────────────────┐
│  Trusted zone (client machine)                              │
│                                                             │
│  ┌───────────────┐   ┌─────────────────┐                    │
│  │  feanorfs CLI │   │ ~/.feanorfs/    │                    │
│  │  (encrypt/    │   │  config.json    │ ← credential ref   │
│  │   decrypt)    │   │  local_state.json│ ← plaintext hashes │
│  └───────┬───────┘   └─────────────────┘                    │
│          │             native credential store / 0600 fallback│
│          │                                                  │
└──────────┼──────────────────────────────────────────────────┘
           │ Rustls HTTPS + encrypted blobs + opaque object IDs
           │
┌──────────┼──────────────────────────────────────────────────┐
│          ▼          Untrusted zone (server)                  │
│  ┌───────────────┐   ┌─────────────────┐                    │
│  │ Axum + Rustls │   │ server-data/    │                    │
│  │ (token req.)  │   │  db.sqlite      │ ← heads, manifests │
│  │               │   │  blobs/<hash>   │   encrypted objects│
│  │               │   │  tls/           │ ← private hub CA   │
│  └───────────────┘   └─────────────────┘                    │
└─────────────────────────────────────────────────────────────┘
```

## Assets

1. **File contents (plaintext)** — the data being synced. Confidentiality is the primary security goal.
2. **Encryption password** — the secret that protects file contents.
3. **Workspace structure** — format-v3 paths and executable intent live inside encrypted tree objects. Legacy formats expose paths, sizes, modification times, and hashes.
4. **Local cache integrity** — `local_state.json` and `config.json` on the client. Tampering could cause desync or data loss. Legacy `local_cache.db` files remain only until the one-time importer archives them.
5. **Hub CA private key and bearer token** — authenticate the private hub endpoint and authorize API access. The CA private key remains only under the hub data directory; invites carry its public certificate.

## Automatic private-hub lifecycle

On a first machine with no saved connection, `feanorfs start [folder]`
provisions `~/.feanorfs/hub-data` and installs one per-user hub service. The
service command contains only the canonical data-directory path. The worker
loads the bearer token and CA material from private files, enables native TLS,
and never receives credentials, pairing codes, invites, or recovery
passphrases through argv or environment variables. Receiver-side tray pairing
likewise sends `fnp1`/`fnp2` only through a bounded stdin pipe into the ordinary
`PairCode`/`start` path; the capability never enters process metadata. Workspace watchers remain
separate services and read their protected local configuration.

The host workspace connects over loopback so its operation does not depend on
the router address. When a workspace invite is exported, FeanorFS rewrites a
loopback URL to a stable hostname derived from the managed hub's public CA only
after that CA matches the durable CA on disk. mDNS tracks the hostname's current
interfaces across DHCP changes. This is endpoint selection, not a trust
decision: the receiving client still performs normal TLS chain and hostname
verification with the capability-pinned public CA and authenticates with the
existing bearer token. A numeric legacy endpoint is persisted as the stable
name only after that authenticated probe succeeds. The CA private key never
leaves the hub data directory.

## Adversary model

### Adversary A: Passive server operator

**Can observe:** All encrypted blobs, workspace heads, reachability manifests containing opaque blob IDs, ciphertext sizes, object counts, access timing, and all API requests. A migrated format-v3 workspace does not store filenames or directory structure in server metadata.

**Goal:** Recover plaintext file contents.

**Result:** **Defended against** for format-v3 object contents and structure, with caveats. Without the encryption key, the server cannot decrypt file, tree, or snapshot objects. However:
- A weak historical password can be brute-forced offline if the attacker knows any `(path, plaintext)` pair. New format-v2/v3 create/link paths accept only canonical 64-character lowercase-hex recovery keys and validate them before writing workspace or global state. Legacy format-v1 keys remain readable solely so those workspaces can migrate; use `feanorfs migrate --rekey` when the historical key was human-chosen.
- Legacy format-v1 and format-v2 workspaces expose file paths in cleartext until migration completes.
- Reachability manifests expose equality, object counts, retention, and access patterns even though their IDs reveal no plaintext names.

### Adversary B: Active server operator (malicious or compromised)

**Can observe and modify:** All blobs, all metadata, all API responses.

**Goal:** Alter decrypted file contents on the client, inject malicious files, or recover plaintext.

**Result:** **Partially defended against.**
- **Ciphertext tampering (new blobs)**: New blobs are sealed with ChaCha20-Poly1305 AEAD (`pack_bytes`). Tampered ciphertext fails authentication on decrypt. Additionally, the client re-hashes downloaded ciphertext against the expected `encrypted_hash` before decrypting.
- **Legacy downgrade (v1 workspaces only)**: Format-v2 and format-v3 workspaces reject non-AEAD blobs. Unmigrated v1 workspaces still fall back to the legacy XOR stream on decrypt. Run `feanorfs migrate`; removing XOR entirely requires separately approved representative field evidence.
- **Metadata lies**: a malicious server can still lie in `SyncResponse` (supply a hash matching substituted ciphertext); AEAD limits this to replaying validly-encrypted blobs for that same path.
- **Replay attacks**: The server can replay an older valid snapshot head or blob. Compare-and-swap prevents honest concurrent writers from silently replacing each other, but it does not authenticate a malicious server's head response.
- **Metadata manipulation**: A malicious server can hide snapshots, omit objects, or return an older head. Authenticated encryption detects modified ciphertext, not omission or rollback.
- **Migration races**: A durable workspace fence rejects writes without the migration token from the initial pull through format stamping. The stamp, flat-row deletion, and fence release share one SQLite transaction.

### Adversary C: Network attacker (MITM)

**Can observe and modify:** All network traffic between client and server.

**Goal:** Recover plaintext, inject malicious updates, or disrupt sync.

**Result:** **Defended against by default.** `feanorfs serve` uses Rustls HTTPS,
requires bearer authentication unless `--allow-open` is explicit, and creates a
durable private CA when no public certificate is supplied. `fnh1` hub invites
and encrypted `fnr1` workspace invites distribute only the public CA;
reqwest/Rustls performs normal chain and hostname verification. FeanorFS never
uses an accept-any-certificate verifier.

`--allow-http` explicitly removes this defense. It is intended only for a
private loopback listener behind a correctly configured TLS reverse proxy or a
development environment. Exposing that listener allows token capture, response
injection, and the passive/active server attacks above.

### Adversary D: Local attacker on client machine

**Can observe:** All files available to the logged-in account, including the workspace and its private state under `~/.feanorfs/workspaces/<opaque-id>/` (endpoint, random credential reference, plaintext hashes, conflict metadata, session markers, and local access history). On a headless system where the native credential store was unavailable at first setup, the private config fallback also contains the key. FeanorFS deliberately leaves no metadata inside the project.

**Goal:** Recover the encryption password, then decrypt all server-side blobs.

**Result:** Config-file disclosure alone no longer reveals credentials when the native store is available. FeanorFS uses macOS Keychain for signed releases, Windows Credential Manager, or Linux Secret Service in-process and leaves only a random reference in JSON. Unsigned macOS/source builds and unavailable stores use atomic `0700`/`0600` Unix files; an already-migrated config fails closed instead of spilling secrets back to disk. **A fully compromised logged-in account remains outside the protection boundary** because malware may invoke that user's credential APIs or capture plaintext from the working tree/process.

### LAN pairing attacker

**Can observe and modify:** mDNS announcements and the ephemeral TCP pairing
exchange. Can race legitimate clients or open connections to the pairing port.

**Goal:** Recover the server token/E2EE key, substitute an invite, or prevent
pairing.

### Agent signals and workspace participants

Named agents coordinate through encrypted signals stored in snapshot history
(`ffmsg1` envelopes in `Snapshot.message`; see
[agent-communication.md](agent-communication.md)). Their privacy and integrity
limits follow from the shared-workspace trust model:

- **Shared reader access.** Every workspace participant holds the workspace
  E2EE key, so every participant can decrypt and read every signal, including
  messages addressed to another agent. Recipient routing is **not an
  access-control boundary**; signals must never carry secrets intended for
  fewer than all participants (credentials, recovery kits, pairing codes,
  `.env` values).
- **Advisory attribution.** `Snapshot.author` is a claimed agent name; v1
  provides no per-agent signatures, so any participant can impersonate another
  name. Treat sender identity as advisory coordination metadata, not
  authentication.
- **No wakeup, no complete-delivery guarantee after bounds reset.** Signals
  cannot start an inactive model; inbox reads never publish read receipts, may
  redeliver, and explicitly set `cursor_reset` when cursor, scan, or result
  bounds mean older signals may have been missed. These are availability
  properties of the coordination protocol, not confidentiality boundaries.
- **Hub opacity is unchanged.** The hub stores only ordinary ciphertext
  objects, manifests, and heads; it never sees signal bodies, routing, or
  snapshot context in plaintext.

**Result:** **Defended against for passive capture and wrong-code substitution;
denial of service remains possible.**

### Continuous active-agent reconciliation

Automatic land/refresh for actively owned agents (`agent run` lifetime or an
enabled configured runner) reuses the existing snapshot, conflict, and
lease machinery, and adds these limits to the model:

- **Incomplete WIP becomes visible.** Automatic snapshots are unfinished
  transport state. The debounce and stable-read machinery reduce torn edits,
  but never eliminate them; only an explicit `result` referencing a settled
  snapshot claims inspection/testing. Reviewers must not treat WIP heads as
  verified.
- **Stale controller status is never authoritative.** Status projections are
  advisory; readers verify the process-lifetime lease before trusting
  `active`, and every controller re-reads files/head at startup instead of
  trusting its own previous status.
- **Waiter exhaustion is bounded.** Head-wait capacity is bounded globally
  and per opaque workspace; waits are capped below the transport read-idle
  timeout and released on disconnect, so a malicious participant can at most
  exhaust its own share of waiter slots, not wedge the hub. Old hubs ignore
  wait parameters; clients fall back to bounded jittered polling.
- **Process isolation limits are unchanged.** Continuous controllers spawn
  processes only through the existing explicit activation surfaces (the user
  launches `agent run`, or an operator configured and enabled a runner).
  Agent worktrees remain data isolation, never process sandboxing; a
  compromised active agent can read anything its user account can read.
- **Ownership is fail-closed.** One process-lifetime lease owns
  reconciliation per agent; a second owner, manual land/refresh during
  active ownership, missing/replaced worktrees, and corrupt controller
  state stop for explicit attention instead of mutating.
- **No replay of ambiguous execution.** The continuous controller retries
  idempotent file reconciliation, but an interrupted runner request is still
  marked ambiguous and never replayed automatically.

**Result:** **Defended against for cross-machine tampering within the
existing shared-key trust model; incomplete-WIP visibility, waiter capacity
exhaustion, and data-only isolation remain documented product limits.**

### Random integrator assignment (dispatcher)

The consumer-layer orchestrator that randomly picks one temporary integrator
per batch (`agent integrator`; `ffint1` profiles inside `ffmsg1` bodies) adds
these limits to the shared-workspace model:

- **Dual dispatchers are unsupported and fail closed.** The workspace
  orchestration lock (`orchestrator/dispatcher.lock`) rejects a second local
  dispatcher; cross-machine dual dispatchers are prevented only by runner
  ownership discipline and fail-safe documentation. Losing the dispatcher
  state file never authorizes a new integrator automatically.
- **Spoofed integrator replies.** A participant can claim another agent name
  and send `accepted`/`result`/`blocked` profiles. The dispatcher matches
  `assignment_id`, attempt, and `reply_to` and treats identity as advisory;
  safety comes from cooperative lifecycle checks, not cryptography. A
  malicious participant with the workspace key can read and forge all
  coordination traffic — the workspace key is the trust boundary.
- **Stale or superseded integrators.** Re-check-before-mutate, immutable
  fallback order, and stale-attempt rejection limit (but cannot cryptographically
  prevent) an accepted agent acting after revocation; the dispatcher must stop
  the controlled agent process or ask the user before an ambiguous fallback.
- **Code-dump and prompt-leak boundaries.** `ffint1` bodies and digests are
  bounded summaries; the NDJSON wakeup events omit bodies, task details, and
  paths. Signals must never carry patches, file contents, raw logs, private
  prompts, credentials, `.env` values, or recovery material.
- **Random selection is not an election.** The auditable draw is reproducible
  from the recorded nonce and roster, but it does not prove liveness, identity,
  or authority. Presence must come from the dispatcher's own runner control,
  never from signal recency.

**Result:** **Defended against for passive capture and wrong-code substitution;
denial of service remains possible.** Assignment and identity are advisory
coordination metadata, not access control.

- The displayed `fnp1` code contains a public 20-bit session tag plus a random
  80-bit single-use code; 60 bits remain secret after its 20-bit rendezvous
  tag. mDNS publishes only protocol version, tag, address,
  and ephemeral port—not the secret, workspace ID, hub URL, token, or key.
- The desktop tray requests a pairing session through a hidden CLI mode whose
  captured stdout contains only the ephemeral code and expiry. The code is not
  placed in argv or logs; the tray never receives the full invite, hub token,
  E2EE key, or PAKE state, and closing the native dialog terminates the CLI
  child and its listener. The tray clears the copied code only when it is still
  the current clipboard value, so newer clipboard contents are preserved.
- RustCrypto `spake2` 0.4 authenticates the one-time secret. A passive observer
  gets no offline password test; an active connection gets one online guess.
  FeanorFS accepts at most three connections before invalidating the session.
- The SPAKE2 output is domain-separated with Blake3 and used only as a
  ChaCha20-Poly1305 key. Separate random nonces and AAD protect the invite and
  key-confirmation response. The invite, derived secret, and pairing code are
  zeroized on drop where their owning types permit it.
- A correct exchange is single-use and expires after five minutes by default.
  A LAN attacker can consume the three connection slots, advertise the same
  public tag, block mDNS, or block TCP; these are denial-of-service attacks and
  do not reveal the invite.
- The encrypted pairing payload includes the public hub CA certificate, so a
  managed loopback URL can be rewritten to its CA-derived stable `.local`
  hostname without trust-on-first-use or disabling hostname verification.
- The automatic hub's leaf certificate includes that stable hostname and mDNS
  updates its public addresses as interfaces or DHCP leases change. A forged
  announcement may redirect traffic, but cannot complete TLS without a leaf
  signed by the pinned CA; endpoint migration also requires the bearer token.
  The durable CA and token do not rotate automatically, and neither enters discovery.
- Upstream states that the `spake2` crate has not received an independent
  third-party audit. Pairing is an introduction convenience layered over hub
  TLS; retain full-invite transfer as a fallback.

### Hub-invite attacker

**Can observe:** An `fnh1` secure hub invite copied from `feanorfs serve`.

**Result:** The invite is a capability and contains the hub URL, bearer token,
and public CA certificate. Possession authorizes hub API access but does not
reveal any workspace E2EE key or decrypt existing format-v3 objects. Interactive
output copies it to the clipboard; redirected output hides it unless
`--show-invite` is explicit. Treat it like a password and rotate the bearer
token if exposed.

Using a replacement `fnh1` invite with an existing folder requires HTTPS and a
bearer token. The client verifies the replacement CA, token, and existing opaque
workspace head before changing local trust. It preserves the E2EE key and
workspace identity, and a failed probe leaves the old connection untouched.
This makes re-pairing after an intentional hub identity rotation fail closed;
the crash-safe server-side rotation operation is implemented separately.

### Off-LAN pairing and inner-TLS relay

**Can observe:** Source and receiver IP addresses, connection timing, random
128-bit pairing session IDs or 256-bit tunnel routes, connection/frame counts,
and bounded opaque frame sizes.

**Cannot observe:** The 80-bit pairing secret, full `fnp2` capability, derived
PAKE key, workspace invite, hub token, workspace ID, API path, object name,
E2EE key, or bytes inside the tunneled Rustls connection.

**Result:** The relay can drop, delay, replay, reorder, or substitute frames and
can exhaust the bounded pending-session pool. SPAKE2 gives observed protocol
frames no offline test for the pairing secret; AEAD and explicit key
confirmation reject substituted or wrong-secret exchanges. Availability and
traffic-analysis attacks remain possible. Pairing sessions expire after fifteen
minutes, exchanges after 30 seconds, and the server forwards at most eight
16-KiB binary frames. Tunnel host offers expire after 90 seconds and are bounded
globally and per route; active tunnels have concurrency, 64-KiB frame, 16-GiB,
and 24-hour limits. Relay URLs require WSS except for explicit loopback tests.

Tunnel clients retain the CA-bound hub hostname as TLS SNI while resolving it to
an ephemeral loopback bridge. The relay can cross-wire or alter bytes, but the
normal Rustls certificate/hostname check and bearer authentication reject the
connection. Learning a route permits targeted denial of service, not hub access.
The relay stores no frames. No hosted default exists yet, and direct NAT
traversal remains out of scope for this implementation. Server HTTP trace spans
omit request URIs so tunnel routes are not persisted by default logging. The
hardened OCI deployment binds internal HTTP to an operator-owned TLS reverse
proxy; that proxy must likewise omit or redact request paths because its access
logs sit outside FeanorFS's logging controls.

### Adversary E: Offline brute-force attacker

**Can observe:** A ciphertext blob and the corresponding file path (e.g., from a server backup leak).

**Goal:** Recover the password and/or plaintext.

**Result:** **Partially defended against.** The key derivation is `blake3(password ‖ path)` with no salt, no KDF, no stretching. Blake3 is fast (~1 GB/s), making brute-force attempts very cheap. A password with &lt; 80 bits of entropy is vulnerable. **Use a high-entropy password (≥ 20 random characters, or a Diceware passphrase with ≥ 6 words).**

## Cryptographic analysis

### Current encryption construction (AEAD, new blobs)

```
key       = blake3("feanorfs-aead-v1" ‖ len(password) ‖ password ‖ len(path) ‖ path)
nonce     = blake3("feanorfs-aead-nonce-v1" ‖ key ‖ len(data) ‖ data)[..12]
blob      = 0x01 ‖ nonce ‖ ChaCha20-Poly1305(key, nonce, plaintext)
```

**Properties:**
- **Authenticated**: tampered ciphertext fails the Poly1305 tag check on decrypt.
- **Deterministic (SIV-style)**: the nonce is derived from the key and plaintext, so the same `(password, path, plaintext)` always produces the same blob. This is required for content-addressed storage (the encrypted hash must be stable across clients) and for cheap change detection. Since each `(key, nonce)` pair only ever seals one plaintext, nonce reuse is not a concern.
- **Path-dependent** with length-prefix domain separation: `(password="ab", path="cdef")` and `(password="abc", path="def")` derive different keys.

**Known properties / weaknesses:**
1. **Determinism leaks reverts**: identical plaintext at the same path always yields identical ciphertext, so the server can observe "this file returned to a previous state." This is an accepted CAS-stability trade-off.
2. **Legacy XOR fallback (v1 only)**: unmigrated workspaces still decrypt pre-AEAD blobs via XOR. Run `feanorfs migrate`; removal requires separately approved representative field evidence.
3. **Fast key derivation**: single Blake3 pass — v2 workspaces require 64-hex generated keys (256-bit CSPRNG).

### Legacy construction (pre-AEAD blobs, decrypt-only)

```
keystream  = Blake3_XOF(len(password) ‖ password ‖ len(path) ‖ path)
ciphertext = plaintext XOR keystream
```

Unauthenticated and malleable — an attacker who knows plaintext at a position can substitute arbitrary content by XORing the difference. Retained only so pre-AEAD blobs remain readable until `feanorfs migrate` re-seals them.

### Hash usage

- `plaintext_hash = blake3(plaintext_bytes)` — stored only in the local cache. Never sent to the server.
- `encrypted_hash = blake3(ciphertext_bytes)` — the CAS blob key. Sent to the server. Used to verify upload integrity (server-side) and download integrity (client re-hashes ciphertext before decrypt).
- The server verifies `blake3(uploaded_bytes) == claimed_hash` on upload, rejecting mismatches with HTTP 400.

## Risk summary

| Risk | Severity | Mitigation | Status |
|---|---|---|---|
| Plaintext leakage via weak password | High | New format-v2/v3 setup accepts only generated-shape 256-bit keys before persistence; migrate legacy format-v1 workspaces with `--rekey` | Implemented for new setup; historical keys require migration |
| Legacy XOR decrypt path (unmigrated v1 workspaces) | Medium | `feanorfs migrate` to format v3; remove XOR only after separately approved representative evidence | Open |
| Ciphertext tampering (AEAD blobs) | — | ChaCha20-Poly1305 authentication | Implemented |
| Network MITM | — | Native Rustls HTTPS, private-CA pinning through capability invites, normal hostname verification | Implemented by default |
| LAN pairing secret interception/substitution | Medium | SPAKE2, AEAD, key confirmation, three online attempts, expiry, secret-free mDNS | Implemented; upstream PAKE crate unaudited |
| LAN pairing denial of service | Low | Short expiry, retry with a new code, full-invite fallback | Known limitation |
| Off-LAN rendezvous observation/substitution | Medium | 128-bit public session IDs, SPAKE2, AEAD invite delivery, key confirmation, WSS, strict frame/session/time bounds | Implemented; relay can deny service and observe traffic metadata |
| Opaque tunnel route disclosure/denial | Medium | Random 256-bit route, four host offers, global/per-route/connection bounds, inner TLS CA + bearer authentication | Implemented; route disclosure permits targeted DoS and traffic analysis, not hub access |
| No server authentication | Medium | Token required by default; `--allow-open` is explicit development mode | Implemented by default |
| Hub CA key loss | Medium | Offline encrypted recovery bundle preserves the CA and token; Argon2id + XChaCha20-Poly1305; crash-safe import fence | Implemented; passphrase loss remains unrecoverable |
| Hub CA key theft | Medium | Unix `0700` TLS directory + `0600` material; stop the hub, run crash-safe CA/token rotation with a mandatory encrypted backup, then authenticate the replacement `fnh1` capability on every client | Implemented; compromise detection remains operational |
| Workspace credential loss | Medium | Offline recovery kit seals the complete portable capability with Argon2id + XChaCha20-Poly1305; import authenticates before local writes and delegates to `start` | Implemented; kit and passphrase loss remain unrecoverable, and the hub must retain the encrypted snapshot |
| Workspace recovery-kit disclosure | High | Kit exposes only versioned KDF/cipher metadata plus authenticated ciphertext; `0600` atomic write, 12-character minimum passphrase, no secret argv/env/logs | Offline guessing remains bounded by passphrase strength; store kit and passphrase separately |
| Unattended local credential access | Medium | Native OS credential store with random config references; atomic private-file fallback only when unavailable | Implemented; logged-in account remains trusted |
| Migration journal stores old and target keys | Medium | Journal stays in the private global workspace directory and is removed after successful cutover | Temporary local exposure |
| Metadata leakage (sizes, counts, equality, timing) | Medium | Format v3 encrypts paths and structure; no size padding is claimed | Accepted limitation |
| No password-stretching KDF for content keys | Medium | New workspaces require generated-shape 256-bit keys; migrate weak historical keys with `migrate --rekey` | Historical format-v1 limitation |
| Silent or unreachable hub stalls every client command | Medium | Bounded transport timeouts: 15 s connect, 60 s read-idle, 8 s version probe; a blackholed or paused hub fails sync with a retryable error instead of wedging the watcher and CLI forever | Implemented (`HTTP_CONNECT_TIMEOUT`/`HTTP_READ_TIMEOUT` in `agent-core/src/api.rs`) |
| Replay attacks (old snapshot heads or blobs) | Low | Immutable history and observed-regression warnings; no external transparency log is claimed | Accepted limitation |
| Update-metadata redirection or code execution | Medium | HTTPS-only bounded no-redirect GitHub API request, pinned API version, stable-semver parsing, exact official tag-URL validation in CLI and tray, explicit browser-open choice, no artifact download/install/execute | Implemented; platform signature/checksum/attestation gates remain authoritative |

## Automatic conflict resolution

The engine's automatic resolution path is the last resort after proactive
`ffwork1` coordination failed to prevent a conflict. Its trust boundaries:

- **Last-resort eligibility is engine-proven.** A job can be prepared only
  when the authenticated projection shows no non-terminal coordination for
  the path; caller prose, skill assertions, or booleans never authorize
  preparation. Legacy unfingerprinted conflicts are manual-only forever.
- **The engine never produces candidates.** The designated agent/harness
  materializes the authenticated legs locally, produces only a candidate
  plus evidence through typed engine APIs (`put`/`submit`), and the engine
  executes a fixed verification policy before anything can be recorded.
- **Designation is auditable.** The causally behind eligible agent is
  selected from transitive ancestry over applied coordination messages;
  only a documented tie or unavailable-owner case falls back to a
  deterministic `ffint1` ranking whose nonce, roster, and ranking are
  persisted and published. Capabilities, explicit yield, task ownership,
  accepted scope, and existing assignments are enforced from projection
  state, never from caller labels.
- **Publication is guarded.** Immediately before the single head
  compare-and-swap, the engine reloads and revalidates the complete
  identity, registry record, head, causal references, candidate, result,
  scope, and ownership. A lost CAS restarts full validation; failures of
  any kind leave the conflict and its artifacts intact for manual action.
- **Humans are asked only for a typed ambiguity.** A human request is one
  bounded question with a closed-enum reason and typed safe options
  (defer/keep unresolved); answers bind to the exact job, assignment,
  fingerprint, and question generation and re-enter the same guarded
  publication validation as an agent result.
- **The hub stays opaque.** `ffres1` assignment/result/revoke/answer
  profiles travel inside the existing encrypted signal stream; the hub
  gains no semantic route, no plaintext metadata, and no resolution logic.

## Remaining security work

Current committed security and release follow-up is tracked in
[TODO.md](../TODO.md). Legacy-crypto removal is not yet an approved
compatibility change; hosted recovery and a default relay are not current
product commitments; and an independent cryptographic review has not been
scheduled. Any of those initiatives requires an explicit product decision,
owner, and acceptance criteria before it is added to the authoritative TODO.

## Process isolation (agents)

FeanorFS's agent workspaces provide **data isolation, not process sandboxing.**

**What IS isolated (data):** each agent works in its own `worktree/` under the private global workspace state; nothing reaches the main folder or the server without going through `agent land`'s three-way diff. Conflicted paths are blocked from sync with all versions preserved (on disk and as immutable content-addressed blobs). An honest agent cannot make a mess.

**What is NOT isolated (processes):** `agent run` sets the child's working directory — nothing else. The child inherits the full filesystem (absolute-path writes escape the agent folder), network access, and environment variables. An agent that pushes to another repo, exfiltrates secrets, or runs a malicious `build.rs` is not stopped by FeanorFS.

**Composition rule:** run untrusted or highly autonomous agents inside a sandboxed harness (Claude Code / Cursor sandboxes, containers, `sandbox-exec`, firejail) pointed at the agent folder. FeanorFS contains honest mistakes; the sandbox contains everything else.

**Reconciliation trust:** manual conflict resolution is performed by a consumer (human or LLM) through explicit `conflicts keep`; all versions remain recoverable. Automatic resolution is a bounded engine-mediated last resort with the same trust boundary: FeanorFS never merges or chooses file content — it designates a resolver from authenticated protocol state, transports an immutable job, executes a fixed verification policy, and publishes only an exact still-current candidate through one compare-and-swap. See [Automatic conflict resolution](#automatic-conflict-resolution). LLM merges should be verified in a spawned agent workspace before `conflicts keep --file`.
