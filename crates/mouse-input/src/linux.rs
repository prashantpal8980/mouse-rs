//! Linux (X11) input backend.
//!
//! * Injection uses the XTEST extension (no special privileges, works on a
//!   normal X11 session).
//! * Capture+suppression grabs the pointer and keyboard and warps the cursor to
//!   screen centre, reading relative motion as the delta from centre (the same
//!   technique the Python version used, done natively). Releasing the grab
//!   restores normal local input.
//!
//! The capture loop never blocks indefinitely: it waits on the X connection fd
//! with a short timeout so a `set_suppress`/`stop` request is always honoured
//! promptly — even while grabbed with no input flowing. That responsiveness is
//! the Linux-side analogue of the Windows suppression failsafe.

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::mpsc::UnboundedSender;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as _, GrabMode, GrabStatus, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use mouse_core::protocol::Button;

use crate::event::InputEvent;
use crate::keys::{Key, NamedKey};
use crate::motion::{Motion, MotionAction, MotionGeom};
use crate::{CaptureSignal, InputCapture, InputError, InputInjector};

impl From<x11rb::errors::ConnectionError> for InputError {
    fn from(e: x11rb::errors::ConnectionError) -> Self {
        InputError::Backend(format!("x11 connection: {e}"))
    }
}
impl From<x11rb::errors::ReplyError> for InputError {
    fn from(e: x11rb::errors::ReplyError) -> Self {
        InputError::Backend(format!("x11 reply: {e}"))
    }
}
impl From<x11rb::errors::ConnectError> for InputError {
    fn from(e: x11rb::errors::ConnectError) -> Self {
        InputError::Backend(format!("x11 connect: {e}"))
    }
}

/// The root window size of the default screen, in pixels.
pub(crate) fn screen_size() -> Result<(i32, i32), InputError> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let s = &conn.setup().roots[screen_num];
    Ok((s.width_in_pixels as i32, s.height_in_pixels as i32))
}

// ---------------------------------------------------------------------------
// Keysym mapping (the X11-specific half of the cross-platform key identity).
// ---------------------------------------------------------------------------

/// Map a named key to its X11 keysym.
fn named_to_keysym(n: NamedKey) -> u32 {
    match n {
        NamedKey::Enter => 0xff0d,
        NamedKey::Escape => 0xff1b,
        NamedKey::Backspace => 0xff08,
        NamedKey::Tab => 0xff09,
        NamedKey::Space => 0x0020,
        NamedKey::Insert => 0xff63,
        NamedKey::Delete => 0xffff,
        NamedKey::Up => 0xff52,
        NamedKey::Down => 0xff54,
        NamedKey::Left => 0xff51,
        NamedKey::Right => 0xff53,
        NamedKey::Home => 0xff50,
        NamedKey::End => 0xff57,
        NamedKey::PageUp => 0xff55,
        NamedKey::PageDown => 0xff56,
        NamedKey::CapsLock => 0xffe5,
        NamedKey::NumLock => 0xff7f,
        NamedKey::ScrollLock => 0xff14,
        NamedKey::PrintScreen => 0xff61,
        NamedKey::Pause => 0xff13,
        NamedKey::Menu => 0xff67,
        NamedKey::ShiftL => 0xffe1,
        NamedKey::ShiftR => 0xffe2,
        NamedKey::CtrlL => 0xffe3,
        NamedKey::CtrlR => 0xffe4,
        NamedKey::AltL => 0xffe9,
        NamedKey::AltR => 0xffea,
        NamedKey::AltGr => 0xfe03,
        NamedKey::SuperL => 0xffeb,
        NamedKey::SuperR => 0xffec,
        NamedKey::F(k) => {
            if k <= 12 {
                0xffbe + (k as u32 - 1)
            } else {
                0xffca + (k as u32 - 13)
            }
        }
    }
}

