This is a **single-bug hunt** on **mouse-rs**, not a feature pass. One question only:

> **When the keyboard fails to follow the mouse to Kali, where does the keystroke actually
> go, and why?**

Work in PowerShell, non-elevated. Don't modify any source — you have none here. Don't try
to fix anything; the entire job is to *reproduce and record*.

Same wiring as before: this laptop = **SERVER** (`192.168.137.1`), Kali = **CLIENT**
(`192.168.137.2`), port 24800, direct cable. Kali is running its panel and is connected.

## The bug, and the clue that makes it reproducible

In real use the mouse crosses to Kali but the typing sometimes stays on Windows — roughly
1 crossing in 10–20. The owner found the trigger:

> **If the mouse-rs panel window is open when you cross, the keyboard is not shared.**

That is what you are going to try to reproduce deliberately.

**Round 3 note — the previous build's four fixes did NOT fix it.** The owner reproduced the
leak again. That negative result was decisive, because it eliminated everything but one
explanation:

> Raw mouse motion is gated on the same `suppress` flag the keyboard hook now reads live.
> So the mouse arriving on Kali *proves* suppression was engaged — and if a keystroke still
> reached Windows while suppression was engaged, **the keyboard hook procedure did not run
> at all.**

The cause: on Windows 7 and later, a low-level hook that once exceeds
`LowLevelHooksTimeout` is **silently removed by the OS** — not skipped for that one
keystroke, removed permanently. Nothing reports it, the handle stays non-null, the pump
afterwards looks perfectly healthy, and there is no API to ask whether a hook is still
alive. One unlucky 300 ms hiccup disables keyboard sharing for the rest of the run, which
is exactly the reported shape: intermittent, tied to the panel being open, and dead for a
whole crossing while the mouse keeps working (motion comes from Raw Input on a different
thread).

**The fix in this build: both hooks are re-installed unconditionally at the start of every
crossing, and once a second while control is on the peer.** The previous build only re-armed
while it could *observe* a stall in progress — far too late, since the pump is perfectly
healthy by then. A fresh hook per crossing is the property that actually matters.

The earlier fixes are still in and still worth having: the hook reads suppression live (no
~8 ms race), its thread runs at time-critical priority (removal is rarer), and it never
writes to the log itself (the logging could stall the very thread it was recording).

Watch for this new line, which should appear **once per crossing**:

```
INFO  keyboard + mouse hooks re-armed for this crossing
```

If it is missing on a crossing, the re-arm isn't running and that is the first thing to
report. If it says `could not re-install a low-level hook`, report that immediately —
that is the failure mode with no fallback.

So a clean run is a real result here, and so is a leak — both are informative.

## Step 0 — install the instrumented build

