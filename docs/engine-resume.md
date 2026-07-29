# mouse-rs Phase 3 — COMPLETE (2026-06-26)

Phase 3 = "wire module 3 (input) to the network into a working KVM". Done, green,
and adversarially reviewed. Start Phase 4 (Windows input) next.

## What Phase 3 delivered
- **`crates/mouse-engine`** — the edge-crossing engine binding the async link
  (`mouse-core::net`) to OS input (`mouse-input`):
  - `geometry.rs` — pure edge-band + exit→entry mapping (tested).
  - `injector.rs` — dedicated injection thread; owns the pressed-set; `release_all`
    failsafe on every exit; bounded `join_bounded`; `Injector::spawn -> Result`.
  - `server.rs` — Local↔Remote state machine; survives Disconnect; handles
    `SuppressRejected` (reverts an optimistic cross when the grab is refused).
  - `client.rs` — inject gated on `active`; arming latch; clean-slate on Enter;
    release-all on Leave/disconnect; **resets state on Connected** (Phase-7-safe).
  - `lib.rs` — `EngineLink`, `parse_hotkey`, `matches_hotkey`.
- **`crates/mouse-input` (Linux/X11)** — `linux.rs`: XTEST injector; grab/warp
  capture with a non-blocking poll loop; `grab() -> Result<bool>` (releases the
  pointer if the keyboard grab is refused → `SuppressRejected`); `emit_held_modifiers`
  via `query_keymap` (exact L/R held modifiers); `Motion` struct for relative
  motion + warp-echo absorption (see "motion" below); `CaptureSignal{Cursor,Input,
  SuppressRejected}`; `Suppressor`; `screen_size()`.
- **`mouse-core::net`** — `pump` sends are cancel-aware via `send_cancelable` (a
  back-pressuring peer can't delay Stop). `LinkEvent::Connected{peer,width,height}`.
- **`main.rs`** — runs link+engine per role; **cross-cancel**: `select!` on both
  task handles, cancel the other when either ends, then off-reactor teardown.

## Verification (all green as of 2026-06-26)
- `cargo build` clean; `cargo test` = **50 pass** (16 core + 17 engine + 17 input);
  `cargo clippy --workspace --all-targets` clean.
- Windows cross-compile `x86_64-pc-windows-gnu` = **1.05 MB exe, 0 warnings**.
- Loopback smoke (live X, plaintext): handshake OK, dims auto-detected (1440×900),
  SIGINT shutdown **~1 ms**, no lingering procs; a real edge-cross exercised
  grab/ungrab live. Script: `…/scratchpad/smoke.sh` (binary is `target/release/mouse-rs`;
  config via an isolated `$HOME/.mouse_share/config.json`).
- **Adversarial workflow** (`verify-engine-fixes`, 9 verifiers + completeness critic)
  confirmed all review fixes correct and caught one real bug, now fixed:

### The motion over-count bug (found by verification, fixed)
The first cut of `handle_event` reset `grab_last` to centre the instant a warp was
issued. The X server batches `MotionNotify` between reads, so real pre-warp events
still queued behind the trigger were then diffed against centre → spurious ~w/4
jumps on every fast near-edge flick, re-warping each time. Fixed by the `Motion`
struct: deltas stay relative to the **true previous event**, and the warp's own
centre echo is absorbed via a `pending_warps` counter (not "assume next event is
the echo"). Covered by 5 unit tests incl. `batched_fast_flick_to_edge_does_not_overcount`.

## Deferred / follow-ups (intentionally not done for MVP)
- **X11 capture test coverage**: the motion math is now unit-tested, but `grab()`,
  `emit_held_modifiers`, and `SuppressRejected` emission still lack tests (would need
  abstracting the X ops behind a trait so a fake backend can drive `capture_loop`).
  The completeness critic flagged this asymmetry; it's the main remaining test gap.
- **Char `lock_hotkey`** (e.g. "a") silently defaults to scroll_lock (a `warn!` fires).
- **XInput2 RawMotion** to replace warp/recenter entirely (avoids the echo dance and
  the ~200 Hz idle `query_pointer` poll while local).
- **Bounded/back-pressured `to_peer`** + motion coalescing under network stall (LAN-fine now).

## Phase map
Phase 1 ✅ core, 2 ✅ net, 3 ✅ Linux input + engine,
**4 ✅ Windows input + suppression failsafe / Fn+Up fix (code-complete, pending a real
Windows-laptop test — see `docs/phase4-windows-design.md` for the test plan)**,
5 = clipboard, 6 = iced+tray UI, 7 = reconnect/packaging. Both backends implement the
same `InputInjector`/`InputCapture` traits, so the engine is unchanged across them.

## Build
`. ~/.cargo/env` first. `cargo test`; Windows exe:
`cargo build --release --target x86_64-pc-windows-gnu`. mingw-w64 + libx11 on the box.
