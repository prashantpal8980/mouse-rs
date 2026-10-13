//! Reading and writing **file lists** on the system clipboard.
//!
//! `arboard` handles text and images and stops there — its whole API is `get_text`,
//! `set_text`, `set_html`, `get_image`, `set_image`. File lists are a different
//! clipboard format on both platforms, so they need their own code.
//!
//! | | Windows | Linux / X11 |
//! |---|---|---|
//! | Format | `CF_HDROP` + `Preferred DropEffect` | `text/uri-list`, `x-special/*-copied-files` |
//! | Via | `clipboard-win` (BSL‑1.0, permissive) | `x11rb`, already a dependency |
//!
//! ## What is deliberately *not* done here
//!
//! Delayed rendering — where pressing Ctrl+V is what triggers the transfer — needs
//! `IDataObject` and COM on Windows, which is a large surface for a small gain. Instead
//! the bytes are fetched into the cache *first* and then an ordinary path is put on the
//! clipboard. Every application already understands that, and clicking a history entry
//! is a natural place for the fetch to happen.

use std::path::PathBuf;

/// How a paste should behave, and what to rebuild if it consumed the file.
///
/// ## Why "move" is the interesting one
///
/// A paste is the file manager copying from whatever path we hand it, and it never
/// tells us where it is pasting to. So we cannot write the bytes to the destination
/// ourselves — but we do not have to. Publishing the selection as a **cut** makes the
/// file manager *relocate* our file instead of duplicating it, and a relocation within
/// one filesystem is a rename: no bytes move, and the file ends up exactly where the
/// user pressed Ctrl+V. That is how a 40 GB paste costs nothing instead of 40 GB.
///
/// What we publish is a hard link to the cached blob, so the move takes the link while
/// the cache keeps the data. `relink` is what lets a second paste work: after the first
/// the link is gone, and X11 asks us live for every paste, so it can be recreated.
/// Shared between the selection owner and the clipboard thread, so a paste can *start*
/// a transfer rather than requiring one to have already happened.
///
/// Copying something on the other machine should cost nothing here — only its
/// description crosses. The bytes are wanted at exactly one moment: when the user
/// pastes. X11 asks the selection owner live for every paste, so that moment is
/// observable, and this is how it is reported to the side that can act on it.
#[derive(Debug, Default)]
pub struct PasteSignal {
    /// Set by the owner thread when a paste asked for files we do not hold yet.
    pub wanted: std::sync::atomic::AtomicBool,
    /// Set by the clipboard thread when it has given up, so the owner stops waiting.
    pub failed: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Default)]
pub struct Publish {
    /// Move rather than copy, where the platform can express it.
    pub move_on_paste: bool,
    /// `(published path, blob to relink it from)`, so a consumed path can be rebuilt.
    pub relink: Vec<(PathBuf, PathBuf)>,
    /// Pixels to advertise alongside the files, for a copied image.
    pub png: Option<Vec<u8>>,
    /// Raised when a paste needs content we have not fetched yet.
    pub on_paste: Option<std::sync::Arc<PasteSignal>>,
}

/// A file list on the clipboard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileList {
    pub paths: Vec<PathBuf>,
}

impl FileList {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// A fingerprint of a file list, for the same "is this what I already saw?" question
/// `last`/`last_img` answer for text and images. Paths only — hashing the contents on
/// every poll would read the whole file twice a second.
pub fn list_key(list: &FileList) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in &list.paths {
        p.hash(&mut h);
    }
    list.paths.len().hash(&mut h);
    h.finish()
}

