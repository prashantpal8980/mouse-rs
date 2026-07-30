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
| R12 | Clipboard transfers should be visible in the log | **open** | Text and image sync log nothing, so a clipboard failure is undiagnosable. Same instrumentation gap as the deleted `kbd_diag` counters. |
| R13 | Panel should have an accessibility tree | **open** | iced/wgpu exposes zero UI Automation descendants, so screen readers can't see it and UI testing can't drive it. Worth a decision before shipping a paid product. |
| R14 | Console flash when launching the GUI from Explorer | **deferred** | Cosmetic; `FreeConsole` runs after the console is allocated. |

---

## Closed by evidence

| Requirement | Outcome |
|---|---|
| Clipboard image sync | **done** — was a regression against the Python build, not a missing feature |
| Panel always-on-top on Windows | **not a bug** — `WS_EX_TOPMOST` is clear after a fresh relaunch; it was an old screenshot harness pinning it |
| Keyboard leaks on crossing | **fixed** (2026‑07‑31) — see `docs/KEYBOARD-LEAK.md` |
