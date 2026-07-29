You are helping me test **mouse-rs**, a Rust KVM-switch (share one keyboard/mouse across two
machines). It is developed on my Kali desktop; this Windows laptop is the other end. Work in
PowerShell. Do not modify any source — this laptop only runs and tests the built binary.

## The setup

- **This laptop = SERVER.** It has the physical keyboard and mouse and sends input.
- **Kali desktop = CLIENT** at `192.168.137.2`, connected by a direct ethernet cable
  (`192.168.137.1` is this laptop). Port `24800`.
- The Kali client is **already running and retrying the connection right now**. The moment
  the server starts here, it should connect on its own with no action on the Kali side.
  That unattended reconnect is one of the things I want confirmed.

## Step 1 — get today's build

A fresh Windows build is being served from the Kali box:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
```

Create `$HOME\mouse-rs` first if needed. Verify the download — it must match exactly:

- size: `11008512` bytes
- SHA256: `7D90332CBD6F2449BBCAA9B9450D10732450A000655E4E37DF0B17EE9B3CD036`

```powershell
Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256
```

If the download fails or the hash differs, **stop and tell me** — the HTTP server on Kali may
have been shut down and I'll restart it. Do not fall back to an older `mouse-rs.exe` already
on this machine; the whole point is testing today's build.

Note: there may be an **older** mouse-rs.exe and an old Python `mouse_share` on this laptop
from previous sessions. Find them and tell me where they are, but don't run them. If a
Python `mouse_share` starts at login here, say so — it should be disabled, it's superseded.

## Step 2 — check the config, do NOT overwrite it

Config lives at `%USERPROFILE%\.mouse_share\config.json`. It already exists and **worked in a
previous session**, so treat it as precious. Read it and report:

- `role` (must be `server`), `listen_port` (24800)
- `exit_edge` / `entry_edge`
- whether `shared_secret` is present — **do not print the secret value to me in full**, just
  confirm it's non-empty and give me its length and first 4 characters. It must be
  byte-identical to Kali's; if the connection later fails auth, that's the first suspect and
  I'll supply the correct value.
- the new fields `autoconnect` and `start_minimized` if present (they may be absent — the
  build defaults them to false, which is fine)

Change nothing yet. Report what you found and wait for my go-ahead if anything looks wrong.

## Step 3 — run it and confirm the link

Run it from PowerShell (not by double-clicking) so we see logs:

```powershell
cd $HOME\mouse-rs
.\mouse-rs.exe server
```

- **Windows SmartScreen may block it** — "More info" → "Run anyway". It's an unsigned
  binary I built an hour ago.
- **Do not run elevated.** The keyboard path was validated in a non-elevated shell and I
  want to keep testing that same configuration.
- A bare `.\mouse-rs.exe` with no subcommand opens the GUI control panel instead. Use
  `server` for this step so the logs stay in the foreground.

Tell me: does it print a listening line, and does the Kali client connect **by itself**
within a few seconds? Paste the log lines verbatim. If it connects with no intervention on
my side, that closes the unattended-reconnect test.

## ⚠ Before you move the mouse — how to get control back

Once connected, moving the cursor off the correct screen edge hands input to Kali, and this
laptop's keyboard/mouse will appear dead. To get control back:

- **Return hotkey: `Ctrl+Win+Alt+Esc`** (pressed on this laptop)
- The lock/cross toggle is `Scroll Lock`
- Worst case, physically unplug the ethernet cable, or Ctrl-C the server in PowerShell

Confirm you have that straight before testing any crossing. If I stop responding mid-test,
that's why — wait for me rather than assuming a crash.

## Step 4 — quick function check

With the link up: cross to Kali and back a couple of times, type a few characters on the
Kali side, and come back. Report whether motion is 1:1, whether corners are reachable, and
whether keystrokes land on the right machine. Brief is fine — this path is already validated,
I just want to know today's build didn't regress it.

## Step 5 — start-at-login (this is genuinely untested code)

This is the part I most want tested. The build writes a per-user entry to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` under the value name `mouse-rs`. It
compiles and cross-compiles clean but **has never run on Windows**, so expect it to possibly
fail — that's useful information, not a problem.

1. Stop the server (Ctrl-C) and launch the GUI: `.\mouse-rs.exe`
2. In the panel's **General** tab, find the **Startup** section and turn on **Start at
   login**. The panel should immediately report success or an error — quote it exactly.
3. Verify the registry entry actually landed:

```powershell
Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'mouse-rs'
```

The value must be the **full path to the exe, wrapped in double quotes**. Tell me the exact
string. A missing entry, an unquoted path, or a wrong path are all real bugs — report them
rather than fixing them.

4. Toggle it **off** and confirm the registry value is gone. Then toggle it back **on**.
5. Also try the **Auto-connect on launch** toggle, then close and reopen the panel, and tell
   me whether it starts sharing by itself without a Start click.

Do **not** reboot to test it unless I ask — tell me the registry state and I'll decide.

## Step 6 — report back

Give me:
1. Whether the Kali client reconnected unattended, with the log lines
2. Anything that errored, hung, or looked wrong — verbatim, don't summarize away detail
3. The exact registry value string from step 5
4. Whether the GUI itself behaved (tray icon present? panel responsive? any crash?)

Be precise about what you actually observed versus what you inferred. If something didn't
work, say so plainly — a negative result here is exactly what I'm after. Don't try to fix
bugs in the binary; you have no source here. Just report.
