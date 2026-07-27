# mouse-rs — Two-Machine Test Runbook

This is the single authoritative runbook for validating `mouse-rs` on real
hardware: a **Kali desktop** (X11) and a **Windows laptop** on the same LAN.

---

## 1. What we're testing & why

The network core, handshake/auth, and connection lifecycle (Phases 1–3) are
already validated by the in-process loopback tests under `cargo test`. What a
loopback **cannot** exercise is real OS input: low-level keyboard/mouse hooks,
DPI scaling, pointer ballistics, scroll, and the OS clipboard. So this runbook
focuses on **Phase 4 (Windows input handling)** and **Phase 5 (text clipboard
sync)** end-to-end across two machines, plus regression checks for the three
bugs that plagued the old Python build (stale "connected" after a peer drop,
Stop/Ctrl-C hanging, and the Fn+Up freeze on Windows).

> **A note on log visibility.** The network layer (`net.rs`) emits internal
> `LinkEvent`s (Listening, Connecting, AuthFailed, Error, Connected,
> Disconnected), but the engines only *log* two of them: `Connected` and
> `Disconnected`. Every other variant is consumed silently (`Some(_) => {}`).
> So several auth/TLS/bind error strings exist in the source but are **never
> printed** — you confirm those cases by **behavior** (e.g. the client exits
> right after the handshake with no "connected" line), not by grepping the log.

---

## 0. Build & deploy

### Build both targets (on Kali)

```bash
cd /home/kalip/mouse-rs
cargo build --release                                   # linux-gnu  -> target/release/mouse-rs
cargo build --release --target x86_64-pc-windows-gnu    # -> target/x86_64-pc-windows-gnu/release/mouse-rs.exe
```

Resulting binaries:
- Kali server: `/home/kalip/mouse-rs/target/release/mouse-rs`
- Windows client: `/home/kalip/mouse-rs/target/x86_64-pc-windows-gnu/release/mouse-rs.exe`

### Copy the .exe to the laptop (Python-era HTTP trick)

On Kali, serve the Windows build directory:

```bash
cd /home/kalip/mouse-rs/target/x86_64-pc-windows-gnu/release
python3 -m http.server 8000
```

