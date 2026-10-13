# mouse-rs

Share one keyboard and mouse between a Linux (X11) desktop and a Windows laptop. Move the
pointer off the edge of one screen and it appears on the other; the keyboard follows it.
Text and files copied on one machine can be pasted on the other.

mouse-rs replaces Barrier and an earlier Python prototype. It is written in Rust for a
link that never hangs, never reports a dead peer as connected, and never leaves a key or
the cursor stuck.

| | |
|---|---|
| **Platforms** | Linux X11 (tested on Kali) and Windows 10/11, in either role |
| **Status** | In daily use on two machines. See [Project status](#project-status). |
| **Tests** | 227 passing; clippy clean on the Linux and Windows targets |

---

## What it does

- **Keyboard and mouse sharing.** Edge crossing in either direction. Windows motion is read
  from Raw Input, so tracking is 1:1 and corners are reachable. A lock hotkey (Scroll Lock by
  default) always returns control, even if the network has wedged.
- **Clipboard.** Text in both directions, plus files and folders over a separate bulk lane.
  A copied file list stays pasteable on Linux after mouse-rs quits.
- **Login screen (Linux).** A system service lets you type the Kali login password from the
  laptop, before anyone is logged in. See [docs/LOGIN-SCREEN.md](docs/LOGIN-SCREEN.md).
- **Follows the network.** Works over Ethernet, a shared router, Wi-Fi or a phone hotspot.
  The server announces itself with signed beacons. The client finds it on any network and
  moves to a better path (cable ↔ Wi-Fi) on its own. See [docs/NETWORKING.md](docs/NETWORKING.md).
- **Secure link.** TLS (rustls) plus an HMAC challenge keyed by a shared secret. A heartbeat
  detects a dead peer within seconds.
- **Control panel and tray.** An iced GUI with status, settings, clipboard history and
  profiles. It can start at login and connect on its own.
- **Diagnosable.** Each machine logs to a file and can upload that log to a collector, so a
  fault on one machine can be read from the other.

## Quick start

### 1. Build

```sh
cargo build --release                                   # Linux binary: target/release/mouse-rs
cargo build --release --target x86_64-pc-windows-gnu    # Windows exe, cross-compiled from Linux
cargo test --workspace                                  # the test suite
```

The Linux GUI links `libxdo`. `crates/mouse/build.rs` points the linker at the tracked
`.buildlibs/libxdo.so`, so a plain `cargo build` works without `libxdo-dev` (the runtime
library `libxdo3` must be installed). For cross-compiling and release archives, see
[docs/PACKAGING.md](docs/PACKAGING.md).

### 2. Configure

Settings live in `~/.mouse_share/config.json` (`%USERPROFILE%\.mouse_share\config.json` on
Windows). The control panel writes this file for you; [config.example.json](config.example.json)
lists every field.

The essentials, on both machines:

| Field | Meaning |
|---|---|
| `name` | This machine's display name |
| `role` | `server` (owns the physical keyboard and mouse) or `client` |
| `shared_secret` | Identical on both machines. Use **Security → Generate** in the panel. |
| `server_host` / `server_port` | Client only: a first address to try. Beacons find the server if it is wrong. |
| `exit_edge` / `entry_edge` | Which screen edge leads to the other machine |
| `discovery_port` | UDP port for beacons (default 24801) |
| `log_upload_url` | Optional collector that receives this machine's log |

The file holds the secret in the clear, so mouse-rs writes it with mode `0600`. Never commit
a real config.

### 3. Run

```sh
mouse-rs              # control panel (the default)
mouse-rs server       # headless server
mouse-rs client       # headless client
```

On Windows, install and update with [scripts/windows-update.ps1](scripts/windows-update.ps1).
It verifies the download's size and SHA-256, keeps the previous exe as `.bak`, restarts
mouse-rs with its previous arguments, and rolls back if the new build doesn't stay up.
[scripts/serve-windows.sh](scripts/serve-windows.sh) builds the exe and serves it, with the
installer, over the LAN.

### 4. Optional: share input at the Linux login screen

```sh
sudo mouse-rs greeter-install     # install or refresh the login-screen service
mouse-rs greeter-status           # check it (no sudo needed)
sudo mouse-rs greeter-uninstall   # remove it
```

Re-run `greeter-install` after changing the shared secret or the peer address, and after
every rebuild. `greeter-status` reports `old` when the installed copy is behind.

## How it is built

```
crates/
  mouse-core/       config, wire protocol, HMAC auth, TLS link, heartbeat,
                    discovery beacons, network watcher, bulk (file) lane
  mouse-input/      input capture and injection behind one trait:
                    X11/XTEST, uinput (login screen), Windows Raw Input + SendInput
  mouse-engine/     the edge-crossing state machine for server and client, link status
  mouse-clipboard/  clipboard sync (text, images, files, folders), history, file holder
  mouse/            the binary: CLI, iced control panel, tray, notifications,
                    login-screen service, log shipping
scripts/            Windows installer and rollback, LAN serving, packaging
docs/               design notes, runbooks, known issues, post-mortems
```

The network link is async (tokio). OS input hooks run on dedicated native threads and talk
to the core over channels, so a stuck hook can never freeze networking, and Stop never hangs.

## Project status

As of 13 October 2026:

| State | Items |
|---|---|
| ✅ Proven on real hardware | Link, Linux input, Windows Raw Input, keyboard capture without leaks, auto-reconnect, panel and tray, start at login, log shipping, text clipboard, Windows installer, discovery beacons, **cable ↔ Wi-Fi failover**, **typing the password at the Linux login screen** |
| ◷ Built, waiting for a hands-on test | Copying files end to end, files pasteable after quitting, Windows toast for a waiting file, stalled-transfer fixes, login-screen polish (toast, private log) |
| ! Open | Cut behaves as copy in Thunar (B4) |
| ⏸ Parked | Alt+Tab occasionally sticks (B13) |
| – Deferred | Panel screen-reader support, Windows login screen, Wayland as server, console flash on Windows |

The full list, with causes and evidence, is in [docs/KNOWN-ISSUES.md](docs/KNOWN-ISSUES.md).
Changes are summarised in [CHANGELOG.md](CHANGELOG.md).

## Documentation

[docs/README.md](docs/README.md) indexes every document.

## Licence

Proprietary. Copyright (c) 2026 Prashant Pal, all rights reserved. See [LICENSE](LICENSE).
No use, copying or distribution is permitted without a written agreement.

mouse-rs depends on open-source crates under permissive licences (MIT, Apache-2.0 and
similar; no GPL). Their notices must ship with any distributed build.