Stop the running instance first (**tray → Quit**), then:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256
```

Must match exactly — size `12264448` bytes, SHA256
`2B0FA75C7180CC44A5DA01112F4A2E30905917EB8185B0DE4091F12927DC6F36`.
If it doesn't match, stop and tell me.

**Start the log clean** so the trial numbering below lines up with the file — move the old
one aside rather than deleting it:

```powershell
$log = "$env:USERPROFILE\.mouse_share\mouse-rs.log"
if (Test-Path $log) { Move-Item $log "$log.prev" -Force }
```

Then launch the panel and confirm Kali connects.

## Step 1 — arm A: cross with the panel focused (10 trials)

This is the arm expected to fail. For each trial 1–10:

1. **Click on the mouse-rs panel window** so it is unmistakably the foreground window.
   Click a neutral area — the header or a tab label — **not** a text field, and don't change
   any setting.
2. Move the mouse across the configured edge to Kali.
3. Type the marker `TRIALA01` (then `TRIALA02`, and so on — one distinct marker per trial),
   then a few words of ordinary text.
4. Return control to Windows.
5. Record for that trial: **did the typing go to Kali, or did it stay on this machine?**

You will be able to tell on this side: if the keystrokes leak, they land in whatever window
is focused *here* — the panel. Watch for the panel reacting to your typing (a focus ring
moving, a tab switching, the window responding at all). If it does, that trial leaked.

**Do not stop early on a success or a failure.** All ten trials, even if the first three all
leak or all pass — the ratio is part of the evidence.

## Step 2 — arm B: cross with Notepad focused (10 trials)

Same procedure, markers `TRIALB01`…`TRIALB10`, except **open Notepad, click into it** so
*it* is the foreground window before each crossing. This is the control arm.

Leaked keystrokes land visibly in the Notepad buffer — this arm is unambiguous to observe.
Leave the Notepad contents alone; you'll upload them.

If arm B never leaks and arm A leaks repeatedly, the focus clue is confirmed and that alone
is a valuable result.

## Step 3 — read the log

The instrumentation writes these signatures. They are mutually exclusive diagnoses —
which one appears *is* the answer, so quote them exactly rather than paraphrasing.

```powershell
$log = "$env:USERPROFILE\.mouse_share\mouse-rs.log"
Select-String -Path $log -Pattern 'KEYBOARD LEAK|pump stalled|keyboard grabbed|keyboard released'
```

| Line you see | What it means |
|---|---|
| `capture pump stalled … stalled_ms=NNN rearmed=true` | The thread was starved anyway despite the new priority, and the hook was re-installed as a precaution. **Report the `stalled_ms` values** — they size the problem. |
| `capture pump stalled … rearmed=false` | Starved *and* the hook could not be re-installed. Worse; say so prominently. |
| `KEYBOARD LEAK … reason=no_wire_mapping` | We swallowed the key here but had no way to represent it on the wire — it reached **neither** machine. Include the `vk=0x..` value; that names the specific key. |
| **Nothing at all, but the trial still leaked** | The keys left Windows correctly and the problem is on Kali's side. Say so explicitly — that flips the whole investigation. |

(The old `reason=suppress_requested_but_not_grabbed` signature is gone: that race is now
fixed by construction, so the line can no longer occur. If you somehow see it, you are
running the wrong binary — re-check the hash.)

Also report, for every crossing, the `keyboard grabbed (control moved to the peer)` line and
its `focus=` field. In arm A it should say **`(THIS IS OUR OWN PANEL)`**; in arm B it should
name Notepad. If arm A's crossings *don't* say that, the trigger isn't focus and you should
tell me — that would be the most important sentence in the report.

And each `keyboard released` line carries `swallowed=` / `leaked=` / `remote_ms=` for the
crossing that just ended. Tabulate those per trial.

## Step 4 — the timing question

For any trial that leaked, note whether the loss happened **at the moment of crossing**
(first keystrokes never arrive) or **after typing on Kali for a while** (it worked, then
stopped mid-session). These are different bugs with different fixes, and the log alone
can't distinguish them — only you can.

## Steps 5 and 6 — SKIP, both already settled

**Clipboard images: confirmed working** in both directions by the owner. Don't retest, and
don't spend time on the clipboard at all unless something about it visibly breaks — in which
case say so, because it would be a regression against a working feature.

**Always-on-top: already answered** — it does not survive a fresh relaunch, so it was the
old screenshot harness pinning `HWND_TOPMOST`, not our code. Closed.

Keep a brief eye out for regressions in both, but neither needs a deliberate test.

## Step 7 — report

**Do not paste the report or the log into chat.** Upload both:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8001/leak-hunt-report.md -Method PUT -InFile $HOME\mouse-rs\leak-hunt-report.md
Invoke-WebRequest -Uri http://192.168.137.2:8001/leak-hunt.log -Method PUT -InFile $env:USERPROFILE\.mouse_share\mouse-rs.log
```

Also upload the Notepad buffer from arm B — it is direct physical evidence of which
keystrokes leaked:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8001/leak-hunt-notepad.txt -Method PUT -InFile <path you saved it to>
```

The report must contain a **per-trial table**: trial, arm, leaked yes/no, the `focus=` value,
and any warning lines from that crossing. Twenty rows. That table is the deliverable — the
prose around it matters much less.

Then give me **10 lines maximum in chat**: the leak rate in each arm, which of the four
signatures appeared, and the filenames you uploaded.

Report plainly if nothing reproduced — "20 trials, 0 leaks" is a real and useful result, and
far more useful than a leak you weren't sure about. Don't soften a null result and don't
manufacture a finding to have something to report.