On the Windows laptop, open a browser to (Kali's ICS-link IP is `192.168.137.2`):

```
http://192.168.137.2:8000/mouse-rs.exe
```

Save the file. When you run it, Windows **SmartScreen** may block it: click
**More info -> Run anyway**.

> Run the `.exe` from a **terminal** (cmd or PowerShell), not by double-clicking,
> so you can see the log output. Example:
> ```
> mouse-rs.exe client
> ```
> A bare launch with no subcommand logs
> `no subcommand (GUI lands in Phase 6); use "server" or "client"` and exits
> (the GUI is Phase 6, not built) — you must pass `server` or `client`.

---

## 1. Configure both machines

**Recommended direction: drive FROM Kali.** Kali = **server** (it has the
physical keyboard/mouse you'll use, captures + forwards). Windows = **client**
(receives + injects locally).

Why this direction: the **client connects outbound**, so you do **not** need a
Windows inbound-firewall rule. (Server re-accepts clients on its listen port;
client just dials out.)

> The role is **forced by the CLI subcommand** (`server` / `client`) and
> overrides the `role` field in config.json (`cfg.role = role;` in main.rs). The
> config `role` field is effectively ignored once you pass a subcommand — but
> keep it consistent to avoid confusion.

Config lives at `~/.mouse_share/config.json` on each machine (BOM-tolerant;
unknown fields are ignored, so old Python configs load). Recognized fields now
include `return_hotkey` (combo escape, matched on the **server**) and `reconnect`
(client auto-reconnect, default true). **The `shared_secret` MUST be identical on
both machines.**

### Pick your edges by physical arrangement

`exit_edge` (on the server) is the edge you shove the cursor **off** of toward
the laptop; `entry_edge` (on the client) is the **opposite** edge the cursor
arrives on. If the **laptop sits to the RIGHT** of the Kali monitor:
- Kali server `exit_edge = "right"`
- Windows client `entry_edge = "left"`

If the laptop sits to the **left**, swap them (`exit_edge="left"`,
`entry_edge="right"`). Opposing edges (Left↔Right, Top↔Bottom) give exact
cursor-position mapping; non-opposing pairs still stay on-screen but landing is
approximate. (If you configure a non-opposing pair the server logs a WARN:
`exit_edge ... and entry_edge ... are not an opposing pair; entry placement
will be approximate`.)

### Kali server — `~/.mouse_share/config.json`

```json
{
  "name": "kali",
  "role": "server",
  "shared_secret": "CHANGE_ME_same_on_both",
  "tls_enabled": false,
  "clipboard_sync": true,
  "lock_hotkey": "scroll_lock",
  "listen_host": "0.0.0.0",
  "listen_port": 24800,
  "exit_edge": "right",
  "entry_edge": "left",
  "screen_width": 1920,
  "screen_height": 1080,
  "edge_threshold_px": 2,
  "warp_threshold_px": 120
}
```

> Set `screen_width`/`screen_height` to your real Kali resolution. On startup the
> server detects the X11 root size; if config differs you'll see a WARN
> `config screen <W>x<H> differs from detected <w>x<h>; using detected` and it
> uses the detected size. `warp_threshold_px` is parsed but **unused** by the
> Rust engine — it is referenced only in `config.rs`, nowhere in the crossing
> logic (Phase 7 placeholder).

### Windows client — `%USERPROFILE%\.mouse_share\config.json`

```json
{
  "name": "laptop",
  "role": "client",
  "shared_secret": "CHANGE_ME_same_on_both",
  "tls_enabled": false,
  "clipboard_sync": true,
  "lock_hotkey": "scroll_lock",
  "server_host": "192.168.137.2",
  "server_port": 24800,
  "exit_edge": "left",
  "entry_edge": "left",
  "screen_width": 1920,
  "screen_height": 1080,
  "edge_threshold_px": 2,
  "warp_threshold_px": 120
}
```

> `server_host` = Kali's IP **on the link the laptop can reach**. On the
> Windows ICS link that's `192.168.137.2`. If the laptop is on Wi-Fi with Kali,
> use Kali's `wlan0` IP `10.36.179.242` instead. `server_port` must match the
> server's `listen_port`. On the client only `entry_edge` matters — it is the
> edge the cursor lands on when it crosses **in** to Windows; set it opposite the
> server's `exit_edge` (the client's `exit_edge` field is unused in this
> direction).

**`lock_hotkey` (default `scroll_lock`) is the ONE hotkey.** It both arms/gates
crossing and is the Windows failsafe self-release key (passed into
`start_capture`). There is no separate return hotkey in the Rust build.

> `tls_enabled` must match on both sides. Leave it `false` for first bring-up
> (fewer moving parts); flip both to `true` later to test the TLS path
> (self-signed cert generated fresh per run, accept-any client verifier).

### To flip direction (drive from the laptop instead)

Make Windows the **server** (`mouse-rs.exe server`) and Kali the **client**
(`mouse-rs client`, with `server_host` = the laptop's IP). Because the client
now dials **in** to Windows, add a **Windows Defender Firewall inbound allow**
for **TCP 24800** (or whatever `listen_port` you chose), e.g. in an Admin
PowerShell:

```powershell
New-NetFirewallRule -DisplayName "mouse-rs" -Direction Inbound -Protocol TCP -LocalPort 24800 -Action Allow
```

---

## 2. Smoke test — does it connect?

**Start order: server first, then client.** Use `RUST_LOG=info` so the
lifecycle log lines appear.

On Kali:
```bash
RUST_LOG=info /home/kalip/mouse-rs/target/release/mouse-rs server 2>&1 | tee /tmp/server.log
```

On Windows (cmd/PowerShell):
```
set RUST_LOG=info
mouse-rs.exe client
```

### What "connected & healthy" looks like

Only the following lines are actually printed (the Listening/Connecting
`LinkEvent`s are internal and are **not** logged):

| Side | Log line (verbatim from source) |
|------|---------------------------------|
| Both (startup) | `starting mouse-rs for <W>x<H> screen` |
| Server | `peer '<name>' connected: <W>x<H>` (from `run_server_engine`) |
| Client | `connected to server '<name>': <W>x<H>` (from `run_client_engine`) |
| Both (steady state) | no `disconnected` line. PING/PONG flows every ~2 s under the hood but is **not logged** — there is no PING/PONG log line at any level |

If both sides print their respective "connected" line and stay quiet, the link
is up and the heartbeat is keeping it alive (default heartbeat 2 s, liveness
timeout 6 s — `LinkParams::default()`).

### Reading failures

Most failure causes below carry their detail inside `LinkEvent::AuthFailed` /
`LinkEvent::Error`, which the engines do **not** log — so diagnose by behavior.
The string in parentheses exists in the source (`net.rs`) but is **not printed**
at info level; treat it as "(verify in logs/behavior)".

| Symptom (observable) | Cause | Fix |
|----------------------|-------|-----|
| Client exits right after the handshake; no "connected to server" line. Server prints no "peer connected" line. (Internal: server `bad secret from <name>` / client `AuthFailed("bad_secret")` — not logged) | `shared_secret` mismatch | Make `shared_secret` byte-identical on both |
| Client hangs after `mouse-rs.exe client` and never prints "connected"; eventually exits | wrong IP / firewall / no route (or 10 s connect timeout) | Verify `server_host`; confirm route (ICS `192.168.137.2`); if driving from laptop, add the inbound TCP rule |
| TLS handshake fails: side exits with no "connected" line (internal `tls setup: ...` / `tls accept: ...` / `tls accept timed out` / `tls connect: ...` — not logged) | `tls_enabled` mismatch or cert/crypto-provider issue | Match `tls_enabled` on both; for bring-up set both `false` |
| Client exits ~10 s after launch with no "connected" line (internal `AuthFailed("handshake timed out")` — not logged) | handshake stalled (10 s `handshake_timeout`) | Check link stability / MTU; retry |
| Server exits immediately at startup (internal `Error("bind <addr> failed: ...")` — not logged) | port in use / bad `listen_host` | Free the port or change `listen_port` |
| Kali server: WARN `could not detect screen size (...); using config <W>x<H>` then no input grab works | not an X11 session (no/invalid `DISPLAY`; e.g. Wayland) | Log into an **X11** session; `mouse-rs` requires X11 on Linux |

> To actually see the otherwise-silent internal events you'd need extra logging
> the binary doesn't emit today — so for auth/TLS/bind failures, rely on the
> behavior column above, not on grepping for those strings.

---

## 3. Edge crossing (core function)

With both sides connected and (per your config) the laptop to the **right** of
Kali:

1. **Cross Kali -> Windows.** First move the cursor **away** from the exit edge
   (this *arms* the cross — prevents auto-crossing when parked at the edge),
   then move it firmly into the Kali **right** edge.
   - **Kali cursor** appears pinned at the right edge and stops moving locally
     (suppression on; local capture grabbed).
   - **Windows cursor** warps to its **left** edge at the matching vertical
     position and becomes active — your mouse/keyboard now drive Windows.
   - **Server log:** `crossed to remote, entry (<ex>,<ey>)`.
2. **Drive Windows.** Move/click/type — input is injected on the laptop.
3. **Return to Kali — two ways:**
   - **Hotkey:** press **Scroll Lock**. Control snaps back to Kali instantly.
     **Server log:** `returned to local`. (On Windows the hotkey self-releases
     suppression in-callback even if a hook is wedged; it is **swallowed** —
     not passed to Windows apps and not forwarded to the peer.)
   - **Cross back:** on Windows move the cursor inward (arms), then back to the
     Windows **left** entry edge; the client sends a `LeaveRequest` and the
     server drops suppression (returns to Local). The server logs
     `returned to local` for this path too (the `LeaveRequest` handler calls the
     same `return_to_local`).

If the X grab is refused (a menu/drag is holding a grab), the server logs WARN
`input grab refused (another grab active?); staying local`, sends
`SuppressRejected` to undo the cross, and the cursor does **not** warp.

---

## 4. Phase 4 — Windows input deep tests

Run these with Kali=server, Windows=client, cursor crossed **over to Windows**
(client active) unless the test says otherwise. Some need an editor/web page
open on Windows to observe effects. Mark each Pass/Fail.

| ID | Test | How | Expected | P/F |
|----|------|-----|----------|-----|
| W1 | **Fn+Up / hook non-block** | While suppressed (crossed to Windows), rapidly mash `Fn`+`Down`/`Up` and other combos | Cursor/input stay responsive; **no multi-second freeze**; no SmartScreen/Event Viewer "removal of low level hooks" entry | |
| W2 | **Hotkey self-release** | While crossed to Windows, press **Scroll Lock** | Control returns to Kali within ~1 frame; server logs `returned to local`; the hotkey is swallowed locally and is **NOT** forwarded to the peer (it is consumed as the return trigger) | |
| W3 | **Watchdog pump-stale release** | Hard to force by hand; if capture pump stalls (e.g. under debugger) | After ~2 s (8×250 ms `STALE_TICKS`) suppression auto-releases; the watchdog sends `SuppressRejected`, engine reverts to Local `(verify in logs)` | |
| W4 | **Watchdog hook-independent hotkey** | If the keyboard hook is dead, hold **Scroll Lock** | Watchdog polls the key via `GetAsyncKeyState` every 250 ms and force-releases suppression even with the hook gone `(verify in logs)` | |
| W5 | **Echo absorption** | Cross to Windows and move the mouse around | Injected moves carry `INJECT_MAGIC` (`0x6D6F_7573_5253`) in `dwExtraInfo` and are **not** echoed back to Kali (`on_mouse` drops them); cursor is smooth, no jitter/jump | |
| W6 | **3rd-party injection isolation** | On Windows run AutoHotkey (or similar) to inject a mouse move while crossed | The 3rd-party move (no `INJECT_MAGIC`) is **captured and forwarded** to Kali (only `mouse-rs`-tagged events are absorbed) | |
| W7 | **DPI 150% scaling** | Set Windows display scaling to 150%; cross over and move/click at known points | Cursor lands on the **exact** intended position (no ~1.5× over/undershoot); no corner-jam. `PER_MONITOR_AWARE_V2` set once at startup (`ensure_dpi_aware` guarded by `Once`) | |
| W8 | **EPP on/off 1:1** | Toggle "Enhance pointer precision" (Control Panel ▸ Mouse); move the real mouse a fixed distance both ways | Cumulative motion forwarded is the same regardless of acceleration (absolute injection bypasses ballistics) | |
| W9 | **L/R + extended keys** | Type with **Right Shift**, **Right Ctrl**, **Right Alt**, arrows/Home/End | Right Shift injects with scancode 0x36 (NOT extended); RCtrl/RAlt/nav keys ARE extended; characters/shortcuts behave correctly, no malformed keys | |
| W10 | **AltGr** | On a layout that uses AltGr (FR/DE), type an AltGr symbol (e.g. AltGr+E for €) | Captured as a single `AltGr` (VK_RMENU 0xA5, extended), no phantom LCtrl leaking; Level-3 symbol is correct; Ctrl+C still works on peer | |
| W11 | **Scroll direction & magnitude** | Scroll up/down (and sideways) over a scrollable page on Windows | Scrolls the right direction by a sane amount; a huge/malformed value is clamped to ±32 steps (`v.clamp(-32, 32)`), no runaway wheel-spin | |
| W12 | **Teardown no-hang** | Press **Ctrl-C** in the Windows client terminal | Process exits within ~0.5–1 s; capture+watchdog threads joined with a 500 ms bound (`join_bounded`); no hang, no stuck keys (ReleaseAll on exit) | |

---

## 5. Phase 5 — Clipboard tests

Requires `clipboard_sync: true` on **both** machines (Step 0 of config). Text
**only** — images are deferred (`ClipboardImage` messages are silently ignored
by both the engine and the clipboard thread). Poll interval is 500 ms, so
**wait ~600 ms** after each copy. Use `RUST_LOG=info` (and watch
`/tmp/server.log` + the Windows console).

| ID | Test | How | Expected | P/F |
|----|------|-----|----------|-----|
| C1 | **Seed (no spurious broadcast)** | On Kali: `echo -n 'INITIAL_SEED' \| xclip -selection clipboard`; wait ~600 ms | No Clipboard message sent — pre-existing clipboard is seeded as `last`, not broadcast | |
| C2 | **Kali -> Windows** | On Kali: `echo -n 'TEST_A' \| xclip -selection clipboard`; wait; paste on Windows (Ctrl+V) | Windows pastes `TEST_A` | |
| C3 | **No echo (same text)** | On Kali copy `TEST_A` again; wait | **No** new Clipboard message reaches Windows (`last` guard) | |
| C4 | **Windows -> Kali** | On Windows copy `TEST_B`; wait; paste on Kali (Ctrl+V / middle-click) | Kali pastes `TEST_B` | |
| C5 | **No round-trip echo** | On Kali copy `TEST_C`; wait ~1.2 s | Windows applies `TEST_C` once; it is **not** echoed back to Kali (apply updates `last`) | |
| C6 | **Connected gate / no stale flush** | Disconnect Windows (kill client). On Kali copy `WHILE_DISCONNECTED`. Reconnect Windows. | While disconnected nothing is queued (broadcast gated on `connected`); on reconnect the **current** clipboard syncs **once** naturally; no flood, no stale duplicate | |
| C7 | **Size cap (>1 MiB)** | On Kali: `python3 -c "print('X'*(1024*1024+1), end='')" \| xclip -selection clipboard`; wait | DEBUG log `clipboard text <N> bytes exceeds cap; not syncing` (cap = `MAX_TEXT_BYTES` = 1024*1024); nothing sent; Windows clipboard unchanged. (Note: this is a `tracing::debug!` line — raise `RUST_LOG` to `debug` to see it.) | |
| C8 | **Empty ignored** | On Kali: `xclip -selection clipboard -i /dev/null`; wait | No message sent (`text.is_empty()` guard); Windows clipboard unchanged | |
| C9 | **Teardown** | Ctrl-C the server | Log `Ctrl-C received; stopping`; clipboard thread joins within 800 ms (`join_bounded(..., 800ms)`; else WARN `clipboard thread did not stop within ...; detaching`); process exits cleanly | |

> If a clipboard backend is unavailable you'll see WARN
> `clipboard unavailable; sync disabled: <e>` — clipboard tests N/A on that box.

---

## 6. The three Python bugs — regression checks

| Bug | 2-machine action | Expected (the fix) |
|-----|------------------|--------------------|
| **#1 Stale "connected" after peer drop** | Two cases: (a) **hard-kill** the server on Kali: `killall -9 mouse-rs` (clean socket close); (b) **silent drop**: pull the ICS cable / drop Wi-Fi (no FIN) | The client never stays stuck "connected". (a) On a clean kill the client sees EOF/RST quickly and logs `disconnected from server: PeerClosed` (or `Io(...)`). (b) On a silent drop the heartbeat liveness window fires and it logs `disconnected from server: TimedOut` within the **6 s** `liveness_timeout`. Either way the client exits cleanly. (Conversely, kill the **client** → server logs `peer disconnected: <reason>` and **keeps listening** for the next client — the engine resets to Local instead of dying.) The literal `TimedOut` string only appears for the silent-drop case. |
| **#2 Stop never hangs** | While connected and actively driving, press **Ctrl-C** on the Kali server terminal; time it | Server logs `Ctrl-C received; stopping` and exits in **well under 1–2 s** (cancel is selected on, so it never waits for the 2 s heartbeat tick). X grab released, cursor movable again. Same holds for the Windows client (see W12). |
| **#3 Fn+Up no freeze** | On Windows, while crossed/suppressing, mash `Fn`+`Up`/`Down` repeatedly | No multi-second freeze; input stays live. If anything ever does wedge, **Scroll Lock** (W2, in-callback self-release) or the watchdog (W3/W4) releases suppression. |

Optional in-repo confirmation of the timing fixes (run on Kali):
```bash
cargo test -p mouse-core net::tests::dead_peer_is_detected_via_liveness_timeout -- --nocapture
cargo test -p mouse-core net::tests::cancel_stops_promptly_without_hanging -- --nocapture
```
Both should pass quickly (dead-peer detected in ~0.5 s with fast test params;
cancel asserted `< 900 ms`).

---

## 6b. New since the last hardware run (Phase 6/7) — must-retest

These four are code-complete and pass the in-repo tests, but the first three
exercise **Windows-only** or **desktop-only** paths that cannot be verified off the
laptop. Run each and record pass/fail. **Your real layout is Windows=server /
Kali=client**, so set these in the matching config.

| ID | Test | Action | Expected (the fix) |
|----|------|--------|--------------------|
| **F1** | **Frozen-cursor / full reach (Windows=server capture)** | Make Windows the server. Cross to the Kali client and sweep the mouse across the **entire** client screen — corner to corner, slow and fast. | Client cursor reaches **every** part of the screen 1:1 with hand motion. **No** "stuck in a small central box", **no** acceleration/runaway, **no** drift while idle. (Fix: `windows.rs` forwards `pt − frozen`, the swallowed-centre delta, not the X11 previous-event delta.) If the cursor *accelerates* the further you move, the freeze-by-swallow assumption failed — report it; that's the one risk I couldn't test. |
| **F2** | **Close = quit (Kali/Xfce GUI)** | Launch the GUI (`mouse-rs` with no subcommand) on Kali. Click the window **✕**. Then separately try the tray **Quit**. | App **exits** both ways (process gone, `pgrep mouse-rs` empty). It does **not** vanish into a tray you can't get back (the stranded-window bug). Tray **Open** raises/focuses the window while it's still running. |
| **F3** | **Combo return hotkey** | Set `return_hotkey` (e.g. `ctrl+win+alt+esc`) **in the server config** (Windows, in your layout). Cross to the client, then press the full combo. | Control snaps back to the server (`returned to local` in the server log); the cursor is local again. Single-key **Scroll Lock** (`lock_hotkey`) still also returns. Partial combos (missing a modifier) do nothing. |
| **F4** | **Auto-reconnect** | With `reconnect:true` on the client (default; checkbox in GUI Client section): connect, then on the server `killall -9 mouse-rs` (or stop+restart it). Leave the client running. Restart the server ~5–15 s later. | Client does **not** exit on the drop — it logs `disconnected…` once, then retries with backoff (1 s→15 s) and **reconnects on its own** when the server returns (`connected to server …` again), no manual client restart. Also: start the client **before** the server — it should keep polling and connect once the server appears. Wrong `shared_secret` still **stops** (no retry storm). |

In-repo confirmation of F4 (run on Kali):
```bash
cargo test -p mouse-core net::tests::client_reconnects_after_server_restart -- --nocapture
cargo test -p mouse-core net::tests::client_keeps_retrying_until_server_appears -- --nocapture
cargo test -p mouse-core net::tests::client_stops_when_reconnect_disabled -- --nocapture
```

---

## 7. If something goes wrong

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| Client never prints "connected", hangs then exits | wrong `server_host`/`server_port`, firewall, or no route | Use Kali ICS IP `192.168.137.2` (or `wlan0` `10.36.179.242`); if driving from laptop add inbound TCP allow |
| Client exits right after handshake, no "connected" line | `shared_secret` differs (auth rejected; the reason is not logged at info) | Make `shared_secret` byte-identical both sides |
| Connects then drops (`disconnected from server: PeerClosed` / `TimedOut` / `Io`) | peer killed (clean=PeerClosed/Io), or flaky link / cable (silent=TimedOut) | Expected if the peer died (bug-fix #1); else check Wi-Fi/cable, MTU |
| Cursor won't cross | crossing not armed, or wrong edges | Move **inward first** to arm, then to the edge; confirm `exit_edge`/`entry_edge` are opposing and match physical layout |
| WARN `input grab refused (another grab active?); staying local` | a menu/drag/locker holds an X grab on Kali | Close the menu/drag, retry the cross |
| Cursor jams in a corner / lands off by ~1.5× (Windows) | DPI awareness not applied | Confirm 150% test (W7); ensure you're running the built `.exe` (`PER_MONITOR_AWARE_V2` set at startup) |
| Double / echoed motion, cursor jitter | warp-echo absorption regressed | Check W5; with `INJECT_MAGIC` working there's no echo. Spurious ~half-screen jumps on fast flicks = echo not absorbed |
| Keys stuck after exit/disconnect | release-all failsafe not reached | Should auto-release on exit/disconnect (`ReleaseAll`, threads joined within the 500 ms bound). If stuck, re-run and Ctrl-C cleanly |
| Clipboard not syncing | `clipboard_sync:false`, backend missing, empty/oversized text | Set `clipboard_sync:true` both sides; check for `clipboard unavailable...` WARN; text only, ≤1 MiB, non-empty |
| Kali: no grab, WARN `could not detect screen size` | not an X11 session (e.g. Wayland / no `DISPLAY`) | Log into **X11**; `mouse-rs` needs X11 on Linux |

---

## 8. Reporting back

For the next session, capture and report:

1. **Both terminal logs** (with `RUST_LOG=info`; use `debug` if you want C7's
   over-cap line):
   - Kali server: `/tmp/server.log` (from the `tee` in Step 2).
   - Windows client: copy the console output to a file, e.g.
     `mouse-rs.exe client > client.log 2>&1` (or paste the console buffer).
2. **The connection lines** proving the link came up:
   `peer '<name>' connected: <W>x<H>` (server) and
   `connected to server '<name>': <W>x<H>` (client). These are the only
   "connected" lines actually emitted.
3. **A pass/fail line per test ID**: smoke (Step 2), W1–W12, C1–C9, and the
   three regression checks (#1/#2/#3). For each FAIL, include the exact log
   lines around the failure and what you observed on screen.
4. **Environment notes**: Kali resolution, Windows resolution + scaling %,
   keyboard layout (for W9/W10), which link was used (ICS `192.168.137.2` vs
   `wlan0`), and `tls_enabled` true/false.