// ----------------------------------------------------------------------------
// Windows
// ----------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    /// The clipboard's change counter, unless this process made the latest change.
    pub fn foreign_copy_stamp() -> Option<u64> {
        use windows::Win32::System::DataExchange::{GetClipboardOwner, GetClipboardSequenceNumber};
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        // SAFETY: read-only queries about the clipboard and a window handle.
        unsafe {
            if let Ok(owner) = GetClipboardOwner() {
                let mut pid = 0u32;
                GetWindowThreadProcessId(owner, Some(&mut pid));
                if pid == GetCurrentProcessId() {
                    return None;
                }
            }
            Some(u64::from(GetClipboardSequenceNumber()))
        }
    }

    use super::FileList;
    use std::path::PathBuf;

    /// Read `CF_HDROP` off the clipboard.
    pub fn read_files() -> Option<FileList> {
        use clipboard_win::{formats, get_clipboard};
        // Opening the clipboard can fail simply because another process holds it for a
        // moment; that is normal on a poll and not worth logging every time.
        let paths: Vec<PathBuf> = get_clipboard(formats::FileList).ok()?;
        if paths.is_empty() {
            return None;
        }
        Some(FileList { paths })
    }

    /// Add `CF_HDROP` **without** clearing what is already on the clipboard.
    ///
    /// This is what lets one clipboard hold an image *and* a file at the same time: the
    /// pixels for a browser or a chat window, the file for Explorer. Writing the file
    /// normally would empty the clipboard first and destroy the image, which is why
    /// this needed a separate path rather than a second ordinary write.
    pub fn add_files_keeping_rest(list: &FileList) -> bool {
        use clipboard_win::{options, raw, Clipboard};
        let strings: Vec<String> = list
            .paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let _clip = match Clipboard::new_attempts(10) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("could not open the clipboard to add files: {e}");
                return false;
            }
        };
        match raw::set_file_list_with(strings.as_slice(), options::NoClear) {
            Ok(()) => {
                tracing::info!(
                    count = strings.len(),
                    paths = %strings.join(" | "),
                    "ADDED files alongside the image on the Windows clipboard — pasteable in Explorer and in a browser"
                );
                true
            }
            Err(e) => {
                tracing::warn!("could not add files to the clipboard: {e}");
                false
            }
        }
    }

    /// Publish a file list, optionally asking Explorer to *move* rather than copy.
    ///
    /// Explorer reads the `Preferred DropEffect` format to decide. `DROPEFFECT_MOVE`
    /// makes it relocate the file we published — a rename within the same volume, so a
    /// large paste costs nothing and lands where the user pressed Ctrl+V, instead of
    /// duplicating gigabytes out of our cache.
    pub fn publish(list: &FileList, how: super::Publish) -> bool {
        use clipboard_win::{formats, raw, Clipboard, Setter};
        const DROPEFFECT_MOVE: u32 = 2;

        let strings: Vec<String> = list
            .paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let _clip = match Clipboard::new_attempts(10) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("could not open the clipboard to publish files: {e}");
                return false;
            }
        };
        if let Err(e) = formats::FileList.write_clipboard(strings.as_slice()) {
            tracing::warn!("could not put files on the clipboard: {e}");
            return false;
        }
        if how.move_on_paste {
            match raw::register_format("Preferred DropEffect") {
                Some(fmt) => {
                    let effect = DROPEFFECT_MOVE.to_le_bytes();
                    if let Err(e) = raw::set_without_clear(fmt.get(), &effect) {
                        // Not fatal: the paste still works, it just copies.
                        tracing::warn!("could not ask Explorer to move rather than copy: {e}");
                    }
                }
                None => tracing::warn!("could not register the drop-effect format"),
            }
        }
        tracing::info!(
            count = strings.len(),
            mode = if how.move_on_paste { "move" } else { "copy" },
            paths = %strings.join(" | "),
            "PUBLISHED files to the Windows clipboard (CF_HDROP) — Explorer's Paste should now be enabled"
        );
        true
    }

    /// Put a file list on the clipboard as `CF_HDROP`, so Explorer pastes real files.
    ///
    /// `set_clipboard` cannot be used here: `FileList`'s `Setter` is implemented for the
    /// unsized `[T]`, so the guard is taken explicitly and the slice written through it.
    pub fn write_files(list: &FileList) -> bool {
        use clipboard_win::{formats, Clipboard, Setter};
        let strings: Vec<String> = list
            .paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let _clip = match Clipboard::new_attempts(10) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("could not open the clipboard to publish files: {e}");
                return false;
            }
        };
        // Every path published is named. Explorer greying out Paste after a
        // "successful" transfer is otherwise impossible to tell apart from a transfer
        // that never happened, and both look like silence from here.
        match formats::FileList.write_clipboard(strings.as_slice()) {
            Ok(()) => {
                tracing::info!(
                    count = strings.len(),
                    paths = %strings.join(" | "),
                    "PUBLISHED files to the Windows clipboard (CF_HDROP) — Explorer's Paste should now be enabled"
                );
                true
            }
            Err(e) => {
                tracing::warn!("could not put files on the clipboard: {e}");
                false
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Linux / X11
// ----------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::FileList;
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
        SelectionNotifyEvent, SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
    };
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;
    use x11rb::COPY_DEPTH_FROM_PARENT;

    /// How long to wait for the current selection owner to answer. A well-behaved app
    /// replies in microseconds; anything slower than this is a hung owner, and blocking
    /// the clipboard thread on it would stall clipboard sync entirely.
    const REPLY_TIMEOUT: Duration = Duration::from_millis(400);

    /// The desktop-specific "these files were copied" formats, in the order we prefer
    /// them. GNOME's carries an explicit copy/cut verb; `text/uri-list` is the common
    /// denominator every file manager understands.
    const FILE_TARGETS: &[&str] = &[
        "x-special/gnome-copied-files",
        "x-special/mate-copied-files",
        "x-special/KDE-copied-files",
        "text/uri-list",
    ];

    struct X {
        conn: RustConnection,
        window: u32,
        clipboard: u32,
        prop: u32,
    }

    fn open() -> Option<X> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let screen = &conn.setup().roots[screen_num];
        let window = conn.generate_id().ok()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .ok()?;
        let clipboard = intern(&conn, "CLIPBOARD")?;
        let prop = intern(&conn, "MOUSE_RS_CLIP")?;
        conn.flush().ok()?;
        Some(X { conn, window, clipboard, prop })
    }

    fn intern(conn: &RustConnection, name: &str) -> Option<u32> {
        conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok().map(|r| r.atom)
    }

    /// Ask the current selection owner for one of the file targets.
    pub fn read_files() -> Option<FileList> {
        let x = open()?;
        for target_name in FILE_TARGETS {
            let Some(target) = intern(&x.conn, target_name) else { continue };
            if x.conn
                .convert_selection(x.window, x.clipboard, target, x.prop, x11rb::CURRENT_TIME)
                .is_err()
            {
                continue;
            }
            let _ = x.conn.flush();

            let deadline = Instant::now() + REPLY_TIMEOUT;
            loop {
                if Instant::now() > deadline {
                    break;
                }
                match x.conn.poll_for_event() {
                    Ok(Some(Event::SelectionNotify(ev))) if ev.requestor == x.window => {
                        if ev.property == 0 {
                            break; // owner has no such target: try the next one
                        }
                        let reply = x
                            .conn
                            .get_property(true, x.window, x.prop, AtomEnum::ANY, 0, u32::MAX / 4)
                            .ok()
                            .and_then(|c| c.reply().ok());
                        if let Some(r) = reply {
                            if let Some(list) = parse_uri_list(&r.value) {
                                return Some(list);
                            }
                        }
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        }
        None
    }

    /// Who owns the clipboard and when they took it, unless the owner is us.
    ///
    /// Every copy re-claims the selection with a fresh timestamp, even of exactly the same
    /// files, so this changes on each Ctrl+C where the file list alone does not.
    pub fn foreign_copy_stamp() -> Option<u64> {
        let x = open()?;
        let owner = x.conn.get_selection_owner(x.clipboard).ok()?.reply().ok()?.owner;
        if owner == 0 {
            return None;
        }
        // Our own file publications: not a copy the user made.
        if HELD.lock().ok().and_then(|h| h.as_ref().map(|h| h.window)) == Some(owner) {
            return None;
        }
        let stamp = intern(&x.conn, "TIMESTAMP").and_then(|target| {
            x.conn.convert_selection(x.window, x.clipboard, target, x.prop, x11rb::CURRENT_TIME).ok()?;
            x.conn.flush().ok()?;
            let deadline = Instant::now() + REPLY_TIMEOUT;
            while Instant::now() < deadline {
                match x.conn.poll_for_event() {
                    Ok(Some(Event::SelectionNotify(ev))) if ev.requestor == x.window => {
                        if ev.property == 0 {
                            return None;
                        }
                        let r = x
                            .conn
                            .get_property(true, x.window, x.prop, AtomEnum::ANY, 0, 1)
                            .ok()?
                            .reply()
                            .ok()?;
                        return r.value.get(..4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => return None,
                }
            }
            None
        });
        Some((u64::from(owner) << 32) | u64::from(stamp.unwrap_or(0)))
    }

    /// Ask for the content and wait for it to land, so a paste of something not yet
    /// transferred behaves like a slow copy rather than an empty one.
    ///
    /// The wait is generous because the alternative is worse: answering with paths that
    /// do not exist yet makes the file manager paste nothing, or half a file. Bounded
    /// all the same — a peer that has gone away must not leave the paste hanging for
    /// ever.
    pub(super) fn fetch_for_paste(sig: &super::PasteSignal, paths: &[PathBuf]) {
        use std::sync::atomic::Ordering;
        let missing = || paths.iter().any(|p| !p.exists());
        if !missing() {
            return;
        }
        sig.failed.store(false, Ordering::SeqCst);
        sig.wanted.store(true, Ordering::SeqCst);
        tracing::info!(count = paths.len(), "paste asked for content we do not hold yet; fetching");

        let deadline = Instant::now() + PASTE_FETCH_TIMEOUT;
        while Instant::now() < deadline {
            if !missing() {
                tracing::info!("content arrived; answering the paste");
                return;
            }
            if sig.failed.load(Ordering::SeqCst) {
                tracing::warn!("the fetch failed; answering the paste with what exists");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::warn!("gave up waiting for the paste's content");
    }

    /// Longest a paste will wait for content to arrive.
    ///
    /// Ten minutes sounds absurd until the file is 7 GB on a slow link — and a paste
    /// that gives up early is worse than one that takes a while, because it silently
    /// produces nothing.
    const PASTE_FETCH_TIMEOUT: Duration = Duration::from_secs(600);

    /// Parse either `text/uri-list` or the `copy\nfile:///…` GNOME variant.
    pub(super) fn parse_uri_list(bytes: &[u8]) -> Option<FileList> {
        let text = String::from_utf8_lossy(bytes);
        let mut paths = Vec::new();
        for line in text.split(['\r', '\n']) {
            let line = line.trim().trim_end_matches('\0');
            // The GNOME format's first line is the verb ("copy" / "cut"), not a URI.
            if line.is_empty() || line == "copy" || line == "cut" || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("file://") {
                // Strip an (empty) host component, then percent-decode.
                let raw = rest.strip_prefix("localhost").unwrap_or(rest);
                paths.push(PathBuf::from(percent_decode(raw)));
            }
        }
        if paths.is_empty() {
            None
        } else {
            Some(FileList { paths })
        }
    }

    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub(super) fn percent_encode(path: &str) -> String {
        let mut out = String::with_capacity(path.len());
        for b in path.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// Take ownership of the CLIPBOARD selection and serve the file targets from a
    /// background thread.
    ///
    /// X11 has no clipboard *storage*: the owning process answers every paste, live. So
    /// publishing a file list means owning the selection and staying available to
    /// answer — and it also means the list dies when mouse-rs exits, which is ordinary
    /// X11 behaviour rather than something we can fix here.
    pub fn write_files(list: &FileList) -> bool {
        write_files_with_image(list, None)
    }

    /// Publish files, and optionally the same content as an image, from one owner.
    ///
    /// One X11 selection owner can advertise as many targets as it likes, so serving
    /// `image/png` alongside the file targets means a single copy pastes as pixels into
    /// a browser and as a file into Thunar. Doing it in two separate calls could not
    /// work: the second owner would take the selection from the first.
    pub fn write_files_with_image(list: &FileList, png: Option<Vec<u8>>) -> bool {
        publish(list, super::Publish { png, ..Default::default() })
    }

    /// The file list this process currently serves on the selection, kept so it can be
    /// handed to a holder process at exit (see [`handoff`]). `id` ties the record to one
    /// publish, so an owner thread that loses the selection clears only its own record,
    /// never the one from a newer publish. `window` is the X window that owns the
    /// selection; `handoff` checks the server still says it does before acting.
    struct Held {
        id: u64,
        window: u32,
        verb: &'static str,
        paths: Vec<PathBuf>,
    }

    static HELD: Mutex<Option<Held>> = Mutex::new(None);
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    /// The program and arguments that start a holder, set once by the application.
    static HOLDER: OnceLock<(PathBuf, Vec<String>)> = OnceLock::new();

    /// The two bodies a file list is served as: the `text/uri-list` form and the GNOME
    /// form, whose first line is the paste verb.
    fn bodies(paths: &[PathBuf], verb: &str) -> (String, String) {
        let uris: Vec<String> = paths
            .iter()
            .map(|p| format!("file://{}", percent_encode(&p.to_string_lossy())))
            .collect();
        let uri_list = format!("{}\r\n", uris.join("\r\n"));
        let gnome = format!("{verb}\n{}", uris.join("\n"));
        (uri_list, gnome)
    }

    /// Publish a file list with a paste mode and relink data.
    pub fn publish(list: &FileList, how: super::Publish) -> bool {
        if list.paths.is_empty() {
            return false;
        }
        // The verb the file manager obeys. "cut" makes it relocate our hard link rather
        // than duplicate the data — the whole point on a large file.
        let verb = if how.move_on_paste { "cut" } else { "copy" };
        let (uri_list, gnome) = bodies(&list.paths, verb);
        let png = how.png;
        let relink = how.relink;
        let on_paste = how.on_paste;
        let wanted_paths = list.paths.clone();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut held) = HELD.lock() {
            *held = Some(Held { id, window: 0, verb, paths: wanted_paths.clone() });
        }

        std::thread::Builder::new()
            .name("clip-files-owner".into())
            .spawn(move || {
                serve_selection(id, uri_list, gnome, png, relink, on_paste, wanted_paths);
                forget_held(id);
            })
            .is_ok()
    }

    /// Record which window now owns the selection, for this publish only.
    fn record_window(id: u64, window: u32) {
        if let Ok(mut held) = HELD.lock() {
            if let Some(h) = held.as_mut().filter(|h| h.id == id) {
                h.window = window;
            }
        }
    }

    /// Drop the record for `id` once its owner thread has given up the selection.
    fn forget_held(id: u64) {
        if let Ok(mut held) = HELD.lock() {
            if held.as_ref().is_some_and(|h| h.id == id) {
                *held = None;
            }
        }
    }

    /// Set the program and arguments that start a holder. Called once at startup by the
    /// application; without it [`handoff`] does nothing, so tests never spawn anything.
    pub fn set_holder(program: PathBuf, args: Vec<String>) {
        let _ = HOLDER.set((program, args));
    }

    /// Hand the file list we serve to a detached holder process, so it stays pasteable
    /// after mouse-rs exits. Does nothing unless a holder is set, we still own the
    /// selection, and every file is still on disk.
    pub fn handoff() {
        let Some((program, args)) = HOLDER.get() else { return };
        let held = match HELD.lock() {
            Ok(mut g) => g.take(),
            Err(_) => return,
        };
        let Some(held) = held else { return };
        if held.paths.is_empty() || !held.paths.iter().all(|p| p.exists()) {
            tracing::info!("not handing the clipboard files on: some are no longer on disk");
            return;
        }
        let Some(x) = open() else { return };
        let owner = x
            .conn
            .get_selection_owner(x.clipboard)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.owner);
        if owner != Some(held.window) {
            // Someone else copied after us; their selection is theirs to keep.
            return;
        }
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&encode_payload(held.verb, &held.paths));
                    // Dropping stdin closes it, which is how the holder knows the list is all in.
                }
                // Reaped in the background so it does not linger as a zombie while we run.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                tracing::info!(files = held.paths.len(), "handed the file clipboard to a holder, so it survives exit");
            }
            Err(e) => tracing::warn!("could not start a clipboard holder: {e}"),
        }
    }

    /// The holder's input: the paste verb on the first line, then the paths separated by
    /// NUL. A NUL cannot occur in a path, so nothing needs escaping.
    fn encode_payload(verb: &str, paths: &[PathBuf]) -> Vec<u8> {
        let mut out = verb.as_bytes().to_vec();
        out.push(b'\n');
        for (i, p) in paths.iter().enumerate() {
            if i > 0 {
                out.push(0);
            }
            out.extend_from_slice(p.as_os_str().as_bytes());
        }
        out
    }

    /// The holder process: serve the listed files until another copy takes the selection.
    /// Returns then, so an abandoned holder never outlives the copy that replaced it.
    pub fn hold_from_payload(payload: &[u8]) {
        let (verb_bytes, rest) = match payload.iter().position(|b| *b == b'\n') {
            Some(i) => (&payload[..i], &payload[i + 1..]),
            None => return,
        };
        // Only the two verbs we ever publish; anything else is served as a copy.
        let verb = if verb_bytes == b"cut" { "cut" } else { "copy" };
        let paths: Vec<PathBuf> = rest
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(std::ffi::OsStr::from_bytes(s)))
            .collect();
        if paths.is_empty() {
            return;
        }
        let (uri_list, gnome) = bodies(&paths, verb);
        // id 0 is never recorded, so the holder keeps no `HELD` record of its own.
        serve_selection(0, uri_list, gnome, None, Vec::new(), None, paths);
    }

    /// Own the selection until someone else takes it. Returns when we lose ownership,
    /// which is exactly when the user copies something else — including via our own
    /// text/image path.
    fn serve_selection(
        id: u64,
        uri_list: String,
        gnome: String,
        png: Option<Vec<u8>>,
        relink: Vec<(PathBuf, PathBuf)>,
        on_paste: Option<std::sync::Arc<super::PasteSignal>>,
        wanted_paths: Vec<PathBuf>,
    ) {
        let Some(x) = open() else { return };
        let Some(targets_atom) = intern(&x.conn, "TARGETS") else { return };
        let mut serve: Vec<(u32, Vec<u8>)> = Vec::new();
        for name in FILE_TARGETS {
            if let Some(a) = intern(&x.conn, name) {
                let body = if *name == "text/uri-list" { &uri_list } else { &gnome };
                serve.push((a, body.as_bytes().to_vec()));
            }
        }
        // The image target, when this copy carries pixels as well as a path. Same
        // owner, same selection — that is what lets one copy satisfy both a browser and
        // a file manager.
        if let Some(bytes) = png {
            for name in ["image/png", "image/x-png"] {
                if let Some(a) = intern(&x.conn, name) {
                    serve.push((a, bytes.clone()));
                }
            }
        }
        let utf8 = intern(&x.conn, "UTF8_STRING");

        if let Err(e) = x
            .conn
            .set_selection_owner(x.window, x.clipboard, x11rb::CURRENT_TIME)
        {
            tracing::warn!("could not take the X11 clipboard to publish files: {e}");
            return;
        }
        let _ = x.conn.flush();
        record_window(id, x.window);
        tracing::info!(
            targets = FILE_TARGETS.len(),
            // The verb the file manager acts on. `cut` should make it relocate our hard
            // link — a rename, costing nothing. If a paste of this shows a copy progress
            // dialog instead, the file manager declined to honour it, and that is the
            // difference between a large paste being instant and taking its full size
            // again on disk.
            verb = %gnome.lines().next().unwrap_or("?"),
            uris = %uri_list.replace(['\r', '\n'], " "),
            "PUBLISHED files to the X11 clipboard — Thunar's Paste should now be enabled"
        );

        loop {
            let Ok(event) = x.conn.wait_for_event() else { return };
            match event {
                Event::SelectionRequest(req) => {
                    // Only a request for the *file* targets is a paste. Everything else
                    // is a passer-by: a clipboard manager asks for TARGETS the instant a
                    // selection changes, and XFCE's does exactly that — which raised this
                    // signal one millisecond after every publish, so a 4 GB transfer
                    // began without anybody pasting anything, and the entry left
                    // "waiting for you" so fast the panel never showed the prompt at all.
                    // That is why the banner appeared on one machine and not the other:
                    // one of them runs a clipboard manager.
                    let wants_the_files = serve.iter().any(|(a, _)| *a == req.target);
                    if wants_the_files {
                        if let Some(sig) = &on_paste {
                            fetch_for_paste(sig, &wanted_paths);
                        }
                    }
                    // A previous paste may have *moved* the file we published. Rebuild
                    // the link before answering, so pasting the same item into a second
                    // place works exactly like the first.
                    for (link, blob) in &relink {
                        if !link.exists() && blob.exists() {
                            if let Some(parent) = link.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            match std::fs::hard_link(blob, link) {
                                Ok(()) => tracing::debug!(path = %link.display(), "relinked for another paste"),
                                Err(e) => tracing::warn!(path = %link.display(), "could not relink: {e}"),
                            }
                        }
                    }
                    let ok = answer(&x, &req, targets_atom, utf8, &serve, &uri_list);
                    // Which target a file manager actually asked for, and whether we
                    // could answer it. A paste that silently does nothing is usually a
                    // target we do not serve, and this is the only place that shows it.
                    tracing::debug!(target = req.target, answered = ok, "a paste asked us for the file list");
                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: req.time,
                        requestor: req.requestor,
                        selection: req.selection,
                        target: req.target,
                        property: if ok { req.property } else { 0 },
                    };
                    let _ = x.conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
                    let _ = x.conn.flush();
                }
                // Someone else took the clipboard: our list is superseded, and holding
                // the thread open would leak one per copy.
                Event::SelectionClear(_) => return,
                _ => {}
            }
        }
    }

    fn answer(
        x: &X,
        req: &SelectionRequestEvent,
        targets_atom: u32,
        utf8: Option<u32>,
        serve: &[(u32, Vec<u8>)],
        uri_list: &str,
    ) -> bool {
        if req.property == 0 {
            return false;
        }
        if req.target == targets_atom {
            let mut atoms: Vec<u32> = vec![targets_atom];
            atoms.extend(serve.iter().map(|(a, _)| *a));
            if let Some(u) = utf8 {
                atoms.push(u);
            }
            return x
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    req.requestor,
                    req.property,
                    AtomEnum::ATOM,
                    &atoms,
                )
                .is_ok();
        }
        // A plain-text paste of a file selection should give the paths, not nothing.
        if Some(req.target) == utf8 {
            return x
                .conn
                .change_property8(
                    PropMode::REPLACE,
                    req.requestor,
                    req.property,
                    req.target,
                    uri_list.as_bytes(),
                )
                .is_ok();
        }
        for (atom, body) in serve {
            if *atom == req.target {
                return x
                    .conn
                    .change_property8(PropMode::REPLACE, req.requestor, req.property, req.target, body)
                    .is_ok();
            }
        }
        false
    }

    // Silences an unused warning on builds where nothing reads it.
    #[allow(dead_code)]
    fn _unused(_: Property) {}
}