/// Reverse: an X11 keysym to a named key, if it is one we name.
fn keysym_to_named(ks: u32) -> Option<NamedKey> {
    let n = match ks {
        0xff0d | 0xff8d => NamedKey::Enter, // Return or KP_Enter
        0xff1b => NamedKey::Escape,
        0xff08 => NamedKey::Backspace,
        0xff09 => NamedKey::Tab,
        0x0020 => NamedKey::Space,
        0xff63 => NamedKey::Insert,
        0xffff => NamedKey::Delete,
        0xff52 => NamedKey::Up,
        0xff54 => NamedKey::Down,
        0xff51 => NamedKey::Left,
        0xff53 => NamedKey::Right,
        0xff50 => NamedKey::Home,
        0xff57 => NamedKey::End,
        0xff55 => NamedKey::PageUp,
        0xff56 => NamedKey::PageDown,
        0xffe5 => NamedKey::CapsLock,
        0xff7f => NamedKey::NumLock,
        0xff14 => NamedKey::ScrollLock,
        0xff61 => NamedKey::PrintScreen,
        0xff13 => NamedKey::Pause,
        0xff67 => NamedKey::Menu,
        0xffe1 => NamedKey::ShiftL,
        0xffe2 => NamedKey::ShiftR,
        0xffe3 => NamedKey::CtrlL,
        0xffe4 => NamedKey::CtrlR,
        0xffe9 => NamedKey::AltL,
        0xffea => NamedKey::AltR,
        0xfe03 => NamedKey::AltGr,
        0xffeb => NamedKey::SuperL,
        0xffec => NamedKey::SuperR,
        0xffbe..=0xffc9 => NamedKey::F((ks - 0xffbe + 1) as u8),
        0xffca..=0xffd5 => NamedKey::F((ks - 0xffca + 13) as u8),
        _ => return None,
    };
    Some(n)
}

/// The keysym for a base character (used for injection).
fn char_to_keysym(c: char) -> u32 {
    let cp = c as u32;
    if (0x20..=0xff).contains(&cp) {
        cp // Latin-1 keysyms equal their codepoint
    } else {
        0x0100_0000 | cp // Unicode keysym
    }
}

/// A character from a keysym, if it represents one (used for capture).
fn keysym_to_char(ks: u32) -> Option<char> {
    if (0x20..=0x7e).contains(&ks) || (0xa0..=0xff).contains(&ks) {
        char::from_u32(ks)
    } else if (0x0100_0000..=0x0110_ffff).contains(&ks) {
        char::from_u32(ks & 0x00ff_ffff)
    } else {
        None
    }
}

/// Map an arbitrary keysym (named, char, or raw) to our wire `Key`.
fn keysym_to_key(ks: u32) -> Option<Key> {
    if ks == 0 {
        return None;
    }
    if let Some(n) = keysym_to_named(ks) {
        return Some(Key::Named(n));
    }
    if let Some(c) = keysym_to_char(ks) {
        return Some(Key::Char(c));
    }
    Some(Key::Raw(ks))
}

/// Map our wire `Key` to a keysym for injection.
fn key_to_keysym(key: &Key) -> u32 {
    match key {
        Key::Named(n) => named_to_keysym(*n),
        Key::Char(c) => char_to_keysym(*c),
        Key::Raw(u) => *u,
    }
}

// ---------------------------------------------------------------------------
// Keyboard map: keysym <-> (keycode, shift level), with a spare keycode for
// injecting keysyms the current layout doesn't have a key for (e.g. unicode).
// ---------------------------------------------------------------------------

struct KeyboardMap {
    min_keycode: u8,
    per: usize,
    keysyms: Vec<u32>, // flat, row-major: row r = keycode (min+r), per columns
    spare_keycode: u8, // a keycode with no symbols, reusable for remap
}

impl KeyboardMap {
    fn fetch(conn: &RustConnection) -> Result<KeyboardMap, InputError> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max - min + 1;
        let reply = conn
            .get_keyboard_mapping(min, count)?
            .reply()
            .map_err(|e| InputError::Backend(format!("get_keyboard_mapping: {e}")))?;
        let per = reply.keysyms_per_keycode as usize;
        let keysyms = reply.keysyms;

