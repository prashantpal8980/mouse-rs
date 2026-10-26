Follow-up to your Windows test report on **mouse-rs** — thank you, it was excellent, and it
found six real defects. All six are fixed. This is a **focused retest of just those fixes**,
not a full pass. Work in PowerShell, non-elevated, and don't modify any source.

Same setup as before: this laptop = **SERVER** (`192.168.137.1`), Kali = **CLIENT**
(`192.168.137.2`), port 24800, direct cable. Kali is running its panel and is connected right
now.

## Step 0 — swap in the new build

An older build is running here. Stop it first (**tray → Quit**, which you confirmed works),
then:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8000/mouse-rs.exe -OutFile $HOME\mouse-rs\mouse-rs.exe
Get-FileHash $HOME\mouse-rs\mouse-rs.exe -Algorithm SHA256
```

Must match exactly — size `12352512` bytes, SHA256
`35A305B55931483FFEED31916D5A739DCA06DCC6C264B66D56951D420D7CD2C0`.
If it doesn't match, stop and tell me.

Note your current state, carried over from last time: **`Start at login` is ON**,
**`autoconnect` is ON**. So a bare launch will start sharing by itself — expected, not a bug.

## What changed, so you know what you're checking

| # | Was | Now |
|---|---|---|
| 1 | Tray `Open mouse-rs` did nothing | un-minimizes *then* focuses |
| 2 | `config.json` truncate-rewritten in place | temp file → fsync → atomic rename |
| 3 | Docs said "General tab" | it's the **Connection** tab (scroll to bottom) |
| 4 | Log silent on Stop / saves / registry writes | all logged |
| 5 | Tray items never greyed | `Start`/`Stop` follow engine state |
| 6 | `kbd_diag` spam once a second while remote | removed entirely |

## Step 1 — fix 1: tray `Open` (the important one)

This is the fix I most need confirmed, because it **cannot be verified on Linux** — X11 window
managers restore on activate, so the bug never reproduced there and the fix can't be proven
there either.

1. Minimize the panel (title-bar minimize, or `ShowWindow(SW_MINIMIZE)` as you did before).
2. Right-click the tray icon → **Open mouse-rs**.
3. Report whether the window **restores and comes to the foreground**. Check `IsIconic` and
   the window rect — last time it stayed at `(-32000,-32000)`.

Then the compound case that made this severe: turn on **Start minimized** (Connection tab,
bottom), Save, Quit, relaunch. The panel should come up minimized — confirm you can then
reach it **only** via tray → Open. That combination previously had no escape route.

Afterwards please turn **Start minimized back off** and Save, so the machine is left in a
normal state.

## Step 2 — fix 5: tray greying

Right-click the tray icon in each state and report the **enabled/greyed** state of all four
items:

- while **sharing is running**: `Start` should be greyed, `Stop` active
- while **stopped**: `Start` active, `Stop` greyed

`Open mouse-rs` and `Quit` should always be active. Last time all four were always enabled.

## Step 3 — fix 4: log coverage

The log is `%USERPROFILE%\.mouse_share\mouse-rs.log`. Exercise Stop, Start, a Save, and the
`Start at login` toggle, then check the log contains lines matching:

- `stop requested from the panel`
- `engine stopped; sharing is off`
- `saved config to ... (implicit, on Start)`
- `saved config to ...` (from the explicit Save button)
- `added the start-at-login entry` / `removed the start-at-login entry`

Tell me which appear and which don't. A deliberate Stop and a crash should now be
distinguishable in the log — that was the point.

## Step 4 — fix 6: the diag spam is gone

Cross to Kali, type for ~30 seconds, come back. Confirm the log has **no `kbd_diag` lines at
all** and stays small. Previously it emitted one per second while remote (222 samples last
run). Report the log's size before and after the crossing.

Keyboard suppression itself should be unchanged — if keystrokes now leak to Windows while
remote, that is a serious regression and I need to know immediately.

## Step 5 — fix 2: atomic config writes

`config.json` is now written via a temp file and renamed. Two things to verify:

1. After pressing **Start** and after pressing **Save**, list `%USERPROFILE%\.mouse_share\`
   and confirm **no leftover files** like `config.tmp1234` or `config.tmp*`. A stray temp
   would be a second copy of your shared secret on disk.
2. Confirm `shared_secret` still has the same length and first 4 characters as before
   (`54` chars, starting `Uu5v`), and that the other values are unchanged. Don't print the
   secret.

## Step 6 — no regressions

Briefly confirm the things that already worked still do: panel opens and renders, Kali
connects unattended, `Start at login` registry value is still
`"C:\Users\prade\mouse-rs\mouse-rs.exe"`, tray Start/Stop/Quit behave, edge crossing works.
Short confirmations are fine — I'm looking for breakage, not fresh detail.

## Step 7 — one piece of housekeeping (ask me first)

You reported that Python `mouse_share` still starts at login via
`%APPDATA%\...\Startup\mouse_share.lnk`, and will hook input alongside mouse-rs at next
logon. **Ask me before touching it**, then if I agree, move that shortcut somewhere harmless
(don't delete it) and tell me where you put it.

## Step 8 — report

Same method as last time — **do not paste the report or logs into chat**:

```powershell
Invoke-WebRequest -Uri http://192.168.137.2:8001/retest-report.md -Method PUT -InFile $HOME\mouse-rs\retest-report.md
Invoke-WebRequest -Uri http://192.168.137.2:8001/retest-mouse-rs.log -Method PUT -InFile $env:USERPROFILE\.mouse_share\mouse-rs.log
```

Screenshots welcome (`-Method PUT` any `.png`), especially the tray menu showing greyed items.
Use **new filenames** with a `retest-` prefix so they don't overwrite last time's.

Be exhaustive in the file, then give me **10 lines maximum** in chat: which of the six fixes
pass, which fail, and the filenames you uploaded. Report failures plainly — I would much
rather hear a fix didn't work than have it confirmed politely. Don't try to fix anything; you
have no source here.