// ----------------------------------------------------------------------------
// Anything else: no file clipboard, and say so once rather than pretend.
// ----------------------------------------------------------------------------

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
mod imp {
    pub fn foreign_copy_stamp() -> Option<u64> {
        None
    }

    use super::FileList;
    pub fn read_files() -> Option<FileList> {
        None
    }
    pub fn write_files_with_image(_list: &FileList, _png: Option<Vec<u8>>) -> bool {
        false
    }
    pub fn publish(_list: &FileList, _how: super::Publish) -> bool {
        false
    }
    pub fn add_files_keeping_rest(_list: &FileList) -> bool {
        false
    }
    pub fn write_files(_list: &FileList) -> bool {
        false
    }
}

/// Read a file list from the system clipboard, if it holds one.
pub fn read_files() -> Option<FileList> {
    imp::read_files()
}

/// Changes every time another program copies something, even the very same files; `None`
/// when this process made the latest change (or it cannot be told). Lets a second Ctrl+C
/// on the same file be noticed, which comparing the file list never could.
pub fn foreign_copy_stamp() -> Option<u64> {
    imp::foreign_copy_stamp()
}

/// Put a file list on the system clipboard.
pub fn write_files(list: &FileList) -> bool {
    imp::write_files(list)
}

/// The plain-text form a published file list also answers with.
///
/// The selection owner serves this for `UTF8_STRING`, so a paste into a text editor
/// gives the paths rather than nothing. That is useful — and it is also read straight
/// back by our own text poll, which then records it as something the user copied and
/// syncs a `file:///…/cache/trees/…` string to the other machine. Knowing the exact
/// string is what lets that be suppressed instead of appearing as junk history.
pub fn uri_text(list: &FileList) -> String {
    // Encoded here rather than borrowed from the X11 module: the same string has to be
    // computable on Windows, where that module does not exist.
    let enc = |path: &str| -> String {
        let mut out = String::with_capacity(path.len());
        for b in path.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    };
    let uris: Vec<String> = list
        .paths
        .iter()
        .map(|p| format!("file://{}", enc(&p.to_string_lossy())))
        .collect();
    format!("{}\r\n", uris.join("\r\n"))
}

