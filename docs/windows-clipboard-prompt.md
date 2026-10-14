Install the current mouse-rs build, **run it as administrator**, confirm it really is
elevated, and then stop and wait.

**This build undoes a fix that made things worse.** The build installed earlier today
broke every modifier chord on this machine: Ctrl stopped crossing to Kali at all, so
Ctrl+Space never reached it, and Alt+Tab needed about five attempts. If that is what you
have been seeing, it was this. Installing this build is the fix.

What went wrong, because it is worth stating plainly. The earlier build tried to detect a
modifier whose release had been lost, by comparing what it had told Kali was held against
what `GetAsyncKeyState` reported. That rested on the premise that the async key state sees
the real keyboard regardless of hooks. **It does not — not for keys mouse-rs itself
suppresses.** While control is on Kali this program swallows every keystroke so it does
not also land on Windows, and a swallowed key is never dispatched, so it never updates the
async key state either. The result: for every modifier the owner held, "we told Kali it is
down" and "the OS says it is up" was the *normal* state, and the code read that as a lost
release and cancelled the key. From Kali's own log:

```
14:47:11.296  CtrlL  pressed=true    <- Ctrl pressed
14:47:11.304  CtrlL  pressed=false   <- the faulty fix, 8 ms later
14:47:11.656  Space  pressed=true    <- Space arrives with no Ctrl held
14:47:11.904  CtrlL  pressed=false   <- the real release, far too late
```

It fired 111 times in five minutes. This build removes it entirely.

The premise is also settled now, by this machine's own counters rather than by argument:
per crossing, `swallowed=414` and `hook_calls=768` while **`raw_keys=0`**. Raw Input is a
separate delivery path that mouse-rs already registers as an independent witness, and it
saw none of those 414 suppressed keys. So nothing on this side can observe a key we
swallow, and recovery has to live on the machine receiving the keys — which is where it
now does, and only there.

**The original Alt+Tab stickiness is therefore back to being merely mitigated**, by Kali
releasing a modifier that has been held through five seconds of total silence. That is
confirmed working in the field. It is not a cure, and the real cause is now **unknown
again** — the earlier explanation (a higher-integrity window killing our hook mid-chord)
is contradicted by those same counters: no crossing in the log ever had `hook_calls=0`.

Nothing else in this build changes behaviour on Windows. The `uinput` backend and the
login-screen service are Linux-only, and **the login-screen work is on hold — there is
nothing to set up here for it and nothing to report about it.**

## Step 1 — stop the running instance, by process

```powershell
Get-Process mouse-rs -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
Get-Process mouse-rs -ErrorAction SilentlyContinue   # expect no output
```

Safe: Windows removes the low-level hooks when the process exits, so local input returns
to normal even if it was suppressing at that moment.

## Step 2 — download and verify

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
(Get-Item $HOME\mouse-rs\mouse-rs.exe).Length
(Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256).Hash
```

Must be exactly size `12280832` bytes, SHA256
`A5058D67E06E2CA9F7E7E777E1E954FDDA1B4294EFEC6861C7C2711724A64316`.
If either differs, stop and say so — do not launch it.

## Step 3 — confirm the config

Nothing needs changing this round. Just confirm, and **do not print `shared_secret` or
paste the config**:

```powershell
$c = Get-Content $env:USERPROFILE\.mouse_share\config.json -Raw | ConvertFrom-Json
$c | Select-Object name, screen_width, screen_height, autoconnect, server_host, listen_port
$c.log_upload_url
$c.clipboard_auto_transfer_max
$c.clipboard_cache_budget
```

Expected: `name = laptop`, `1920x1080`, `http://192.168.137.2:8001`,
`clipboard_auto_transfer_max = 16777216`, `clipboard_cache_budget = 17179869184`.

If `clipboard_cache_budget` is missing or still `2147483648`, set it — a 2 GB cache is
smaller than a single file the owner routinely moves, which leaves it permanently over
budget:

```powershell
$p = "$env:USERPROFILE\.mouse_share\config.json"
$c = Get-Content $p -Raw | ConvertFrom-Json
$c | Add-Member -NotePropertyName clipboard_cache_budget -NotePropertyValue 17179869184 -Force
$c | ConvertTo-Json -Depth 10 | Set-Content $p -Encoding UTF8
(Get-Content $p -Raw | ConvertFrom-Json).clipboard_cache_budget
```

## Step 4 — launch it **elevated**, and this round it genuinely matters

```powershell
Start-Process -FilePath "$HOME\mouse-rs\mouse-rs.exe" -Verb RunAs
```

`-Verb RunAs` asks Windows for elevation, so **a UAC prompt appears on screen**. Only the
owner can answer it. If it is still waiting, say so and stop — do not try to click it.

