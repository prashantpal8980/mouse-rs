# Known issues

Things that are wrong, missing, or unproven — kept here so they are not rediscovered as
surprises later. `docs/UI-REQUIREMENTS.md` tracks what was *agreed*; this tracks what is
*broken or unfinished*.

Status values: **open** (not started) · **unverified** (built, never proven on real
hardware) · **deferred** (known, deliberately not now) · **parked** (the owner has said to
stop work on it; say what would restart it) · **awaiting detail** (reported, not yet
described well enough to act on).

---

## Awaiting detail from the owner

### B1 — something about keyboard/mouse sharing on the Windows side
**closed 2026-10-09 as a duplicate of [B13](#b13--alttab-sticks-a-modifier-release-is-lost-and-the-key-stays-logically-down)** · raised 2026-10-09

Kept for the trail. The owner described the Alt+Tab fault the following day, and it is on
the Windows side and about input sharing, which is everything B1 said. Reopen if it turns
out they meant something else.

The owner reported a change or fault concerning input sharing on the Windows side and
said they would describe it later. Not enough detail to act on, and **deliberately not
guessed at** — the words could mean the login-screen feature wanting a Windows
equivalent, or a distinct bug seen while sharing.

> "When I share keyboard and mouse, I want to change that to the Windows side … this is a
> bug … will let you know."

**Next step:** ask what happens, on which machine, and what should happen instead.

**Possibly already answered by [B13](#b13--alttab-sticks-a-modifier-release-is-lost-and-the-key-stays-logically-down)**, reported the following day and definitely on the Windows side. Confirm with the owner before closing this.

---

## Built but never proven on hardware

### B2 — login-screen sharing — **closed 2026-10-16, signed off by the owner**

**16 Oct 2026, owner sign-off.** On the boot at 19:07 the login-screen service connected to the
laptop at 13:37:32 UTC, within a second of starting. The laptop took control at 13:37:45, the
owner typed the password from the laptop, the session opened at 13:38:00, and the service stood
down cleanly with no double input. It logs 9 key events by count only, never by name. The installed
copy dates from 15 Oct 13:05. Later builds changed only the panel and the clipboard, which this
input-only service doesn't use.


**13 Oct 2026, proven.** Boot at 12:34. The service connected at 12:34:47; the fifth crossing stuck at 12:34:57; 20 key events were injected; logind opened the session at 12:35:03; the service stood down at 12:35:05. This ran on the 9 Oct copy, so the 12 Oct fixes (toast, no key names logged, no clipboard to the login screen, shipped log, no restart churn) are still waiting for `greeter-install` and the laptop update. `greeter-install` now stages the binary and renames it over the old one, so it cannot fail with ETXTBSY while the old copy churns.
**on hold at the owner's request 2026-10-09** (they cannot reboot right now) · built 2026-10-08 (`61b0d72`, `7b143a6`) · tested by the owner 2026-10-09 · R21

**2026-10-12 boot — what the logs showed.** The service **did** run. It started at
08:33:15 with the login screen, created its uinput devices, and the laptop logged
`peer 'kali-desktop' connected` at 08:33:17. The laptop never crossed to it: no
`crossed to remote` in the ten seconds before the password was typed at 08:33:26 on a
local keyboard. Nothing told the owner Kali was ready, or that the pointer had to be moved
across. Fixed the same day:

* the login-screen instance says so in its handshake; the laptop logs it loudly, shows
  "(login screen)" next to the peer, raises a toast, and withholds the clipboard;
* key names are never logged at the login screen (the first 8 keys of a crossing were
  logged by name, which there means the password);
* no file lane at the login screen (it reconnected every second, flooding both logs);
* the service waits quietly through a session instead of restarting every 3 s, which had
  rotated the boot-time evidence out of the capped log within a day;
* its log ships as `kali-desktop-login-screen-mouse-rs.log`; before, it shipped nowhere;
* `greeter-status` and the desktop log now flag a **stale service copy**. The copy at
  `/usr/local/bin/mouse-rs` was from 9 Oct and only a root `greeter-install` refreshes it.

**Still unproven:** typing the password from the laptop at a real login screen.

The owner rebooted to try it, got the SDDM password prompt with no keyboard, and had to
plug one in again. Checked afterwards: **no unit, no `/etc/mouse-rs`, and not one line
mentioning the greeter in either log generation.** The feature had never been installed —
it did not fail, it was never turned on. Building it and saying "the toggle is in the
panel" was not enough to make that clear.

The underlying defect is that it was **only testable by logging out**, so the first
feedback anyone gets is standing at a password prompt unable to type.

**Fixed 2026-10-09:**

* `mouse-rs greeter-status` now *verifies the whole chain* instead of listing files —
  unit, enablement, binary, settings, `/dev/uinput`, whether anything loads the module,
  and whether the peer answers on its port — and ends with a plain verdict on whether the
  login screen will accept the other machine's keyboard. Checks cascade, so an
  uninstalled service gives one instruction rather than four.
* The unit now declares `Wants=`/`After=modprobe@uinput.service`, and the install writes
  `/etc/modules-load.d/mouse-rs-uinput.conf`. `uinput` is a module on this kernel
  (`CONFIG_INPUT_UINPUT=m`) and was **not loaded**; the static `/dev/uinput` node does
  normally autoload it on open, but relying on that from inside a sandboxed unit is how
  you get `ENODEV` at boot and no keyboard at the prompt.

**2026-10-09, second round — the toggle worked and the panel said it had not.** The owner
turned it on, saw no password prompt, and reported nothing had happened. It had: polkit
authorised without prompting (they are in `sudo`), and `greeter-install` ran as root and
succeeded. Reconstructed from systemd's journal, because nothing on our side logged it.

Two real bugs, both fixed:

* **`State::installed` tested `/etc/mouse-rs/config.json` with `Path::is_file`.** That
  directory is `0700 root` because it holds the shared secret, so the panel — which runs
  as the user — gets permission-denied, and `is_file` returns `false` for that exactly as
  it does for a missing file. A fully installed, enabled service therefore reported itself
  absent, which pinned `enabled` to false and left the toggle at **off** however many
  times it was clicked. It now judges from the unit file and the settings *directory*,
  both of which an unprivileged process can legitimately see. Two tests pin it, one
  asserting the panel never disagrees with real systemd about enablement.
* **A failed toggle was reported only into a transient panel banner and logged nowhere.**
  `set_enabled` now logs the exit code, stdout and stderr either way, and the panel logs
  the mismatch case. Diagnosing this took a journal dig that should have been a log line.

`greeter-status` also no longer reports FAIL for the root-only config when run
unprivileged — that was reporting our own lack of permission as the service's fault.

**Current state: installed and enabled, confirmed against systemd.** Untested at a real
login prompt, which needs a reboot.

### B3 — the stalled-transfer fixes were never confirmed on hardware — **confirmed 2026-10-15**
**closed** · built 2026-09-04 (`01555ac`)

Three receiver-side fixes — "already downloading" no longer read as failure, progress
surviving a paste retry, and the throttled over-budget log — were measured against the
shipped logs but the owner moved on before re-testing a large copy end to end. The
pacer fix before it (`38593c1`) is in the same position.

**Next step:** one 4 GB copy, watching whether the progress bar climbs monotonically
instead of resetting.

**Confirmed 2026-10-15:** a 4.65 GB, 6-file copy from Kali into Explorer. Each file was served once
at offset 0, with no restarts or retries, at about 76 MB/s. The owner reported steady progress.
Found B35 along the way.

---

## Open defects

### B13 — Alt+Tab sticks: a modifier release is lost and the key stays logically down
**fixed 2026-10-26, confirmed on hardware the same day** (owner reproduced it at 15:04–15:05 IST before the update: 4 stuck Ctrl crossings; after the update at 15:06, repeated hold-Ctrl-cross-release tests: 0) · reported 2026-10-09

**The trigger (owner, 26 Oct, 14:10–14:15 IST):** back on Windows after a crossing, the wheel
didn't scroll; on Kali, Alt+Tab didn't work. One tap of Ctrl fixed each.

**Cause, from Kali's log and the hook code:** in 3,350 crossings, the only keys that ever arrive
within 5 ms of "peer took control" are modifiers (CtrlL 20, AltL 18, ShiftL 8, ShiftR 7). They are the
crossing snapshot (`emit_held_modifiers`), which forwards what Windows reports as held. At 08:39:54 and
08:42:30 UTC a CtrlL press arrived that way with **no release** for 13–16 s. Windows thought Ctrl was
held while nobody held it. How it gets there: the hook swallowed *every* key event while input was on
the peer, including the release of a key whose press Windows had already seen. Hold Ctrl, cross, let go
on the other side: Windows never hears the release. Ctrl stays down on Windows (the wheel zooms instead
of scrolling), and each following crossing forwards the stuck Ctrl to Kali (Alt+Tab becomes Ctrl+Alt+Tab).
A physical Ctrl tap sends the missing release to whichever side has the keyboard.

**Fix:** `held.rs` records, from hook events only (no polling, per B14), which keys Windows saw go down
while input was local. While suppressing, the release of such a key is forwarded to the peer **and**
let through to Windows. Unit-tested on Linux; the Windows hook path compiles but needs the owner's check.

**Was parked 2026-10-09 by the owner** until the trigger was known.

> "Alt tab issue comes up very rarely and I think it only happens due to some trigger,
> but till the trigger is unknown keep it down right now and park the issue."

**Do not work on this.** It is rare, it self-recovers, and the previous attempt to fix it
blind cost six hours of unusable keyboard ([B14](#b14--modifiers-were-released-8-ms-after-the-press-regression-fixed)).
Two failed diagnoses in one day is the signal to stop guessing and wait for data.

**What stays in place while it is parked** — both already shipped and working, neither
needs touching:

* the five-second idle sweep on the receiving side (confirmed firing in the field);
* `ReleaseAll` on `Message::Leave`, so a stuck modifier cannot outlive a crossing.

**What would unpark it:** the trigger. The one thing no log has yet is what the owner was
*doing* when it happened. Both machines upload logs automatically, so if it recurs there
is nothing to capture by hand — just a note of roughly what was going on (which app had
focus, whether a volume/Fn key or a notification was involved, whether it had just
crossed). The 46 recorded instances all look alike in the logs, which is exactly why the
missing piece is outside them.

**Resist the obvious next guess.** Anything that polls "is this key really held?" on the
Windows side is already disproven — see B14 for the measurement. A future fix has to come
from knowing the trigger, or from the receiving side.

---

<details>
<summary>Earlier investigation, kept for the record — note the diagnosis below was wrong</summary>

Intermittent, and the owner's description is exact:

> "If I click on alt and tab it is changing the window, but as soon as we are supposed to
> release the alt and tab button it is supposed to change the window normally — but it
> don't do that, it will keep the window stuck and I have to click manually from the
> mouse. To correct it I go to the Windows and click control two or three times, and
> again alt-tab and everything works correctly."

That is a **stuck Alt**. Holding Alt is what keeps the window switcher open; the switcher
commits and closes on Alt *release*. If the release never arrives, the switcher stays up
and the window appears frozen until a mouse click breaks it out.

**Measured, not guessed.** From the shipped logs:

* **100 crossings happened while a modifier was still logically held** — `AltL` and
  `CtrlL` most often. The signature is an `AltL pressed=true` followed by
  `peer released control` with no matching release.
* **24 warnings on the laptop that Windows was refusing to deliver our keyboard hook**,
  each naming the window in front and both integrity levels.
* Press/release counts are lopsided (`CtrlL` 257/55, `AltL` 203/121). Treat that figure
  as *suggestive only* — auto-repeat inflates it badly for keys that are held
  (`Backspace` 52/9 is repeat, not loss). The two points above are the real evidence.

**Cause.** Windows does not call a low-level keyboard hook belonging to a lower-integrity
process while a higher-integrity window has the foreground — the same UIPI rule behind
R20. When such a window takes focus *in the gap between Alt-down and Alt-up*:

1. the Alt press was captured and forwarded (hook still working);
2. the foreground changes, and Windows stops calling our hook;
3. the Alt release is **never seen by mouse-rs**, so it is never forwarded;
4. the peer keeps Alt logically down, and nothing resyncs it.

**Why only sometimes.** The culprits are windows that flash up for a moment, so it only
bites when one appears inside that gap. By frequency in the log:

| Window in front | Times | What it is |
|---|---|---|
| `OmApSvcBroker` | 18 | OEM service broker, elevated |
| `OSD_FnKey` | 1 | the Fn-key / volume on-screen overlay |
| `Task Manager`, `Windows Security`, others | 5 | transient system UI |

`OSD_FnKey` is worth singling out: pressing a volume or brightness key pops an elevated
overlay, which is enough to kill the hook mid-chord.

**Why pressing Ctrl a few times fixes it.** Not fully confirmed, and probably two things
at once: moving back to Windows triggers `ReleaseAll` on the peer (`client.rs`, on
`Message::Leave`), which clears whatever it believed held; and fresh Ctrl press/release
pairs that *do* get through resync the modifier state.

**Fix the owner can apply today:** every one of the 24 warnings recorded
`our_level="0x2000 (normal)"` — **mouse-rs on the laptop is not running elevated**, even
though the install prompt asked for it. 23 of the 24 blocking windows were
`0x3000 (elevated (administrator))`, ordinary admin rather than SYSTEM, so an elevated
mouse-rs would be at the same level and would keep its hook. The toggle already exists:
Startup → *Start at login as administrator* (R20). **Not done — Windows is frozen by the
owner's instruction until they ask.**

**Mitigated 2026-10-09 on the receiving side** (`mouse-engine/src/injector.rs`): a
modifier still held after five seconds of complete silence is released, because a human
holding Alt generates *something* — another key, a mouse movement — and total silence with
Alt down means nobody is holding it. Only modifiers qualify, and not while a mouse button
is down, since a modifier-drag is a real gesture someone can hold still in. Five tests,
one of which proves the release comes from the idle sweep rather than from teardown.

This is a **safety net, not the cure.** It recovers within five seconds instead of
requiring a crossing; it cannot know the release was lost any sooner, because only the
sending machine ever saw it.

**The sending-side "cure" was wrong and has been withdrawn (2026-10-09).** It shipped as
`22C7FC47…` and was pulled the same evening after the owner reported *"the software is not
sharing ctrl now"* and *"alt+tab only works when I click like 5 times"*.

It compared what we had told the peer was held against `GetAsyncKeyState`, on the premise
that the async key state "reads the real keyboard regardless of hooks". **That premise is
false for keys this program itself suppresses.** A key swallowed by our low-level hook is
never dispatched, so it never reaches the async key state either — which means for every
modifier the user holds, *"we said down, the OS says up"* is the **normal** state, not a
lost key-up. The reconcile therefore fired on every chord:

```
14:47:11.296  CtrlL  pressed=true    ← user presses Ctrl
14:47:11.304  CtrlL  pressed=false   ← the reconcile, 8 ms later
14:47:11.656  Space  pressed=true    ← Space arrives with no Ctrl held
14:47:11.904  CtrlL  pressed=false   ← the user's real release, far too late
```

Ctrl+Space never reached the peer at all, and Alt+Tab worked only when Tab happened to
land inside the few milliseconds before the sweep — hence "5 times". It fired 111 times in
five minutes (`AltL` 59, `CtrlL` 33, `ShiftL` 18), two of them one tick apart, which is
the tell: it was releasing a key the user was still holding.

**The measurement that settles it, and that the earlier diagnosis above got wrong.** Per
crossing on the owner's hardware: `swallowed=414`, `hook_calls=768`, **`raw_keys=0`**.
Raw Input is a wholly separate delivery path from the hook chain, registered here as an
independent witness (`RIDEV_INPUTSINK`), and it did not see a single one of those 414
suppressed keys. So *no* Win32 call can observe a key we swallow, and no sending-side
scheme of this shape can work.

The same counters also undercut the UIPI explanation given above: **no crossing in the log
ever had `hook_calls=0`**, and the "Raw Input saw keystrokes the hook never got"
diagnostic has never once fired. A hook blackout mid-chord is therefore *not* the
demonstrated cause of B13, however plausible it reads. The 46 crossings that ended with
Alt still logically held are real and are recorded, but the mechanism behind them is
**not yet established.**

The guard against rebuilding this is a test in `windows.rs`
(`the_modifier_table_is_only_a_crossing_start_snapshot`), which fails if
`stuck_mask`/`reconcile_modifiers`/the per-key sent-down mask reappear in that file, and
carries the measurement in its docs. `TRACKED_MODIFIERS` survives for its one sound use:
reporting what is held at the *start* of a crossing, when nothing has been suppressed yet
and the OS state is still truthful.

**Where it stands.** Recovery lives only on the receiving side, which is the one side that
knows what it is still holding:

* the five-second idle sweep above — confirmed working in the field, one firing logged at
  13:51:41 releasing a stuck `ShiftR`;
* `ReleaseAll` on `Message::Leave`, so a stuck modifier can never outlive a crossing.

**Still worth doing:** elevate mouse-rs on the laptop. All 24 hook-blocked events recorded
`our_level="0x2000 (normal)"` against blockers at ordinary elevated admin. Note this is
now a *lead*, not a known cure — those warnings concern crossing start, and the counters
above show the hook was alive during every crossing.

</details>

### B14 — modifiers were released 8 ms after the press (regression, fixed)
**fixed 2026-10-09** · shipped and withdrawn the same day

The sending-side B13 "cure" broke every modifier chord: Ctrl was unusable, Alt+Tab took
about five attempts. Full account under [B13](#b13--alttab-sticks-a-modifier-release-is-lost-and-the-key-stays-logically-down).

Full analysis: **[POSTMORTEM-2026-10-09-modifier-regression.md](POSTMORTEM-2026-10-09-modifier-regression.md)**
— five separate failures had to line up for it to reach the hardware, and the install and
recovery steps failed three more times on their own.

Kept as its own entry for one reason worth not losing: **a fix built on an untested
premise, shipped to hardware that is the only place it can run, did more damage than the
bug it targeted.** B13 is intermittent and self-recovering; this made the keyboard
unusable. The premise — "`GetAsyncKeyState` sees through hooks" — was stated in a doc
comment and never checked against the `raw_keys` counter already in the same file, which
disproved it.

Guarded by `injector::tests::a_chord_held_while_the_user_keeps_typing_is_never_broken`,
which runs on the Linux host (unlike the Windows-only tests that shipped the regression)
and was confirmed to fail against the faulty behaviour before being kept.

### B4 — cut-and-paste across machines behaves as copy in Thunar — **won't fix (owner, 2026-10-14)**

- The owner's decision: whoever cuts can delete the original themselves, so a cross-machine cut working as a copy is acceptable. Do not work on this.
**open**

mouse-rs publishes the GNOME `x-special/gnome-copied-files` format with the verb `cut`
(`crates/mouse-clipboard/src/files.rs:468`), which should make the file manager relocate
the file rather than duplicate it. Confirmed that `verb=cut` really is published; Thunar
copies anyway.

Appears to be Thunar declining a foreign application's cut rather than anything wrong on
our side, so the fix may not be ours to make. Unconfirmed.

**Impact:** a "cut" leaves the original in place. Nothing is lost, but the user has to
delete it themselves.

### B5 — files on the clipboard stop working once mouse-rs exits — **intended behaviour; holder removed (owner, 2026-10-15, R33)**
The owner's rule is that quitting means quitting: nothing may outlive mouse-rs, so copies stop being pasteable when it exits. The `hold-files` helper described below was removed. Kept for the trail.

**was: fixed 2026-10-12 on X11** · verified with a real X selection owner, not yet on a desktop paste

X11 has no clipboard daemon: the selection owner is asked **live** for every paste. So when
mouse-rs quit, a file list it had put on the clipboard silently stopped being pasteable.

**Fixed:** the file list being served is recorded (`files.rs`, `HELD`). When clipboard sync
shuts down (Stop, Quit or exit), and mouse-rs still owns the selection and every file is still
on disk, it starts a detached holder: the same binary with the hidden `hold-files`
subcommand, which takes the list on stdin and keeps serving it until someone else copies.
The holder exits as soon as another copy takes the selection, so it never outlives its use.

**Verified:** `hold-files` served the list (`text/uri-list` and GNOME forms checked with `xclip`),
and exited when another copy replaced it. `handoff_tests` (ignored by default, needs a display)
checks that a live owner hands its list over. **Not yet proven:** a paste into Thunar after
mouse-rs has exited.

### B6 — Windows shows a tray tooltip but no toast
**unverified** · tracked under R18 · built 2026-08-14

The toast is implemented in `crates/mouse/src/notify.rs`: it raises a Windows toast through
PowerShell's built-in application ID, so it needs no installer. It has never been seen on the
Windows machine. Until it is, treat it as built but unproven.

**Impact (if it does not show):** on Windows, a waiting transfer is easy to miss.

### B7 — the panel has no accessibility tree
**deferred** · tracked as R13

iced/wgpu exposes zero UI Automation descendants, so screen readers cannot see the panel
and UI automation cannot drive it. **Why deferred:** it is a limit of the GUI toolkit, not a
bug in mouse-rs code, and fixing it means either a toolkit change or a second, accessible
front end. That is a product decision (see the sale plan), not a fix to ship in this round.
Until then the tray menu is the one fully accessible control surface.

### B8 — console flashes when launching the GUI from Explorer
**deferred** · tracked as R14

Cosmetic. `FreeConsole` runs after Windows has already allocated the console.

---

## Platform gaps

### B9 — login-screen sharing is Linux-only
**open**

Nothing equivalent exists on Windows. The Windows login screen (Winlogon) runs on a
separate, more protected desktop, so this is unlikely to be the same shape of fix.
**May be what B1 is about** — do not assume.

**Why deferred:** there is no supported way for an ordinary program to inject input at the
Winlogon desktop, and the one route that might work (a SYSTEM service on a secure desktop)
is a different product with its own signing and review burden. Not attempted in this round.

### B10 — Wayland can receive input but cannot send it
**deferred** · 2026-10-12

The new uinput backend injects fine under Wayland, so a Wayland machine can be a
*client*. Capture is still X11-only (`crates/mouse-input/src/linux.rs`), so a Wayland
machine cannot be the *server*. Kali is X11 today, so nothing is blocked right now.

**Why deferred:** Wayland capture needs the compositor's portal (PipeWire/libei), a separate
integration per desktop. Build it when a Wayland machine has to be the server.

---

### B33 — Linux as server / Windows as client — **signed off by the owner 2026-10-15**
**closed for Linux → Windows** (keys, shortcuts, scrolling, clicks, clipboard, folder round trip all confirmed in daily use) · Linux → Linux and Windows → Windows still need same-OS hardware · found 2026-10-15

Every hardware run so far has had the laptop as server: the shipped logs hold 72 starts with
`role=Server`, all `name=laptop`, and none on Kali. So the code for the other direction exists
but has never moved a real mouse:

* **Linux capture** (`X11Capture`, `crates/mouse-input/src/linux.rs`): written in the first
  commit (June 2026), barely touched since, never run on hardware.
* **Windows injection** (`WinInjector`, `crates/mouse-input/src/windows.rs`): never driven by a
  real server.

**Windows → Windows** and **Linux → Linux** use only these same pieces, so they are unproven
too. Clipboard and files do not depend on the role, so they should behave as already
signed off.

**Expected limit on Windows as client:** Windows does not let a normal program type into a
window running as administrator (UIPI). Input injected while one of those windows has focus
is dropped unless mouse-rs itself runs elevated.

**Next step:** the owner swaps roles (Kali = Server, laptop = Client) and runs the
mouse, keyboard, edge and clipboard checks in both directions.

**15 Oct 2026, first run with the roles swapped (Kali server, laptop client), from the shipped logs:**
about 25 crossings each way; motion arrived on the laptop; keys and chords arrived
(Ctrl+Alt+Left/Right, Alt+Tab); the client reconnected by itself when Kali's app restarted;
clipboard text Kali → laptop (29 bytes) and a file Kali → laptop (60 bytes, hash verified).
Edges are exit `right`, entry `left`. Kali's take-back key is Scroll Lock (`lock_hotkey`);
`return_hotkey` is empty there.
**Folder round trip, same day, confirmed:** a `test` folder (`.md`, `.png`, a 699 MB `.zip`, an 834 MB `.mkv`)
went Kali → Explorer (~77–80 MB/s, "every file of this copy has been pasted") and back
Explorer → Thunar (~78 MB/s). `sha256sum -c` against the originals passed on all 4 files.
**Still to confirm by hand:** clicks and scrolling land on the laptop (the client doesn't log them),
and that typing shows up in a Windows app.
**Linux → Linux and Windows → Windows:** these use the same capture and inject pieces, each of
which has now run on hardware. Only same-OS hardware can prove them.
Small nit: the client logs `WARN … MOTION ARRIVED BUT NO KEYS` for every crossing where nothing
was typed. That is noise, not a fault.

### B34 — only the client proves who it is; the server is never checked — **fixed 2026-10-26, awaiting hardware check**
found 2026-10-15 while writing `docs/lock-screen-security-plan.html`

**Was:** `client_handshake` accepted the server's `AuthOk { ok: true }` with no proof that the server
knew the shared secret, and `tls.rs` accepts any certificate. A machine that took over the
server's address could act as the server and type into the client, including at the login screen.
The HMAC was also not tied to the TLS session, so a machine in the middle could pass the
handshake through between the real client and server and then read every keystroke.

**Fix (protocol 2):** mutual, TLS-bound proofs (`auth.rs`). The client sends a nonce in its Hello;
the server answers with its nonce and `HMAC(secret, "server" ‖ binding ‖ client nonce ‖ server nonce)`;
the client checks it **before** sending its own proof (label "client"). `binding` is 32 bytes from
the TLS keying-material exporter, which differs on each side of a machine in the middle, so relayed
proofs fail. A version 1 peer is refused (`old_version`), so there is no downgrade.
A refusal is now shown in the window ("refused — the other machine runs an older mouse-rs; update it",
and so on), and the client keeps retrying instead of stopping, so updating the other machine is enough.
Tests: a fake server is refused before the client proves anything; a v1 client is refused;
proofs fail across sessions, across sides, and with other nonces.

**Still open (by design, fixed by pairing on 27 Oct):** whoever proves first gives out one HMAC an
attacker could test guesses against offline. A generated 32-character secret makes that useless;
a short hand-typed secret does not. The pairing code (PAKE) removes the typed secret.
With `tls_enabled: false` (config file only, not in the UI) there is no binding.

### B35 — a copy whose small files are pasted before its big ones is never marked done
**open** · found 2026-10-15 in the B3 run (laptop log, 11:37–11:38 UTC)

A 4.65 GB, 6-file copy from Kali pasted completely into Explorer: every file streamed in full,
with no restarts. But the entry never logged "every file of this copy has been pasted", so its
row keeps its old status (here "waiting", because it was over the ask-first limit). The two
small files came from the cache and were pasted first. Explorer's paste of a cached file then
removes mouse-rs's own copy ("pasted copy complete; mouse-rs's own copy removed"), so
`Transfers::has_arrived` no longer finds those hashes in the cache or in `arrived`, and the set
never counts as arrived. On 15 Oct at 08:21 the order was the other way round (big files first),
and it was marked done.
**Fix (small):** record the hash in `arrived` when a paste removes our own copy, in
`win_virtual` and `stream_fs`.
**Impact:** cosmetic, plus a waiting-file count in the tray. The files themselves are complete.

### B36 — a screenshot arrived on the laptop as a file path after two screenshots in a row — **fixed 2026-10-21**
Reported by the owner on 21 Oct: twice in half an hour, a screenshot pasted as a path. Stop and
Start cleared it.

**Cause, on Kali.** Every image received from the laptop is offered on Kali as pixels *and*
as a `.png` file, so the clipboard also answers text requests with that file's `file://` link.
Our own text poll must ignore that link, and it did so by remembering only the **newest** one.
Publishing is asynchronous, so for a moment after a second image arrives the clipboard still
serves the **first** image's link. The poll read it, saw that it differed from the newest,
and sent it to the laptop as text. Half a second later it sent the second link too. The
laptop's clipboard, which held the screenshot, was replaced by a path. Logs: 12:34:42 UTC
(sent 149 then 147 bytes, after a 1920×1080 and then a 532×576 image) and 12:41:54 (148 + 148).
These are the only two cases in the logs since 14 Oct. Not network-related: the second happened
on the cable, with no link change.

**Fix.** The last 8 file links we publish are remembered (`files::own_uri_text`), and the text
poll ignores any of them (`files::is_own_text`). The test
`an_older_own_file_link_still_on_the_clipboard_is_not_sent_as_text` replays the sequence, and fails
without the fix.

### B37 — on Wi-Fi the pointer froze, then jumped — **UDP motion built 2026-10-26; stutter remains**
On the home 2.4 GHz router, with a video loading, the pointer vanished for up to ~1 s and then jumped.
Kali's `ss -tin` showed 9.4% of the input connection's packets lost or reordered; over TCP each
loss holds back everything behind it until the resend (Windows waits ≥ 300 ms). Priority marking
(qos.rs) did not help. Pointer motion now goes over UDP as running totals (`motion_udp.rs`), so a
lost datagram is covered by the next one. Measured after: longest gap 133–144 ms (was ~1 s), but
4–10% of moves still arrive more than 25 ms late and the heartbeat round trip still peaks at
~720 ms. Kali's side is clean (−25 dBm, power save off), so the remaining delay is the laptop's
Wi-Fi adapter or the router. On the owner's Motorola hotspot (5 GHz, Wi-Fi 6), with motion still
on TCP: round trip median 3–4 ms, max 6–143 ms, 1.9% of moves over 25 ms late. So the home 2.4 GHz
router is most of the remaining stutter.
Every minute on Wi-Fi still had a 127–148 ms freeze. The owner found the trigger by hand (27 Oct):
opening Kali's Wi-Fi menu (nm-applet) made the pointer lag at once, because it starts a scan and
the adapter leaves the channel to listen on the others. Windows scans by itself in the background.
Built (owner's choice of options, 27 Oct): only while the pointer is on the other machine, each
Windows Wi-Fi adapter is put in media streaming mode with background scans off
(`qos::wifi_quiet`); back on the laptop, on disconnect or at exit the WLAN handle closes and
Windows restores its settings, so the network list and auto-connect are normal whenever the user
is at that machine. Linux needs root for the same, so Kali is not covered. Also built: the
receiving side carries the pointer through a gap of up to ~30 ms at its speed
(`motion_udp::Conceal`), and the server repeats the final total 25 and 80 ms after motion stops,
which takes back a guess past where the hand stopped and covers a lost final datagram. Motion datagrams now carry the send time and the
client logs "pointer delay over the last minute", which sees held-back moves, not just gaps.

### B38 — on a phone hotspot the two machines never found each other — **fixed 2026-10-26, confirmed on hardware**
Both on the owner's Motorola hotspot (5 GHz, Wi-Fi 6): Kali got 10.144.3.242, the laptop's Wi-Fi
got no address (169.254.85.145), so neither IPv4 discovery nor a connection could work, and the
log said nothing about why. Now the server also beacons over IPv6 all-nodes multicast from its
link-local address and listens on IPv6, and the client falls back to that address when no IPv4
address answers (pointer motion then stays on TCP). While no server answers, the client logs once
a minute what it has, what it heard and what it tried. Still unknown: whether the laptop's
firewall rule for mouse-rs covers Public networks (a new hotspot is Public by default), and
whether this phone isolates hotspot clients from each other.
Confirmed 26 Oct (after a fix: `if_addrs` hides fe80:: addresses, so the first build found no
interface): with the laptop still at 169.254.x.x, Kali connected over
`[fe80::…%wlan0]:24800` 4 s after the cable came out, and an image copy worked over the bulk lane.
UDP motion now also runs over IPv6 (the first hotspot run had motion on TCP).

### B39 — the bulk lane never connected off the cable — **fixed 2026-10-26**
The bulk lane always dialled the configured `server_host` (192.168.137.1), so on Wi-Fi it timed
out and every large item went inline. It now dials the address the input link connected to
(`LinkHealth::peer`).

---

## Build and release fragility

### B11 — the served Windows exe was a symlink into the build tree
**fixed 2026-10-09** · found 2026-10-08

`~/mouse-serve/mouse-rs.exe` is a symlink to
`target/x86_64-pc-windows-gnu/release/mouse-rs.exe`, not a copy. Any Windows rebuild
therefore changes what the laptop would download while the prompt still states the old
hash — the laptop fails the checksum step, and it reads as a corrupt download rather than
"someone rebuilt it".

Shared code (`config.rs`, `run.rs`) has already diverged from the served build, so this is
live, not theoretical.

**Fixed:** `scripts/serve-windows.sh` now copies the binary (via a temp file and an
atomic rename, so a download in flight never sees a half-written exe) instead of linking
it. Verified: the served file is a real 12,087,808-byte copy, not a link. The prompts stay
symlinked — the same script regenerates them and rewrites the hash in them, so a link
there can never disagree with what it states.

### B12 — a bare `cargo build` fails on this machine
**fixed 2026-10-12**

`libxdo-dev` is not installed, so the `libxdo.so` dev symlink is missing and linking fails
with `unable to find library -lxdo`.

**Fixed:** `crates/mouse/build.rs` adds `.xdo-shim/` to the link search path on Linux, so a plain
`cargo build` links without any `RUSTFLAGS`. Verified with `env -u RUSTFLAGS cargo build -p mouse`.
Installing `libxdo-dev` still works and is simply not needed.

**Workaround:** `RUSTFLAGS="-L $PWD/.xdo-shim" cargo build --release`
**Fix:** `sudo apt install libxdo-dev`

---

## Network following (R22)

### B15 — discovery cannot cross a network that isolates its devices
**unverified** · built 2026-10-12

Discovery sends a beacon to each network's broadcast address on UDP 24801. Some networks stop
one device from reaching another: guest Wi-Fi, some office and hotel networks, and some
hotspot settings ("AP isolation" or "client isolation"). On those, beacons never arrive, and
the client falls back to `server_host` and to the addresses it already knows. A server on an
isolated network is only found if its address is typed in.

**Impact:** no automatic discovery on that network. Connecting still works if the address is set.

**Also check on Windows:** the first time the server listens for beacons, Windows Firewall may
ask about mouse-rs on private networks. Allow it, or beacons do not reach the server.

### B16 — the two machines must share a network before they can connect
**by design, not a defect**

A client only reaches a server on a network it is attached to. If the two machines are on
different networks with no route between them (for example Kali on Ethernet to one router, and
the laptop on Wi-Fi to another), no software can join them. The moment they share a network,
the beacon finds the server and the link comes up within a few seconds.

### B17 — files already held here were never put on the clipboard — **fixed 2026-10-13**

A copy on the peer of files Kali already held (here, a 3.1 GB video left in the cache by an
earlier test) was classed `Ready` and recorded in the history, but only the `Remote` and
`NeedsConsent` branches published anything. The panel said "3.1 GB · ready" while Thunar said
"There is nothing on the clipboard to paste". Log: `13:20:25 DECISION: ready immediately …
status=Ready`, with no PUBLISHED line after it. Fixed by `publish_ready_set`, which publishes
the cached files as a move, so the paste is a rename. It logs "already held here; put on the
clipboard straight away".

### B18 — Clear left the clipboard naming deleted files — **fixed 2026-10-13**

After the 3.1 GB video was pasted from the cache, Clear in the Clipboard tab removed
`cache/trees/`, which the clipboard still pointed at. The next Ctrl+V failed in Thunar with
"Error when getting information for file …/.mouse_share/cache/trees/…: No such file or
directory". Clear now sends `ClipCommand::Refresh`. If what the clipboard offers no longer
exists, the newest file entry is offered again: from the cache if it is still held, otherwise
from the streaming folder. Same day: the consent banner now shows only while an entry is
*waiting*, not after Allow. Files under the ask-first limit no longer prefetch big files to
disk before a paste, and "Remote" reads "on the other computer · paste any time".

### B19 — copying the same files again was ignored — **fixed 2026-10-13, confirmed on hardware 2026-10-14** (a re-copy at 12:36:12 reached Kali as entry 21 and pasted in full at about 78 MB/s)

After clearing the history and the cache, a second Ctrl+C on the same video in Explorer sent
nothing, because a local copy was detected by comparing the file list with the last one seen.
The fix also watches the clipboard's own change marker (`GetClipboardSequenceNumber` on
Windows; the owner window plus its TIMESTAMP on X11), ignoring changes mouse-rs made itself.
Same files with a new marker counts as a new copy and is announced again; the log says "the
same files were copied again". Text and images still dedupe by content.

### B20 — a file from Linux could not be pasted in Explorer until it had fully arrived — **fixed, confirmed on hardware 2026-10-14**

- Confirmed by ladder steps 2, 3 and 4 (video, folder, zip Linux → Windows): 834 MB at ~76 MB/s, 4.9 GB of videos in 67 s, a 699 MB zip with a cancel and re-paste.

Windows put cache paths on the clipboard as `CF_HDROP` before the files existed, so Explorer
failed at once. Built 2026-10-13, **not yet tested on hardware**: `win_virtual.rs` offers files
still on the peer as OLE virtual files (`FileGroupDescriptorW` + `FileContents` `IStream`,
with `IDataObjectAsyncCapability`) from an STA thread that pumps messages, including while a
read waits. Explorer writes the bytes straight to the paste target with its own progress.
Large files use the same memory pipe as Linux. Reads go through `remote_read.rs`, shared with
the Linux streaming folder.

### B21 — big pastes held to 17 MB/s; a paste of something the peer forgot waited 3 minutes — **fixed 2026-10-13**

- **Speed.** The same 3.1 GB video streamed at 68 MB/s at 19:40 and at 17 MB/s at 23:06, with the
  pointer on Kali both times. The hard 16 MiB/s ceiling while crossed was tied to
  `is_congested` (8 ms over baseline), which is far below anything felt, and stayed on for the
  whole second run. It now needs `pointer_at_risk`: 3× baseline and at least 30 ms extra. The
  adaptive pace still uses the gentle signal. The sender logs "transfer slowed to protect the
  pointer" / "back to full speed" on every change.
- **Forgotten file.** After the laptop's history was cleared, a second paste of the streamed
  video asked every 10 s for 180 s and then failed. The laptop had answered each request with
  `BlobUnavailable`, which never reached the waiting paste. The pipe (and the cache-path
  signal) now fail at once. A pending stop from the previous paste could also swallow a new
  paste's first request, costing 10 s; a reader's request now clears it.

### B22 — a screenshot pasted as a file link; a cancelled paste counted up for days — **fixed 2026-10-13, confirmed on hardware 2026-10-14**

- **Echo.** A Kali screenshot went to the laptop as an image (correct). The laptop then mistook
  its own clipboard write for the user copying the `.png` again ("the same files were copied
  again", from the B19 re-copy detection) and sent it back as a file. Kali's clipboard then held
  `file:///run/user/1000/mouse-rs/incoming/9/clipboard-image-540x587-….png` instead of the
  picture. Cause: mouse-rs opens the Windows clipboard without a window, so the recorded owner
  is *nobody*, which was treated as another program. Fixed twice over: no owner means "can't
  tell", and every write by mouse-rs records the clipboard sequence number, which is then never
  treated as a copy.
- **Cancel.** Cancelling a streamed paste in Thunar stopped the transfer but left the history
  entry "fetching", so its time-left estimate grew without end. The pipe now reports
  `Released { finished }`; a paste cancelled part-way returns the entry to "on the other
  computer · paste any time" and logs it.
- **Hardware check, 2026-10-14.** The echo came back once at 12:09 because the laptop still ran
  the 23:35 build. The fix (`59085CC8…`) was built at 23:53, after that update. Once the laptop
  was updated (12:12), three screenshots pasted as images, and the log shows no "copied again"
  line. The echo is **confirmed fixed**.
- **Cancel retest, 2026-10-14 12:31.** A 3.3 GB video pasted into Thunar was cancelled after 485 MB.
  Kali logged "the paste was cancelled; transfer stopped, the file is still offered"; the laptop
  logged "the peer cancelled; stopped sending". **Confirmed fixed.** The same run confirms B21:
  485 MB in 6.7 s (about 72 MB/s), with one 2-second slowdown while pointer latency was high.

### B23 — Kali's panel logs "Timeout when presenting surface" every few seconds — **fixed 2026-10-14**

- **Cause, measured.** 1,804 of the 1,819 lines came after more than 10 minutes without input,
  and Kali's screen saver and DPMS standby trigger at exactly 600 s. With the display blanked or
  off, the driver will not present a frame, so the panel's timed redraws (median every 11 s)
  time out. iced_winit logs each one at error level. Nothing is broken.
- **Fix.** A `ScreenOffNoise` layer (`crates/mouse/src/logging.rs`) reads the message through
  `Layer::event_enabled`. It keeps the first present timeout of each screen-off period and drops
  the rest until 2 minutes pass without one. Other iced_winit errors, including other present
  errors, are untouched. Tests: `one_present_timeout_is_kept_per_screen_off_period` and
  `only_the_present_timeout_is_thinned` (end to end through a subscriber). Still to confirm: the
  next real blanking on Kali should leave a single line.

- `iced_winit` logs `ERROR … Error Timeout when presenting surface`, often every 4–15 s.
  The Kali log holds 1,809 of these lines, from 2026-09-17 to 2026-10-14. Nobody has seen
  a visible effect, but the noise buries real errors in the shipped logs. It probably happens
  while the panel window is hidden or unmapped and iced keeps redrawing. Not investigated.

### B24 — reading a streamed file out of order restarts the stream on every seek — **closed, won't fix (owner, 2026-10-14)**

- **Decision.** mouse-rs is copy and paste: Ctrl+C on one machine, Ctrl+V on the other, for any
  file or folder. It is not a way to stream or play media from the other machine. Opening a file
  straight from the streaming folder (`/run/user/1000/mouse-rs/incoming`) instead of pasting it
  is not a supported use, so how well a player or thumbnailer reads from there does not matter.
  Do not work on this again.
- **What was tried and reverted.** Up to four buffered regions per file (`4412e56`) and turn-taking
  between two readers (`475de52`), both reverted (`09c8d01`, `7a43032`). The hardware test still
  failed. VLC and the thumbnailer reading at once exposed a race: a cancelled stream's late
  Begin/Abort can clear the record of the new request at the same offset, so the next request
  went without a cancel and the sender ignored it as a repeat. That race needs a request id in
  `BlobRequest`/`BlobMeta`. It only appears with several readers or rapid re-requests, which a
  paste does not do. If a paste ever shows "already streaming this file; ignoring the repeat
  request" on the sender, start from there.
- **Why the folder is harmless.** It holds nothing on disk. A file there is a view onto the
  laptop's file, with at most a 64 MB in-memory buffer while something reads it, freed when the
  reader closes it. Opening a file there leaves nothing behind, and the folder disappears when
  mouse-rs stops (B26).

- **Update, 13:05.** B25 below was caused by these seeks. The restarts are still open; the
  corruption they caused is fixed.

- At 2026-10-14 12:34:19 something on Kali read the streamed 3.3 GB video from the FUSE folder at
  scattered offsets: 0, the last 200 KB, 124, 509 MB and so on. It was probably a thumbnailer or a
  player, since a Thunar paste reads in order. Each jump cancelled the request to the laptop and
  sent a new one. That made about 155 requests in 30 seconds before the read settled into one
  sequential pass. Nothing failed, and the following pass ran at full speed. But each restart
  throws away data already in flight, so playing a video straight from the streaming folder would
  be slow. Possible fix: serve small reads far from the current position, like the header and
  index at the end, as one-off range requests, without dropping the main stream. Not investigated.

### B25 — a re-copied video failed with "Error splicing file"; the earlier paste came out corrupt — **fixed 2026-10-14**

Three defects, all on Kali's side of a streamed (≥ 64 MB) Windows → Linux paste:

- **"Error splicing file. Input/output error."** A file's in-memory pipe lives as long as the
  file is offered, so copying the same video again reuses it. After a paste finished, the pipe
  kept its old "last grew" time. When the same video was pasted again 3½ minutes later, the
  first read already looked stalled past the 180 s limit and failed at once. The logs show
  this: "no data, or the file failed verification; failing the copy offset=0" at 12:40:34 and
  12:41:11, without a single request to the laptop. Now `reopen` after a release resets the
  pipe completely, including the hash state, so a hashed file is verified again rather than
  waved through on the last paste's result.
- **Corrupt earlier paste.** When a read jumps (B24), the pipe clears its buffer and restarts at
  the new offset at once. But the old stream keeps delivering until the clipboard thread
  cancels it, and `push` appended those bytes at the new offset. The 12:34 paste ran during
  ~155 such jumps and "finished" with the right length but the wrong bytes. A 3.3 GB video
  is checked by length only, so nothing caught it. `push` now takes the chunk's file offset
  and drops anything that does not start exactly where the buffer ends.
- **Lock icon / read-only paste.** The streaming folder reported files as 0444 and folders as
  0555. Thunar copies the mode, so every pasted file was read-only and a pasted folder would
  refuse new files. Now it reports 0644 and 0755. `open` still refuses any write.

Tests: superseded-stream bytes are dropped; a re-copy gets a clean pipe that asks again and
re-verifies; the real-mount test checks the pasted copy is writable. 239 tests pass, plus both
mount tests; clippy is clean on Linux and Windows.

### B26 — stopping mouse-rs with SIGTERM leaves the streaming folder's mount stale — **fixed 2026-10-14**

- Seen 2026-10-14 13:02: after `kill`, `/run/user/1000/mouse-rs/incoming` gave "Transport
  endpoint is not connected" until `fusermount3 -u`. Not yet checked whether a fresh start
  recovers from that by itself. A logout or crash could leave it the same way.
- **Fix.** The next start already cleared a dead mount (`fusermount3 -u -z` before mounting).
  The gap was the time between dying and starting again. FUSE's `auto_unmount` would close it,
  but fuser allows it only with `allow_other`, which needs root to enable in
  `/etc/fuse.conf` and would let other users read the folder. So `StreamFs::mount` now starts a
  small `sh` watcher before mounting. It is in its own process group, ignores HUP/INT/TERM, and
  waits on a pipe from mouse-rs. When mouse-rs ends, however it ends, the kernel closes the pipe
  and the watcher runs `fusermount3 -u -z`.
- **Checked.** A new test mounts in a child process, kills it with SIGKILL and requires the mount
  gone within 5 s. It fails without the watcher. On Kali at 13:20, `kill` on the real app left
  the folder unmounted and usable 1 s later, and the restart remounted and reconnected.

### B27 — cancelling a Linux → Windows paste in Explorer left the entry counting up at 30% — **fixed 2026-10-14, confirmed on hardware 13:51**

First hardware run of step 2 (`win_virtual.rs`), 13:30. Pasting an 834 MB video worked at about
76 MB/s. The same video, copied again and cancelled in Explorer at 30%, was logged as "Explorer
closed the streamed file", but the entry was never reset. Two causes, both from reusing one
pipe per file:

- The first paste had read to the end, so the pipe's `finished` flag was set. The laptop's build
  never cleared it for the next paste, so the cancel was reported as a completed paste. Already
  fixed on Kali by B25's full reset in `reopen`; the laptop did not have it yet.
- `pipe_for` kept the history entry the pipe was **first** made for (15). The re-copy was entry
  16, so even a correctly reported cancel would have reset 15 and left 16 counting up. It now
  moves the pipe to the newest entry.

Test: `a_cancelled_paste_of_a_recopied_file_is_reported_for_the_new_entry` (pasted in full, copied
again, cancelled part-way: it must report entry 16). Without the fix it reports `[15]`.

### B28 — a copy of streamed videos plus a small file stayed at "0% of 4.9 GB" after it was all pasted — **fixed 2026-10-14**

Linux → Windows, 13:52. Four videos and one `.md`, 4.9 GB in all, pasted into Explorer in 67 s.
Every file arrived, but the entry kept showing "0% of 4.9 GB" with a Stop button. The videos
streamed through memory, so they never entered the cache. The `.md` went through the cache,
and Explorer asking for it set the whole entry to "fetching". Completion was then judged only
by "is every file in the cache?", which streamed files can never satisfy, and the pasted `.md`
had been dropped from the cache anyway. Clearing Kali's history played no part.

Fix: `Transfers` records every file that arrived in full this session, whether streamed to the
last byte or verified into the cache. `has_arrived(set)` ends "fetching" once every file has
arrived, even if mouse-rs no longer holds them. Test:
`a_set_mixing_streamed_and_cached_files_arrives_once_every_file_has`.

### B29 — Kali sometimes reports a single Ctrl+C in Thunar as two copies — **fixed 2026-10-14**

- 2026-10-14, 13:30:46 and 15:40:11: "the same files were copied again" came about 2 s after the
  real copy, after control had already gone back to the laptop, so the owner was not pressing
  anything on Kali. The laptop got the same offer twice and could notify twice ("A file is
  waiting"). Nothing else went wrong.
- Ruled out: a clipboard manager. None runs, and Thunar still owned the clipboard afterwards. At
  rest, Thunar answers the `TIMESTAMP` query in 3 ms with a stable value.
- Suspected: the first poll after a copy catches Thunar busy, so `TIMESTAMP` misses the 400 ms
  limit and the stamp is recorded with a zero time. The next poll gets the real time, and the
  change looks like a new copy. Not proven. The "copied again" line now logs `before` and `now`
  stamps (high half the owner window, low half its time). If `before` ends in `00000000` when it
  next happens, the fix is to ignore a change where only the time went from unknown to known.
- **Logged, 16:26:04.** `before=160001600464fab now=1600016004652bb`. Same Thunar window, both
  times known, 784 ms apart. So the timeout guess was **wrong**: Thunar really took the
  clipboard twice for one copy (a double Ctrl+C, or a held C repeating, since the laptop forwards
  key repeats).
- **Fix.** `files::is_new_copy`: on Linux, a repeat by the same window within 2 s of the
  previous copy is the same copy. Another window, an unknown time, or a re-copy after 2 s still
  counts. Windows stamps carry no time, so there any change still counts. Test:
  `one_copy_seen_twice_by_thunar_is_one_copy`, using the logged stamps.

### B30 — a re-paste counted as done while its video was still streaming; 3,500 identical log lines — **fixed 2026-10-14**

Seen 16:38:30 on Kali, during the folder test (which itself passed both ways). B28's
`arrived` set kept a file pasted earlier, so a new paste of a set holding that video counted as
"every file pasted" once the small files arrived, while the video was still streaming. Each
following frame set Ready again and logged it: 3,528 lines in one second. Not visible to the
owner, but the entry would show done early on a re-paste, and the log drowned.

Fix: a file leaves `arrived` when a new stream of it begins. The "every file … pasted" line
and status change happen only on the transition to Ready. Test: the B28 test now pastes the
set again and requires "not arrived" with half the video in. It fails without the fix.

### B31 — a streamed paste that closed before the end-of-stream frame was never recorded as done — **fixed 2026-10-14**

Windows → Kali, 699 MB zip, 16:36:30. Thunar read the last byte and closed the file **before**
the end-of-stream frame arrived. The close freed the pipe's buffer, so the frame's
`pipe.complete()` was false: no "streamed to the paste in full" line, the file was never
recorded as arrived, and the entry could stay "fetching" after a finished paste. The owner's
copy was fine (the zip extracted).

Fix: `drive_pipes` treats `Released { finished: true }` (the paste read every byte) as the
arrival, records it, and hands the entry to the clipboard loop (`take_pasted`), which marks
it pasted through the same `mark_pasted` as B28/B30. Whichever comes first, the close or the
end frame, the paste is recorded. Test:
`a_paste_that_closes_before_the_end_frame_still_counts_as_arrived` (fails without the fix).

### B32 — Thunar does not show a new copy in the streaming folder until it is reloaded — **closed with B24 (owner, 2026-10-14)**

- Only matters when browsing the streaming folder by hand, which is not a supported use (see B24).

- 2026-10-14 18:40 IST. The owner opened `/run/user/1000/mouse-rs/incoming` 12 s before the next
  Windows copy arrived, and the view stayed empty. Folder `10` existed. F5 (View → Reload) shows
  it. File managers watch folders through inotify, and a FUSE filesystem gets no inotify events
  for changes it makes itself. Kernel cache invalidation (`fuser` notifier `inval_entry`) does
  not reach a GIO file monitor.
- Impact: only when someone browses the folder by hand (the B24 test). A Ctrl+V paste names
  the file directly and is unaffected. Possible later fix: offer "Show in folder" from the
  panel, which opens the folder after the entry exists.
