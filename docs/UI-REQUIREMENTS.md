# Control-panel requirements

Behaviour the owner has asked for, tracked here so it survives between sessions.

This file exists because a requirement was lost: "restore to default settings" was
discussed and then appeared in no document, so it was never built and never reviewed.
Requirements agreed in conversation go here **when they are agreed**, not when they are
implemented — an unwritten requirement is indistinguishable from one that was never raised.

Status values: **done** · **open** · **deferred** (agreed, deliberately not now).

This file tracks what was **agreed**. What is **broken or unproven** lives in
[KNOWN-ISSUES.md](KNOWN-ISSUES.md) — check both before assuming a feature works.

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
| R10 | **Clipboard** keeps its own tab | **done** (owner's decision, over my suggestion to fold it); clipboard *settings* superseded by R23 on 2026-10-13 |
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
| R16 | The Layout tab must show only what **this machine's role** actually uses | **done** (2026‑09‑02) | Agreed 2026‑07‑31. Verified again before building rather than taken on trust: `client.rs:37` reads `entry_edge` and the client engine never mentions `exit_edge`, while `server.rs:50` reads both and checks they oppose. So on a client the Exit-edge picker wrote a value nothing would read — the original dead knob — and the overlap ⚠ compared that dead value against the live one, so it could contradict a correctly-configured pair. **Built:** the picker and the warning appear only on the server; the client shows Entry edge alone, labelled for what it does there ("where the pointer arrives, and where to push back") with a line saying why there is no exit edge, rather than silently omitting it. Three tests pin the decisions. Full reasoning in `docs/mockups/edges-explained.html`. |
| R17 | Clipboard history is a **clipboard manager**, not a log — entries must be re-usable | **done** (2026‑08‑04) | The owner confirms this was always the intent; building it as a view-only log was a misread of the requirement, not a scope change. Entries used to be non-interactive `container`s with no `on_press`, and an image was stored as the label string `[image 800×600, 240 KB]` with the pixels discarded. Now: every row is a button that puts the item back on the clipboard and syncs it to the peer like a fresh copy; images keep their encoded PNG; repeats move to the front instead of duplicating; pin protects an entry from eviction and from Clear. |
| R18 | Copy **any file** between machines through the clipboard, extension-agnostic | **in progress** | Asked for 2026‑08‑04. HTML, exe, ISO — no allow-list. Under the user's auto threshold it transfers silently; above it, a non-blocking prompt asks first. Must never degrade mouse/keyboard latency. Exceeds Windows' own clipboard history, which retains no files at all. Full design in `docs/mockups/clipboard-manager-plan.html`. **Built 2026‑08‑04, whole path:** bulk lane (second connection, lane negotiated in the handshake so one port still suffices, binary framing, RTT-paced); `CF_HDROP` on Windows and an X11 selection owner on Linux; content-addressed cache with verification, dedup, resume and pinning-aware eviction; receiver-side auto/ask decision; entry states including moved-or-deleted and changed; non-focus-stealing consent banner; large images moved off the input lane too. **Not yet exercised on hardware.** Folders are copied whole and rebuilt with their structure intact. **Completed 2026‑08‑14:** cancel on a running transfer (both ends — the receiver's Stop sends `BlobCancel` so the *sender* stops too, rather than only hiding the progress); a real progress bar; **Not now** and **Always allow ‹size›** on the banner; a desktop notification and a tray tooltip when something is waiting; empty directories inside a copied folder now recreated; following symlinks is a setting, with loop detection when on. Also fixed: the sender read a whole file into an unbounded queue as fast as the disk allowed, so a 4 GB copy meant ~4 GB of RAM — it is now bounded by a configurable send buffer, which is what the plan's cost table always claimed. **Remaining gap:** Windows shows the tray tooltip but no toast (that needs an app user model ID registered at install time), and X11 file publishing still dies with the process. |
| R20 | The panel must say when Windows is blocking input, and offer elevated autostart | **done** (2026‑08‑15) | Reported as "I can't cross while Genshin Impact is in front". Cause: Windows UIPI — the OS does not call a lower-integrity process's `WH_MOUSE_LL`/`WH_KEYBOARD_LL` hooks while an elevated window has the foreground, so the pointer is never seen reaching the edge and no crossing is attempted. Not fixable in-process; running elevated is the only remedy. Built: a status line on the Startup tab stating which state this machine is in and what it costs, a **Start at login as administrator** toggle backed by a Task Scheduler `ONLOGON /RL HIGHEST` task (the only mechanism that starts elevated at login without a UAC prompt every time), and a log warning naming the offending window and both integrity levels. The toggle needs mouse-rs itself elevated to register the task, and says so when refused rather than reporting a bare error. |
| R19 | Every clipboard limit must be **user-configurable**, split Basic / Advanced | **partly done** (2026‑08‑04) | A slow setting the user chose deliberately is their trade to make; the software's job is to state the cost, not refuse the value. **Built:** history size (Basic); sync-text, sync-images, biggest-item MB, memory-budget MB and largest-image-edge px (Advanced), with live worst-case cost lines for the memory budget and the image limit. `MAX_TEXT_BYTES`, `MAX_IMAGE_BYTES` and `MAX_IMAGE_PIXELS` are gone as hardcoded constants. **Also built:** ask-before-sending threshold (Basic), sync-files, disk budget for transferred files, and the bulk lane's own enable/port (Advanced). **Completed 2026‑08‑04 after an audit found them missing:** keep-history-after-restart (Basic, with the privacy cost stated), skip-prompt-when-quick (measured, not guessed), pause-while-crossed, and a user-pinned transfer speed limit. Pause-while-crossed had been the worse case — it existed and was tested but nothing ever called it or set the flag it read, so it was dead code rather than a missing feature. **2026‑08‑14:** added a send-buffer size, follow-symlinks and notify toggles — and fixed a quieter version of the same defect: *every* clipboard and bulk setting was read once at Start and none of them were in the restart-required list, so editing one while sharing was live changed the panel and nothing else, with no prompt. They are all listed now, except the ask-first limit, which is pushed to the running thread instead. |
| R21 | Share the keyboard and mouse **at the Linux login screen**, so the password can be typed from the peer | **built, untested on hardware** (2026‑10‑08) | Asked for 2026‑10‑08: "I just want it to autostart before inputting the password". Autostart is an XDG desktop entry, so it fires when the XFCE session starts — after SDDM has taken the password — and Linux injection used XTEST, which is bound to one X server through `DISPLAY` and an auth cookie that does not exist yet at the greeter. So the only way to log in was to plug a second keyboard into the machine that already had one attached to its peer. **Built:** a `uinput` injection backend (`crates/mouse-input/src/uinput.rs`) that creates a virtual keyboard and an absolute pointer at kernel level, so events reach whatever is in front — greeter, session, text console or a Wayland compositor — with nothing session-specific to break; a `greeter` mode that runs as a client with no clipboard and reads a root-owned config from `/etc/mouse-rs/`, because a pre-login service must not depend on `/home` being mounted; `greeter-install` / `-uninstall` / `-selftest` subcommands; and a hardened system unit that hands over the moment logind reports an active graphical session, since two clients injecting one stream would double every keystroke. The pointer is absolute-only on purpose — a relative virtual device is subject to local pointer acceleration, so remote motion would mean different distances on the two machines. **Cost, stated deliberately:** it is a network-driven keystroke injector running before anyone has authenticated, so `shared_secret` becomes what stands between the LAN and this machine's login prompt. Off unless installed by hand. **Not offered:** greeter mode as a *server*, which would mean capturing this machine's keyboard pre-login — a different and much larger capability. 18 tests, including `systemd-analyze verify` on the generated unit and the real `CLASS=greeter` / `CLASS=manager` records this machine produces. **2026‑10‑08, same day:** asked for it as a toggle rather than a command — now on the Startup tab, gated the first time by a one-time panel stating the cost (the pattern agreed for R20: a choice plus a warning shown once, whichever way it is answered). The panel has no privilege, so it asks `pkexec` and the desktop's own agent prompts — the password never passes through mouse-rs. The toggle reads back from systemd rather than from a saved setting, so it reflects `systemctl` changes made outside the app and refuses to move if the prompt is dismissed; it reports *enabled*, not *running*, because the service is deliberately stopped throughout a session. Added `greeter-status` for an unprivileged check. Gating the new `Message` variants to Linux was required, not tidiness: declaring them unconditionally while handling them only on Linux broke the Windows build on a non-exhaustive match. |
| R22 | Keep working **whichever network** the two machines share — Ethernet, Wi-Fi, a shared router, or a phone hotspot — with no address typed in, and follow a change of network | **built, untested on two machines** (2026‑10‑12) | Asked 2026‑10‑12: a cable unplugged, a move from Wi-Fi to Ethernet for speed, and both machines disconnected all have to keep working. Built as: the server beacons a signed list of its addresses on each network (`beacon.rs`, UDP 24801); the client races every address it knows and keeps the first to answer; a change of network drops the session at once and reconnects; an authenticated new connection replaces a stale session. With no network the client says so rather than "server not found". |
| R23 | **All settings in one place: ⋯ → Settings**, out of the tabs. The main window keeps status, Start/Stop and the clipboard history | **built 2026-10-14, awaiting the owner's review** | Owner, 2026-10-13. Supersedes R10 for the clipboard *settings*; the clipboard *history* stays on the main window. Plan: [PLAN-SETTINGS-AND-LAUNCH.md](PLAN-SETTINGS-AND-LAUNCH.md) Part A |
| R24 | Settings **grouped by function with sub-sections**, like Android's Settings (Connection, Screens & edges, Keyboard, Clipboard, Startup, Security, Appearance, Diagnostics, About) | **built 2026-10-14, awaiting the owner's review** | Owner, 2026-10-13. Categories proposed in the plan, for the owner to confirm |
| R25 | Every setting marked **Basic or Advanced**; normal users see Basic only | **built 2026-10-14, awaiting the owner's review** | Owner, 2026-10-13. Extends R19 from the clipboard to every setting |
| R28 | **The main window has only two tabs: Connection and Clipboard (history).** Layout, the clipboard's settings, Startup, Security and everything else move to ⋯ → Settings | **built 2026-10-14, awaiting the owner's review** | Owner, 2026-10-14: "only the clipboard, the history part and the connection … The layout, the clipboard setting, the startup setting, auto launch all these things are supposed to be properly categorized in the setting panel." Refines R23 |
| R29 | Inside Settings, **the most-used categories come first and the advanced ones last**, and on each page the everyday settings sit at the top with an **Advanced** section at the bottom, like Android's Settings | **built 2026-10-14, awaiting the owner's review** | Owner, 2026-10-14. Refines R24/R25: order by how often people use a setting, not alphabetically or by subsystem |
| R30 | **Settings layout confirmed** (mockup `docs/mockups/settings-categories.html`): the main Connection tab keeps the **role** and **the other computer** (address, Detect, found list), and ports, auto-reconnect and machine name go to Settings → Connection. Category order: *Everyday* Clipboard, Screens & edges, Startup, Keyboard & hotkeys; *Setup* Connection, Security & pairing, Appearance; *Help* Diagnostics, About. Advanced is a closed section **at the bottom of each page** (not a global switch). The unused `warp_threshold_px` ("return distance") is **dropped** | **built 2026-10-14, awaiting the owner's review** | Owner, 2026-10-14, answering four questions on the mockup |
| R31 | **Restyle the main window to match the Settings pages**: the machine chips at the top, the Connection/Clipboard tabs, and the tab contents. No large empty space under the content | **open** | Owner, 2026-10-14, with a screenshot: "the settings part … looking visually very good … do a remake for this part as well … this unnecessary blank space". Stays within R28: the main window still has only Connection and Clipboard |
| R26 | **Restore settings that went missing**: edge sensitivity and the Python app's return distance, plus the 6 config fields that never had a control | **built 2026-10-14, awaiting the owner's review** | Owner noticed settings had gone, 2026-10-13. Audit in the plan: 8 of 51 fields have no control; `warp_threshold_px` is stored but unused |
| R27 | Large-paste settings, all user-configurable: **stream-to-paste size threshold** (default 64 MB), **wait for a dropped connection** (default 180 s), **keep a resumable copy** (default **off**; doubles disk during a paste but survives restarts) | **done** (2026‑10‑13) | Owner, 2026-10-13: "the cost in double memory becomes the feature… by default it would be turned off". In the Clipboard tab's advanced transfer settings; moves to Settings → Clipboard → Transfers with R23 |

---

## Closed by evidence

| Requirement | Outcome |
|---|---|
| Clipboard image sync | **done** — was a regression against the Python build, not a missing feature |
| Panel always-on-top on Windows | **not a bug** — `WS_EX_TOPMOST` is clear after a fresh relaunch; it was an old screenshot harness pinning it |
| Keyboard leaks on crossing | **fixed** (2026‑07‑31) — see `docs/KEYBOARD-LEAK.md` |

### R23–R30 — settings in categories (built 2026-10-14)

- **Main window:** two tabs. **Connection** has the role and the other computer (address, Detect,
  found list); on the server it lists this computer's addresses instead. **Clipboard** has the
  history only. The ⋯ menu has Settings, Profiles… (opens Settings → Connection) and Quit.
- **⋯ → Settings:** a list of categories in three groups. *Everyday*: Clipboard, Screens & edges,
  Startup, Keyboard & hotkeys. *Setup*: Connection, Security & pairing, Appearance. *Help &
  advanced*: Diagnostics, About. Each page shows its everyday settings first and ends with an
  Advanced section, closed by default.
- **Moved, not re-authored:** an audit of the view code compared every control label before and
  after. All 69 are present (the old "Advanced settings" switch became the per-page sections).
  Every message the panel handles is still sent by some control.
- **New controls (R26):** edge sensitivity (Screens → Advanced), discovery port (Connection →
  Advanced), cache folder and the fingerprint (hash) limit (Clipboard → Advanced), log upload
  address and interval (Diagnostics → Advanced). Edge sensitivity and discovery port raise the
  restart prompt like other engine settings. `warp_threshold_px` was removed from the config;
  old files that still carry it load fine.
- **Moved out of the ⋯ menu:** theme → Appearance, Restore defaults → About.
- **Screenshot aid:** `MOUSE_RS_OPEN_SETTINGS=<page>` opens the panel at a page with its
  Advanced section open ("index" for the list).
