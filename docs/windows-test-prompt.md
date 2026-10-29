You are helping me test **mouse-rs**, a Rust KVM-switch (share one keyboard/mouse across two
machines). It is developed on my Kali desktop; this Windows laptop is the other end. Work in
PowerShell. Do not modify any source — this laptop only runs and tests the built binary.

## The setup

- **This laptop = SERVER.** It has the physical keyboard and mouse and sends input.
- **Kali desktop = CLIENT** at `192.168.137.2`, connected by a direct ethernet cable
  (`192.168.137.1` is this laptop). Port `24800`.
- **Both machines should run the GUI control panel**, not the headless subcommands. The
  Kali side is already running its panel with auto-connect on, so it is **retrying the
  connection right now** and should link up by itself the moment this side starts.

> **Why you may have never seen a UI on this laptop:** the `mouse-rs.exe` currently here is
> an old build from before the GUI existed, when a bare launch just printed
> `no subcommand (GUI lands in Phase 6)` and exited. Today's build has the full panel and
> system tray on Windows. Getting it is step 1.

## Step 1 — get today's build

A fresh Windows build is being served from the Kali box:

```powershell
mkdir $HOME\mouse-rs -Force
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
```

Verify the download — it must match exactly:

- size: `13303296` bytes
- SHA256: `77B1DF697739BD7843A0DAD8D805E060EE6448D56F829584E218C907E2CEB96F`

```powershell
Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256
```

If the download fails or the hash differs, **stop and tell me** — the HTTP server on Kali may
have been shut down and I'll restart it. Do not fall back to an older `mouse-rs.exe` already
on this machine; the whole point is testing today's build.

Also: there may be an older mouse-rs.exe and an old Python `mouse_share` on this laptop from
previous sessions. Find them, tell me where they are, and **don't run them**. If either
starts at login, say so — the Python one is superseded and should be disabled.

## Step 2 — check the config, do NOT overwrite it

Config lives at `%USERPROFILE%\.mouse_share\config.json`. It already exists and **worked in a
previous session**, so treat it as precious. Read it and report:

- `role` (must be `server`), `listen_port` (24800)
- `exit_edge` / `entry_edge`
- whether `shared_secret` is present — **do not print the secret value to me in full**, just
  confirm it's non-empty and give me its length and first 4 characters. It must be
  byte-identical to Kali's; if the link later fails auth, that's the first suspect and I'll
  supply the correct value.
- the fields `autoconnect` and `start_minimized` if present (they may be absent — the build
  defaults them to false, which is fine)

Change nothing yet. Report what you found and wait for my go-ahead if anything looks wrong.

## Step 3 — launch the GUI

**A bare launch with no subcommand opens the control panel.** That is what I want here:

```powershell
cd $HOME\mouse-rs
.\mouse-rs.exe
```

- **Windows SmartScreen may block it** — "More info" → "Run anyway". It's an unsigned
  binary I built today.
- **Do not run elevated.** The keyboard path was validated in a non-elevated shell and I
  want to keep testing that same configuration.

**PowerShell will show no output** — a GUI launch drops the console (`FreeConsole`) so the
panel doesn't drag a console window behind it. That's expected. Instead, this build writes a
log file next to the config:

```powershell
Get-Content $env:USERPROFILE\.mouse_share\mouse-rs.log -Tail 30
```

The panel also shows that path under **Connection → Log file**. Both `Startup` and
`Log file` live at the **bottom of the Connection tab**, below `Listen port` — you must
scroll that pane to see them. There is no "General" tab. Check the file exists and has
content — the first line should be `logging to …\mouse-rs.log`, and you should see
`starting mouse-rs for …` when you press Start. **If the file is missing or empty, tell me:
that's a real bug** (this logging path is new today and has never run on Windows). Verify
the path shown in the panel matches where the file actually is.

For a verbose graphics trace if something renders wrong: `$env:RUST_LOG="debug"` before
launching — the default filter keeps the wgpu/winit startup noise at warn so the log stays
readable.

Report what you see: does the panel window open, is it readable and responsive, and **is
there a tray icon** in the notification area? On Windows the icon may be hidden behind the
`^` chevron — check there before reporting it missing.

## ⚠ Before you move the mouse — how to get control back

Once connected, moving the cursor off the correct screen edge hands input to Kali, and this
laptop's keyboard/mouse will appear dead. To get control back:

- **Return hotkey: `Ctrl+Win+Alt+Esc`** (pressed on this laptop)
- The lock/cross toggle is `Scroll Lock`
- Worst case, physically unplug the ethernet cable, or kill the process from Task Manager

Confirm you have that straight before testing any crossing. If I stop responding mid-test,
that's why — wait for me rather than assuming a crash.

## Step 4 — connect from the panel

In the panel, confirm the **Role** is set to **Server**, then press **Start**. Watch the
status area at the top.

Tell me: does the Kali client connect **by itself** within a few seconds of pressing Start,
with no action taken on the Kali side? That closes the unattended-reconnect test. Quote what
the panel's status shows (and the log file if it has anything).

Then cross to Kali and back a couple of times, type a few characters on the Kali side, and
come back. Report whether motion is 1:1, whether corners are reachable, and whether
keystrokes land on the right machine. Brief is fine — this path is already validated, I just
want to know today's build didn't regress it.

## Step 5 — start-at-login (this is genuinely untested code)

This is the part I most want tested. The build writes a per-user entry to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` under the value name `mouse-rs`. It
compiles and cross-compiles clean but **has never run on Windows**, so expect it to possibly
fail — that's useful information, not a problem.

1. In the panel's **Connection** tab, **scroll to the bottom** to find the **Startup**
   section, and turn on **Start at login**. The panel reports success or an error
   immediately — quote it exactly.
2. Verify the registry entry actually landed:

```powershell
Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'mouse-rs'
```

The value must be the **full path to the exe, wrapped in double quotes**. Tell me the exact
string. A missing entry, an unquoted path, or a wrong path are all real bugs — report them
rather than fixing them.

3. Toggle it **off** and confirm the registry value is gone. Then toggle it back **on**.
4. Also turn on **Auto-connect on launch**, press Save, then close and reopen the panel, and
   tell me whether it starts sharing by itself without a Start click.
5. Try the **tray menu** (right-click the tray icon): it should offer Open / Start / Stop /
   Quit. Test each and report what happens — Quit should fully exit and remove the icon.

Do **not** reboot to test autostart unless I ask — tell me the registry state and I'll decide.

## Step 6 — write the report to a file and upload it (do NOT print it in chat)

**Do not paste the report, logs, or command output into our conversation.** Write everything
to files and upload them to the Kali box over the cable. There is a receive endpoint waiting:

```powershell
# Write the report first.
$report = "$HOME\mouse-rs\windows-report.md"
# ... write your findings into $report ...

# Then upload it, plus the log and any screenshots.
Invoke-WebRequest -Uri http://192.168.137.2:8001/windows-report.md -Method PUT -InFile $report
Invoke-WebRequest -Uri http://192.168.137.2:8001/mouse-rs.log     -Method PUT -InFile $env:USERPROFILE\.mouse_share\mouse-rs.log
# Screenshots are welcome — PNG uploads the same way and I can view them:
Invoke-WebRequest -Uri http://192.168.137.2:8001/panel.png -Method PUT -InFile $HOME\mouse-rs\panel.png
```

Each upload replies `saved <name> (<n> bytes)`. `Invoke-WebRequest http://192.168.137.2:8001/`
lists what has arrived, so you can confirm before finishing. If a PUT fails, tell me the
error — the endpoint may have been stopped — and don't fall back to pasting.

Filenames: use plain names like `windows-report.md`, `mouse-rs.log`, `panel-general.png`.
Don't worry about overwriting; just tell me what you uploaded.

Put this in the report, in full detail — long is fine now that it's a file, not chat:

1. Whether the Kali client connected unattended, and what the panel's status showed
2. Whether `%USERPROFILE%\.mouse_share\mouse-rs.log` exists with sensible content, and
   whether the path shown in the panel matches it
3. Anything that errored, hung, rendered wrong, or looked ugly — **verbatim**, don't
   summarize away detail; include full error text and any stack traces
4. The exact registry value string from step 5
5. Tray menu results, item by item
6. Screenshots of each panel tab if you can take them

Then in chat, give me **only a short summary — 10 lines maximum**: what passed, what failed,
and the names of the files you uploaded. I'll read the detail from the files myself.

Be precise about what you actually observed versus what you inferred. If something didn't
work, say so plainly — a negative result here is exactly what I'm after. Don't try to fix
bugs in the binary; you have no source here. Just report.
