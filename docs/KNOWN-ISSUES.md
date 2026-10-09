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
**awaiting detail** · raised 2026-10-09

The owner reported a change or fault concerning input sharing on the Windows side and
said they would describe it later. Not enough detail to act on, and **deliberately not
guessed at** — the words could mean the login-screen feature wanting a Windows
equivalent, or a distinct bug seen while sharing.

> "When I share keyboard and mouse, I want to change that to the Windows side … this is a
> bug … will let you know."

**Next step:** ask what happens, on which machine, and what should happen instead.

---

## Built but never proven on hardware

### B2 — login-screen sharing has never been run for real
**unverified** · built 2026-10-08 (`61b0d72`, `7b143a6`) · tracked as R21

The uinput backend, the greeter service and the panel toggle are all written and tested,
but nothing has run at an actual SDDM login prompt. Verification needs root and a logout,
so only the owner can do it.

`mouse-rs greeter-selftest` exists precisely because the real thing is only observable
where there is no terminal to read an error from — run that first.

**Risk if wrong:** low. The service is off until installed, and failure means the login
screen behaves as it does today rather than breaking further.

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

### B11 — the served Windows exe is a symlink into the build tree
**open** · found 2026-10-08

`~/mouse-serve/mouse-rs.exe` is a symlink to
`target/x86_64-pc-windows-gnu/release/mouse-rs.exe`, not a copy. Any Windows rebuild
therefore changes what the laptop would download while the prompt still states the old
hash — the laptop fails the checksum step, and it reads as a corrupt download rather than
"someone rebuilt it".

Shared code (`config.rs`, `run.rs`) has already diverged from the served build, so this is
live, not theoretical.

**Fix:** have `scripts/serve-windows.sh` copy the binary instead of linking it.

### B12 — a bare `cargo build` fails on this machine
**open**

`libxdo-dev` is not installed, so the `libxdo.so` dev symlink is missing and linking fails
with `unable to find library -lxdo`. The repo carries `.xdo-shim/libxdo.so`;
`scripts/serve-windows.sh` exports it, but a plain cargo invocation does not.

**Workaround:** `RUSTFLAGS="-L $PWD/.xdo-shim" cargo build --release`
**Fix:** `sudo apt install libxdo-dev`
