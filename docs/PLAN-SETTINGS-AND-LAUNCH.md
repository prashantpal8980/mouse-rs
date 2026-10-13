# Plan: settings reorganisation and launch readiness

Written 2026-10-13 from the owner's instructions. Launch is **1 November 2026**. Nothing
is released before then. This file is the to-do list for both pieces of work; the agreed
behaviour also lives in [UI-REQUIREMENTS.md](UI-REQUIREMENTS.md) (R23–R26).

Order of work agreed on 2026-10-13: **copy and paste first** (Part C), then the settings
reorganisation (Part A), then launch readiness (Part B).

---

## Part A — Settings: one place, grouped by what they do

### What the owner asked for

1. **All settings move to one place: ⋯ (three dots) → Settings.** Settings are no
   longer spread across tabs. Today they are split across Connection, Layout, Security,
   Clipboard and Startup, and the Clipboard tab mixes its history with its settings.
2. **Grouped by function, with sub-sections, like Android's Settings app** (Display →
   Brightness, …). Each category opens a page; each page has headed sub-sections.
3. **Each setting is either Basic or Advanced.** Normal users see the Basic ones.
   Advanced ones sit behind an "Advanced" expander on each page, or a global switch.
4. **Bring back settings that are missing.** The Python app had more controls than the
   current panel; see the audit below.
5. The main window keeps what is used every day: status, Start/Stop, and the clipboard
   history (the manager, R17). It no longer holds settings.

### Audit: settings that have no control in the panel today

The config has 51 fields; the panel exposes 43. Missing:

| Setting | What it does | Was in the Python app? | Proposed |
|---|---|---|---|
| `edge_threshold_px` | How close to the edge (px) the pointer must get to cross | **Yes** | Screens & edges → Advanced ("Edge sensitivity") |
| `warp_threshold_px` | In the Python app: how far from the edge the pointer was placed on return | **Yes** | **Not used by the Rust engine at all** — decide: implement it as "Return distance", or drop it |
| `discovery_port` | UDP port for finding the other computer | No (new) | Connection → Advanced |
| `cache_dir` | Where received files are held before pasting | No | Clipboard → Files → Advanced |
| `clipboard_hash_limit` | Size above which files are not hashed | No | Clipboard → Transfers → Advanced |
| `log_upload_url` | Where this computer's log is sent for diagnosis | No | Diagnostics → Advanced |
| `log_upload_secs` | How often the log is sent | No | Diagnostics → Advanced |
| `at_login_screen` | Internal flag | — | Never shown |

`elevation_notice_seen` and `login_screen_notice_seen` are internal too, and stay hidden.
The panel's **theme** choice is in the ⋯ menu today; it moves to Appearance.

### Proposed categories (owner to confirm or change)

B = Basic, A = Advanced.

**1. Connection**
- *This computer:* name (B), role server/client (B)
- *Finding the other computer:* server address and port (B, client only), listen address and
  port (A, server only), discovery port (A)
- *Staying connected:* auto-reconnect (B)
- *Profiles:* saved setups for different places or networks (B), with an explanation

**2. Screens & edges**
- *Arrangement:* which edge leads to the other computer (B; only the edges this role uses, R16)
- *This screen:* resolution with Detect (A)
- *Crossing feel:* edge sensitivity (A); return distance (A) only if it is implemented

**3. Keyboard & hotkeys**
- *Getting control back:* lock / cross hotkey (B), return hotkey (A)

**4. Clipboard**
- *What to share:* clipboard sync on/off, text, images, files (B)
- *Notifications:* tell me when something arrives (B)
- *History:* how many items to keep (B), keep history after restart (B)
- *Files:* where pasted files go (B), move instead of copy on paste (A), also save copied images
  as a file (A), follow symbolic links (A), maximum files per copy (A)
- *Transfers:* send small files at once, up to … (A), quick-send window (A), pause transfers
  while the pointer is on the other computer (A), speed limit (A), send buffer (A), file lane on/off
  and port (A), cache size and folder (A), hashing limit (A)
- *Limits:* maximum item size (A), memory budget (A), maximum image size, inline-image limit (A)

**5. Startup**
- *When I log in:* start at login (B), connect automatically (B), start minimised (B)
- *Windows:* start elevated, so admin windows don't block crossing (B, Windows only)
- *Linux:* share input at the login screen (B, Linux only)

**6. Security & privacy**
- *Pairing:* shared secret with Show / Generate (B); a pairing code later (Part B)
- *Encryption:* TLS on/off (A, with a warning)

**7. Appearance**
- Theme (B)

**8. Diagnostics** (the whole page is Advanced)
- Log file location (shown), log upload address and interval

**9. About**
- Version, licence, Restore default settings (R5), Check for updates (Part B, after launch prep)

### Rules carried over

- Restart prompts (R1–R4) keep working wherever a setting moves.
- Every clipboard limit stays configurable (R19), with Basic/Advanced now applied everywhere.
- Moving a setting must not change its stored name in `config.json`, so existing
  installs keep their values.

---

## Part B — Launch readiness: removing every unnecessary step

The bar: **download, run, done** on both computers, and updates that need no effort.
Grouped by the moment the user meets them, in priority order within each group.

### Getting the program
| # | Step a user has today | Fix | Priority |
|---|---|---|---|
| B-1 | Windows shows "Windows protected your PC" for an unsigned program; antivirus may distrust a keyboard-capturing app | **Code-signing certificate.** Takes days to weeks to obtain, so order early | **Must, before 1 Nov** |
| B-2 | Extract a zip / tarball; run a script on Linux | One download per system: Windows installer or single exe; Linux AppImage or `.deb` | Must |
| B-3 | Possibly `apt install` two tray libraries on Linux | Bundled in the AppImage, or declared by the `.deb` | Must (comes with B-2) |
| B-4 | Windows Firewall "Allow access" prompt | Installer adds the firewall rule | Should |
| B-5 | Nowhere public to download from (the repo is private) | A downloads-only location; decided by the owner, not before launch prep | Must, owner's call |

### First run
| # | Step | Fix | Priority |
|---|---|---|---|
| B-6 | Copy a 32-character secret between computers | 6-digit pairing code shown on one, typed on the other | **Must** |
| B-7 | Choose server/client and edges | Sensible defaults; confirm the edge with one push | Should |

### Updating
| # | Step | Fix | Priority |
|---|---|---|---|
| B-8 | Download and replace files by hand, on both computers | **Update button**: check, download, verify a signature, restart. Later: update the other computer too | **Must** |
| B-9 | After a Linux update, toggle the login-screen service off and on (two password prompts) | The update refreshes it with one prompt | Should |

### Getting help
| # | Step | Fix | Priority |
|---|---|---|---|
| B-10 | Find and send log files from both computers | "Report a problem" button that bundles both logs | Should |
| B-11 | Third-party licence notices must ship with the program | Generate a notices file at build time | **Must** (legal) |

Already right, and to be kept: one self-contained program file per system, settings outside
the program folder, no IP address to type, start at login with auto-connect, automatic
reconnect, and the login-screen option as a single toggle.

---

## Part C — Copy and paste (next, in progress)

The owner reports that copy and paste "is quite distorted and not working the way I want it
to". It is the next piece of work. What is wrong, and what the owner wants instead, gets
recorded here before any change, so the target is agreed first.

- [ ] Owner's description of what goes wrong (which direction, which content, what happens)
- [ ] Evidence from both machines' logs for those cases
- [ ] Agreed behaviour, written as requirements
- [ ] Fix, then the owner tests by hand
