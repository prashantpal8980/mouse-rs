# mouse-rs — project status (resume here)

A production-grade Rust rewrite of the Python `mouse_share` KVM switch: share one
keyboard/mouse between a Kali/X11 desktop and a Windows laptop over a LAN. Fixes the
Python version's three bugs (stale "connected" state, Stop/Reload hangs, Fn+Up
cursor freeze) by construction.

## Where we are (as of 2026-07-28)

| Phase | What | State |
|---|---|---|
| 1 | Cargo workspace, config, protocol, HMAC auth | ✅ done, tested |
| 2 | Async link: rustls TLS (ring), heartbeat liveness, CancellationToken Stop | ✅ done, **hardware-validated** |
| 3 | Linux/X11 input + the edge-crossing engine | ✅ done, **hardware-validated** |
| 4 | Windows input backend (Raw Input capture + SendInput inject) | ✅ done, **mouse hardware-validated**; keyboard see below |
| 5 | Clipboard sync (text) | ✅ code-complete — **still needs a deliberate 2-machine pass** |
| 6 | **iced** control panel + tray GUI | ✅ done — 6 themes, status hero, tabs, profiles, clipboard history, live tray tooltip |
| 7 | auto-reconnect + packaging | ✅ reconnect **hardware-validated**; `scripts/package.sh` (Win zip + Linux tar, bundled libxdo) |

**Verification status:** 76 tests pass; clippy clean (`-D warnings`) and release builds link
on BOTH `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-gnu` (plus the headless
`--no-default-features` build).

## Hardware status (two-machine runs, Jul 27–28)

Real layout in use: **Windows laptop = server** (has the keyboard/mouse),
**Kali desktop = client**. Note this is the *opposite* of the earlier Phase-4 test
layout, which is why the Windows **capture** path took most of the late bug-fixing.

- ✅ **Mouse sharing works flawlessly** — 1:1 tracking, corners reachable, return-by-edge.
  This came from replacing the cursor-position model with **Raw Input** (`WM_INPUT`),
  which gives true device deltas immune to edge clamping and pointer acceleration.
- ✅ **Keyboard works** — validated 2026-07-28 over a long stable session, non-elevated
  PowerShell. See "The keyboard story" below; diagnostics are still compiled in.
- ✅ **Auto-reconnect validated on hardware** — the Kali client sat retrying against a
  server that wasn't speaking its protocol yet, then connected on its own the moment the
  real server came up. No manual restart. This is F4 from the runbook, now closed.
- ⬜ **Clipboard (C1–C9) not yet deliberately exercised** on hardware.

## The keyboard story (read this before touching `windows.rs`)

Two separate faults were tangled together, which is why it looked intermittent:

1. **Hook starvation (fixed 2026-07-27).** The `WM_INPUT` mouse stream was processed on
   the same thread as the low-level keyboard hook. Fast mouse movement blew the hook's
   ~300 ms `LowLevelHooksTimeout`, so Windows skipped it and keys leaked to local apps.
   Fix: Raw Input moved to its own dedicated thread (`spawn_raw_input_thread`), leaving
   the hook thread doing only trivial work.
2. **Sessions dying underneath the test (fixed 2026-07-28).** `net.rs` classified *every*
   session read error as `ProtocolError`, which `is_recoverable()` treats as fatal — so a
   server restart (which arrives as a TCP reset) permanently stopped the client's
   reconnect loop. A dead link looks exactly like "my keys stayed on the laptop", so this
   masked whether fix #1 had actually worked.

With both fixed, the first long uninterrupted session showed keyboard and mouse both
working correctly. Fix #1 was likely always sound; we simply never got a clean session in
which to observe it.

⚠️ **Temporary diagnostics are still compiled in** — `DIAG_*` counters and a once-per-second
`kbd_diag` line in `crates/mouse-input/src/windows.rs` (~line 913). They only print while
suppressing (i.e. while control is on the peer). **Remove them** once the keyboard has held
up across a few days of normal use.

## Secret handling on disk (2026-07-28)

`Config::save` writes **0600 inside a 0700 directory** on Unix — `config.json` and every
profile carry `shared_secret` in the clear, and that secret is what authorises a peer to
inject keystrokes, so it must not be readable by other local users. Both the create mode
*and* an explicit permission set are applied **before** any bytes are written: `mode()`
alone only governs newly-created files, so an existing 0644 config would otherwise keep its
world-readable mode and leak the new secret. Windows relies on inherited ACLs.
Regression test: `config::tests::saved_config_is_owner_only_even_when_overwriting_a_lax_file`
(verified to fail against the old `fs::write`). Files created before this change need a
one-time `chmod 700 ~/.mouse_share && chmod 600 ~/.mouse_share/config.json`.

mouse-rs never persists TLS material — the self-signed cert is generated in memory per run.

## Logging (2026-07-29)

Headless `server`/`client` runs keep coloured stdout logs. A **GUI launch logs to a file**
instead — `~/.mouse_share/mouse-rs.log`, beside `config.json`, shown in the panel under
**General → Log file**.

This exists because a GUI launch has no terminal: on Windows `main` calls `FreeConsole()`
before tracing initialises, so stdout goes nowhere and a user reporting "it stopped working"
had literally nothing to send. It matters more than it sounds for a product we intend to
sell — logs are the whole support story.

- **Size-capped at 2 MB with one previous generation** (`mouse-rs.log.1`), so worst case is
  ~4 MB. Not optional: `windows.rs` still emits a once-per-second diagnostic line while
  control is on the peer, which is a few MB a day unbounded.
- **Appends** across launches — the panel gets opened and closed constantly, and truncating
  each time would discard the session that just misbehaved.
