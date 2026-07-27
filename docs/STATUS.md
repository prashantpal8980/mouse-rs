# mouse-rs — project status (resume here)

A production-grade Rust rewrite of the Python `mouse_share` KVM switch: share one
keyboard/mouse between a Kali/X11 desktop and a Windows laptop over a LAN. Fixes the
Python version's three bugs (stale "connected" state, Stop/Reload hangs, Fn+Up
cursor freeze) by construction.

## Where we are (as of 2026-06-26)

| Phase | What | State |
|---|---|---|
| 1 | Cargo workspace, config, protocol, HMAC auth | ✅ done, tested |
| 2 | Async link: rustls TLS (ring), heartbeat liveness, CancellationToken Stop | ✅ done, tested |
| 3 | Linux/X11 input + the edge-crossing engine | ✅ done, tested (live loopback) |
| 4 | Windows input backend + Fn+Up suppression failsafe | ✅ code-complete — **needs laptop test** |
| 5 | Clipboard sync (text) | ✅ code-complete — **needs 2-machine test** |
| 6 | egui + tray GUI | ✅ control panel, tray (live tooltip), status, Quit, profiles, detect-screen, clipboard history |
| 7 | auto-reconnect + packaging | ✅ reconnect + `scripts/package.sh` (Win zip + Linux tar, bundled libxdo) |

**Verification status:** 56 tests pass; clippy clean and release builds link on BOTH
`x86_64-unknown-linux-gnu` and `x86_64-pc-windows-gnu`. Phases 1–3 are also validated
by a live loopback run. Phases 4 and 5 are the two things that can only be validated
on real hardware (the Windows laptop) — see "Next" below.

## Crate layout
- `crates/mouse-core` — config, `Message` protocol + codec, HMAC auth, TLS, async `net` link.
- `crates/mouse-input` — `InputInjector`/`InputCapture` traits; `linux.rs` (X11/XTEST),
  `windows.rs` (SendInput + LL hooks + 4-layer failsafe), shared `motion.rs`.
- `crates/mouse-engine` — Local↔Remote edge-crossing state machine (`server.rs`/`client.rs`),
  injector thread, `ClipboardRoute` wiring.
- `crates/mouse-clipboard` — arboard-based text clipboard sync thread (loop-free).
- `crates/mouse` — the `mouse-rs` binary (`server`/`client` subcommands; GUI = Phase 6).

## How each tricky thing was de-risked (all untestable-locally work)
- Every phase that couldn't be run here got an adversarial workflow/review; findings
  were fixed and re-verified. Key catches: the motion over-count bug (Phase 3), the
  ShiftR-extended + watchdog defects (Phase 4), the clipboard disconnected-queue bug
  (Phase 5). See `docs/engine-resume.md` and `docs/phase4-windows-design.md`.

## Build / run
```
. ~/.cargo/env
cargo test --workspace
cargo clippy --workspace --all-targets
# Windows exe (mingw + libx11 already on the Kali box):
cargo build --release --target x86_64-pc-windows-gnu   # -> target/x86_64-pc-windows-gnu/release/mouse-rs.exe
```
Config is the SAME `~/.mouse_share/config.json` as the Python app (role `server`/`client`).
Run headless: `mouse-rs server` (Kali) / `mouse-rs client` (laptop). Linux must be X11.

## Next (pick up here tomorrow)
1. **(recommended) Two-machine test of Phases 4 + 5** before adding more — they're the
   only un-hardware-tested code. Test plans: Fn+Up freeze, Ctrl+C composition, DPI,
   hotkey escape (`docs/phase4-windows-design.md`); clipboard copy/paste both ways +
   no echo loop + sync-on-reconnect. Deploy via a Kali `python -m http.server` the
   laptop pulls (the Python-era workflow).
2. **Phase 6 ✅ DONE** — detect-screen display + Detect button, network profiles
   (save/load/delete under `~/.mouse_share/profiles/`), clipboard-history view, live
   tray tooltip. (Remaining nicety: console double-click flash on Windows.)
3. **Phase 7 ✅ DONE** — auto-reconnect (client retries with exponential backoff 1s→15s
   after any recoverable drop, and before the server is up; `cfg.reconnect`, default on)
   + `scripts/package.sh` (Windows zip + Linux tarball, slim headless build, bundled
   libxdo). See `docs/PACKAGING.md`.
4. **Hardware retest** — F1–F4 in `docs/TESTING.md` (frozen-cursor, close=quit, combo
   hotkey, auto-reconnect) need the two-machine run; the first three are off-box paths.
5. **Image clipboard** — finish the deferred `ClipboardImage` path (RGBA<->PNG + cap).