        // Find a keycode whose every level is NoSymbol — safe to repurpose.
        let mut spare = max; // fall back to the last keycode
        for kc in (min..=max).rev() {
            let row = (kc - min) as usize * per;
            if keysyms[row..row + per].iter().all(|&s| s == 0) {
                spare = kc;
                break;
            }
        }

        Ok(KeyboardMap {
            min_keycode: min,
            per,
            keysyms,
            spare_keycode: spare,
        })
    }

    /// Keysym at level 0 for a keycode (the base, unshifted symbol).
    fn base_keysym(&self, keycode: u8) -> u32 {
        if keycode < self.min_keycode {
            return 0;
        }
        let row = (keycode - self.min_keycode) as usize * self.per;
        self.keysyms.get(row).copied().unwrap_or(0)
    }

    /// Find a keycode + whether shift is needed to produce `keysym`.
    fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        let max = self.min_keycode as usize + self.keysyms.len() / self.per;
        for kc in self.min_keycode as usize..max {
            let row = (kc - self.min_keycode as usize) * self.per;
            if self.keysyms.get(row) == Some(&keysym) {
                return Some((kc as u8, false));
            }
            if self.per > 1 && self.keysyms.get(row + 1) == Some(&keysym) {
                return Some((kc as u8, true));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Injector
// ---------------------------------------------------------------------------

pub struct X11Injector {
    conn: RustConnection,
    root: Window,
    kbd: KeyboardMap,
    shift_keycode: u8,
}

impl X11Injector {
    pub fn new() -> Result<X11Injector, InputError> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        let kbd = KeyboardMap::fetch(&conn)?;
        let shift_keycode = kbd.lookup(named_to_keysym(NamedKey::ShiftL)).map(|(kc, _)| kc).unwrap_or(50);
        Ok(X11Injector {
            conn,
            root,
            kbd,
            shift_keycode,
        })
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16, root: Window) -> Result<(), InputError> {
        self.conn
            .xtest_fake_input(type_, detail, 0, root, x, y, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Resolve a keysym to a keycode for injection, remapping a spare keycode if
    /// the layout has no key for it. Returns (keycode, needs_shift).
    fn keycode_for(&mut self, keysym: u32) -> Result<(u8, bool), InputError> {
        if let Some(found) = self.kbd.lookup(keysym) {
            return Ok(found);
        }
        // Remap the spare keycode to this keysym (both levels), then use it.
        let spare = self.kbd.spare_keycode;
        let syms = vec![keysym, keysym];
        self.conn
            .change_keyboard_mapping(1, spare, 2, &syms)?;
        self.conn.flush()?;
        // Reflect it in our cached map so repeated injections are cheap.
        let row = (spare - self.kbd.min_keycode) as usize * self.kbd.per;
        if let Some(slot) = self.kbd.keysyms.get_mut(row) {
            *slot = keysym;
        }
        Ok((spare, false))
    }
}

impl InputInjector for X11Injector {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.fake(MOTION_NOTIFY_EVENT, 1, dx as i16, dy as i16, x11rb::NONE)
    }

    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        self.fake(MOTION_NOTIFY_EVENT, 0, x as i16, y as i16, self.root)
    }

    fn mouse_button(&mut self, button: Button, pressed: bool) -> Result<(), InputError> {
        let detail = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
        };
        let type_ = if pressed {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(type_, detail, 0, 0, x11rb::NONE)
    }

    fn mouse_scroll(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        // X11 scroll = button clicks: 4 up, 5 down, 6 left, 7 right.
        // Cap the click count so a malformed/huge peer message can't spin XTEST
        // forever, and use unsigned_abs so i32::MIN never panics.
        let click = |detail: u8, times: i32| -> Result<(), InputError> {
            let n = times.unsigned_abs().min(32);
            for _ in 0..n {
                self.fake(BUTTON_PRESS_EVENT, detail, 0, 0, x11rb::NONE)?;
                self.fake(BUTTON_RELEASE_EVENT, detail, 0, 0, x11rb::NONE)?;
            }
            Ok(())
        };
        if dy != 0 {
            click(if dy > 0 { 4 } else { 5 }, dy)?;
        }
        if dx != 0 {
            click(if dx > 0 { 7 } else { 6 }, dx)?;
        }
        Ok(())
    }

    fn key(&mut self, key: &Key, pressed: bool) -> Result<(), InputError> {
        let keysym = key_to_keysym(key);
        if keysym == 0 {
            return Ok(());
        }
        let (keycode, needs_shift) = self.keycode_for(keysym)?;
        let type_ = if pressed {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        // Wrap the keypress in shift if the symbol lives on the shifted level.
        if needs_shift && pressed {
            self.fake(KEY_PRESS_EVENT, self.shift_keycode, 0, 0, x11rb::NONE)?;
        }
        self.fake(type_, keycode, 0, 0, x11rb::NONE)?;
        if needs_shift && !pressed {
            self.fake(KEY_RELEASE_EVENT, self.shift_keycode, 0, 0, x11rb::NONE)?;
        }
        Ok(())
    }

    fn cursor_pos(&mut self) -> Result<(i32, i32), InputError> {
        let reply = self
            .conn
            .query_pointer(self.root)?
            .reply()
            .map_err(|e| InputError::Backend(format!("query_pointer: {e}")))?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }
}

// ---------------------------------------------------------------------------
// Capture + suppression
// ---------------------------------------------------------------------------

pub struct X11Capture {
    suppress: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl X11Capture {
    pub fn start(
        sink: UnboundedSender<CaptureSignal>,
        suppress: Arc<AtomicBool>,
    ) -> Result<X11Capture, InputError> {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = suppress.clone();
        let st2 = stop.clone();

        // Connect on the capture thread (grab + read must share a connection).
        // Validate we can connect before spawning, surfacing errors to the caller.
        let (conn, screen_num) = x11rb::connect(None)?;
        let join = std::thread::Builder::new()
            .name("x11-capture".into())
            .spawn(move || {
                if let Err(e) = capture_loop(conn, screen_num, sink, s2, st2) {
                    tracing::error!("x11 capture loop ended: {e}");
                }
            })
            .map_err(|e| InputError::Backend(format!("spawn capture thread: {e}")))?;

        Ok(X11Capture {
            suppress,
            stop,
            join: Some(join),
        })
    }
}

impl InputCapture for X11Capture {
    fn set_suppress(&self, on: bool) {
        self.suppress.store(on, Ordering::SeqCst);
    }

    fn stop(mut self: Box<Self>) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            join_bounded(j, std::time::Duration::from_millis(500));
        }
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            join_bounded(j, std::time::Duration::from_millis(500));
        }
    }
}

