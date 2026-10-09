Install the current mouse-rs build, **run it as administrator**, confirm it really is
elevated, and then stop and wait.

Windows has been held back for several rounds while work went on on the Linux side. This
build brings it level. The headline is a fix for something the owner has been living with:

**Alt+Tab would sometimes stick.** The switcher opened, changed windows, and then refused
to commit — the window stayed stuck until clicked with the mouse, and clearing it took a
few taps of Ctrl. Diagnosed from the shipped logs: it is a **stuck Alt**, because holding
Alt is what keeps the switcher open and it commits on release. The release was going
missing.

The cause is this machine, and it is the same rule behind the Genshin Impact problem.
Windows does not call a low-level keyboard hook belonging to a lower-integrity process
while a higher-integrity window has the foreground. When one takes focus **in the gap
between Alt-down and Alt-up**, the press is forwarded and the release is never seen by us
at all — so nothing ever tells the other machine that Alt came back up.

It is intermittent because of *which* windows do it. Counted in this machine's own log:

| Window in front | Times | What it is |
|---|---|---|
| `OmApSvcBroker` | 18 | an OEM service broker, elevated |
| `OSD_FnKey` | 1 | the Fn-key / volume on-screen overlay |
| `Task Manager`, `Windows Security`, others | 5 | transient system UI |

Those flash up for a moment, so it only bites when one lands inside that gap. **Pressing a
volume key is enough to cause it.**

Fixed in two places, and this build is the important half:

* **Here (the sending side) — the actual cure.** Every ~120 ms while control is on the
  peer, mouse-rs now compares what it has *told* the peer is held against what the
  keyboard physically reports through `GetAsyncKeyState`, which works regardless of
  hooks. Any modifier the peer thinks is down but which is really up gets its release
  sent. **Without this build the fault is still there.**
* On the receiving side, already shipped: a modifier held through five seconds of total
  silence is released as a safety net.

Also in this build, though none of it changes behaviour on Windows: a `uinput` injection
backend and a login-screen service for Linux, and a stuck-transfer fix. **The
login-screen work is on hold and is Linux-only — there is nothing to set up here for it,
and nothing to report about it.**

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

Must be exactly size `12087808` bytes, SHA256
`22C7FC47018FFAD233C096333D6164FA916EBD913B2034848D13C72DDE94D7FF`.
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

Why it matters more than usual: every one of the 24 hook-blocked events in this machine's
log recorded `our_level="0x2000 (normal)"` — **mouse-rs was not running elevated**, even
though the last round asked for it. And 23 of the 24 blocking windows were
`0x3000 (elevated (administrator))`, which is ordinary admin rather than SYSTEM. An
elevated mouse-rs sits at the same level and keeps its hook, so elevation removes the
most common cause of the Alt+Tab fault outright. The code fix above handles the rest.

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
  never saw the release. Sent it ourselves.` — **the new fix doing its job.** Not an
  error. It names which modifier. Worth reporting how often it appears, because that
  number is how we know how bad the problem actually was.
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
