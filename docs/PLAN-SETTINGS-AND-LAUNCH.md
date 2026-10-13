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

### The test ladder: one kind of content at a time

Agreed 2026-10-13. Trying to fix every kind of copy at once is what went wrong before, so
each rung is tested, fixed and **signed off by the owner** before the next one starts. A
rung is done when it works in the direction tested; the reverse direction is its own rung.

| Rung | What is copied | Direction | Status |
|---|---|---|---|
| 1a | **Image pixels**: a screenshot, or "Copy image" in a browser; pasted into an Explorer folder **and** into apps | Linux → Windows | **signed off 13 Oct** |
| 1b | **Image file**: a `.png`/`.jpg` copied in the file manager | Linux → Windows | closed 13 Oct (owner: image copy/paste resolved) |
| 2 | **Video file** (a large one, hundreds of MB) | Linux → Windows | waiting |
| 3 | **Folder** with several files and sub-folders | Linux → Windows | waiting |
| 4 | **Zip file** | Linux → Windows | waiting |
| 5 | Image pixels, pasted into an app **and into a Thunar folder** | Windows → Linux | **signed off 13 Oct** |
| 6 | Image file | Windows → Linux | closed 13 Oct (owner: image copy/paste resolved) |
| 7 | **Video file** | Windows → Linux | **fixed (streaming folder), waiting for retest** |
| 8 | Folder, zip | Windows → Linux | waiting |

Image pixels and image files are different rungs because they take different paths:
pixels go through the clipboard itself, while a file is fetched over the file lane when
you paste.

**For every rung the owner notes:**
1. What was copied, and roughly how big it was.
2. Where it was pasted (Paint, Word, a folder in Explorer, the desktop…).
3. The time of the copy and the time of the paste, to the minute.
4. What happened, and what should have happened.

Claude then reads both machines' logs for that minute and records here, under the rung:
what happened, the cause, the agreed behaviour, the fix, and the owner's sign-off.

#### Rung 5 — image pixels, Windows → Linux (tested first, by the owner's choice)

- **Owner's report (13 Oct):** a Windows screenshot showed up in the panel, but Ctrl+V in
  Thunar did nothing. It only pasted after clicking the image in the panel's history.
- **Logs:** 14:44:57, a 1920×1080 image (1.3 MB) arrived over the file lane, was verified and
  applied, as pixels only. 14:45:15, the history click published "pixels and a file"; Thunar's
  Paste then worked.
- **Cause:** two receive paths, both pixel-only. The inline path (small images) never
  staged a file. The bulk path's completion handler was passed `None` for the cache, so it
  silently fell back to pixels. Only the history click did both.
- **Agreed behaviour:** copy on one machine, paste on the other, anywhere, with nothing to click.
  A received image pastes as a picture into apps and as a `.png` into a folder.
- **Fix:** `write_received_image` in `mouse-clipboard` now does this for every received image,
  and logs which forms it offered and why. It applies on both OSes.
- **Sign-off:** owner, 13 Oct — a Windows screenshot pasted straight into a Thunar folder with Ctrl+V.

#### Rung 1a — image pixels, Linux → Windows

- Same cause and fix as rung 5. Signed off by the owner on 13 Oct, after the laptop update.

#### Rung 7 — video file, Windows → Linux (in progress, 13 Oct)

- **Owner's report:** a 3.1 GB `.mkv`. Ctrl+V in Thunar said there was nothing to paste; the panel
  showed "over your 16 MB limit, Bring it over". After fetching, the file had to be clicked in
  the history before it pasted.
- **Logs (Kali):** 15:58:11 described on the clipboard (Paste enabled). 15:58:12 Thunar's paste
  started the fetch and was made to *wait*. 16:01:43 all 3.1 GB arrived and was verified, but
  Thunar had given up after its ~30 s selection timeout. At 15:58:40 a screenshot replaced the
  clipboard, which is why the history click was needed afterwards.
- **Cause:** "paste waits for the bytes" only works for transfers under ~30 s. Speed: the file lane
  is capped at 16 MB/s while the pointer is on the receiving machine (`bulk_pause_while_crossed`),
  so 3.1 GB took about 3 min 17 s.
- **Agreed behaviour (owner):** Ctrl+C / Ctrl+V for files of any size (10, 20, 30 GB) exactly like
  images. Keyboard and mouse never stall. The size limit only *warns*, it never blocks the paste,
  and is configurable in Settings. Clicking "yes" must never require a second copy.
- **Fix (built 13 Oct, running on Kali):** `mouse-clipboard/src/stream_fs.rs`, as planned below.
  Tests: tree building, waiting reads with verified tail, and an `#[ignore]` real-mount test
  (`cargo test -p mouse-clipboard -- --ignored mounted`) that copies a file out while it is still
  arriving. The panel prompt is now a heads-up on Linux. Not yet done: the 16 MB/s crossed cap,
  and Windows as the receiver (rung 2: its paste still waits for the whole file).
- **Design:** a read-only FUSE folder at `$XDG_RUNTIME_DIR/mouse-rs/incoming`
  (crate `fuser` 0.18, MIT, no libfuse, mounts via setuid `fusermount3`, verified present). Remote
  files are published from there as a *copy*, so Thunar's paste starts at once and shows its own
  progress. Reads are answered from `blobs/<hash>.part` as it grows (replies sent from a waiter
  thread, so nothing blocks the filesystem). The final bytes are withheld until the cache has
  verified the file, so a corrupt transfer fails the copy. Opening a file raises the existing
  `PasteSignal` to start the fetch. Hook points in `mouse-clipboard/src/lib.rs`: the `NeedsConsent`
  and `Remote` branches of `ClipboardFiles`, `reuse_entry`'s incomplete-files branch, and the
  completion handler (skip re-publishing while a FUSE paste is live). Fallback when FUSE is
  unavailable: today's behaviour. Then revisit the 16 MB/s crossed cap, and make the consent
  prompt a warning.
