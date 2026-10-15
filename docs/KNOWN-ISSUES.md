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

### B2 — login-screen sharing: PROVEN 13 Oct 2026 (password typed from the laptop); refresh pending

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

### B3 — the stalled-transfer fixes were never confirmed on hardware
**unverified** · built 2026-09-04 (`01555ac`)

Three receiver-side fixes — "already downloading" no longer read as failure, progress
surviving a paste retry, and the throttled over-budget log — were measured against the
shipped logs but the owner moved on before re-testing a large copy end to end. The
pacer fix before it (`38593c1`) is in the same position.

**Next step:** one 4 GB copy, watching whether the progress bar climbs monotonically
instead of resetting.

---

## Open defects

### B13 — Alt+Tab sticks: a modifier release is lost and the key stays logically down
**parked 2026-10-09 by the owner** · reported 2026-10-09 · **cause unknown**

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

### B5 — files on the clipboard stop working once mouse-rs exits
**fixed 2026-10-12 on X11** · verified with a real X selection owner, not yet on a desktop paste

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

### B33 — Linux has never been the server; Windows has never been the client
**unverified** · found 2026-10-15

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
**Still to confirm by hand:** clicks and scrolling land on the laptop (the client doesn't log them),
and that typing shows up in a Windows app.
**Linux → Linux and Windows → Windows:** these use the same capture and inject pieces, each of
which has now run on hardware. Only same-OS hardware can prove them.
Small nit: the client logs `WARN … MOTION ARRIVED BUT NO KEYS` for every crossing where nothing
was typed. That is noise, not a fault.

### B34 — only the client proves who it is; the server is never checked
**parked** (security phase, after functionality) · found 2026-10-15 while writing `docs/lock-screen-security-plan.html`

`client_handshake` (`crates/mouse-core/src/net.rs`) accepts the server's `AuthOk { ok: true }`
without any proof that the server knows the shared secret. `tls.rs` also accepts any certificate
identity. A machine that gets the client to connect to it (by taking over `server_host`'s address
on the LAN) can therefore act as the server and type into the client, including at the login screen.
It also receives `HMAC(secret, nonce)` for a nonce it chose, which lets it test guesses at a weak,
hand-typed secret offline. Discovery is not affected, because beacons are sealed with the secret.

**Fix (planned, layer 1 of the plan):** device key pairs pinned at a one-time pairing, and mutual TLS.
**Smaller stop-gap if needed before launch:** make the server answer a client nonce with its own HMAC,
so both sides prove the secret.

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
