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

### B4 — cut-and-paste across machines behaves as copy in Thunar
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

### B19 — copying the same files again was ignored — **fixed 2026-10-13**

After clearing the history and the cache, a second Ctrl+C on the same video in Explorer sent
nothing, because a local copy was detected by comparing the file list with the last one seen.
The fix also watches the clipboard's own change marker (`GetClipboardSequenceNumber` on
Windows; the owner window plus its TIMESTAMP on X11), ignoring changes mouse-rs made itself.
Same files with a new marker counts as a new copy and is announced again; the log says "the
same files were copied again". Text and images still dedupe by content.

### B20 — a file from Linux could not be pasted in Explorer until it had fully arrived

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

### B22 — a screenshot pasted as a file link; a cancelled paste counted up for days — **fixed 2026-10-13**

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
  line. The echo is **confirmed fixed**. The cancel fix is still awaiting its retest.
