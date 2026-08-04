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
        match formats::FileList.write_clipboard(strings.as_slice()) {
            Ok(()) => true,
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
    use std::path::PathBuf;
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
        let uris: Vec<String> = list
            .paths
            .iter()
            .map(|p| format!("file://{}", percent_encode(&p.to_string_lossy())))
            .collect();
        if uris.is_empty() {
            return false;
        }
        let uri_list = format!("{}\r\n", uris.join("\r\n"));
        let gnome = format!("copy\n{}", uris.join("\n"));

        std::thread::Builder::new()
            .name("clip-files-owner".into())
            .spawn(move || serve_selection(uri_list, gnome))
            .is_ok()
    }

    /// Own the selection until someone else takes it. Returns when we lose ownership,
    /// which is exactly when the user copies something else — including via our own
    /// text/image path.
    fn serve_selection(uri_list: String, gnome: String) {
        let Some(x) = open() else { return };
        let Some(targets_atom) = intern(&x.conn, "TARGETS") else { return };
        let mut serve: Vec<(u32, Vec<u8>)> = Vec::new();
        for name in FILE_TARGETS {
            if let Some(a) = intern(&x.conn, name) {
                let body = if *name == "text/uri-list" { &uri_list } else { &gnome };
                serve.push((a, body.as_bytes().to_vec()));
            }
        }
        let utf8 = intern(&x.conn, "UTF8_STRING");

        if x.conn
            .set_selection_owner(x.window, x.clipboard, x11rb::CURRENT_TIME)
            .is_err()
        {
            return;
        }
        let _ = x.conn.flush();

        loop {
            let Ok(event) = x.conn.wait_for_event() else { return };
            match event {
                Event::SelectionRequest(req) => {
                    let ok = answer(&x, &req, targets_atom, utf8, &serve, &uri_list);
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
    use super::FileList;
    pub fn read_files() -> Option<FileList> {
        None
    }
    pub fn write_files(_list: &FileList) -> bool {
        false
    }
}

/// Read a file list from the system clipboard, if it holds one.
pub fn read_files() -> Option<FileList> {
    imp::read_files()
}

/// Put a file list on the system clipboard.
pub fn write_files(list: &FileList) -> bool {
    imp::write_files(list)
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
