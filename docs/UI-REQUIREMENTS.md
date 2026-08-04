# Control-panel requirements

Behaviour the owner has asked for, tracked here so it survives between sessions.

This file exists because a requirement was lost: "restore to default settings" was
discussed and then appeared in no document, so it was never built and never reviewed.
Requirements agreed in conversation go here **when they are agreed**, not when they are
implemented — an unwritten requirement is indistinguishable from one that was never raised.

Status values: **done** · **open** · **deferred** (agreed, deliberately not now).

---

## Settings feedback

| # | Requirement | Status |
|---|---|---|
| R1 | When a saved setting needs a restart to take effect, say so, and offer **[Restart sharing now]** / **[Later]** | **done** (2026‑07‑31) |
| R2 | Name *which* settings are stale, not just that something is | **done** |
| R3 | "Later" must leave a persistent marker, not silently hide the mismatch | **done** — collapses to a one-line marker |
| R4 | Show it once per change, not repeatedly | **done** — it is state-driven, so it clears itself when the engine restarts |
| R5 | **Restore default settings** | **done** (2026‑07‑31) — resets preferences only, never machine facts |
| R6 | Fewer controls in the footer; **Stop must not fall off the edge** at normal width | **done** (2026‑07‑31) |
| R7 | Compact the status hero — useful, but was taking a third of the window | **done** — ~196px → ~70px |
| R8 | Stop burying everyday settings below the fold | **done** — new **Startup** tab; Connection went from 9 fields to 4 |
| R9 | Explain what **Profiles** are in the UI | **done** — description in the Startup tab |
| R10 | **Clipboard** keeps its own tab | **done** (owner's decision, over my suggestion to fold it) |
| R11 | Real IP detection instead of a hardcoded example address | **done** — Detect sweeps local subnets; server side lists this machine's addresses |
| R15 | The hero must light the machine that currently has the mouse — **on both roles** | **done** (2026‑07‑31) |

### R15 — the lit chip

Reported as "the blue light doesn't show which device I'm on". It never could on a **client**:
`StatusSnapshot::engaged` is role-relative — on the server it means the cursor has left for
the peer, on the client it means the peer is driving *this* machine — and the hero used it
without folding in the role. On a client `!engaged || !server_side` and `engaged && server_side`
collapse to the constants `true` and `false`, so the local chip was lit permanently and the
peer chip could never light, whichever way the mouse went. Kali is the client here, which is
why it showed on the desktop panel.

Present since the first iced panel, not introduced by the redesign — but the redesign is when
it became visible, because R7 shrank the hero from ~196px to ~70px and dropped the
`server`/`client` labels, leaving the highlight as the only thing left saying where control
was. `control_is_local()` now holds the rule, with a test per role.

### R1–R4 — restart prompt

Settings split into two silently different groups, which was the whole problem: pressing
Save reported "saved" and changed nothing about the live session, with no way to tell.

**Apply immediately:** theme, log path display, start-at-login (writes to the OS on
toggle), autoconnect / start-minimized (they only govern launch).

**Apply only on restart:** machine name, role, shared secret, TLS, clipboard sync, lock
hotkey, return hotkey, listen host/port, server host/port, auto-reconnect, exit edge, entry
edge, screen size.

Implementation: `App::live_cfg` holds the config the running engine actually read;
`pending_restart_changes()` diffs it against the edited config and returns the human labels
of what differs. The banner only appears while sharing is running — nothing can be stale
against an engine that isn't there. "Restart sharing now" stops and restarts the engine,
which re-baselines `live_cfg`, so the prompt clears itself and the confirmation is truthful.

Note: machine name is treated as restart-requiring even though an earlier draft listed it
as immediate — it is sent in the handshake, so the peer keeps showing the old name until
the session is remade.

Covered by tests in `crates/mouse/src/gui.rs`: only-changed-settings are listed, immediate
settings raise no prompt, and screen width+height count as one change (so the "N changes"
count is not overstated).

### R5 — restore defaults

In the **⋯ menu** (not the footer — it is destructive and rare, and sitting next to Save is
what caused the misclick that reset the Kali config).

Three deliberate choices, all about not destroying something unrecoverable. It resets
*preferences*; it never touches facts about the machine:

- **Shared secret kept** — must match the other machine; clearing it breaks the pairing with
  no way to recover the value.
- **Machine name kept** — an identity the peer displays, not a setting.
- **Screen size re-detected, not defaulted** — the 1920×1080 default is wrong on any other
  display and silently corrupts edge geometry. This actually happened: it reset a 1440×900
  machine to 1920×1080 and cleared auto-connect.
- **Edits the form only.** The file is untouched until Save, so it can be reviewed and
  abandoned. One misclick cannot destroy a working setup.

Feedback line: `defaults restored — kept your name, secret and screen (1440×900); press Save to apply`.

### R6–R11 — layout

Prototyped in `docs/mockups/ui-prototype.html` (clickable) and reviewed before building.

- **Footer went from 8 controls to 2**: Save, and a single Start/Stop toggle. Start and Stop
  were mutually exclusive with one always greyed, so they became one button. Profiles,
  Restore defaults, Quit and the theme swatches moved to the **⋯** menu. Quit was a
  duplicate of the window ✕, which already quits.
- **Hero** is two machine chips plus the live control line, on every tab.
- **Startup tab** carries machine name, the three launch toggles, the log path and Profiles.
- Window is **560×720**, down from 600×840, because nothing needs to scroll.

**Lesson recorded during this work:** the first redesign pass silently dropped the lock
hotkey, the return hotkey, Show/Hide secret and the edge-overlap warning, because the tabs
were rebuilt from scratch instead of transcribed. A placement change must move controls, not
re-author them.

---

## Still open

| # | Requirement | Status | Note |
|---|---|---|---|
| R12 | Clipboard transfers should be visible in the log | **done** (2026‑08‑04) | Text, images, files, requests, refusals, verification failures and evictions all log now. Was the same instrumentation gap as the deleted `kbd_diag` counters — every failure mode added by file transfer would otherwise have been invisible. |
| R13 | Panel should have an accessibility tree | **open** | iced/wgpu exposes zero UI Automation descendants, so screen readers can't see it and UI testing can't drive it. Worth a decision before shipping a paid product. |
| R14 | Console flash when launching the GUI from Explorer | **deferred** | Cosmetic; `FreeConsole` runs after the console is allocated. |
| R16 | The Layout tab must show only what **this machine's role** actually uses | **open** | Agreed 2026‑07‑31, never built. `client.rs:36` reads only `entry_edge`; the client engine never looks at `exit_edge`. So on the client the Exit-edge picker is a dial wired to nothing — it writes a config value no code reads — and the overlap ⚠ compares that dead value, so it can fire on a correctly-configured pair. On the client: hide Exit edge, drop the warning, relabel Entry edge as "push the cursor back in here to go home". Keep both on the server. Full reasoning in `docs/mockups/edges-explained.html`. |
| R17 | Clipboard history is a **clipboard manager**, not a log — entries must be re-usable | **done** (2026‑08‑04) | The owner confirms this was always the intent; building it as a view-only log was a misread of the requirement, not a scope change. Entries used to be non-interactive `container`s with no `on_press`, and an image was stored as the label string `[image 800×600, 240 KB]` with the pixels discarded. Now: every row is a button that puts the item back on the clipboard and syncs it to the peer like a fresh copy; images keep their encoded PNG; repeats move to the front instead of duplicating; pin protects an entry from eviction and from Clear. |
| R18 | Copy **any file** between machines through the clipboard, extension-agnostic | **in progress** | Asked for 2026‑08‑04. HTML, exe, ISO — no allow-list. Under the user's auto threshold it transfers silently; above it, a non-blocking prompt asks first. Must never degrade mouse/keyboard latency. Exceeds Windows' own clipboard history, which retains no files at all. Full design in `docs/mockups/clipboard-manager-plan.html`. **Built 2026‑08‑04, whole path:** bulk lane (second connection, lane negotiated in the handshake so one port still suffices, binary framing, RTT-paced); `CF_HDROP` on Windows and an X11 selection owner on Linux; content-addressed cache with verification, dedup, resume and pinning-aware eviction; receiver-side auto/ask decision; entry states including moved-or-deleted and changed; non-focus-stealing consent banner; large images moved off the input lane too. **Not yet exercised on hardware.** Folders are copied whole and rebuilt with their structure intact. **Known gaps:** empty directories inside a copied folder are not recreated, and symlinks inside one are skipped rather than followed. |
| R19 | Every clipboard limit must be **user-configurable**, split Basic / Advanced | **partly done** (2026‑08‑04) | A slow setting the user chose deliberately is their trade to make; the software's job is to state the cost, not refuse the value. **Built:** history size (Basic); sync-text, sync-images, biggest-item MB, memory-budget MB and largest-image-edge px (Advanced), with live worst-case cost lines for the memory budget and the image limit. `MAX_TEXT_BYTES`, `MAX_IMAGE_BYTES` and `MAX_IMAGE_PIXELS` are gone as hardcoded constants. **Also built:** ask-before-sending threshold (Basic), sync-files, disk budget for transferred files, and the bulk lane's own enable/port (Advanced). **Completed 2026‑08‑04 after an audit found them missing:** keep-history-after-restart (Basic, with the privacy cost stated), skip-prompt-when-quick (measured, not guessed), pause-while-crossed, and a user-pinned transfer speed limit. Pause-while-crossed had been the worse case —  existed and was tested but nothing ever called it or set , so it was dead code rather than a missing feature. |

---

## Closed by evidence

| Requirement | Outcome |
|---|---|
| Clipboard image sync | **done** — was a regression against the Python build, not a missing feature |
| Panel always-on-top on Windows | **not a bug** — `WS_EX_TOPMOST` is clear after a fresh relaunch; it was an old screenshot harness pinning it |
| Keyboard leaks on crossing | **fixed** (2026‑07‑31) — see `docs/KEYBOARD-LEAK.md` |
