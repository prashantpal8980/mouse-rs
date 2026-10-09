# Known issues

Things that are wrong, missing, or unproven — kept here so they are not rediscovered as
surprises later. `docs/UI-REQUIREMENTS.md` tracks what was *agreed*; this tracks what is
*broken or unfinished*.

Status values: **open** (not started) · **unverified** (built, never proven on real
hardware) · **deferred** (known, deliberately not now) · **awaiting detail** (reported,
not yet described well enough to act on).

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

### B2 — login-screen sharing: installed and enabled, not yet proven at a real prompt
**on hold at the owner's request 2026-10-09** (they cannot reboot right now) · built 2026-10-08 (`61b0d72`, `7b143a6`) · tested by the owner 2026-10-09 · R21

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
**open** · reported 2026-10-09 · **diagnosed, cause found**

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

**Cured on the sending side 2026-10-09** (`mouse-input/src/windows.rs`), once the owner
lifted the Windows freeze. Every ~120 ms while control is on the peer, the capture thread
compares what it has *told* the peer is held against what `GetAsyncKeyState` reports —
which works regardless of hooks, and so can see the release the hook missed — and sends
the release itself. Throttled to 120 ms rather than run on the 8 ms pump because that
thread is `TIME_CRITICAL` and must never be why a hook callback misses its deadline.

The decision is extracted as a pure `stuck_mask(sent_down, physically_down)` so the one
piece of reasoning is testable: a release is owed exactly when we said *down* and the
keyboard says *up*. Keys physically down that we never forwarded are not ours to release.
Two tests — but note they compile only for the Windows target and therefore **do not
execute on the Linux build host**.

Shipped to the laptop as `22C7FC47…`. **Unverified in use:** the owner has to hit the
fault again, and the fix logs a line naming the modifier each time it fires, which is
also how we learn how often it was happening.

**Still worth doing:** elevate mouse-rs on the laptop. All 24 hook-blocked events recorded
`our_level="0x2000 (normal)"` against blockers at ordinary elevated admin, so elevation
removes the most common cause outright rather than correcting after the fact.

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
**open**

X11 has no clipboard daemon: the selection owner is asked **live** for every paste
(`crates/mouse-clipboard/src/files.rs:41`). So when mouse-rs quits, anything it had put
on the clipboard silently stops being pasteable.

**Impact:** copy a file, quit mouse-rs, paste — nothing happens, with no error. Standard
X11 behaviour that every application shares, but surprising all the same.

**Possible fix:** hand off to a running clipboard manager before exit, or persist and
re-own on next start.

### B6 — Windows shows a tray tooltip but no toast
**open** · tracked under R18

A file waiting for a decision updates the tray tooltip; there is no desktop notification.
A real toast needs an Application User Model ID registered at install time, which the
current "download the exe and run it" distribution has nowhere to do.

**Impact:** on Windows, a waiting transfer is easy to miss.

### B7 — the panel has no accessibility tree
**open** · tracked as R13

iced/wgpu exposes zero UI Automation descendants, so screen readers cannot see the panel
and UI automation cannot drive it. Worth a decision before selling this.

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

### B10 — Wayland can receive input but cannot send it
**open**

The new uinput backend injects fine under Wayland, so a Wayland machine can be a
*client*. Capture is still X11-only (`crates/mouse-input/src/linux.rs`), so a Wayland
machine cannot be the *server*. Kali is X11 today, so nothing is blocked right now.

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
**open**

`libxdo-dev` is not installed, so the `libxdo.so` dev symlink is missing and linking fails
with `unable to find library -lxdo`. The repo carries `.xdo-shim/libxdo.so`;
`scripts/serve-windows.sh` exports it, but a plain cargo invocation does not.

**Workaround:** `RUSTFLAGS="-L $PWD/.xdo-shim" cargo build --release`
**Fix:** `sudo apt install libxdo-dev`