Why it still matters: every one of the 24 hook-blocked events in this machine's log
recorded `our_level="0x2000 (normal)"` — **mouse-rs has never actually run elevated**,
though the last two rounds asked for it. 23 of the 24 blocking windows were
`0x3000 (elevated (administrator))`, ordinary admin rather than SYSTEM, so an elevated
mouse-rs sits at the same level and keeps its hook.

Treat this as worth doing on its own merits, **not** as a cure for Alt+Tab. Those
warnings all concern the start of a crossing, and the counters above show the hook was
alive throughout every crossing in the log — so elevation is a lead, not the answer.

If the owner declines, report that plainly rather than falling back to a normal launch.

Then confirm it really is elevated. Do not treat the launch succeeding as proof:

```powershell
Start-Sleep -Seconds 10
$p = Get-Process mouse-rs -ErrorAction SilentlyContinue
if (-not $p) { "NOT RUNNING" } else {
  # A non-elevated shell cannot read an elevated process's path. "Access is denied" here
  # is the confirmation, not a failure.
  try { $null = $p.Path; "readable -> NOT elevated" } catch { "access denied -> elevated, as intended" }
}
```

## Step 5 — read back the startup lines

```powershell
Select-String -Path $env:USERPROFILE\.mouse_share\mouse-rs.log `
  -Pattern 'shipping this machine|bulk lane|connected|CROSSING WILL NOT WORK|believed it held' |
  Select-Object -Last 15 | ForEach-Object { $_.Line }
```

Confirm and report:

1. **shipping this machine's log for diagnosis** — the upload is working;
2. the peer connected;
3. anything about the **bulk lane** — best-effort, so a failure means file transfer is
   unavailable while mouse and keyboard are unaffected;
4. any **CROSSING WILL NOT WORK** line, **with the two integrity levels it prints**. If
   `our_level` is still `0x2000` after Step 4, elevation did not take and that is the
   single most important thing to say.

## Step 6 — report, then stop

Five lines maximum in chat: the size, whether the hash matched, whether it is elevated,
the connect line, and anything about the bulk lane or a CROSSING warning. **Do not paste
the log** — it uploads automatically, which is the entire point.

Then **stay idle**. Do not copy files, do not open the panel, do not try Alt+Tab, do not
launch or touch any game. The owner drives every crossing, copy and paste by hand.

## What the owner will check, by hand, after you stop

Do not attempt any of this yourself — it needs real key presses, and synthetic ones would
land on Kali (see the rule below). It is here so you know what the round is for and do not
get in the way:

1. Cross to Kali and press **Ctrl+Space**. It should reach Kali and trigger what the owner
   has bound to it. On the previous build nothing arrived but a bare Space.
2. Cross to Kali and **Alt+Tab** once. It should switch on the first press, not the fifth.
3. Hold **Ctrl** or **Shift** on its own for a few seconds while typing — the modifier
   must stay held for as long as it is physically down.

If any of those still fail, that is a real finding and the log will show it: the owner's
machine uploads automatically, so **nothing needs pasting.**

## RULE — no synthetic input, at any point, for any reason

**Do not send clicks or keystrokes. Do not use UI automation, UIA Invoke, `SendInput`,
`SetCursorPos`, or AutoHotkey-style helpers. Do not click the tray, the taskbar, or any
menu.**

This is a correctness requirement, not caution. When control is on the peer, mouse-rs
captures local mouse input and **forwards it to Kali**. A previous attempt to click the
tray sent real clicks onto the owner's Kali desktop and never reached Windows at all.
Every step above is a shell command; none need the pointer. If you want to click, stop and
say what you are stuck on.

**The one exception is the UAC prompt in Step 4.** It is a system dialog, it will appear,
and only the owner can answer it. Do not try to dismiss it — say it is waiting, and stop.

## What the log lines mean, if you see them

You do not need to act on any of these; they are here so you can report them accurately.

- `a modifier was physically up but the peer still believed it held — our keyboard hook
  never saw the release. Sent it ourselves.` — **this line must not appear at all.** It
  belonged to the withdrawn fix. If you see it, the old build is still running: the
  install did not take, and you should say so rather than continue.
- `CROSSING WILL NOT WORK while this window is in front` — the focused window outranks us,
  so the OS is not delivering our input hooks. **Report the window name and both levels.**
  If this appears while mouse-rs is elevated, say so loudly: it would mean something beyond
  elevation is involved, most likely an anti-cheat driver.
- `DECISION: WAITING FOR YOU` — expected for anything over 16 MB. Only the description
  crossed; the bytes wait for a paste. Not a problem.
- `paste asked for it — fetching now` — a paste started a transfer. Should appear **once**
  per transfer, not repeatedly.
- `nothing to request and nothing in flight` — a genuine paste failure. Report it.
- `over the disk budget and cannot get under it` — should be rare now, and at most once a
  minute. Repeated appearances mean Step 3's budget did not take.
- `could not ask Explorer to move rather than copy` — the drop-effect hint was refused.
  Not fatal: pasting still works, it just copies. Worth reporting.
