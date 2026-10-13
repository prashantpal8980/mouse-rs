# Changelog

Dates are when the work was committed. Item codes refer to
[docs/KNOWN-ISSUES.md](docs/KNOWN-ISSUES.md) (B…) and
[docs/UI-REQUIREMENTS.md](docs/UI-REQUIREMENTS.md) (R…).

## 2026-10-13

- **Network following (R22).** Signed UDP discovery beacons, connections raced over every
  known path, a network watcher that reconnects when interfaces change, and a server that
  lets a new session supersede a stale one. Cable ↔ Wi-Fi failover is proven on hardware.
  Each connect logs which interface it used.
- **Login screen (B2).** Proven: the Kali password was typed from the laptop. The laptop
  is now told when the peer is at its login screen. It shows a toast and doesn't send the
  clipboard there. Key names are never logged at the login screen. The service's log is
  shipped, it waits instead of restarting every 3 s, and a stale installed copy is flagged.
  `greeter-install` renames the binary into place, so it cannot fail with "Text file busy".
- **Files survive quitting (B5).** A `hold-files` child process keeps a copied file list
  pasteable on Linux after mouse-rs exits.
- **Windows updater.** `scripts/windows-update.ps1` verifies, installs, restarts mouse-rs
  with its previous arguments, and rolls back on failure.
- **Plain `cargo build` (B12)** links on Linux without `RUSTFLAGS`.
- Documentation: new README, [docs/NETWORKING.md](docs/NETWORKING.md), docs index.
- Licence changed to proprietary (all rights reserved); crates marked `publish = false`.
- README rewritten for users: install, connect, update and bug-report sections, plus the one-click roadmap. The panel now warns when the login-screen copy is older than the app.

## 2026-10-08 to 2026-10-09

- Input sharing at the Linux login screen through uinput, with a panel toggle.
- A known-issues list; stuck-modifier release; Alt+Tab investigation (B13, parked).
- Post-mortem of the modifier regression; verified Windows rollback script.

## 2026-08-14 to 2026-09-04

- Files and folders of any size over a dedicated bulk lane: transfer on paste,
  delivery straight into the destination folder, resumable across restarts, bounded cache.
- Transfer speed fixes; progress for whole sets; stalled-transfer fixes (B3).
- Log shipping from both machines; elevation guidance on Windows; edges shown per role (R16).

## 2026-07-28 to 2026-08-05

- iced control panel and tray; start at login; config written `0600`.
- Windows Raw Input capture; keyboard-leak fix; clipboard images; IP detection.
- Clipboard history manager.

## 2026-06-26

- Initial commit: workspace, config, protocol, HMAC auth, TLS link with heartbeat,
  Linux input, edge-crossing engine, text clipboard.
