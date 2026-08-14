Install the current mouse-rs build, **switch on log shipping**, and then stop and wait.

Log shipping is the point of this round. From now on this machine uploads its own log to
the Kali box every 30 seconds and once more on exit, so a two-machine fault can be read
from one place instead of someone asking you for it each time. Set it once here and it
keeps working.

The owner does all the hands-on testing. Nothing below asks you to copy a file, open the
panel, or exercise the feature. That is deliberate.

## RULE — no synthetic input, at any point, for any reason

**Do not send clicks or keystrokes. Do not use UI automation, UIA Invoke, `SendInput`,
`SetCursorPos`, or AutoHotkey-style helpers. Do not click the tray, the taskbar, or any menu.**

This is a correctness requirement, not caution. When control is on the peer, mouse-rs captures
local mouse input and **forwards it to Kali**. A previous attempt to click the tray sent real
clicks onto the owner's Kali desktop and never reached Windows at all. Every step below is a
shell command; none need the pointer. If you want to click, stop and say what you are stuck on.

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

Must be exactly size `11988992` bytes, SHA256
`06CA1BCA5A8EC47BE5A7125EB41E514D24246BB2CAA5D67985C73BE4F4D6F7A8`.
If either differs, stop and say so — do not launch it.

## Step 3 — switch log shipping on

Edit the config **as JSON**, not with string replacement, so a malformed file cannot be
written. The engine only reads these at startup, which is why this comes before the relaunch.

```powershell
$p = "$env:USERPROFILE\.mouse_share\config.json"
$c = Get-Content $p -Raw | ConvertFrom-Json
$c | Add-Member -NotePropertyName log_upload_url  -NotePropertyValue "http://192.168.137.2:8001" -Force
$c | Add-Member -NotePropertyName log_upload_secs -NotePropertyValue 30 -Force
$c | ConvertTo-Json -Depth 10 | Set-Content $p -Encoding UTF8
(Get-Content $p -Raw | ConvertFrom-Json).log_upload_url
```

The last line must print `http://192.168.137.2:8001`. **Do not print `shared_secret`, and do
not paste the config.**

## Step 4 — confirm the config otherwise survived

```powershell
Get-Content $env:USERPROFILE\.mouse_share\config.json | ConvertFrom-Json |
  Select-Object name, screen_width, screen_height, autoconnect, server_host, listen_port
```

Expected: `name = laptop`, `screen_width = 1920`, `screen_height = 1080`. If the name is
`mouse-rs` or the screen size is wrong, say so — it was reset.

## Step 5 — relaunch and confirm three things

```powershell
Start-Process $HOME\mouse-rs\mouse-rs.exe
Start-Sleep -Seconds 12
Select-String -Path $env:USERPROFILE\.mouse_share\mouse-rs.log `
  -Pattern 'shipping this machine|bulk lane|connected|DECISION|PUBLISHED' |
  Select-Object -Last 15 | ForEach-Object { $_.Line }
```

Confirm and report:

1. a line saying **shipping this machine's log for diagnosis** — that is the upload working;
2. the peer connected;
3. whether anything mentions the **bulk lane**. There are two connections now, input and
   bulk, sharing one port. The bulk lane is best-effort — if it fails, file transfer is
   unavailable but mouse and keyboard are unaffected.

## Step 6 — report, then stop

Five lines maximum in chat: the size, whether the hash matched, whether log shipping started,
the connect line, and whatever the log said about the bulk lane. **Do not paste the log** —
it is being uploaded automatically now, which is the entire point.

Then **stay idle**. Do not copy files, do not open the panel, do not try the feature. The owner
drives every crossing, copy and paste by hand.

## What the new log lines mean, if you see them

You do not need to act on these; they are here so you can report them accurately.

- `DECISION: fetching now` — a file arrived under the ask-first limit and is being pulled
  automatically.
- `DECISION: WAITING FOR YOU` — a file is over the limit and **nothing will transfer** until
  the owner presses "Bring it over" in the panel. This is the state that used to be invisible.
- `PUBLISHED files to the Windows clipboard (CF_HDROP)` — files are now really on the
  clipboard and Explorer's Paste should be enabled. If this line is present and Paste is still
  greyed, that is a genuine bug worth reporting.