- **0600 on Unix**, matching the config hardening; the log carries hostnames and addresses.
- **Default filter puts the graphics stack at warn** (`DEFAULT_FILTER` in `logging.rs`).
  A single launch was writing ~4.5 KB of pretty-printed `WindowAttributes`, adapter
  enumerations and surface-format lists, burying the two lines a user actually needs.
  Measured 4478 → 879 bytes with the two key lines visible. `RUST_LOG` still overrides.
- Falls back to stdout if the file can't be opened — a read-only home should degrade the
  logs, not stop the app starting.

Verified on Linux by launching with stdout sent to `/dev/null`: the file appeared at 0600 and
captured startup plus `connected to server 'laptop'`. Four tests cover rotation, append,
location and permissions. **The Windows side of this is untested** — it's the platform the
fix is for.

## Tray menu (validated 2026-07-29)

Open / Start / Stop / Quit all work, driven over D-Bus rather than by clicking the panel —
see TESTING.md §6c for the commands. Start is a no-op while the engine is already running,
and Quit exits cleanly: socket closed, process gone, tray item unregistered from the
watcher, so the panel is left with no ghost icon.

One cosmetic leak, in the `tray-icon` dependency rather than our code: each run writes
`/run/user/1000/tray-icon/tray-icon-<pid>-1-0.png` and never removes it on exit, so the
files accumulate one per run. They're tiny and live on tmpfs (gone at logout), so this is
noted rather than fixed.

## Panel fixes from UI testing (2026-07-29)

Two issues that UI testing turned up, both now closed:

- **Layout tab warned on nothing.** `exit_edge = left` with `entry_edge = left` saved
  silently, even though leaving through one edge can only mean arriving through the facing
  one — any other pairing stacks the two screens on top of each other. The Layout tab now
  shows an inline warning naming the edge that would be coherent. It **warns rather than
  blocks**: the setting is a physical-desk description, and refusing to save someone's
  unusual arrangement is worse than telling them it looks wrong. Backed by
  `Edge::opposite()` (`config.rs`) and `config::tests::edges_oppose_and_round_trip`.
- **Profile messages showed the typed name, not the written one.** Saving `../../pwned`
  correctly wrote `pwned.json` (the sanitizer has always held), but the panel then echoed
  the raw string *and* set the dropdown selection to it — so the selection pointed at a
  profile that did not exist, and a following Delete reported the traversal string back.
  Fixed at the source in `SaveProfileAs`, which now uses `sanitize_profile_name`'s result
  for both the message and the selection. The path-traversal guard itself was never
  bypassed; this was the display and selection lying about it.

## Crate layout
- `crates/mouse-core` — config, `Message` protocol + codec, HMAC auth, TLS, async `net` link.
- `crates/mouse-input` — `InputInjector`/`InputCapture` traits; `linux.rs` (X11/XTEST),
  `windows.rs` (Raw Input capture + SendInput inject + LL hooks for suppression),
  `motion.rs` (X11-only warp bookkeeping).
- `crates/mouse-engine` — Local↔Remote edge-crossing state machine (`server.rs`/`client.rs`),
  injector thread, `hotkey.rs` combo parsing, `status.rs`, `ClipboardRoute` wiring.
- `crates/mouse-clipboard` — arboard-based text clipboard sync thread (loop-free) + history.
- `crates/mouse` — the `mouse-rs` binary: `server`/`client` CLI + the iced GUI
  (`gui.rs`, `ui_theme.rs`, `tray.rs`) behind the default-on `gui` feature.

## How each tricky thing was de-risked
- Every phase that couldn't be run here got an adversarial workflow/review; findings
  were fixed and re-verified. Key catches: the motion over-count bug (Phase 3), the
  ShiftR-extended + watchdog defects (Phase 4), the clipboard disconnected-queue bug
  (Phase 5). See `docs/engine-resume.md` and `docs/phase4-windows-design.md`.
- The two bugs reviews *didn't* catch were both `#[cfg(windows)]` capture code that had
  never executed, and a network error path no in-repo test reproduced (a graceful test
  shutdown yields a clean EOF, not the RST a real process-kill produces). Lesson: code
  that can't run locally stays unproven no matter how many reviews it survives.

## Build / run
```
. ~/.cargo/env
export RUSTFLAGS="-L /home/kalip/mouse-rs/.xdo-shim"   # only needed if libxdo-dev is absent
cargo test --workspace
cargo clippy --workspace --all-targets
# Windows exe (mingw + libx11 already on the Kali box):
cargo build -p mouse --release --target x86_64-pc-windows-gnu
#   -> target/x86_64-pc-windows-gnu/release/mouse-rs.exe
```
The GUI build links `libxdo` (via the tray). The **headless** build
(`--no-default-features`) needs neither libxdo nor gtk and is fully self-contained.

Config is the SAME `~/.mouse_share/config.json` as the Python app (role `server`/`client`).
Run headless: `mouse-rs server` / `mouse-rs client`. Bare `mouse-rs` opens the GUI.
Linux must be X11.

## Next
1. **Soak the keyboard** through normal daily use. If it holds, strip the `DIAG_*`
   counters and the `kbd_diag` line from `windows.rs` and cut a clean release build.
2. **Clipboard pass (C1–C9)** in `docs/TESTING.md` §5 — the one feature never deliberately
   exercised on two machines.
3. **Image clipboard** — finish the deferred `ClipboardImage` path (RGBA<->PNG + cap).
4. **Console double-click flash** on Windows — the last cosmetic nicety.
5. **Product work** (see the commercial goal): installers, code signing, auto-update.
