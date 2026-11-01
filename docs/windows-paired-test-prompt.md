This is a **paired recording**, not a hunt. You are the recorder on the Windows side; the
owner performs every crossing by hand. Kali is recording simultaneously and the two logs
will be lined up by UTC timestamp afterwards.

**You cannot drive this yourself and must not try.** Injected cursor motion does not trigger
an edge crossing — edge detection reads Raw Input from the physical device — so synthetic
input produces nothing. Wait for the owner.

## The question this run answers

When control moves to Kali, the mouse always follows. Sometimes the keyboard does too, and
sometimes it stays on Windows. The owner can reproduce **both** outcomes on demand:

- **Panel in the FOREGROUND** when crossing → keyboard usually stays on Windows (the bug)
- **Panel sent to the BACKGROUND** first → keyboard follows to Kali (works)

Two arms, one variable. That is what makes this run worth more than everything before it.

## RULE — no synthetic input, at any point, for any reason

**Do not send clicks or keystrokes. Do not use UI automation, UIA Invoke, SendInput,
`SetCursorPos`, or AutoHotkey-style helpers. Do not try to open the tray, the taskbar, or
any menu.**

This is not caution, it is a correctness requirement. When control is on the peer, mouse-rs
captures local mouse input and **forwards it to Kali**. A previous attempt to click the tray
sent real clicks onto the owner's Kali desktop and never reached Windows at all, which is
why it could not complete this step. Every action below is a shell command; none of them
need the pointer.

Nothing in this run requires clicking. If you find yourself wanting to click, stop and say
what you are stuck on instead.

## Step 0 — install the build (entirely command-line)

Stop the running instance **by process, not by tray**:

```powershell
Get-Process mouse-rs -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
Get-Process mouse-rs -ErrorAction SilentlyContinue   # expect no output
```

Killing it is safe: Windows removes the low-level hooks when the process exits, so local
input returns to normal even if it was suppressing at the time.

Download and verify:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
(Get-Item $HOME\mouse-rs\mouse-rs.exe).Length
(Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256).Hash
```

Must be exactly size `13546496` bytes, SHA256
`A2F17B9C178809F27C84D19F7986A6389DC3262D85423C1CD36B64BED70B3BA5`.
If either differs, stop and say so — do not launch it.

Rotate the log so this run stands alone, then relaunch:

```powershell
$log = "$env:USERPROFILE\.mouse_share\mouse-rs.log"
if (Test-Path $log) { Move-Item $log "$log.prepaired" -Force }
Start-Process $HOME\mouse-rs\mouse-rs.exe
Start-Sleep -Seconds 6
Get-Content $log -Tail 20
```

Confirm from that output that the engine started and `kali-desktop` connected. Then report
the byte size, the hash, and the connect line, and say **"ready"**.

## Step 0b — the fix under test, so you know what to look for

This build addresses a root cause that is now measured rather than theorised: **Windows does
not deliver `WH_KEYBOARD_LL` callbacks to a process while that same process owns the
foreground window.** Evidence — every crossing made with another process focused had a live
hook (14, 20, 56 callbacks); crossings made with the mouse-rs panel focused had
`hook_calls=0` while Raw Input counted up to 93 real keystrokes, and `mouse_calls=139`
proved the same thread was still being called for the mouse.

The fix: when a crossing begins and one of our own windows holds the foreground, mouse-rs
hands the foreground to the window the user was last on, and hands it back on return. Look
for this line, which should appear on every panel-focused crossing:

```
INFO our own window had focus at the crossing; handed foreground to another process
     so the keyboard hook is delivered  to='...' pid=...
```

If instead you see `foreground could NOT be handed off`, report it prominently — that is the
fix failing rather than the bug recurring, and it means something refused
`SetForegroundWindow`.

## Step 1 — what the owner will do

Between 2 and 5 crossings, mixing the two arms, in whatever order they choose. On each one
they will cross to Kali, type for a few seconds, and come back. They will tell you which arm
each crossing was — or you can infer it from the `focus=` field in the log.

Your job while this happens: **nothing but read the log file.** No screenshots, no synthetic
input, no window manipulation — beyond the rule above, focusing or moving any window would
change the very variable under test. `Get-Content -Tail` is the only tool you need.

## Step 2 — what to extract, per crossing

Every crossing produces one `keyboard grabbed` line and one `keyboard released` line. Pull
these fields from the released line:

| Field | Meaning |
|---|---|
| `raw_keys` | keystrokes Raw Input saw — **the ground truth for "did they type"** |
| `hook_calls` | times the OS called our keyboard hook |
| `mouse_calls` | times the OS called our mouse hook — **the new field, and the point of this run** |
| `swallowed` | keys we captured and sent to Kali |
| `remote_ms` | how long control was on Kali |
| `focus=` | which window was foreground at the crossing |

Build a table with one row per crossing plus an **arm** column (panel foreground / panel
background).

## Step 3 — the comparison that matters

`mouse_calls` is the field this run exists for. Both hooks are installed on the same thread
and re-armed together, so on a crossing where `hook_calls=0`:

| Reading | What it means |
|---|---|
| `mouse_calls > 0`, `hook_calls = 0` | The thread is alive and being called — the failure is **specific to the keyboard hook** |
| `mouse_calls = 0`, `hook_calls = 0` | **Nothing on that thread is being called** — the failure is thread-wide |

These need opposite fixes, which is the whole reason for this run. State which one the data
shows, plainly, and don't hedge it.

Also compare the arms directly: in the working arm, does `hook_calls` roughly track
`raw_keys` (about 2 hook calls per key, one down one up)? A big shortfall would mean keys
are being lost even when the hook is alive.

## Step 4 — report

**Do not paste the log into chat.** Upload it:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8001/paired.log -Method PUT `
  -InFile $env:USERPROFILE\.mouse_share\mouse-rs.log
Invoke-WebRequest -Uri http://192.168.137.2:8001/paired-report.md -Method PUT `
  -InFile $HOME\mouse-rs\paired-report.md
```

Then **10 lines maximum** in chat: the per-crossing table in compact form, and the
`mouse_calls` verdict.

Report exactly what the counters say. If the two arms show no difference, say that — a
negative result here is just as valuable, and a manufactured pattern would send the fix in
the wrong direction.
