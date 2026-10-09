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

## Do it with one line, from an **Administrator** PowerShell

Do not paste a multi-line block into the console — a blank line ends the `>>`
continuation prompt, so parts of it silently never run. That has already happened once
here. The script is served instead; this fetches and runs it.

Open *Terminal (Admin)* or *Windows PowerShell (Admin)* from the Win+X menu, answer the
UAC prompt, then paste this single line:

```powershell
iwr http://192.168.137.2:8000/rollback.ps1 -OutFile $env:TEMP\mr.ps1 -UseBasicParsing; iex (Get-Content $env:TEMP\mr.ps1 -Raw)
```

`Get-Content -Raw` is deliberate: the server sends `.ps1` as binary, so
`iex (iwr ...).Content` is handed a byte array and fails. Reading the saved file back as
text avoids that, and avoids the execution policy too, since nothing invokes a `.ps1`
directly.

The script stops before touching anything if the shell is not elevated — mouse-rs runs
elevated, so an ordinary shell can neither stop it nor replace the file it holds open. It
also verifies the download against its hash before replacing the exe, keeps the build it
replaces as `mouse-rs.exe.bak`, re-hashes the installed file, and starts mouse-rs with the
elevation this shell already has. It prints what it did at each step and is safe to re-run.

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
