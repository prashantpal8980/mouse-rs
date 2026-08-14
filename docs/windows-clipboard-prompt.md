Install the current mouse-rs build on this machine, confirm the new clipboard settings are
present, and then **stop and wait**. The owner does all the hands-on testing.

Your whole job is: get this machine onto the new build, prove it came back up connected, and
report a handful of facts. Nothing in this document asks you to copy a file, open the panel,
or exercise the feature. That is deliberate.

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

Must be exactly size `11902464` bytes, SHA256
`1F2D3856E621BFD8D52824904750B18A84A56771F58B5C7452CFE4104586571C`.
If either differs, stop and say so — do not launch it.

## Step 3 — confirm the config survived, and that the new keys are readable

Old configs stay valid: every new setting has a default, so nothing needs editing by hand.
This step only proves the file is intact and shows what the defaults resolved to.

```powershell
$c = Get-Content $env:USERPROFILE\.mouse_share\config.json | ConvertFrom-Json
$c | Select-Object name, screen_width, screen_height, autoconnect, server_host, listen_port
$c | Select-Object clipboard_sync_files, clipboard_auto_transfer_max, bulk_enabled, bulk_port,
                   clipboard_max_files_per_copy, clipboard_follow_symlinks, clipboard_notify,
                   bulk_send_buffer_mb, clipboard_persist_history
```

Expected on this machine: `name = laptop`, `screen_width = 1920`, `screen_height = 1080`.
**Do not print `shared_secret`.**

The second block may show blanks for the newest keys — that is correct and expected. They are
absent from a config written by an older build and fall back to their defaults; they get written
out the first time the panel saves. What matters is that the command runs and the first block is
still right. If `name` is `mouse-rs` or the screen size is not 1920x1080, say so — it was reset.

## Step 4 — relaunch and confirm both lanes came up

```powershell
Start-Process $HOME\mouse-rs\mouse-rs.exe
Start-Sleep -Seconds 8
Get-Content $env:USERPROFILE\.mouse_share\mouse-rs.log -Tail 25
```

From that output, confirm and report:

- the engine started, and `kali-desktop` connected;
- whether any line mentions the **bulk lane** connecting or failing. There are two connections
  now — input and bulk — sharing one port by default. The bulk lane is best-effort: if it fails,
  file transfer is unavailable but the mouse and keyboard are unaffected, so a failure here is
  worth reporting rather than worth panicking about.

## Step 5 — report, then stop

Six lines maximum in chat: the size, whether the hash matched, the six config fields from the
first block, the connect line, whatever the log said about the bulk lane, and anything that
looked wrong. **Do not paste the log.**

If a full log is useful, upload it rather than pasting:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8001/windows-clipboard.log -Method PUT `
  -InFile $env:USERPROFILE\.mouse_share\mouse-rs.log
```

Then **stay idle**. Do not copy files, do not open the panel, do not try the feature. The owner
drives every crossing, copy and paste by hand, and will ask you directly if a log is needed.