/// Join `handle`, but never block longer than `timeout`: if the capture thread
/// overshoots (e.g. a wedged X server stuck in a reply), detach it rather than
/// hang the caller. Its only post-stop work is releasing the grab, so detaching
/// is safe — and this keeps the "Stop never hangs" guarantee even off the reactor.
fn join_bounded(handle: JoinHandle<()>, timeout: std::time::Duration) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(timeout).is_err() {
        tracing::warn!("x11 capture thread did not stop within {timeout:?}; detaching");
    }
}

fn capture_loop(
    conn: RustConnection,
    screen_num: usize,
    sink: UnboundedSender<CaptureSignal>,
    suppress: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<(), InputError> {
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let w = screen.width_in_pixels as i32;
    let h = screen.height_in_pixels as i32;
    let cx = w / 2;
    let cy = h / 2;
    let mut kbd = KeyboardMap::fetch(&conn)?;
    let fd = conn.stream().as_raw_fd();

    // Held keys produce one press + one release instead of a repeat stream, so the
    // forwarded volume stays sane and the peer's pressed-set is authoritative.
    if let Err(e) = set_detectable_autorepeat(&conn) {
        tracing::debug!("detectable auto-repeat unavailable: {e}");
    }

    let mut grabbed = false;
    // We asked to suppress but the grab was refused (another grab is active). Latch
    // so we report it once and don't busy-retry until the request is withdrawn.
    let mut grab_failed = false;
    let mut last_pos: Option<(i32, i32)> = None;
    // Pointer-motion tracking within the current grab: deltas are taken relative to
    // the previous event (robust to the server batching MotionNotify), and warp
    // echoes are absorbed by a pending-warp counter (see `Motion`).
    let geom = MotionGeom { cx, cy, w, h };
    let mut motion = Motion::new((cx, cy));

    loop {
        if stop.load(Ordering::SeqCst) {
            if grabbed {
                let _ = ungrab(&conn);
            }
            return Ok(());
        }

        // Reconcile suppression state with the requested state.
        let want = suppress.load(Ordering::SeqCst);
        if want && !grabbed && !grab_failed {
            if grab(&conn, root)? {
                // Discard anything queued before the grab took effect (stale
                // pre-grab motion) so the first forwarded delta is genuine.
                drain_pending(&conn)?;
                warp_center(&conn, root, cx, cy)?;
                motion = Motion::new((cx, cy));
                // Snapshot modifiers physically held at the crossing so the peer
                // doesn't miss a held Shift/Ctrl/Alt/Super (X only signals the
                // down-transition, which already happened while we were local).
                emit_held_modifiers(&conn, &kbd, &sink)?;
                grabbed = true;
                last_pos = None;
            } else {
                // Couldn't take the grab (e.g. a menu/DnD/locker holds one). Tell
                // the engine so it reverts to Local instead of stranding the user
                // in a pseudo-Remote state with input leaking locally.
                grab_failed = true;
                let _ = sink.send(CaptureSignal::SuppressRejected);
                tracing::warn!("input grab refused (another grab active?); staying local");
            }
        } else if !want && (grabbed || grab_failed) {
            if grabbed {
                ungrab(&conn)?;
            }
            grabbed = false;
            grab_failed = false;
            last_pos = None;
        }

        // Drain any pending events.
        while let Some(event) = conn
            .poll_for_event()
            .map_err(|e| InputError::Backend(format!("poll_for_event: {e}")))?
        {
            if grabbed {
                handle_event(&event, &conn, root, geom, &mut motion, &mut kbd, &sink)?;
            } else if let Event::MappingNotify(_) = event {
                kbd = KeyboardMap::fetch(&conn)?;
            }
        }

        // While local, poll the pointer so the engine can watch the crossing edge.
        // (Global motion isn't delivered without a grab; a cheap query is reliable.)
        if !grabbed {
            let pos = query_pointer_pos(&conn, root)?;
            if last_pos != Some(pos) {
                last_pos = Some(pos);
                let _ = sink.send(CaptureSignal::Cursor { x: pos.0, y: pos.1 });
            }
        }

        // Wait for the fd to become readable or a short timeout, so suppression
        // toggles and stop are honoured even with no input flowing.
        wait_readable(fd, 5);
    }
}

/// Ask the X server for detectable auto-repeat on the core keyboard (XKB).
fn set_detectable_autorepeat(conn: &RustConnection) -> Result<(), InputError> {
    use x11rb::protocol::xkb::{self, ConnectionExt as _};
    conn.xkb_use_extension(1, 0)?
        .reply()
        .map_err(|e| InputError::Backend(format!("xkb use_extension: {e}")))?;
    conn.xkb_per_client_flags(
        u16::from(xkb::ID::USE_CORE_KBD),
        xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
        xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
        xkb::BoolCtrl::default(),
        xkb::BoolCtrl::default(),
        xkb::BoolCtrl::default(),
    )?
    .reply()
    .map_err(|e| InputError::Backend(format!("xkb per_client_flags: {e}")))?;
    Ok(())
}

/// Discard every event currently queued on the connection.
fn drain_pending(conn: &RustConnection) -> Result<(), InputError> {
    while conn
        .poll_for_event()
        .map_err(|e| InputError::Backend(format!("drain poll_for_event: {e}")))?
        .is_some()
    {}
    Ok(())
}

/// Current absolute pointer position on `root`.
fn query_pointer_pos(conn: &RustConnection, root: Window) -> Result<(i32, i32), InputError> {
    let r = conn
        .query_pointer(root)?
        .reply()
        .map_err(|e| InputError::Backend(format!("query_pointer: {e}")))?;
    Ok((r.root_x as i32, r.root_y as i32))
}

/// Emit synthetic `Key` presses for modifiers physically held at the moment of
/// grabbing. Uses `query_keymap` (the full physical key bitmap) rather than the
/// pointer's logical modifier mask, so the peer gets the *exact* keys down —
/// distinguishing left vs right Shift/Ctrl/Alt/Super — and never misses a held
/// modifier whose down-transition happened while we were still local.
fn emit_held_modifiers(
    conn: &RustConnection,
    kbd: &KeyboardMap,
    sink: &UnboundedSender<CaptureSignal>,
) -> Result<(), InputError> {
    let keys = conn
        .query_keymap()?
        .reply()
        .map_err(|e| InputError::Backend(format!("query_keymap: {e}")))?
        .keys; // [u8; 32], bit (byte*8 + bit) set == that keycode is down
    for (byte_i, byte) in keys.iter().enumerate() {
        if *byte == 0 {
            continue;
        }
        for bit in 0..8 {
            if byte & (1 << bit) == 0 {
                continue;
            }
            let keycode = (byte_i * 8 + bit) as u8;
            let ks = kbd.base_keysym(keycode);
            if let Some(n) = keysym_to_named(ks) {
                if n.modifier_token().is_some() {
                    let _ = sink.send(CaptureSignal::Input(InputEvent::Key {
                        key: Key::Named(n),
                        pressed: true,
                    }));
                }
            }
        }
    }
    Ok(())
}

/// Try to grab the pointer and keyboard. Returns `Ok(true)` if both grabs
/// succeeded, `Ok(false)` if either was refused (another client holds an active
/// grab — a menu, drag-and-drop, or a screen locker), in which case we hold no
/// grab. The caller reverts to Local on `false` instead of stranding the user in
/// a pseudo-Remote state with input still leaking to local apps.
fn grab(conn: &RustConnection, root: Window) -> Result<bool, InputError> {
    use x11rb::protocol::xproto::EventMask;
    let mask = EventMask::POINTER_MOTION
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE;
    let p = conn
        .grab_pointer(
            false,
            root,
            mask,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )?
        .reply()
        .map_err(|e| InputError::Backend(format!("grab_pointer: {e}")))?;
    if p.status != GrabStatus::SUCCESS {
        return Ok(false);
    }
    let k = conn
        .grab_keyboard(
            false,
            root,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )?
        .reply()
        .map_err(|e| InputError::Backend(format!("grab_keyboard: {e}")))?;
    if k.status != GrabStatus::SUCCESS {
        // Took the pointer but not the keyboard — release the pointer so we hold
        // nothing, then report refusal.
        conn.ungrab_pointer(x11rb::CURRENT_TIME)?;
        conn.flush()?;
        return Ok(false);
    }
    conn.flush()?;
    Ok(true)
}

fn ungrab(conn: &RustConnection) -> Result<(), InputError> {
    conn.ungrab_pointer(x11rb::CURRENT_TIME)?;
    conn.ungrab_keyboard(x11rb::CURRENT_TIME)?;
    conn.flush()?;
    Ok(())
}

fn warp_center(conn: &RustConnection, root: Window, cx: i32, cy: i32) -> Result<(), InputError> {
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, cx as i16, cy as i16)?;
    conn.flush()?;
    Ok(())
}

