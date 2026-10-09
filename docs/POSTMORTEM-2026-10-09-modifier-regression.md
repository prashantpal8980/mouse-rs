# Post-mortem — the day Ctrl stopped working

**2026-10-09.** A fix for an intermittent stuck-Alt bug (B13) made every modifier chord
unusable for about six hours. Ctrl never reached the peer at all, so Ctrl+Space was dead;
Alt+Tab worked roughly one press in five. Recovered by rolling the laptop back to the
4 Sep build.

This is written because the bug was the *small* part. Five separate things had to go wrong
for it to reach the owner's hardware, and each one is independently worth fixing.

---

## Timeline

| Time | Event |
|---|---|
| 18:18 | B13 (stuck Alt+Tab) diagnosed from shipped logs. Cause recorded as a UIPI hook blackout. |
| 18:43 | Receiving-side mitigation: release a modifier held through 5 s of silence. **Sound; still in place.** |
| 19:05 | Sending-side "cure" committed and shipped to the laptop as `22C7FC47…`. |
| ~19:3x | Owner installs it. |
| 20:11 | Owner: *"unable to use combination ctrl + space"*, then *"not sharing ctrl now… alt+tab only work when i click like 5 time"*. |
| 20:33 | Cause found, fix withdrawn, guards added (`d907f27`). |
| 21:56 | Owner asks to roll back instead. 4 Sep build rebuilt and served. |
| 22:2x | Three failed install attempts (see failure 5). |
| 22:4x | Working again. |

---

## Failure 1 — the fix rested on a premise I never measured

The code compared what we had told the peer was held against `GetAsyncKeyState`, and
released anything the OS reported as up. The premise, written in its doc comment:

> `GetAsyncKeyState` reads the real keyboard regardless of hooks, so it can see the
> release the hook missed.

**That is false for keys this program suppresses itself.** While control is on the peer,
our `WH_KEYBOARD_LL` hook returns 1 for every key, so the event is discarded and never
dispatched — and therefore never reaches the async key state either. So for *every*
modifier the user holds, "we said down, the OS says up" is the **normal** state. The
reconcile read that as a lost key-up and cancelled the key ~8 ms after the press:

```
14:47:11.296  CtrlL  pressed=true    <- user presses Ctrl
14:47:11.304  CtrlL  pressed=false   <- the reconcile
14:47:11.656  Space  pressed=true    <- Space arrives bare
14:47:11.904  CtrlL  pressed=false   <- the real release, far too late
```

111 firings in five minutes. Alt+Tab survived only when Tab landed inside the few
milliseconds before the sweep — hence "5 times".

**Why it got through:** I wrote the premise down in a comment and treated having written
it as having established it. It was never checked against anything.

**What would have caught it:** a counter already in the same file. `raw_keys` reports
keystrokes seen by Raw Input, a wholly separate delivery path, registered here as an
independent witness. Per crossing: `swallowed=414`, `hook_calls=768`, **`raw_keys=0`**.
Raw Input saw *none* of the 414 suppressed keys — so nothing on the sending side can
observe a key we swallow. One grep would have killed the design before it was written.

---

## Failure 2 — the diagnosis under the fix was also wrong

B13's recorded cause was: a higher-integrity window takes focus between Alt-down and
Alt-up, Windows stops calling our hook, the release is never seen.

The evidence offered for it was 24 `CROSSING WILL NOT WORK` warnings naming elevated
windows. But those warnings are emitted at **crossing start**, about whether a crossing
can begin — they say nothing about a gap mid-chord. I used evidence about one moment as
evidence about a different one.

The same counters contradict the story outright:

* **no crossing in the log ever had `hook_calls=0`**;
* the purpose-built "Raw Input saw keystrokes the hook never got" diagnostic has **never
  fired, once**.

**B13's real cause is therefore still unknown.** The fault is real — 46 crossings ended
with Alt still logically held — but the mechanism is not established. The bug list now
says so instead of asserting a mechanism.

**Why it got through:** the hypothesis was plausible, fitted the symptom, and explained
the intermittency. I never looked for evidence that would *disconfirm* it, though the
counters that do were already being logged.

---

## Failure 3 — the tests could not have caught it, in two different ways

**They never ran.** They lived in `crates/mouse-input/src/windows.rs`, which only compiles
for the Windows target. On this Linux build host they compile under
`cargo clippy --target x86_64-pc-windows-gnu` and **never execute**. "Tests pass" was true
and meaningless — the suite that passed did not include them.

**They tested the wrong thing.** They pinned `stuck_mask(sent_down, physically_down)`, a
pure function encoding the false premise. A test of a false premise passes. There was no
test anywhere asserting the user-visible behaviour: *press Ctrl, press Space, Ctrl must
still be held.*

**Now in place:**

