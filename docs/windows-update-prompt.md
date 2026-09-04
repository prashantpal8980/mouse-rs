Install the current mouse-rs build on this machine and confirm it comes back up healthy.

This is the **standing update procedure** — the same URL always serves whatever build is
current, and the size and hash below are rewritten automatically every time a build is
published. So this document is always up to date; there is never a newer one to ask for.

## RULE — no synthetic input, at any point, for any reason

**Do not send clicks or keystrokes. Do not use UI automation, UIA Invoke, `SendInput`,
`SetCursorPos`, or AutoHotkey-style helpers. Do not try to click the tray, the taskbar, or
any menu.**

This is a correctness requirement, not caution. When control is on the peer, mouse-rs
captures local mouse input and **forwards it to Kali**. A previous attempt to click the tray
sent real clicks onto the owner's Kali desktop and never reached Windows at all, which is
why it could not complete this step. Every action below is a shell command; none of them
need the pointer. If you find yourself wanting to click, stop and say what you are stuck on.

## Step 0 — record what is installed *now*, before replacing it

Do this first. Step 2 overwrites the exe, and the answer to "did the keyboard leak come back
because the fixed build was never installed here?" is destroyed the moment it does.

```powershell
(Get-Item $HOME\mouse-rs\mouse-rs.exe).LastWriteTime
(Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256).Hash
Select-String -Path $env:USERPROFILE\.mouse_share\mouse-rs.log `
  -Pattern 'handed foreground|could NOT be handed off' | Measure-Object | Select-Object Count
Select-String -Path $env:USERPROFILE\.mouse_share\mouse-rs.log `
  -Pattern 'NEVER CALLED OUR KEYBOARD HOOK|released \(no leaks\)|SOME KEYS LEAKED' |
  Select-Object -Last 12 | ForEach-Object { $_.Line }
```

Report all four results. They separate two very different causes:

- **the hash differs from the one in Step 2 and the hand-off count is 0** — this machine is
  running a build from before the fix. The leak is not a regression; the fix is not here yet.
- **the hash matches Step 2 and hand-off lines exist** — the fix is installed and running, and the
  question becomes whether it succeeded (`handed foreground to …`) or failed (`could NOT be
  handed off`) on the crossings that leaked. Say which of the two appears, and how often.

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

Must be exactly size `12080128` bytes, SHA256
`E8A7C3FE459FE8CE06114A23F72A4BC11645FAE956F31FD8C7BB0272144B269A`.
If either differs, stop and say so — do not launch it.

## Step 3 — check the config was not damaged

`Restore defaults` in the panel previously reset machine-specific values. Read the config
and report these four fields:

```powershell
Get-Content $env:USERPROFILE\.mouse_share\config.json |
  ConvertFrom-Json |
  Select-Object name, screen_width, screen_height, autoconnect, server_host, listen_port
```

Expected on this machine: `name = laptop`, `screen_width = 1920`, `screen_height = 1080`.
**Do not print `shared_secret`.** If the name is `mouse-rs` or the screen size is not
1920x1080, say so — it was reset and needs correcting before anything else is judged.

## Step 4 — relaunch and confirm

```powershell
Start-Process $HOME\mouse-rs\mouse-rs.exe
Start-Sleep -Seconds 6
Get-Content $env:USERPROFILE\.mouse_share\mouse-rs.log -Tail 20
```

Confirm from that output that the engine started and `kali-desktop` connected.

## Step 5 — report

Six lines maximum in chat: the size, the hash match, the four config fields, the connect
line, and anything that looked wrong. Do not paste the log.

If you need to send a full log or report, upload rather than paste:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8001/windows-update.log -Method PUT `
  -InFile $env:USERPROFILE\.mouse_share\mouse-rs.log
```

Then stay idle and wait — the owner drives all crossings and typing by hand.
