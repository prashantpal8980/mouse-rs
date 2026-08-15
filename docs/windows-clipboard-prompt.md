Install the current mouse-rs build, **run it as administrator**, keep log shipping on,
and then stop and wait.

Two things matter this round:

1. **Elevation.** Windows does not deliver low-level mouse or keyboard hooks to a process
   running at a lower integrity level than the window in front. So while an elevated window
   is focused — a game launcher, an installer, a UAC prompt, Task Manager — the pointer
   cannot cross to the other machine, and until this build nothing ever said why. Running
   mouse-rs elevated is the only remedy; there is no in-process workaround, by design.
2. **Log shipping**, set up last round. Leave it on. Both machines upload their own logs, so
   neither side has to be asked for one.

The owner does all the hands-on testing. Nothing below asks you to copy a file, open the
panel, or exercise the feature. That is deliberate.

## RULE — no synthetic input, at any point, for any reason

**Do not send clicks or keystrokes. Do not use UI automation, UIA Invoke, `SendInput`,
`SetCursorPos`, or AutoHotkey-style helpers. Do not click the tray, the taskbar, or any menu.**

This is a correctness requirement, not caution. When control is on the peer, mouse-rs captures
local mouse input and **forwards it to Kali**. A previous attempt to click the tray sent real
clicks onto the owner's Kali desktop and never reached Windows at all. Every step below is a
shell command; none need the pointer. If you want to click, stop and say what you are stuck on.

**The one exception is the UAC prompt in Step 4.** It is a system dialog, it will appear, and
only the owner can answer it. Do not try to dismiss it — say it is waiting, and stop.

## Step 1 — stop the running instance, by process

```powershell
Get-Process mouse-rs -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
Get-Process mouse-rs -ErrorAction SilentlyContinue   # expect no output
```

Safe: Windows removes the low-level hooks when the process exits, so local input returns to
normal even if it was suppressing at that moment.

## Step 2 — download and verify

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
(Get-Item $HOME\mouse-rs\mouse-rs.exe).Length
(Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256).Hash
```

Must be exactly size `12006400` bytes, SHA256
`9530B5FC5D0B748B41F9EC7B161D2EBE057218BE59FED895CAE184F73E6BD6CB`.
If either differs, stop and say so — do not launch it.

## Step 3 — confirm the config survived

Log shipping was set last round and should still be there. Nothing needs editing.

```powershell
$c = Get-Content $env:USERPROFILE\.mouse_share\config.json -Raw | ConvertFrom-Json
$c | Select-Object name, screen_width, screen_height, autoconnect, server_host, listen_port
$c.log_upload_url
```

Expected: `name = laptop`, `1920x1080`, and `http://192.168.137.2:8001`. **Do not print
`shared_secret` and do not paste the config.** If `log_upload_url` is blank, set it:

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

If the owner declines, the command throws and nothing starts. Report that plainly rather than
falling back to a normal launch: a non-elevated instance is exactly the state this round
exists to change, and starting one quietly would make the whole test meaningless.

Then confirm it really is elevated. Do not treat the launch succeeding as proof — that is the
one thing this step is for:

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
  -Pattern 'shipping this machine|bulk lane|connected|CROSSING WILL NOT WORK|DECISION|PUBLISHED' |
  Select-Object -Last 15 | ForEach-Object { $_.Line }
```

Confirm and report:

1. **shipping this machine's log for diagnosis** — the upload is working;
2. the peer connected;
3. anything about the **bulk lane** — best-effort, so a failure means file transfer is
   unavailable while mouse and keyboard are unaffected;
4. any **CROSSING WILL NOT WORK** line.

## Step 6 — report, then stop

Five lines maximum in chat: the size, whether the hash matched, whether it is elevated, the
connect line, and anything about the bulk lane or a CROSSING warning. **Do not paste the
log** — it uploads automatically, which is the entire point.

Then **stay idle**. Do not copy files, do not open the panel, do not try the feature, do not
launch or touch any game. The owner drives every crossing, copy and paste by hand.

## What the log lines mean, if you see them

You do not need to act on any of these; they are here so you can report them accurately.

- `CROSSING WILL NOT WORK while this window is in front` — the focused window runs at a higher
  Windows integrity level than mouse-rs, so the OS is not delivering our input hooks and the
  pointer cannot cross. **Report the window name and both levels.** If this appears while
  mouse-rs is elevated, say so loudly: it would mean something beyond elevation is blocking
  the hooks, most likely an anti-cheat driver.
- `DECISION: fetching now` — a file arrived under the ask-first limit and is being pulled.
- `DECISION: WAITING FOR YOU` — a file is over the limit and **nothing transfers** until the
  owner presses "Bring it over" in the panel's Clipboard tab.
- `PUBLISHED files to the … clipboard` — files really are on the clipboard and Paste should be
  enabled. If this line is present and Paste is still greyed, that is a genuine bug.
- `our own window had focus … could NOT be handed off` — should no longer appear; the hand-off
  now falls back to the shell window, then the desktop, then minimising our own window. If it
  does appear, report it.