fn handle_event(
    event: &Event,
    conn: &RustConnection,
    root: Window,
    geom: MotionGeom,
    motion: &mut Motion,
    kbd: &mut KeyboardMap,
    sink: &UnboundedSender<CaptureSignal>,
) -> Result<(), InputError> {
    match event {
        Event::MotionNotify(e) => {
            // `_near_edge` is the shared near-edge hint the Windows backend uses; on
            // X11 we recentre after *every* real move instead (see below).
            let (action, _near_edge) = motion.on_motion(e.root_x as i32, e.root_y as i32, &geom);
            if let MotionAction::Forward { dx, dy } = action {
                let _ = sink.send(CaptureSignal::Input(InputEvent::MouseMove { dx, dy }));
                // Pin the local cursor at centre by warping back on every real move.
                // An X11 pointer grab does NOT freeze the cursor — it keeps tracking
                // the physical mouse — so recentring only near an edge let the cursor
                // visibly drift across the central region of the local screen while
                // control was on the peer (the "Kali cursor still moves while remote"
                // bug). Recentring every move keeps it parked at centre, matching the
                // Python build. Deltas are taken event-to-event, so the extra warps
                // don't distort forwarded motion, and each warp's synthetic centre
                // echo is absorbed via `note_warp_issued` / `pending_warps`.
                warp_center(conn, root, geom.cx, geom.cy)?;
                motion.note_warp_issued();
            }
        }
        Event::ButtonPress(e) => emit_button(e.detail, true, sink),
        Event::ButtonRelease(e) => emit_button(e.detail, false, sink),
        Event::KeyPress(e) => emit_key(e.detail, true, kbd, sink),
        Event::KeyRelease(e) => emit_key(e.detail, false, kbd, sink),
        Event::MappingNotify(_) => {
            *kbd = KeyboardMap::fetch(conn)?;
        }
        _ => {}
    }
    Ok(())
}

