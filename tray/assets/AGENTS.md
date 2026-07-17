# tray assets

## Purpose

Own platform-neutral launcher and application-icon assets for the native FeanorFS tray.

## Ownership

- `com.feanorfs.tray.desktop` — Linux application-menu entry; launches the thin tray without a workspace requirement.
- `com.feanorfs.tray.svg` — scalable Linux application icon installed under the matching reverse-DNS name.

## Local Contracts

- Launcher assets may start `feanorfs-tray` only; they must not embed workspace paths, credentials, invites, or onboarding flags.
- Keep the desktop entry usable before setup and independent of any particular package install prefix.

## Work Guidance

- Keep the SVG self-contained and free of external fonts or network resources.

## Verification

- Linux package verification checks that both files are present in `.deb`, `.rpm`, and tar payloads.

## Child DOX Index

No child directories.
