# The keyboard leak — root cause and fix

**Fixed 2026‑07‑31**, confirmed on hardware by the owner.

## Symptom

Crossing to the peer moved the mouse but sometimes left the keyboard on Windows. Roughly
1 crossing in 10–20 in casual use; far more often — around 80% — when the mouse-rs panel
was the focused window. The keystrokes were not lost: they went into whichever window had
focus on Windows, which was usually the mouse-rs panel itself. The owner could watch
themselves typing into the control panel while the mouse was on Kali.

## Root cause

**Windows does not deliver `WH_KEYBOARD_LL` callbacks to a process while that same process
owns the foreground window.**

Mouse motion is captured through Raw Input on a separate thread, which is unaffected — so
the mouse kept crossing while the keyboard did not. That asymmetry is the signature.

Measured evidence:

| Foreground window at crossing | `hook_calls` | Outcome |
|---|---|---|
| Another process (terminal) | 14, 20, 56 | keyboard follows |
| **mouse-rs panel (our own)** | **0** | keyboard stays on Windows |

On one failing crossing, Raw Input counted **93 real keystrokes** for which the keyboard
hook was called **zero** times, while `mouse_calls=139` on the same thread proved the
thread was alive and still being called for the mouse. So the failure was specific to the
keyboard hook, not to the thread or the process.

## Fix

When a crossing begins and one of our own windows holds the foreground, hand the foreground
to the window the user was last on, and hand it back when control returns
(`crates/mouse-input/src/windows.rs`, the pump loop). This removes the condition under
which Windows withholds the callbacks, rather than working around its effect. It is also
the behaviour a user expects: once control moves to the other machine, the panel has no
reason to hold focus.

Logged on every crossing that needs it:

```
INFO our own window had focus at the crossing; handed foreground to another process
     so the keyboard hook is delivered  to='...' pid=...
WARN our own window had focus at the crossing and foreground could NOT be handed off
```

## Why four earlier attempts failed

Each fixed a plausible cause rather than a proven one. **The hook was never broken** —
`SetWindowsHookEx` succeeded every time, the handle was valid, the message pump was healthy,
no stall was ever detected. It simply was not being called.

1. **Time-critical thread priority** — assumed the pump was being starved. It wasn't.
2. **Moving logging off the pump thread** — assumed file I/O was stalling it. Correct in
   principle (logging from the hook thread could cause the leak it was recording) and worth
   keeping, but not the cause.
3. **Re-arming the hook on a detected stall** — assumed `LowLevelHooksTimeout` had removed
   it. No stall was ever detected.
4. **Re-arming on every crossing** — fired 24/24 crossings and changed nothing, which is
   what finally disproved the removal theory: a hook installed 7 seconds earlier received
   zero callbacks while 93 keystrokes flowed past it.

## The instrumentation that actually solved it

The bug was invisible for four rounds because every counter measured the wrong thing.
What broke it open, in order:

1. **Client-side `moves=` / `keys=` per crossing** (`mouse-engine/src/client.rs`) — proved
   the keystrokes never left Windows, exonerating the network, the engine and the peer.
2. **`hook_calls`** — separated "the hook ran and let the key past" from "the hook was
   never called". These are opposite bugs and were previously both silent.
3. **`raw_keys`, from Raw Input** — the independent witness. Without it, `hook_calls=0`
   could not be told apart from "the user typed nothing", and that ambiguity produced one
   badly inflated leak-rate estimate.
4. **`mouse_calls`** — proved the thread was alive, killing the thread-wide theory.

**Lesson worth keeping:** a counter that cannot distinguish "the thing didn't happen" from
"nobody tried" is not evidence. Every one of these fields exists because an earlier round
produced a confident conclusion that later turned out to be unsupported.

Related: an earlier build's "clean" 18-crossing run was reported as passing, but it predated
`hook_calls` — a dead hook would have logged as healthy. Do not trust a clean run from an
instrument that cannot observe the failure.