fn emit_button(detail: u8, pressed: bool, sink: &UnboundedSender<CaptureSignal>) {
    let send = |ev: InputEvent| drop(sink.send(CaptureSignal::Input(ev)));
    match detail {
        1 => send(InputEvent::MouseButton { button: Button::Left, pressed }),
        2 => send(InputEvent::MouseButton { button: Button::Middle, pressed }),
        3 => send(InputEvent::MouseButton { button: Button::Right, pressed }),
        // Scroll buttons fire press+release; emit one scroll per press only.
        4 if pressed => send(InputEvent::MouseScroll { dx: 0, dy: 1 }),
        5 if pressed => send(InputEvent::MouseScroll { dx: 0, dy: -1 }),
        6 if pressed => send(InputEvent::MouseScroll { dx: -1, dy: 0 }),
        7 if pressed => send(InputEvent::MouseScroll { dx: 1, dy: 0 }),
        _ => {}
    }
}

fn emit_key(keycode: u8, pressed: bool, kbd: &KeyboardMap, sink: &UnboundedSender<CaptureSignal>) {
    let keysym = kbd.base_keysym(keycode);
    if let Some(key) = keysym_to_key(keysym) {
        let _ = sink.send(CaptureSignal::Input(InputEvent::Key { key, pressed }));
    }
}

