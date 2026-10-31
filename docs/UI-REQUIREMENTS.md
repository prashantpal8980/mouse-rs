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
| R31 | **Restyle the main window to match the Settings pages**: the machine chips at the top, the Connection/Clipboard tabs, and the tab contents. No large empty space under the content | **built 2026-10-14, awaiting the owner's review** | Owner, 2026-10-14, with a screenshot: "the settings part … looking visually very good … do a remake for this part as well … this unnecessary blank space". Stays within R28: the main window still has only Connection and Clipboard |
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

### R31 — main window restyled (built 2026-10-14)

- **Top card:** each computer is a tile with an icon, its name and "1440×900 · this computer".
  The one with the mouse is lit and says "● has the mouse". Both tiles keep that line (blank on
  the other one) so their height doesn't change as control moves.
- **Tabs:** full width, two equal halves.
- **Connection:** cards for "This computer is the…" and "Other computer" (on the server, "How the
  other computer finds this one"), then **At a glance**: encryption, what the clipboard shares,
  and the take-control-back key, all read-only, so the main window stays free of settings (R28).
  Then a "More connection settings ›" row into Settings → Connection.
- **Clipboard:** the history in a card with a heading and Clear, then a "Clipboard settings ›" row.
- The settings list and these rows share one helper (`nav_row`), so the two parts of the app look
  the same.

### R32 — lock-screen unlock password (agreed in principle 2026-10-15; **boot login screen built 27 Oct as part of R35**, idle screen lock moved to v1.1 on 29 Oct)

*Status 29 Oct:* the password of R35/R38, with "Ask at the login screen" on by default, is what guards the boot login
screen; the login-screen copy checks it itself. The idle **screen lock** while someone is logged in is covered only
when "Ask when someone is logged in here" is on; a separate lock-screen check is v1.1 (the launch plan's cut list).

- The owner's rule: before the other computer's keyboard can type into a login or lock screen, the
  person must give a **mouse-rs unlock password**. It is separate from the OS login password, and
  its purpose is to stop anyone at the paired machine from brute-forcing the login password.
- The protected machine checks the password itself; it never trusts the other side's word. Wrong
  tries are limited and slowed.
- The proposed design (OPAQUE + Argon2id, device-key pairing, brute-force limits) and 8 open
  decisions are in `docs/lock-screen-security-plan.html`. Nothing there is agreed beyond the rule
  above until the owner answers those decisions.

### R33 — quitting means quitting (agreed 2026-10-15)

- The owner's rule: closing the window (×), **⋯ → Quit**, tray Quit and **Stop sharing** stop
  mouse-rs at once. Nothing keeps running in the background after that: no helper process, no
  daemon, nothing holding the clipboard. "If I close any application, it means that I don't want
  that application to work right now."
- Once mouse-rs has quit, what it copied stops being pasteable, and a transfer in flight stops.
  That is expected, not a bug.
- **The one exception is a warning, never a refusal.** If a file is still being copied in either
  direction, quitting or stopping first shows "Quit while a transfer is running?" with
  **Keep going** / **Quit anyway** (or **Stop anyway**). Quit anyway stops at once, wherever the
  transfer is. Keep going changes nothing. Either choice is the user's.
- 2026-10-15, extended at the owner's request: **Restart now** (applying saved settings) and
  Windows **Restart as administrator** get the same warning, because both stop the engine.
  **Save** only writes the settings file and doesn't touch a running transfer (the change waits
  for a restart), so it doesn't warn.
- The login-screen service is not part of this rule. It is a separate system service the user
  turns on explicitly (R21).

### R34 — main-window fixes from the owner's review (2026-10-15)

- **Long names wrap.** A file name in the clipboard history breaks anywhere when it has to, so it
  never runs out over the 📍 and ✕ buttons.
- **One Clear.** The history card had two "Clear" buttons: one cleared the history, the other
  deleted received-file copies. Now there is one. It clears the history and deletes mouse-rs's
  own copies of received files. Pinned entries and running transfers are kept, and pasted files
  are never touched. The disk line stays as information: "received files kept by mouse-rs: …".
- **Detect explains itself.** A hover tip on the button, and a line under the field: what the
  address is for, and that Detect searches the network for the server.
- **At a glance is clearly read-only.** A subtitle says so, and each line opens the Settings page
  where it is changed (Encryption → Security & pairing, Clipboard → Clipboard, Take control
  back → Keyboard & hotkeys).

### R35 — connection PIN (agreed 2026-10-26)
A short PIN (4 digits, like a UPI PIN) must be entered before two devices connect. It is set on the server, typed
on the client, and checked on the server with try limits. **On by default.** The user can change it or turn it off
in Settings → Security; turning it off needs the current PIN. The owner's words: security people want it on,
but "if the person don't want their password because that is annoying for them they can turn it off".
R32 (lock-screen password) gets the same rule: on by default, and the user can change it or turn it off.
Timing and try limits are listed in `docs/LAUNCH-PLAN.md`.

### R36 — update notice (agreed 2026-10-26, built 2026-10-29)
When a newer version is published, the app says so ("Version x.y is available") and offers **Update**.
Users won't go looking on a website, so without this nobody updates. The check runs at start and once a day,
only while mouse-rs is running (R33).
- **As built:** each release has `latest.json` (version, notes, and per system the file's URL, size, SHA-256) and
  `latest.json.sig`, an Ed25519 signature made by `scripts/sign-release.sh` with the key in
  `~/.config/mouse-rs-release/` (never in the repo; losing it means no installed copy accepts an update again).
  The app trusts nothing unsigned, and a download only if size and hash match.
- Banner: **Version x.y is available** — Update / Later (Later hides it until the next start). Update downloads next
  to the program, checks it, swaps it in, then shows **Version x.y is installed — Restart mouse-rs** (owner, 29 Oct: name the app, so nobody thinks the computer
  restarts). It restarts only the app and goes through
  the "transfer running" warning (R33) and the new copy waits for the old one to exit.
- If the folder cannot be written (installed somewhere only an administrator can change): "The update did not
  install" with **Open download page**. A failed *check* shows nothing; About shows why.
- About: version, Updates status + **Check now**, "Check for updates automatically" (on by default).
- On Linux the login-screen copy is not updated by this; the existing "login screen runs an older copy" notice
  asks to reinstall it.
- Addresses: `https://github.com/prashantpal8980/mouse-rs-downloads/releases/latest/download/latest.json`.
  Test builds set `MOUSE_RS_UPDATE_URL` / `MOUSE_RS_VERSION` at build time (`scripts/fake-release.sh`).

### R37 — no manual network setup (agreed 2026-10-26)
A user who can't set a static IP must still be able to connect. Two devices on the same Wi-Fi, hotspot or
Ethernet find each other on their own, and switching between Wi-Fi and Ethernet needs no settings change.
Typing an IP address stays available as an Advanced option.

### R35 / R38 — the password and pairing as agreed on 27 Oct (explained in `docs/security-explained.html`)
- **Direction:** the password protects the **client**, the machine being driven. It is set and checked on the client
  and typed on the server, which has the keyboard. "The danger is on the client side… anyone can be server and able to
  connect with the client and able to drive the machine."
- **Any characters**, at least 4. One password, two switches on the client:
  **Ask at the login screen** (on by default; turning on login-screen sharing asks for a password to be set first) and
  **Ask when someone is logged in here** (off by default).
- **Reminder:** while the logged-in switch is off, the client's main window shows a reminder at most once a day:
  Set a password / Not now / Don't show this again.
- **Typed once per run on the server:** the box appears on the server's main window when the client refuses; what was
  typed is kept for that client until mouse-rs restarts on the server. The client retries every 2 s while waiting.
- **Wrong tries:** 3, then the client accepts none for 1 hour; 3 more, 2 hours; then 4, 8… A correct one resets it.
  (Owner's words: "locked for 1 hour… 3 more tries… increased to 2 hours". Doubling after that is an assumption.)
- **Change / turn off / forgot:** on the client, with no old password asked: being at the computer is the proof.
  A change refreshes the login screen's copy through the desktop's own permission prompt (pkexec), not a command.
- **Pairing on the main window** only until the computer is paired (or while a code shows); always in Settings.
- **R38 — pairing with a code.** Server: Settings → Security & pairing (also on the main window) → **Pair a new device**
  shows a 6-digit code for 5 minutes. Client: type it under "Pair with a code" (main window or Security page) → **Pair**.
  With no server address set, the client tries every machine on its networks that answers on the server port. The code
  works once and dies after 3 wrong tries. A fresh server's placeholder secret is replaced by a random one first.
  The typed shared secret moved to Security → Advanced.

### R39 — preferred network (agreed 2026-10-27)
When the server can be reached over both the cable and Wi-Fi, the client uses the one the user prefers. Settings →
Connection on the client: **Preferred network** = Cable (Ethernet) first (default) / Wi-Fi first / Whichever answers
first. Owner: "cable should be priority for me… choose from the drop down". If the preferred network stops working the
other takes over, and the network watch switches back when it returns.


### R40 — full app redesign from the owner's Claude Design file (agreed 2026-10-28)
The owner designed every screen in Claude Design ("mouse-rs App Screens", 20 screens: M1–M6 main window, S0–S10
settings, B1 banners, D1–D2 dialogs) and asked for it to be implemented as shown. Owner: "I liked the very idea how it
is looking right now… there are six colour implementations… implement that UI".
- **Six new themes** replace the old six: Graphite (dark, cyan), **Amber (dark, default)**, Plum & Cream, Forest &
  Mint, Slate & Sand (these three are *mixed*: a dark header and status card over a light body), Porcelain (light).
  The theme is now saved in the config. Appearance shows them as preview cards, plus **Match system light / dark**
  (dark → Graphite, light → Porcelain).
- **Changes save automatically.** No Save button; the footer says "Changes save automatically" and holds one action,
  Start/Stop sharing. Settings the running session read at start still raise the **Restart needed** banner.
- **Main window:** status card with a title sentence ("Connected to laptop"), both computers as tiles (the one with the
  mouse outlined, "● Has the mouse") and a ⇄ swap-roles button. Underlined tabs Connection · Clipboard (with count).
  Connection is a grouped list: This computer (Shares mouse | Is controlled), Connection details (expands in place:
  addresses, pairing, server address), Connection PIN (client), Take control back, then the paired computers. (No Recent clipboard here since R47.)
- **Before pairing** the Connection tab is a 3-step setup: what does this computer do → find the other computer → pair
  with its code. After pairing it folds into the list above.
- **Server PIN prompt** sits in the main window ("laptop wants its PIN"), not a pop-up.
- **Banners above the tabs:** at most one shown in full, the rest one line each. Order: restart needed, file waiting,
  permissions, PIN reminder, a computer asking to control this one.
- **Settings index** groups: Everyday (Clipboard, Screens & edges, Keyboard & hotkeys), This computer (Startup,
  Connection, Security & pairing), App (Appearance, Diagnostics, About). Each row shows its current value.
- **Screens & edges:** a picture of the two screens and one choice "laptop sits Left/Right/Above/Below"; the matching
  entry edge is set for you, overriding it is under Advanced ("Arrives on laptop at: Auto…").
- **Hotkeys are recorded**: press Change, then press the key (Esc cancels).
- **Profiles** are a list with the active one marked and Switch/× on the others; also listed in the ⋯ menu.
- **Diagnostics:** Copy path and Open folder. **About:** Restore defaults asks for confirmation first.
- **Dialogs:** quit/stop during a transfer (D1) and the one-time login-screen explanation (D2) are centred dialogs
  over a dimmed window.

### R41 — turning on pre-login / elevated access asks properly (agreed 2026-10-28)
Owner: when someone enables something that runs before the password, "it should ask… you allow the elevated
permissions… then set the password and only then" can the login screen be typed into.
- **Linux, Share input at the login screen:** switching it on *always* opens the explanation (not only the first
  time). If no PIN is set, the PIN is set in that same dialog ("Save PIN and turn on"); "Ask at the login screen" is
  forced on. Then the system's own password prompt (pkexec) installs the service with that PIN in its copy of the
  settings. The PIN comes before the system prompt so the installed copy is never PIN-less.
- **Windows, Start at login as administrator:** from a normal window it now shows Windows' own Yes/No prompt; on Yes a
  short elevated `mouse-rs autostart-elevated-on|off` makes the change. It no longer refuses with "restart as
  administrator first".
- **Parked (owner, 28 Oct): typing into the Windows sign-in/lock screen is not in v1.0.** Windows shows that screen on
  a separate secure desktop that an ordinary or administrator program cannot reach; only a system service could. That
  is a different, much larger piece of work with its own security review, and there is no time for it before the
  1 Nov launch. Linux has it because its login screen can be given input through the kernel's own input device by a
  system service we already ship (B2, `docs/LOGIN-SCREEN.md`). Revisit after launch.

### R42 — lane port: example, checks, firewall (agreed 2026-10-28)
Owner: the lane-port box should show an example instead of being blank, must not accept ports like 20, 22, 80, 443
or one already in use, and a port typed there should get the permission it needs.
- Placeholder `e.g. 24802`; the line under it says blank shares the main port and that both computers need the same port.
- Refused, with the reason in red and the old value kept: below 1024 (system services), and on the server a port
  another program already holds (tried by binding it). The main port and this session's own lane port are fine.
- Windows server with its own lane port and no firewall rule for mouse-rs: an "Allow…" button adds one inbound rule
  for the mouse-rs program (all ports, all network profiles) through Windows' Yes/No prompt
  (`mouse-rs firewall-allow`, elevated). Linux desktops have no firewall on by default, so nothing to add there.

### R43 — screens & edges decided on the computer with the mouse (agreed 2026-10-28)
Owner: on Kali (client) the side was changed but the crossing did not change; the picture did not say where the pointer
leaves and appears; no restart prompt was visible; only the server should decide this.
- The server decides: it leaves through `exit_edge` and places the pointer on the client's `entry_edge`. The client now
  **learns its way back from where the pointer arrives** (engine, every arrival) and the panel saves it, without a
  restart. The client's page shows the arrangement read-only: "Set on laptop… change it there".
- Server picture: both screens have four clickable edge strips — this one's choose where the pointer leaves, the other's where it appears (no longer hidden in Advanced). The lit one is "leaves: right", the other screen sits on
  that side with its "appears: left" strip lit, an arrow between. Advanced keeps "Appears on … at: Auto…".
- "N changes wait for a restart" + a Restart button now sit in the footer on every page, Settings included.

### R44 — one key per pairing, and both sides know exactly whom they paired with (agreed 2026-10-30)
Found by the owner (B50): the Windows VM paired with the laptop and then connected to Kali, because every computer
handed out one shared secret. Owner: "it should properly authenticate which key it is assigned to… which is the device
which is offering this pairing", and asked what happens when roles are swapped.
- **Every install has a device ID**: random, made once, never changes, never typed. The login-screen copy carries the
  same ID, because it is the same computer.
- **Pairing makes a new key for those two computers only.** Both sides derive it from the code exchange (SPAKE2), so
  the key never crosses the network, not even encrypted. Each side's device ID and name are sealed into the exchange:
  a correct code proves the other end is the computer that is showing that code, and tells each side which computer
  that is. A third machine cannot put its own ID in, and pairing with one computer gives nothing that opens another.
- **Each computer keeps a list of paired computers** (name, ID, key, date). Settings → Security & pairing shows the list
  with **Remove**. Pairing the same computer again replaces its entry.
- **Connecting:** the computer being controlled says which device it is; the computer with the mouse looks up the
  key for that device and proves itself first with it. The controlled computer accepts only a computer on its own
  list. One that is not on the list is refused before any key is used ("not paired with this computer"), and the
  refusal is shown on both sides by name.
- **Swapping roles uses the same key, with no re-pairing.** A pairing belongs to the two computers, not to which one
  has the mouse. Each proof is labelled with its side, both device IDs and this session, so a proof made in one
  direction can never be replayed in the other; a second key per direction would add nothing.
- **Discovery** beacons are signed with each pairing's key; a computer paired with nobody sends none (pairing finds
  computers by scanning, as before).
- **The connection PIN (R35)** is bound to the pairing key as well, so a machine that is not paired cannot collect a
  PIN proof to guess at offline.
- **The old single shared secret no longer lets anyone in.** Computers set up before this pair once more after the
  update; the typed "shared secret" box is removed. Protocol version 3: an older peer is told to update.

### R36 addendum — install from About too (agreed 2026-10-30)
Owner: after **About → Check now** finds a version, the next step must be right there: "if I am checking it, I want to
install directly from there". The button beside the update status follows the state: **Check now** → **Update now**
→ download percentage → **Restart mouse-rs**; after a failed install, **Try again**. The main-window banner stays.

### R38 addendum — the code boxes behave like boxes (agreed 2026-10-30)
Owner: no blinking cursor showed which box was being typed in, and one wrong digit could not be fixed on its own. Each
of the six boxes is now a real input: the selected one shows the cursor and a highlighted border, typing moves to
the next box, any box can be clicked to change just that digit, deleting a digit steps back one box, and a whole code
pasted into any box fills the boxes from there.

### R45 — reconnect to the computer you were last with; switch with one click (agreed 2026-10-30)
Owner: like Wi-Fi or Bluetooth, "it prioritized the device last it is connected to… I just want to conveniently change
it to VM, I just click on it or connect". And: "automatic" is about **finding** a computer, not choosing one — when both
computers move from the router to a phone hotspot, mouse-rs finds the same computer at its new address by itself. "It
doesn't automatically decide who to connect."
- **Each computer remembers the paired computer it was last with**, and reconnects only to that one: after a restart, a
  network drop or a network change. It never picks some other paired computer because that one answered first.
- **The main window lists the paired computers.** The one in use is marked; each other one has **Connect**. Clicking it
  switches: the current link closes and only the clicked computer is used from then on (and remembered as the last one).
- **On the computer being controlled**, Connect chooses which computer may drive it. **On the computer with the mouse**,
  Connect chooses which computer to drive: it closes the current link and waits for the clicked one only, even if
  another paired computer keeps trying.
- A computer with the mouse that is not waiting for anyone in particular accepts the first paired computer that asks
  for it, and that one becomes its last; while it is busy with one, another is told "busy with <name>" by name.
- A newly paired computer becomes the one in use.
- **Each row says where it stands** (owner, 30 Oct, on the first version): Connect on a computer not in use; once
  clicked, the row shows "connecting… / not connected yet" in amber with **Disconnect**; connected, a green dot and the
  row tinted green, still with Disconnect. Disconnect closes the link (stops sharing). **Stop sharing cancels any wait**:
  with sharing off nothing shows as waiting and every row offers Connect.
- **Finding stays automatic**: the same computer is found again on whatever network both are on (beacons, R37/R39).

### R46 — connected to several computers at once, switch with a shortcut (agreed 2026-10-31)
Owner: "the person who is using two desktop usually comes with multiple device like it could have three, four or any
number of devices. The connected at once is I think is much more better… I need it finished today". Replaces the
one-at-a-time rule of R45 on the computer with the mouse; the controlled side is unchanged (it is with one computer).
- **The computer with the mouse stays connected to every paired computer that is on**, all at once. Mouse and keyboard
  go to one of them at a time; switching is instant because nothing reconnects.
- **Shortcuts: Ctrl+Alt+Shift+1, 2, 3…** One per position in a list. The list is set on the computer with the mouse and
  holds **this computer too**, so one number brings control home. Pressing a number jumps straight into that computer
  (the pointer appears in the middle of its screen); pressing this computer's number brings it back here.
- **Order:** on the main window the paired computers can be moved up and down; the position is the number. In Settings
  each entry's shortcut can be changed to any combination (e.g. Ctrl+Shift+Alt+3 for this computer).
- **The edge** still leads to one computer: the one most recently switched to (or clicked). A screen layout with a
  computer on each edge is v1.1.
- **Clipboard:** superseded by R47 (one clipboard for all connected computers).
- The main window shows each paired computer as connected (green) or not, independently; Connect on the computer with
  the mouse makes that one the one being used.

### R47 — one clipboard for all connected computers; no clipboard list on Connection; quick update checks on test builds (agreed 2026-10-31)
Owner: "all three device gonna share the clipboard… if I copy a thing it is gonna available for device one… two three
four the number doesn't matter"; "I don't want to show any recent clipboard… on the connection page"; "make sure the
update check time gets decreased… on our testing device… the checking will be only when the device starts".
- **Text and images up to 256 KB** copied on any connected computer reach every other connected computer at once,
  whichever one is in use. The computer with the mouse relays between the controlled ones.
- **Larger images and files** are fetched over the file lane, which serves one computer at a time. A copy made on the
  computer with the mouse goes to the computer in use, and to each other computer that was connected at the time
  when it comes into use. A computer that connects later is not handed an old copy.
  *Not yet:* files or a large image copied on one controlled computer do not reach another controlled computer.
- **Connection page:** no Recent clipboard list. The Clipboard tab has the history.
- **Update checks:** a release build checks once, when mouse-rs starts. A test build (one pointed at a local release
  server) also checks every minute.