* `injector::a_chord_held_while_the_user_keeps_typing_is_never_broken` — runs on the Linux
  host, holds Ctrl across several sweep ticks with input flowing, and fails if Ctrl is
  released before Space. **Verified to fail against the faulty behaviour before it was
  kept**, so it is known to bite rather than assumed to.
* `windows.rs::the_modifier_table_is_only_a_crossing_start_snapshot` — fails if the
  reconcile reappears, and carries the measurement in its docs.

---

## Failure 4 — a continuously-active fix for an occasional fault

B13 was intermittent and self-recovering: the 5 s sweep cleared it, and any crossing back
cleared it via `ReleaseAll`. Annoying, not blocking.

The fix ran **every 120 ms, always, on every modifier**. Its failure mode was total and
constant. I never asked what happens if it is wrong — and the answer was "the keyboard
stops working", which is far worse than the symptom being treated.

**The rule that follows:** the more often a correction runs, the better its premise has to
be. For an intermittent, self-recovering fault, prefer the recoverable fix on the side
that holds the state — which is where it now lives, and only there.

---

## Failure 5 — the recovery was worse than it needed to be

Three install attempts failed, each for a separate avoidable reason.

**a. The install overwrote the only working binary before verifying it.** The original
steps downloaded straight over `mouse-rs.exe` and checked the hash afterwards. A bad
download would have destroyed the one build that worked — which is precisely why this
rollback needed a download instead of a local restore. *Now:* download to `.new`, verify,
then replace; the replaced build is kept as `mouse-rs.exe.bak`.

**b. The script reported success after failing.** `Move-Item` errored and the next line
still printed `installed`. There was no error handling and no post-condition check.
*Now:* `$ErrorActionPreference = 'Stop'`, the process is proven stopped before anything is
touched, and the installed file is re-hashed before success is claimed.

**c. It assumed an ordinary shell could replace a running elevated binary.** mouse-rs was
running elevated, so `Stop-Process` returned *Access is denied* and the old exe kept its
own file locked. *Now:* elevation is checked up front and the script stops with
instructions before touching anything.

**d. Multi-line paste into the PowerShell console silently drops code.** A blank line ends
the `>>` continuation prompt, so everything after it runs detached — which is also how the
false `installed` surfaced. I handed over a block of that shape twice. *Now:* the steps
live in `scripts/windows-rollback.ps1`, served and run by one pasted line.

**e. The obvious one-liner was broken.** `iex (iwr …).Content` fails because the server
sends `.ps1` as `application/octet-stream`, so `.Content` is a byte array and `iex`
rejects it. Found by *running* it against the live server under `pwsh` on this machine —
not by reading it. Fixed with `Get-Content -Raw`, which sidesteps the execution policy
too.

---

## The common cause

Every one of these is the same mistake in a different costume: **reasoning substituted for
measurement, in an environment where the code cannot be executed.**

Windows code cannot run here. The owner's hardware is the only place it runs, and they
test by hand. That makes confident-sounding prose the cheapest thing to produce and the
least reliable thing to ship. Four of the five failures would have been caught by running
or grepping something that was already available.

---

## What changed

1. **State the premise, name the measurement.** For anything touching a platform API,
   write down the premise *and* the evidence that confirms it, before writing the fix. If
   no evidence exists, ship a diagnostic first and a fix second.
2. **Host-executable behaviour test, or it is not tested.** Every input-path fix needs a
   test that runs on this Linux host and asserts user-visible behaviour. Windows-target
   tests are documentation; they must be labelled as not executing here.
3. **Confirm a new guard fails before keeping it.** Done for the chord test.
4. **Match the fix's blast radius to the fault.** Nothing that runs continuously to
   correct an occasional, self-recovering fault.
5. **Do not say "fixed" before the hardware says so.** The logs upload automatically; read
   them after one crossing before using the word.
6. **Never overwrite the only working artifact before verifying its replacement**, and
   always check post-conditions rather than assuming the previous line worked.

---

## What went right, and is worth keeping

* **Log shipping made the diagnosis conclusive in minutes.** Both machines upload to
  `~/mouse-reports`; the smoking-gun trace came from the receiver's own log, with
  millisecond timestamps. Nothing had to be reproduced or pasted.
* **A diagnostic added for an unrelated question settled this one.** `raw_keys` was added
  to tell "the hook was dead" apart from "nothing was typed". It is what proved no API can
  see a suppressed key. Cheap counters on a hot path keep paying off.
* **The standing bug list preserved B13's history**, so the wrong diagnosis could be
  corrected in place rather than silently forgotten.
* **The owner's report was precise** — *"ctrl is not sharing"* and *"alt+tab only works
  when I click like 5 times"* were two symptoms of one cause, and naming both is what made
  the shared mechanism obvious immediately.