/// Block until `fd` is readable or `timeout_ms` elapses.
fn wait_readable(fd: std::os::unix::io::RawFd, timeout_ms: i32) {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single valid pollfd, count 1, kernel only writes revents.
    unsafe {
        libc::poll(&mut pfd as *mut libc::pollfd, 1, timeout_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_named_roundtrip() {
        for n in [
            NamedKey::Enter,
            NamedKey::Up,
            NamedKey::F(1),
            NamedKey::F(12),
            NamedKey::F(13),
            NamedKey::F(24),
            NamedKey::ShiftL,
            NamedKey::SuperR,
        ] {
            let ks = named_to_keysym(n);
            assert_eq!(keysym_to_named(ks), Some(n), "named roundtrip {n:?}");
        }
    }

    #[test]
    fn keysym_char_roundtrip() {
        for c in ['a', 'Z', '1', '/', '€'] {
            let ks = char_to_keysym(c);
            assert_eq!(keysym_to_char(ks), Some(c), "char roundtrip {c:?}");
        }
    }

    #[test]
    fn key_to_keysym_and_back() {
        for key in [
            Key::Named(NamedKey::Enter),
            Key::Char('q'),
            Key::Char('€'),
        ] {
            let ks = key_to_keysym(&key);
            assert_eq!(keysym_to_key(ks), Some(key));
        }
    }
}
