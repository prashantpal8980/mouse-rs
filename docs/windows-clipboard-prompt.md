Install the current mouse-rs build, **run it as administrator**, raise one config value,
and then stop and wait.

Nothing was wrong with the transfers last round. All three 4 GB archives crossed
completely and landed on the owner's desktop at full size. What was wrong was everything
the software *said* about them: the progress bar snapped back toward zero every few
seconds, and the panel reported failure over downloads that were running perfectly at
15 MB/s. The owner reasonably concluded the transfers were dying. They were not.

Three fixes, all on the receiving side, plus one setting that has to be raised on **both**
machines:

* **"Already downloading" is no longer read as "failed."** When a paste asked for a set,
  the code answered with a yes/no — and `no` covered two unrelated situations: nothing
  could be requested, and nothing *needed* requesting because it was already in flight.
  A paste is re-checked constantly by the file manager during a large transfer, so every
  one of those re-checks declared failure on a healthy download. It now reports
  `Asked` / `AlreadyInFlight` / `Idle` and only the last is a problem.
* **Progress survives a retry.** The baseline counted only whole completed files, so a
  re-check part-way through a 4 GB file recomputed progress as "nothing yet" and reset the
  bar. Partially-received bytes now count, and the figure is never allowed to move
  backwards. This is the "climbs to 3% and stops, over and over" symptom exactly.
* **The over-budget notice is written at most once a minute.** It was logging ten times a
  second for as long as the cache sat over its limit, which buried everything useful.

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

Must be exactly size `12080128` bytes, SHA256
`0E6DC10DC82CB12CCCDE53BC0D9CDCF8BD7D660AE05868C7705CEECC3AC85AFE`.
If either differs, stop and say so — do not launch it.

## Step 3 — raise the disk budget to 16 GB

**This is the one thing this round actually needs from you.** The default changed from
2 GB to 16 GB, but a value already written in the config wins over the default, so it has
to be set rather than left alone.

2 GB was smaller than a single item the owner routinely moves. One 4 GB archive put the
cache permanently over budget, and from then on every transfer fought an evictor for room
it could never win — which is what produced the constant over-budget logging.

```powershell
$p = "$env:USERPROFILE\.mouse_share\config.json"
$c = Get-Content $p -Raw | ConvertFrom-Json
$c | Add-Member -NotePropertyName clipboard_cache_budget -NotePropertyValue 17179869184 -Force
$c | ConvertTo-Json -Depth 10 | Set-Content $p -Encoding UTF8
(Get-Content $p -Raw | ConvertFrom-Json).clipboard_cache_budget
```

The last line must print `17179869184`. Report what it printed.

While you are in there, confirm the rest is unchanged — **do not print `shared_secret`
and do not paste the config**:

```powershell
$c = Get-Content $env:USERPROFILE\.mouse_share\config.json -Raw | ConvertFrom-Json
$c | Select-Object name, screen_width, screen_height, autoconnect, server_host, listen_port
$c.log_upload_url
$c.clipboard_auto_transfer_max
```

Expected: `name = laptop`, `1920x1080`, `http://192.168.137.2:8001`, and
`clipboard_auto_transfer_max = 16777216`. If `log_upload_url` is blank, set it:

```powershell
$p = "$env:USERPROFILE\.mouse_share\config.json"
$c = Get-Content $p -Raw | ConvertFrom-Json
$c | Add-Member -NotePropertyName log_upload_url  -NotePropertyValue "http://192.168.137.2:8001" -Force
$c | Add-Member -NotePropertyName log_upload_secs -NotePropertyValue 30 -Force
$c | ConvertTo-Json -Depth 10 | Set-Content $p -Encoding UTF8
```

## Step 4 — launch it **elevated**

```powershell
Start-Process -FilePath "$HOME\mouse-rs\mouse-rs.exe" -Verb RunAs
```

`-Verb RunAs` asks Windows for elevation, so **a UAC prompt appears on screen**. Only the
owner can answer it. If it is still waiting, say so and stop — do not try to click it.

If the owner declines, the command throws and nothing starts. Report that plainly rather
than falling back to a normal launch: Windows does not deliver low-level input hooks to a
process running below the foreground window's integrity level, so a non-elevated instance
cannot cross the pointer while a game or installer is in front.

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
  -Pattern 'shipping this machine|bulk lane|connected|CROSSING WILL NOT WORK|nothing to request' |
  Select-Object -Last 15 | ForEach-Object { $_.Line }
```

Confirm and report:

1. **shipping this machine's log for diagnosis** — the upload is working;
2. the peer connected;
3. anything about the **bulk lane** — best-effort, so a failure means file transfer is
   unavailable while mouse and keyboard are unaffected;
4. any **CROSSING WILL NOT WORK** line.

## Step 6 — report, then stop

Five lines maximum in chat: the size, whether the hash matched, what the budget line
printed, whether it is elevated, and the connect line. **Do not paste the log** — it
uploads automatically, which is the entire point.

Then **stay idle**. Do not copy files, do not open the panel, do not try the feature, do
not launch or touch any game. The owner drives every crossing, copy and paste by hand.

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

- `nothing to request and nothing in flight` — the *new* genuine-failure line. If a paste
  fails now, this is what says so, and it means something real. Report it.
- `CROSSING WILL NOT WORK while this window is in front` — the focused window runs at a
  higher Windows integrity level than mouse-rs, so the OS is not delivering our input
  hooks. **Report the window name and both levels.** If this appears while mouse-rs is
  elevated, say so loudly: it would mean something beyond elevation is blocking the hooks,
  most likely an anti-cheat driver.
- `DECISION: WAITING FOR YOU` — expected for anything over 16 MB. Only the description
  crossed; the bytes wait for a paste. Not a problem.
- `paste asked for it — fetching now` — a paste started a transfer. Note that this should
  now appear **once** per transfer, not once every few hundred milliseconds. If it still
  repeats, Step 2 did not take.
- `over the disk budget and cannot get under it` — should now be rare, and at most once a
  minute. If it appears repeatedly after Step 3, the budget did not get written.
- `could not ask Explorer to move rather than copy` — the drop-effect hint was refused.
  Not fatal: pasting still works, it just copies. Worth reporting if it appears.