/// Put a file list on the clipboard with an explicit paste mode.
///
/// `move_on_paste` is what makes a large file cost nothing to paste: the file manager
/// relocates the hard link we published instead of duplicating the data behind it.
pub fn publish(list: &FileList, how: Publish) -> bool {
    imp::publish(list, how)
}

/// Name the program and arguments that start a clipboard holder. The application calls
/// this once at startup; the holder is the same binary run with a hidden subcommand that
/// calls [`hold_from_payload`].
#[allow(unused_variables)]
pub fn set_holder(program: std::path::PathBuf, args: Vec<String>) {
    #[cfg(all(unix, not(target_os = "macos")))]
    imp::set_holder(program, args);
}

/// Called as the clipboard shuts down. On X11, where a selection dies with its owner,
/// the file list we are serving is handed to a holder process so it stays pasteable.
/// Windows keeps the clipboard itself, so there is nothing to do there.
pub fn handoff_on_exit() {
    #[cfg(all(unix, not(target_os = "macos")))]
    imp::handoff();
}

/// The holder process: serve the file list in `payload` until another copy replaces it.
#[allow(unused_variables)]
pub fn hold_from_payload(payload: &[u8]) {
    #[cfg(all(unix, not(target_os = "macos")))]
    imp::hold_from_payload(payload);
}

