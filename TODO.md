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

Done when the fail-closed workflows publish notarized macOS and Authenticode
Windows products from one immutable tag. Unsigned GitHub releases must not be
presented as trusted macOS or Windows installers.

### F2. Accept onboarding on ordinary desktop sessions

Blocked on F1 for macOS and Windows.

- [ ] Install through the trusted `.dmg`, `.exe`, `.deb`, `.rpm`, and
  `.pkg.tar.zst` products as ordinary users; accept or report a reproducible
  defect in tray-first Start/Join/Not Now, login persistence, update behavior,
  and clean uninstall.
- [ ] Repeat the released Arch package and tray flow in a real CachyOS Wayland
  session. The currently available CachyOS session is i3/X11, so automated SSH
  evidence cannot honestly satisfy the Wayland acceptance requirement.

Record only OS/version and secret-free acceptance or reproduction evidence.

## AI tasks

### AI-1. Publish and validate the exact next release

- [ ] Push the completed v0.7.2 product changes, create the immutable tag, and
  validate every artifact, checksum, and attestation that can publish without
  F1 credentials.
- [ ] Install the exact published CLI archive on this Mac and the exact
  `.pkg.tar.zst` desktop product on CachyOS. Verify matching versions, tray
  visibility/folder switching, managed services, mDNS, `doctor`, and a bounded
  cross-machine sync while preserving the Mac workspaces as authoritative.
- [ ] Confirm the release clean-package jobs pass on Debian and Fedora and that
  no key, token, invite, route, passphrase, filename, or file content appears in
  release evidence, service argv/environment, logs, or discovery.

If signing credentials are unavailable, publish clearly labeled unsigned
`.dmg`/`.pkg`/`.exe` artifacts only through the explicit no-signing release
path requested by the founder; never label them trusted or notarized.
