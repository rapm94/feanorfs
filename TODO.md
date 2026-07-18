# FeanorFS product TODO

This is the only authoritative open-work list. Shipped work belongs in
`CHANGELOG.md`; remove completed or superseded items instead of retaining a
backlog history.

## Founder tasks

These require account ownership or representative human acceptance. Never
commit credentials or paste them into issues, logs, or chat.

### F1. Provide trusted desktop-signing access

- [ ] Add Developer ID Application/Installer and App Store Connect notarization
  credentials to GitHub Actions for the universal macOS `.dmg`/`.pkg`.
- [ ] Configure Azure Artifact Signing through GitHub OIDC for the Windows CLI,
  tray, and installer `.exe`.

Done when the existing fail-closed workflows publish notarized macOS and
Authenticode Windows products from one immutable tag. Unsigned GitHub releases
must not be presented as trusted macOS or Windows installers.

### F2. Accept onboarding on clean user machines

Blocked on F1 for macOS and Windows.

- [ ] Install through `.dmg`, `.exe`, `.deb`, `.rpm`, and `.pkg.tar.zst` on
  ordinary user accounts; verify the tray-first Start/Join/Not Now flow,
  automatic login persistence, update behavior, and clean uninstall.
- [ ] Record the OS/version and either accept the flow or provide a reproducible
  defect without workspace secrets. Include a real CachyOS Wayland session.

## AI tasks

### AI-1. Make non-empty-folder joining predictable before mutation

- [ ] Show a bounded preflight summary for local-only, remote-only, same, and
  conflicting paths before joining a non-empty destination.
- [ ] Load the destination `.feanorfsignore` before the first scan and transfer
  the mirror's ignore policy during pairing, requiring confirmation when the
  two policies differ.

Done when joining never begins a large upload or conflict set without first
showing what will happen and how the local/cloud choices work.

### AI-2. Handle large files deliberately

- [ ] Detect files over the current 100 MiB transport limit before upload and
  report bounded exact paths with ignore/remove guidance.
- [ ] Design and implement authenticated chunked encrypted transport before
  claiming support for legitimate files above that limit.

### AI-3. Finish conflict and failure UX

- [ ] Add the tested bulk local/cloud conflict choices to the tray with path
  counts, clear consequences, and a strong confirmation step.
- [ ] Bound repeated conflict-path terminal output and make JSON/human output
  exit cleanly on a closed stdout pipe.
- [ ] Report pairing completion separately from initial sync and service/tray
  installation; make partial first-run retries resume from the correct stage.

### AI-4. Prove Linux desktop behavior outside CI containers

- [ ] Diagnose missing native tray libraries before launch and provide the exact
  package-manager repair without requiring source-build knowledge.
- [ ] Verify install, tray visibility, folder switching, pairing, background
  service restart, mDNS discovery, and `doctor` on CachyOS Wayland plus clean
  Debian and Fedora desktops.

### AI-5. Verify trusted desktop releases

Blocked on F1 and F2.

- [ ] Prove tag/target identity, signatures, notarization/stapling,
  checksums/attestations, exact payload, native credential storage, automatic
  services, TLS, MCP, tray-first onboarding, and uninstall for every published
  installer.
- [ ] Confirm secrets, capabilities, private routes, filenames, and file content
  never appear in argv, environment, logs, discovery, or release evidence.