/// Put an image on the clipboard **and** the same content as a file, so one copy can be
/// pasted into a browser or a chat window *and* into a file manager.
///
/// Neither Explorer nor Thunar can paste pixels — their Paste needs a path — while
/// browsers and chat apps want the pixels. Offering only one of the two forces the user
/// to know which they will need before they click, which is a question they should never
/// be asked. The two platforms reach the same result differently: X11 has one selection
/// owner advertising both targets; Windows writes the image first and then adds
/// `CF_HDROP` *without* clearing, because a normal write would empty the clipboard and
/// destroy the image it was meant to accompany.
///
/// `write_image_first` is the platform's ordinary image write, run between opening and
/// adding the file on Windows and ignored on X11, where the same owner serves both.
pub fn write_image_and_file(list: &FileList, png: Vec<u8>, write_image_first: impl FnOnce() -> bool) -> bool {
    #[cfg(windows)]
    {
        let _ = &png;
        // Order matters: the image write clears the clipboard, so it has to go first.
        if !write_image_first() {
            return false;
        }
        imp::add_files_keeping_rest(list)
    }
    #[cfg(not(windows))]
    {
        let _ = write_image_first;
        imp::write_files_with_image(list, Some(png))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_distinguishes_different_lists() {
        // The poll uses this to answer "is this the same selection I already handled?".
        // If it collided, a new copy would be silently skipped.
        let a = FileList { paths: vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")] };
        let b = FileList { paths: vec![PathBuf::from("/tmp/a")] };
        let c = FileList { paths: vec![PathBuf::from("/tmp/b"), PathBuf::from("/tmp/a")] };
        assert_ne!(list_key(&a), list_key(&b));
        assert_ne!(list_key(&a), list_key(&c), "order is part of the identity");
        assert_eq!(list_key(&a), list_key(&a.clone()));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn uri_lists_from_every_desktop_parse() {
        use super::imp;
        // GNOME's format leads with a verb line that is not a URI.
        let gnome = b"copy\nfile:///home/k/a.txt\nfile:///home/k/b.png";
        let got = imp::parse_uri_list(gnome).expect("gnome format");
        assert_eq!(got.paths, vec![PathBuf::from("/home/k/a.txt"), PathBuf::from("/home/k/b.png")]);

        // Plain text/uri-list, CRLF separated, with a comment line.
        let plain = b"#comment\r\nfile:///tmp/x.iso\r\n";
        assert_eq!(
            imp::parse_uri_list(plain).unwrap().paths,
            vec![PathBuf::from("/tmp/x.iso")]
        );

        // Percent-encoding must survive the round trip, or any file with a space in its
        // name arrives at the wrong path.
        let encoded = b"file:///home/k/my%20report%20(final).pdf";
        assert_eq!(
            imp::parse_uri_list(encoded).unwrap().paths,
            vec![PathBuf::from("/home/k/my report (final).pdf")]
        );

        // Non-file URIs are not files and must not be mistaken for them.
        assert!(imp::parse_uri_list(b"https://example.com/x").is_none());
        assert!(imp::parse_uri_list(b"").is_none());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn encoding_a_path_and_reading_it_back_gives_the_same_path() {
        use super::imp;
        for raw in [
            "/home/k/plain.txt",
            "/home/k/with space.txt",
            "/home/k/wíth-ünicode.md",
            "/home/k/100% done.txt",
            "/home/k/a#b?c.txt",
        ] {
            let uri = format!("file://{}", imp::percent_encode(raw));
            let back = imp::parse_uri_list(uri.as_bytes()).expect("parses");
            assert_eq!(back.paths, vec![PathBuf::from(raw)], "round trip failed for {raw}");
        }
    }
}

#[cfg(all(test, unix))]
mod paste_trigger_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// A clipboard manager asks for TARGETS the moment a selection changes, and XFCE's
    /// does. Treating that as a paste started a multi-gigabyte transfer one millisecond
    /// after every copy, and moved the entry out of "waiting for you" so fast the panel
    /// never showed the prompt — which is why the banner appeared on the machine without
    /// a clipboard manager and not on the one with it.
    ///
    /// The rule is simply: only a request for the file targets is a paste.
    #[test]
    fn only_a_request_for_the_file_targets_counts_as_a_paste() {
        // Stand-ins for interned atoms; the identity is all that matters here.
        const TARGETS: u32 = 1;
        const TIMESTAMP: u32 = 2;
        const SAVE_TARGETS: u32 = 3;
        const URI_LIST: u32 = 10;
        const GNOME_COPIED: u32 = 11;

        let serve: Vec<(u32, Vec<u8>)> = vec![(URI_LIST, vec![]), (GNOME_COPIED, vec![])];
        let is_paste = |target: u32| serve.iter().any(|(a, _)| *a == target);

        assert!(!is_paste(TARGETS), "a clipboard manager enumerating targets is not a paste");
        assert!(!is_paste(TIMESTAMP), "nor is a timestamp probe");
        assert!(!is_paste(SAVE_TARGETS), "nor is a manager offering to store the selection");
        assert!(is_paste(URI_LIST), "but asking for the files is");
        assert!(is_paste(GNOME_COPIED), "in either dialect");
    }

    /// And the signal itself must only fire when the content is genuinely absent, so a
    /// second paste of something already here answers immediately.
    #[test]
    fn nothing_is_fetched_when_the_files_are_already_present() {
        let dir = std::env::temp_dir().join("mouse-rs-paste-present");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let here = dir.join("already.mkv");
        std::fs::write(&here, b"present").unwrap();

        let sig = PasteSignal::default();
        imp::fetch_for_paste(&sig, &[here]);
        assert!(
            !sig.wanted.load(Ordering::SeqCst),
            "asking for content we already hold must not start a transfer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod handoff_tests {
    use super::*;

    /// Needs a live X display, so it is ignored by default. Run it with
    /// `cargo test -p mouse-clipboard handoff -- --ignored`.
    ///
    /// Publishes a file, then hands it off to a stand-in holder (`sh`) that writes the
    /// payload it receives to a file. The payload must name the verb and the file.
    #[test]
    #[ignore]
    fn a_file_list_is_handed_to_the_holder_while_we_still_own_the_selection() {
        let dir = std::env::temp_dir().join(format!("mouse-handoff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("held.txt");
        std::fs::write(&file, b"x").unwrap();
        let out = dir.join("payload");
        set_holder(
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), format!("cat > '{}'", out.display())],
        );

        assert!(publish(&FileList { paths: vec![file.clone()] }, Publish::default()));
        // Give the owner thread time to take the selection and record its window.
        std::thread::sleep(std::time::Duration::from_millis(500));
        handoff_on_exit();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let got = std::fs::read(&out).expect("the holder should have written the payload");
        assert_eq!(got, format!("copy\n{}", file.display()).into_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Needs a live X display and xclip: `cargo test -p mouse-clipboard -- --ignored recopy`.
    #[test]
    #[ignore]
    fn recopy_the_same_content_changes_the_stamp() {
        let copy = || {
            let mut c = std::process::Command::new("xclip")
                .args(["-selection", "clipboard", "-t", "text/uri-list"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("xclip");
            use std::io::Write as _;
            c.stdin.take().unwrap().write_all(b"file:///tmp/same.mkv\r\n").unwrap();
            c.wait().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
        };
        copy();
        let first = super::foreign_copy_stamp();
        let list_a = super::read_files();
        copy();
        let second = super::foreign_copy_stamp();
        let list_b = super::read_files();
        assert!(first.is_some() && second.is_some());
        assert_eq!(list_a, list_b, "the same files");
        assert_ne!(first, second, "but a new copy");
    }
}
