Roll mouse-rs on this machine back to the build it ran for the five weeks before
2026-10-09, then start it and stop. **This is a rollback, not an update.**

## Why

The build installed earlier today broke every modifier chord: Ctrl stopped crossing to
Kali at all, so Ctrl+Space never arrived, and Alt+Tab needed about five presses. The owner
asked to go back to the version that was working normally, so that is exactly what this
is — the Sep 4 build, unchanged, with none of today's code in it.

Do not treat anything below as a test round. Nothing new is being tried here.

## What this build is, and is not

It is the exe that ran on this machine from 4 Sep to 9 Oct. Windows was deliberately held
back during that period while work went on on the Linux side, so this is the last build
this machine ever ran successfully.

Checked before it was served:

- The wire protocol is **identical** to the current Linux side, so it talks to Kali
  normally. The only shared change since is one config field.
- That config field is additive and the old parser ignores unknown fields, so your
  existing config file loads unchanged. **Do not edit or recreate the config.**

What it therefore does *not* have, all of which this machine never had anyway: the
login-screen work (Linux-only), the transfer-progress fix, and today's broken modifier
code.

## Step 1 — stop the running instance, by process

```powershell
Get-Process mouse-rs -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
Get-Process mouse-rs -ErrorAction SilentlyContinue   # expect no output
```

## Step 2 — download and verify

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
(Get-Item $HOME\mouse-rs\mouse-rs.exe).Length
(Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256).Hash
```

Must be exactly size `12080128` bytes, SHA256
`5076363C55AA0E7B16CE43AD11BDDCC4B3BFE42DDC9EBFE13003CBE54F9011F1`.
If either differs, stop and say so — do not launch it.

## Step 3 — launch it elevated

```powershell
Start-Process -FilePath "$HOME\mouse-rs\mouse-rs.exe" -Verb RunAs
```

`-Verb RunAs` asks Windows for elevation, so **a UAC prompt appears on screen** and only
the owner can answer it. If it is still waiting, say so and stop — do not try to click it.

If the owner declines, launch it normally instead and say that you did. Elevation is worth
having (this machine has never actually run elevated — every hook-blocked event in its log
records `our_level="0x2000 (normal)"`), but it is not what this round is about, and a
working keyboard matters more than elevation.

## Step 4 — confirm it is up, then stop

```powershell
Start-Sleep -Seconds 10
Get-Process mouse-rs -ErrorAction SilentlyContinue | Select-Object Id,ProcessName
Get-Content "$env:LOCALAPPDATA\mouse-rs\mouse-rs.log" -Tail 15
```

Report at most five lines: the size, whether the hash matched, whether it is elevated,
the connect line, and anything that looks like an error. **Do not paste the log** — it
uploads to the Kali side automatically.

Then **stay idle.** The owner will test Ctrl+Space and Alt+Tab by hand. Expect both to
behave the way they did before today.

## RULE — no synthetic input, at any point, for any reason

**Do not send clicks or keystrokes. Do not use UI automation, UIA Invoke, `SendInput`,
`SetCursorPos`, or AutoHotkey-style helpers. Do not click the tray, the taskbar, or any
menu.**

This is a correctness requirement, not caution. When control is on the peer, mouse-rs
captures local input and **forwards it to Kali** — a previous attempt to click the tray
sent real clicks onto the owner's Kali desktop and never reached Windows at all. Every
step above is a shell command; none need the pointer. If you want to click, stop and say
what you are stuck on.

**The one exception is the UAC prompt in Step 3.** It is a system dialog and only the
owner can answer it. Do not try to dismiss it — say it is waiting, and stop.
