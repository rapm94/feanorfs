# FeanorFS product TODO

This is the only authoritative open-work list. Shipped work belongs in
`CHANGELOG.md`; remove completed or superseded items instead of retaining a
backlog history.

## Founder tasks

These require account ownership, infrastructure ownership, or representative
human acceptance. Never commit credentials or paste them into issues, logs, or
chat.

### F1. Provide trusted desktop-signing access

- [ ] Add Developer ID Application/Installer and App Store Connect notarization
  credentials to GitHub Actions for the universal macOS `.dmg`/`.pkg`.
- [ ] Configure Azure Artifact Signing through GitHub OIDC for the Windows CLI,
  tray, and installer `.exe`.

Done when one immutable tag publishes notarized macOS and Authenticode Windows
products. Unsigned releases must remain clearly labeled as development builds.

### F2. Provide the production off-LAN relay

- [ ] Choose the production relay domain, hosting account, budget, retention
  policy, abuse controls, and privacy terms.
- [ ] Add its deployment credentials through GitHub/environment secret stores;
  never place them in the repository or client builds.

Done when a normal user can pair away from home without configuring a relay URL,
while the relay remains unable to see workspace IDs, bearer tokens, paths, or
encrypted payload contents.

### F3. Accept the ordinary-user desktop experience

- [ ] Exercise install, Start/Join/Not Now, folder switching, login persistence,
  upgrade, recovery, and uninstall on supported macOS, Windows, Debian/Ubuntu,
  Fedora, and Arch/CachyOS desktop sessions.
- [ ] Record only OS/version and secret-free acceptance or a reproducible defect.

Signed macOS and Windows acceptance is blocked on F1. Off-LAN default pairing
acceptance is blocked on F2.

## AI tasks

### AI-1. Prove mixed-version upgrades are coherent

- [ ] Add installed-product tests that begin with the previous release's hub,
  workers, CLI, and tray; install the new release; and prove every login job and
  running process moves to one executable version without changing workspace
  identity, encryption state, files, or snapshot history.
- [ ] Make `doctor` report executable-version divergence directly and give one
  safe repair action. Incompatible protocol versions must fail closed with an
  actionable minimum-version message.

### AI-2. Make tray status constant-cost

- [ ] Have the managed worker publish a bounded, secret-free status snapshot in
  global state after each sync. Let routine tray refreshes read that snapshot
  without scanning the project or taking the sync lock.
- [ ] Keep an explicit fresh-status path and add a large-workspace regression
  proving tray polling cannot delay file-change synchronization.

### AI-3. Integrate the default relay after F2

- [ ] Provision the chosen endpoint through existing opaque relay APIs, add
  health/failover telemetry that contains no capabilities or workspace data,
  and cover LAN-to-off-LAN fallback in product smoke tests.
