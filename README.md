# mouse-rs

A fast, safe, Barrier-like KVM software switch. Share one keyboard and mouse
between a Linux (X11) machine and a Windows machine on the same LAN.

This is a ground-up Rust rewrite of an earlier Python prototype, built for
production use: native performance with no GC pauses, compile-time memory/thread
safety, clean async cancellation (no more hangs on Stop/Reload), and robust
dead-link detection via heartbeat + timeouts.

> Status: **early development.** See the phase checklist below.

## Why a rewrite

The Python version suffered three structural bugs that were hard to fix in that
runtime:

1. **Stale "connected" state** — a blocking socket read never noticed a dead peer.
2. **Stop/Reload hangs** — blocked threads can't be interrupted in Python.
3. **Fn+Up froze the cursor** — a fragile input hook with no failsafe.

mouse-rs addresses all three by construction: length-delimited framing with
per-read timeouts and a PING/PONG heartbeat (1, 2), tokio cancellation tokens for
instant clean shutdown (2), and an input layer isolated on its own thread with a
watchdog that force-releases suppression (3).

## Architecture

```
crates/
  mouse-core/   platform-agnostic: config, protocol, auth, async net link
  mouse/        the binary: CLI + (later) egui control panel + tray
  (mouse-input) OS input capture/injection behind a trait — added in Phase 3/4
```

The network link is async (tokio); OS input hooks run on dedicated native threads
and talk to the core over channels, so a stuck hook can never freeze networking.

## Config

mouse-rs reads the same `~/.mouse_share/config.json` as the Python app — your
existing settings carry over unchanged. A UTF-8 BOM is tolerated.

## Build

```sh
cargo build --release                              # this machine
cargo build --release --target x86_64-pc-windows-gnu   # Windows .exe from Linux
cargo test                                         # run the test suite
```

## Roadmap

- [x] Phase 1 — workspace, config, protocol, auth, cross-compile
- [ ] Phase 2 — network core: TLS, heartbeat, cancellation
- [ ] Phase 3 — Linux input (X11 / XTEST)
- [ ] Phase 4 — Windows input + suppression failsafe
- [ ] Phase 5 — clipboard sync
- [ ] Phase 6 — egui control panel + tray
- [ ] Phase 7 — reconnect, packaging, parity

## License

MIT OR Apache-2.0.
