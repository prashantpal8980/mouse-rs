//! Windows input backend.
//!
//! * Injection uses `SendInput` (no special privileges). Every injected event is
//!   stamped with [`INJECT_MAGIC`] in `dwExtraInfo` so the capture hooks can
//!   recognise and absorb our own warp/inject echoes — `LLMHF_INJECTED` is *not*
//!   sufficient because it fires for any process's injection.
//! * Capture + suppression uses two low-level hooks (`WH_MOUSE_LL`,
//!   `WH_KEYBOARD_LL`) installed on a dedicated thread that runs a message pump.
//!   While suppressing, the hook callbacks swallow input (return non-zero) and
//!   forward it to the engine instead, mirroring the X11 grab/warp design.
//!
//! Fn+Up freeze fix (the Python bug): the hook callbacks do strictly O(1),
//! non-blocking, lock-free work and return in microseconds, so Windows never times
//! them out and silently removes the hook. (They allocate only on the rising edge of
//! a crossing — once per crossing, in [`HookState::begin_grab`] — never per event.)
//! Four failsafe layers guarantee input can never stay frozen — see [`watchdog`] and
//! [`keyboard_proc`] (the return hotkey self-releases suppression in-callback,
//! with no dependency on the network or engine being healthy).
//!
//! **Nothing on the pump thread may block, and that includes logging.** `tracing` here
//! writes to a file shared with the GUI thread, so diagnostics are handed to a separate
//! thread through a channel — see [`DiagEvent`]. A hook thread that stalls past
//! `LowLevelHooksTimeout` is skipped by the OS, and the keystroke it should have
//! captured is delivered to the local app instead: the "keyboard didn't follow the
//! mouse" bug.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_KEYBOARD, INPUT_MOUSE,
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW,
    GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowTextW, GetWindowThreadProcessId,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostThreadMessageW, RegisterClassW,
    GetDesktopWindow, GetShellWindow, SetForegroundWindow, SetWindowsHookExW, ShowWindow,
    SW_MINIMIZE, TranslateMessage, UnhookWindowsHookEx,
    UnregisterClassW, HC_ACTION,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG, MSLLHOOKSTRUCT,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WHEEL_DELTA, WM_INPUT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WNDCLASSW,
};

use mouse_core::protocol::Button;

use crate::event::InputEvent;
use crate::keys::{Key, NamedKey};
use crate::{CaptureSignal, InputCapture, InputError, InputInjector};

/// Stamped into `dwExtraInfo` of every event we inject, so the capture hooks can
/// recognise our own warp/inject echoes (distinct from third-party injection,
/// which `LLMHF_INJECTED` cannot tell apart). ASCII "mousRS".
const INJECT_MAGIC: usize = 0x6D6F_7573_5253;

// ---------------------------------------------------------------------------
// Virtual-key constants (stable Win32 values; defined locally for a compact,
// version-independent table).
// ---------------------------------------------------------------------------
mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const PAUSE: u16 = 0x13;
    pub const CAPITAL: u16 = 0x14;
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    pub const PRIOR: u16 = 0x21; // PageUp
    pub const NEXT: u16 = 0x22; // PageDown
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const SNAPSHOT: u16 = 0x2C; // PrintScreen
    pub const INSERT: u16 = 0x2D;
    pub const DELETE: u16 = 0x2E;
    pub const LWIN: u16 = 0x5B;
    pub const RWIN: u16 = 0x5C;
    pub const APPS: u16 = 0x5D; // Menu
    pub const F1: u16 = 0x70;
    pub const NUMLOCK: u16 = 0x90;
    pub const SCROLL: u16 = 0x91;
    pub const LSHIFT: u16 = 0xA0;
    pub const RSHIFT: u16 = 0xA1;
    pub const LCONTROL: u16 = 0xA2;
    pub const RCONTROL: u16 = 0xA3;
    pub const LMENU: u16 = 0xA4; // Left Alt
    pub const RMENU: u16 = 0xA5; // Right Alt / AltGr
    // Generic (layout-agnostic) modifiers, seen when a hook can't resolve a side.
    pub const SHIFT: u16 = 0x10;
    pub const CONTROL: u16 = 0x11;
    pub const MENU: u16 = 0x12;
}

// ---------------------------------------------------------------------------
// DPI awareness + screen metrics
// ---------------------------------------------------------------------------

/// Make the process per-monitor DPI aware exactly once, before any cursor / input
/// / metric call. Without this, `SendInput` absolute coords and `MSLLHOOKSTRUCT.pt`
/// disagree under display scaling (the hard-won Python lesson). Idempotent: a second
/// call (or a manifest already setting awareness) just fails harmlessly.
pub fn ensure_dpi_aware() {
    static DPI: Once = Once::new();
    DPI.call_once(|| unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

fn metric(index: i32) -> i32 {
    unsafe { GetSystemMetrics(index) }
}

/// Primary-monitor size in physical pixels (parity with the X11 default-root model
/// the engine's single-screen edge geometry assumes).
fn primary_size() -> (i32, i32) {
    (metric(SM_CXSCREEN), metric(SM_CYSCREEN))
}

/// Virtual-desktop rect (x, y, w, h) spanning all monitors — used to normalize
/// absolute `SendInput` coords with `MOUSEEVENTF_VIRTUALDESK`.
fn virtual_rect() -> (i32, i32, i32, i32) {
    (
        metric(SM_XVIRTUALSCREEN),
        metric(SM_YVIRTUALSCREEN),
        metric(SM_CXVIRTUALSCREEN).max(1),
        metric(SM_CYVIRTUALSCREEN).max(1),
    )
}

/// Map an absolute screen point to the 0..=65535 normalized space `SendInput`
/// expects for `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
fn normalize_abs(x: i32, y: i32) -> (i32, i32) {
    let (vx, vy, vw, vh) = virtual_rect();
    let nx = ((x - vx) as i64 * 65535 + (vw as i64 - 1) / 2) / (vw as i64 - 1).max(1);
    let ny = ((y - vy) as i64 * 65535 + (vh as i64 - 1) / 2) / (vh as i64 - 1).max(1);
    (nx.clamp(0, 65535) as i32, ny.clamp(0, 65535) as i32)
}

/// Public screen-size entry point (mirrors `linux::screen_size`).
pub(crate) fn screen_size() -> Result<(i32, i32), InputError> {
    ensure_dpi_aware();
    let (w, h) = primary_size();
    if w <= 0 || h <= 0 {
        return Err(InputError::Backend("GetSystemMetrics returned 0".into()));
    }
    Ok((w, h))
}

// ---------------------------------------------------------------------------
// Key mapping: NamedKey <-> VK, plus base-char extraction.
// ---------------------------------------------------------------------------

/// Map a named key to its virtual-key code and whether it is an "extended" key
/// (needs `KEYEVENTF_EXTENDEDKEY` on injection / sets `LLKHF_EXTENDED` on capture).
fn named_to_vk(n: NamedKey) -> (u16, bool) {
    match n {
        NamedKey::Enter => (vk::RETURN, false),
        NamedKey::Escape => (vk::ESCAPE, false),
        NamedKey::Backspace => (vk::BACK, false),
        NamedKey::Tab => (vk::TAB, false),
        NamedKey::Space => (vk::SPACE, false),
        NamedKey::Insert => (vk::INSERT, true),
        NamedKey::Delete => (vk::DELETE, true),
        NamedKey::Up => (vk::UP, true),
        NamedKey::Down => (vk::DOWN, true),
        NamedKey::Left => (vk::LEFT, true),
        NamedKey::Right => (vk::RIGHT, true),
        NamedKey::Home => (vk::HOME, true),
        NamedKey::End => (vk::END, true),
        NamedKey::PageUp => (vk::PRIOR, true),
        NamedKey::PageDown => (vk::NEXT, true),
        NamedKey::CapsLock => (vk::CAPITAL, false),
        NamedKey::NumLock => (vk::NUMLOCK, true),
        NamedKey::ScrollLock => (vk::SCROLL, false),
        NamedKey::PrintScreen => (vk::SNAPSHOT, true),
        NamedKey::Pause => (vk::PAUSE, false),
        NamedKey::Menu => (vk::APPS, true),
        NamedKey::ShiftL => (vk::LSHIFT, false),
        // Right Shift is NOT an extended key — it has its own scancode (0x36), no E0
        // prefix. Flagging it extended injects a malformed "fake shift". (Per MS
        // Keyboard Input docs; the extended keys are RAlt/RCtrl, the nav cluster,
        // arrows, NumLock, Break, PrintScreen, numpad Divide/Enter.)
        NamedKey::ShiftR => (vk::RSHIFT, false),
        NamedKey::CtrlL => (vk::LCONTROL, false),
        NamedKey::CtrlR => (vk::RCONTROL, true),
        NamedKey::AltL => (vk::LMENU, false),
        NamedKey::AltR => (vk::RMENU, true),
        NamedKey::AltGr => (vk::RMENU, true),
        NamedKey::SuperL => (vk::LWIN, true),
        NamedKey::SuperR => (vk::RWIN, true),
        NamedKey::F(k) => (vk::F1 + (k as u16 - 1), false),
    }
}

/// Reverse: a virtual-key code (with its extended bit) to a named key, if named.
fn vk_to_named(v: u16, extended: bool) -> Option<NamedKey> {
    let n = match v {
        vk::RETURN => NamedKey::Enter,
        vk::ESCAPE => NamedKey::Escape,
        vk::BACK => NamedKey::Backspace,
        vk::TAB => NamedKey::Tab,
        vk::SPACE => NamedKey::Space,
        vk::INSERT => NamedKey::Insert,
        vk::DELETE => NamedKey::Delete,
        vk::UP => NamedKey::Up,
        vk::DOWN => NamedKey::Down,
        vk::LEFT => NamedKey::Left,
        vk::RIGHT => NamedKey::Right,
        vk::HOME => NamedKey::Home,
        vk::END => NamedKey::End,
        vk::PRIOR => NamedKey::PageUp,
        vk::NEXT => NamedKey::PageDown,
        vk::CAPITAL => NamedKey::CapsLock,
        vk::NUMLOCK => NamedKey::NumLock,
        vk::SCROLL => NamedKey::ScrollLock,
        vk::SNAPSHOT => NamedKey::PrintScreen,
        vk::PAUSE => NamedKey::Pause,
        vk::APPS => NamedKey::Menu,
        vk::LSHIFT => NamedKey::ShiftL,
        vk::RSHIFT => NamedKey::ShiftR,
        vk::LCONTROL => NamedKey::CtrlL,
        vk::RCONTROL => NamedKey::CtrlR,
        vk::LMENU => NamedKey::AltL,
        vk::RMENU => NamedKey::AltGr,
        vk::LWIN => NamedKey::SuperL,
        vk::RWIN => NamedKey::SuperR,
        // Generic modifiers the LL hook may report: resolve a side using the
        // extended bit (right modifiers are flagged extended), else default left.
        vk::SHIFT => NamedKey::ShiftL,
        vk::CONTROL => {
            if extended {
                NamedKey::CtrlR
            } else {
                NamedKey::CtrlL
            }
        }
        vk::MENU => {
            if extended {
                NamedKey::AltGr
            } else {
                NamedKey::AltL
            }
        }
        0x70..=0x87 => NamedKey::F((v - vk::F1 + 1) as u8),
        _ => return None,
    };
    Some(n)
}

/// The base (unshifted, modifier-independent) character a VK produces on the
/// current layout, if any. `MapVirtualKey(MAPVK_VK_TO_CHAR)` is stateless (unlike
/// `ToUnicode`), giving exactly the wire's "base char" without dead-key mutation.
fn vk_to_basechar(v: u16) -> Option<char> {
    let res = unsafe { MapVirtualKeyW(v as u32, 2 /* MAPVK_VK_TO_CHAR */) };
    // The character is the low word; bit 31 (0x8000_0000) flags a dead key, which we
    // don't filter — a dead key's base char (e.g. '^') is still what we want on the
    // wire. Masking the full low word avoids clearing bit 15 of a real character.
    let cp = res & 0xFFFF;
    if cp == 0 {
        return None;
    }
    char::from_u32(cp).map(|c| c.to_ascii_lowercase())
}

/// Map a captured VK (+extended) to our wire `Key`.
fn vk_to_key(v: u16, extended: bool) -> Option<Key> {
    if let Some(n) = vk_to_named(v, extended) {
        return Some(Key::Named(n));
    }
    if let Some(c) = vk_to_basechar(v) {
        return Some(Key::Char(c));
    }
    if v != 0 {
        return Some(Key::Raw(v as u32));
    }
    None
}

// ---------------------------------------------------------------------------
// Injector
// ---------------------------------------------------------------------------

/// Stateless `SendInput` wrapper. `SendInput` is process-global and thread-agnostic,
/// so the injector holds no handles.
pub struct WinInjector;

impl WinInjector {
    pub fn new() -> Result<WinInjector, InputError> {
        ensure_dpi_aware();
        Ok(WinInjector)
    }
}

fn mk_mouse(flags: u32, dx: i32, dy: i32, data: i32) -> INPUT {
    let mut inp: INPUT = unsafe { std::mem::zeroed() };
    inp.r#type = INPUT_MOUSE;
    inp.Anonymous.mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: data as _,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: INJECT_MAGIC,
    };
    inp
}

fn mk_key(vk: u16, scan: u16, flags: u32) -> INPUT {
    let mut inp: INPUT = unsafe { std::mem::zeroed() };
    inp.r#type = INPUT_KEYBOARD;
    inp.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: INJECT_MAGIC,
    };
    inp
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), InputError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let n = inputs.len() as u32;
    let sent = unsafe { SendInput(n, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if sent != n {
        return Err(InputError::Backend(format!(
            "SendInput inserted {sent}/{n} events"
        )));
    }
    Ok(())
}

/// Build the `INPUT`s for a key transition, pushing into `buf`.
fn push_key_inputs(buf: &mut Vec<INPUT>, key: &Key, pressed: bool) {
    let up = if pressed { 0 } else { KEYEVENTF_KEYUP };
    match key {
        Key::Named(n) => {
            let (v, ext) = named_to_vk(*n);
            let scan = unsafe { MapVirtualKeyW(v as u32, MAPVK_VK_TO_VSC) } as u16;
            let extflag = if ext { KEYEVENTF_EXTENDEDKEY } else { 0 };
            buf.push(mk_key(v, scan, up | extflag));
        }
        Key::Char(c) => {
            let r = unsafe { VkKeyScanW(*c as u16) };
            if r != -1 {
                // Layout has a key for this char: inject the VK so it composes with
                // separately-forwarded modifiers (Ctrl+C etc.). The base char is
                // unshifted so the shift-state byte is normally 0; we ignore it.
                let v = (r & 0xFF) as u16;
                let scan = unsafe { MapVirtualKeyW(v as u32, MAPVK_VK_TO_VSC) } as u16;
                buf.push(mk_key(v, scan, up));
            } else {
                // No key on this layout (e.g. a non-Latin char): inject the literal
                // Unicode unit(s). UTF-16 surrogate pairs become two INPUTs.
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units).iter() {
                    buf.push(mk_key(0, *unit, up | KEYEVENTF_UNICODE));
                }
            }
        }
        Key::Raw(_) => {
            // Raw carries an X keysym we couldn't name (the X11 backend's fallback);
            // it has no portable Windows virtual-key meaning, so injecting it as a VK
            // would press an arbitrary wrong key. Drop it instead.
        }
    }
}

impl InputInjector for WinInjector {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        // Absolute (not relative SendInput) so Windows pointer ballistics can't
        // distort the forwarded delta — the client reproduces motion 1:1.
        let (x, y) = self.cursor_pos()?;
        self.mouse_move_abs(x + dx, y + dy)
    }

    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        let (nx, ny) = normalize_abs(x, y);
        send_inputs(&[mk_mouse(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            nx,
            ny,
            0,
        )])
    }

    fn mouse_button(&mut self, button: Button, pressed: bool) -> Result<(), InputError> {
        let flags = match (button, pressed) {
            (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (Button::Left, false) => MOUSEEVENTF_LEFTUP,
            (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
        };
        send_inputs(&[mk_mouse(flags, 0, 0, 0)])
    }

    fn mouse_scroll(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        // Cap so a malformed/huge peer message can't spin SendInput (mirrors X11).
        let clamp = |v: i32| v.clamp(-32, 32);
        let mut inputs: Vec<INPUT> = Vec::new();
        if dy != 0 {
            inputs.push(mk_mouse(MOUSEEVENTF_WHEEL, 0, 0, clamp(dy) * WHEEL_DELTA as i32));
        }
        if dx != 0 {
            inputs.push(mk_mouse(MOUSEEVENTF_HWHEEL, 0, 0, clamp(dx) * WHEEL_DELTA as i32));
        }
        send_inputs(&inputs)
    }

    fn key(&mut self, key: &Key, pressed: bool) -> Result<(), InputError> {
        let mut buf: Vec<INPUT> = Vec::new();
        push_key_inputs(&mut buf, key, pressed);
        send_inputs(&buf)
    }

    fn cursor_pos(&mut self) -> Result<(i32, i32), InputError> {
        let mut pt = POINT { x: 0, y: 0 };
        let ok = unsafe { GetCursorPos(&mut pt) };
        if ok == 0 {
            return Err(InputError::Backend("GetCursorPos failed".into()));
        }
        Ok((pt.x, pt.y))
    }
}

// ---------------------------------------------------------------------------
// Capture + suppression
// ---------------------------------------------------------------------------

/// State shared between the pump loop and the hook callbacks. Lives in a
/// A diagnostic observed on the hook pump thread, to be *logged somewhere else*.
///
/// Nothing on that thread may block, and `tracing` here writes to a file
/// (`~/.mouse_share/mouse-rs.log`) that the GUI thread also writes to. A blocking
/// write — or contention on the writer's lock — stalls the pump, and a stalled pump
/// is precisely what makes Windows skip the keyboard hook and drop the keystroke on
/// this machine. Logging the leak must not be able to cause the leak, so the pump
/// only ever hands the event off through this (non-blocking, unbounded) channel and
/// a dedicated thread does the I/O.
enum DiagEvent {
    Grabbed {
        focus: String,
    },
    Released {
        swallowed: u64,
        leaked: u64,
        remote_ms: u128,
        /// OS invocations of the keyboard hook during this crossing, and how many of
        /// those fell through without deciding. Together with `swallowed` these say
        /// exactly where a lost keystroke went.
        hook_calls: u64,
        hook_bails: u64,
        /// Physical key-downs seen via Raw Input during the crossing — the independent
        /// witness that makes `hook_calls == 0` interpretable.
        raw_keys: u64,
        /// See [`MOUSE_HOOK_CALLS`] — the discriminator between a keyboard-specific
        /// failure and a thread-wide one.
        mouse_calls: u64,
    },
    Leak {
        vk: u16,
        reason: &'static str,
        n: u64,
    },
    Stalled {
        ms: u128,
        rearmed: bool,
    },
    Rearmed {
        ok: bool,
    },
    FocusHandedOff {
        ok: bool,
        to: String,
    },
    /// The hand-off was *requested* successfully and the foreground still did not leave
    /// this process. See `verify_at` in the pump loop for why this is checked separately.
    FocusHandoffIneffective {
        retried: bool,
    },
}

/// `thread_local` because low-level hook callbacks are dispatched by the OS *on
/// the installing thread* (during its message pump), so the procs and the loop are
/// the same thread — no lock, no global pointer.
struct HookState {
    suppress: Arc<AtomicBool>,
    grabbed: bool,
    sink: UnboundedSender<CaptureSignal>,
    return_vk: u16,
    last_cursor: Option<(i32, i32)>,
    /// Per-remote-session keyboard accounting, reset each time suppression engages.
    /// Exists to answer one question from real use: keys occasionally stay on this
    /// machine after the cursor has crossed, and nothing in the log said why.
    kbd_swallowed: u64,
    kbd_leaked: u64,
    /// When suppression engaged, so a leak can be reported as an offset from the
    /// crossing. Leaks bunched in the first few milliseconds mean the grab hadn't
    /// landed yet; leaks spread through the session mean something else.
    grabbed_at: Option<std::time::Instant>,
    /// Values of [`KBD_HOOK_CALLS`] / [`KBD_HOOK_BAILS`] when this crossing began, so
    /// the released summary can report per-crossing deltas rather than run totals.
    calls_at_grab: u64,
    bails_at_grab: u64,
    raw_keys_at_grab: u64,
    mouse_calls_at_grab: u64,
    /// Hand-off to the diagnostics thread; see [`DiagEvent`]. Never log inline here.
    diag: std::sync::mpsc::Sender<DiagEvent>,
}

thread_local! {
    static HOOK_STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

impl HookState {
    /// Take the rising edge of suppression exactly once: reset the per-crossing
    /// accounting, report which window had focus, and snapshot the modifiers that were
    /// already held so the peer starts the crossing in the same modifier state.
    ///
    /// Called from the hook procs as well as from [`reconcile`], because the procs now
    /// act on the live `suppress` flag: if only the ~8 ms reconcile tick did this, a key
    /// pressed immediately after the crossing would be forwarded *before* the held-
    /// modifier snapshot, and the peer would see (say) a bare `c` ahead of the Ctrl that
    /// was meant to modify it. Idempotent — whichever caller gets there first wins.
    fn begin_grab(&mut self) {
        if self.grabbed {
            return;
        }
        self.grabbed = true;
        self.kbd_swallowed = 0;
        self.kbd_leaked = 0;
        self.grabbed_at = Some(std::time::Instant::now());
        self.calls_at_grab = KBD_HOOK_CALLS.load(Ordering::Relaxed);
        self.bails_at_grab = KBD_HOOK_BAILS.load(Ordering::Relaxed);
        self.raw_keys_at_grab = RAW_KEY_DOWNS.load(Ordering::Relaxed);
        self.mouse_calls_at_grab = MOUSE_HOOK_CALLS.load(Ordering::Relaxed);
        // Formatted here (the three Win32 calls are non-blocking — `GetWindowTextW` on
        // another process's window reads the caption without sending WM_GETTEXT, by
        // design) but written to the log by the diag thread.
        let _ = self.diag.send(DiagEvent::Grabbed {
            focus: foreground_desc(),
        });
        emit_held_modifiers(&self.sink);
    }

    /// Handle a mouse hook event. Returns `true` to swallow it.
    ///
    /// The hook is used only to (a) freeze the local cursor and block local clicks
    /// while grabbed, and (b) carry button/wheel events. **Motion is NOT read here** —
    /// it comes from Raw Input (`WM_INPUT` → [`HookState::on_raw_motion`]), which gives
    /// true device deltas that are immune to cursor position, screen-edge clamping, and
    /// pointer acceleration (the position-based hook model jammed at corners).
    fn on_mouse(&mut self, message: u32, info: &MSLLHOOKSTRUCT) -> bool {
        // Ignore our own injected events (the injector stamps INJECT_MAGIC); swallow
        // them while grabbed so an echo never leaks. The server no longer injects, so
        // this is just defence in depth.
        // Live read, for the same reason as `on_key`: the reconciled flag lags the
        // engine's request by up to a tick, and a click landing in that gap hits the
        // local window instead of the peer.
        let grabbed = self.suppress.load(Ordering::SeqCst);
        if info.dwExtraInfo == INJECT_MAGIC {
            return grabbed;
        }
        if grabbed {
            self.begin_grab();
        }

        if grabbed {
            match message {
                // Motion is swallowed here (freezes the local cursor) but read from Raw
                // Input, not from `pt` — so edge clamping / acceleration can't distort it.
                WM_MOUSEMOVE => {}
                WM_LBUTTONDOWN => self.send_button(Button::Left, true),
                WM_LBUTTONUP => self.send_button(Button::Left, false),
                WM_RBUTTONDOWN => self.send_button(Button::Right, true),
                WM_RBUTTONUP => self.send_button(Button::Right, false),
                WM_MBUTTONDOWN => self.send_button(Button::Middle, true),
                WM_MBUTTONUP => self.send_button(Button::Middle, false),
                WM_MOUSEWHEEL => {
                    let d = wheel_delta(info.mouseData);
                    self.send_scroll(0, d);
                }
                WM_MOUSEHWHEEL => {
                    let d = wheel_delta(info.mouseData);
                    self.send_scroll(d, 0);
                }
                _ => {}
            }
            true // swallow everything while suppressing
        } else {
            // Not suppressing: report the cursor position for edge-watching and let
            // input reach local apps. (Replaces the X11 query_pointer poll.)
            if message == WM_MOUSEMOVE {
                let pos = (info.pt.x, info.pt.y);
                if self.last_cursor != Some(pos) {
                    self.last_cursor = Some(pos);
                    let _ = self.sink.send(CaptureSignal::Cursor { x: pos.0, y: pos.1 });
                }
            }
            false
        }
    }

    /// Handle a keyboard hook event. Returns `true` to swallow it.
    fn on_key(&mut self, info: &KBDLLHOOKSTRUCT) -> bool {
        // Decide from the engine's *live* request, not from `grabbed`. `grabbed` is
        // reconciled on the pump's ~8 ms tick, so between the engine asking for
        // suppression and the tick landing there was a window in which this proc still
        // reported "not suppressing" and let the keystroke through to the local app —
        // the "mouse crossed but my typing stayed here" report. Reading the atomic here
        // is O(1) and closes that window by construction rather than narrowing it.
        if !self.suppress.load(Ordering::SeqCst) {
            return false; // keys flow to local apps while not suppressing
        }
        // Snapshot held modifiers before forwarding anything, if we got here first.
        self.begin_grab();
        let pressed = info.flags & LLKHF_UP == 0;
        let extended = info.flags & LLKHF_EXTENDED != 0;
        let vk = info.vkCode as u16;

        // Failsafe (the Fn+Up fix): the return hotkey self-releases suppression here,
        // in-callback, with no dependency on the network or engine being alive. Still
        // forward it so the engine's own return-to-local path also fires.
        if pressed && vk == self.return_vk {
            self.suppress.store(false, Ordering::SeqCst);
        }

        if let Some(key) = vk_to_key(vk, extended) {
            let _ = self
                .sink
                .send(CaptureSignal::Input(InputEvent::Key { key, pressed }));
        } else if pressed {
            // Swallowed locally but not representable on the wire: the key reaches
            // neither machine. Silent before; indistinguishable from a leak to a user.
            self.kbd_leaked += 1;
            let _ = self.diag.send(DiagEvent::Leak {
                vk,
                reason: "no_wire_mapping",
                n: self.kbd_leaked,
            });
        }
        if pressed {
            self.kbd_swallowed += 1;
        }
        true // swallow while suppressing
    }

    fn send_button(&self, button: Button, pressed: bool) {
        let _ = self
            .sink
            .send(CaptureSignal::Input(InputEvent::MouseButton { button, pressed }));
    }

    fn send_scroll(&self, dx: i32, dy: i32) {
        if dx != 0 || dy != 0 {
            let _ = self
                .sink
                .send(CaptureSignal::Input(InputEvent::MouseScroll { dx, dy }));
        }
    }
}

/// Decode the signed wheel delta (high word of `mouseData`) into wire scroll steps:
/// notches, preserving sign, with a 1-step floor for high-resolution wheels.
fn wheel_delta(mouse_data: u32) -> i32 {
    let raw = (mouse_data >> 16) as i16 as i32;
    let steps = raw / WHEEL_DELTA as i32;
    if steps == 0 && raw != 0 {
        raw.signum()
    } else {
        steps
    }
}

/// UTF-16, null-terminated class name for our hidden Raw Input message window.
const RAW_CLASS: &[u16] = &[
    b'm' as u16, b'o' as u16, b'u' as u16, b's' as u16, b'e' as u16, b'r' as u16, b's' as u16,
    b'_' as u16, b'r' as u16, b'a' as u16, b'w' as u16, 0,
];

/// Create a hidden message-only window and register for raw **mouse** input targeted
/// at it (`RIDEV_INPUTSINK` so samples arrive even though the window is never focused).
/// `WM_INPUT` messages then flow to [`raw_wnd_proc`] via the thread's message pump.
/// Returns the window handle (destroy it + unregister the class on teardown).
unsafe fn create_raw_input_window(hinstance: windows_sys::Win32::Foundation::HINSTANCE) -> HWND {
    let class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(raw_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: RAW_CLASS.as_ptr(),
    };
    // Ignore a nonzero atom vs "already registered" — CreateWindowExW works either way.
    let _ = RegisterClassW(&class);
    let hwnd = CreateWindowExW(
        0,
        RAW_CLASS.as_ptr(),
        std::ptr::null(),
        0,
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        std::ptr::null_mut(),
        hinstance,
        std::ptr::null(),
    );
    if !hwnd.is_null() {
        // Mouse for motion; keyboard purely as an **independent witness**. Raw Input is
        // delivered by a different mechanism from low-level hooks, so it still reports
        // keystrokes when the keyboard hook is dead. Without it, `hook_calls == 0` is
        // ambiguous — a hook the OS never called and a crossing where the user simply
        // typed nothing produce identical counters, and that ambiguity has already led
        // to over-reading a leak rate. It cannot *suppress* (only a hook can), so this
        // is diagnosis, not a second capture path.
        let rids = [
            RAWINPUTDEVICE {
                usUsagePage: 0x01, // generic desktop
                usUsage: 0x02,     // mouse
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x01, // generic desktop
                usUsage: 0x06,     // keyboard
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        if RegisterRawInputDevices(
            rids.as_ptr(),
            rids.len() as u32,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        ) == 0
        {
            tracing::warn!("RegisterRawInputDevices failed; falling back to no motion");
        }
    } else {
        tracing::warn!("raw-input window creation failed; no mouse motion will be captured");
    }
    hwnd
}

/// State for the dedicated raw-input thread (its own `thread_local`, separate from the
/// hook thread's `HOOK_STATE`). Motion is gated on the engine's `suppress` flag.
struct RawState {
    suppress: Arc<AtomicBool>,
    sink: UnboundedSender<CaptureSignal>,
    /// Last absolute sample, only for devices that report absolute (some touchpads/RDP).
    last_abs: Option<(i32, i32)>,
}

thread_local! {
    static RAW_STATE: RefCell<Option<RawState>> = const { RefCell::new(None) };
}

impl RawState {
    /// Forward a raw mouse sample as a device delta while suppressing. These deltas are
    /// position-, clamp-, and acceleration-independent.
    fn on_motion(&mut self, flags: u16, lx: i32, ly: i32) {
        if !self.suppress.load(Ordering::SeqCst) {
            self.last_abs = None;
            return;
        }
        let (dx, dy) = if flags & MOUSE_MOVE_ABSOLUTE != 0 {
            // Absolute device: coords are normalized 0..=65535 over the virtual desktop.
            // Convert to pixels and diff consecutive samples.
            let (vx, vy, vw, vh) = virtual_rect();
            let ax = vx + (lx as i64 * (vw as i64 - 1) / 65535) as i32;
            let ay = vy + (ly as i64 * (vh as i64 - 1) / 65535) as i32;
            let d = match self.last_abs {
                Some((px, py)) => (ax - px, ay - py),
                None => (0, 0), // first sample only establishes the reference point
            };
            self.last_abs = Some((ax, ay));
            d
        } else {
            self.last_abs = None;
            (lx, ly)
        };
        if dx != 0 || dy != 0 {
            let _ = self
                .sink
                .send(CaptureSignal::Input(InputEvent::MouseMove { dx, dy }));
        }
    }
}

/// Window proc for the hidden raw-input window: reads mouse deltas from `WM_INPUT`.
unsafe extern "system" fn raw_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        handle_raw_input(lparam as HRAWINPUT);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Pull the `RAWINPUT` for a `WM_INPUT` into a **stack** buffer (no per-event heap
/// allocation — keeps this hot path cheap) and forward a mouse delta.
unsafe fn handle_raw_input(hri: HRAWINPUT) {
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut raw: RAWINPUT = std::mem::zeroed();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    // A mouse RAWINPUT fits in `RAWINPUT`, so a single call suffices. Returns (u32)-1
    // on error, 0 if nothing was copied.
    let got = GetRawInputData(
        hri,
        RID_INPUT,
        (&mut raw as *mut RAWINPUT).cast(),
        &mut size,
        header,
    );
    if got == 0 || got == u32::MAX {
        return;
    }
    if raw.header.dwType == RIM_TYPEKEYBOARD {
        // Witness only — never forwarded and never suppressed. Counts key-down events
        // so a crossing can distinguish "the hook was dead" from "nothing was typed".
        const WM_KEYDOWN_MSG: u32 = 0x0100;
        const WM_SYSKEYDOWN_MSG: u32 = 0x0104;
        let msg = raw.data.keyboard.Message;
        if msg == WM_KEYDOWN_MSG || msg == WM_SYSKEYDOWN_MSG {
            RAW_KEY_DOWNS.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    if raw.header.dwType != RIM_TYPEMOUSE {
        return;
    }
    let m = &raw.data.mouse;
    let (flags, lx, ly) = (m.usFlags, m.lLastX, m.lLastY);
    RAW_STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(state) = guard.as_mut() {
                state.on_motion(flags, lx, ly);
            }
        }
    });
}

/// Spawn the dedicated Raw Input thread. It owns the hidden message window + pump, so
/// the high-frequency `WM_INPUT` mouse stream never runs on the LL-hook thread — which
/// would otherwise blow the keyboard hook's ~300ms `LowLevelHooksTimeout` and drop keys.
fn spawn_raw_input_thread(
    sink: UnboundedSender<CaptureSignal>,
    suppress: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("raw-input".into())
        .spawn(move || raw_input_loop(sink, suppress, stop))
        .expect("spawn raw-input thread")
}

fn raw_input_loop(
    sink: UnboundedSender<CaptureSignal>,
    suppress: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    RAW_STATE.with(|cell| {
        *cell.borrow_mut() = Some(RawState {
            suppress,
            sink,
            last_abs: None,
        });
    });
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hwnd = unsafe { create_raw_input_window(hinstance) };
    if hwnd.is_null() {
        RAW_STATE.with(|cell| *cell.borrow_mut() = None);
        return;
    }
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    'pump: loop {
        unsafe {
            MsgWaitForMultipleObjectsEx(0, std::ptr::null(), 8, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_QUIT {
                break 'pump;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
    }
    unsafe {
        DestroyWindow(hwnd);
        UnregisterClassW(RAW_CLASS.as_ptr(), hinstance);
    }
    RAW_STATE.with(|cell| *cell.borrow_mut() = None);
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(std::mem::zeroed(), code, wparam, lparam);
    }
    MOUSE_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    let swallow = HOOK_STATE.with(|cell| {
        let mut guard = match cell.try_borrow_mut() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(state) = guard.as_mut() else {
            return false;
        };
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        state.on_mouse(wparam as u32, info)
    });
    if swallow {
        1
    } else {
        CallNextHookEx(std::mem::zeroed(), code, wparam, lparam)
    }
}

/// Times the OS has actually invoked the keyboard hook, counted before any state is
/// touched, and times it was invoked but bailed out without deciding.
///
/// These exist because "the hook was never called" and "the hook was called but could
/// not reach its state and fell through" produce the *identical* user-visible result —
/// the keystroke goes to the local app — and were previously *both* silent. Without
/// separating them, a leak cannot be attributed: one means the OS removed or skipped
/// the hook, the other means our own code let the key past. Plain atomics, so counting
/// costs nothing on the hot path and needs no thread-local.
static KBD_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static KBD_HOOK_BAILS: AtomicU64 = AtomicU64::new(0);

/// Physical key-down events seen via Raw Input, which does not depend on the hook.
/// This is the control variable: `RAW_KEY_DOWNS` rising while `KBD_HOOK_CALLS` stays at
/// zero is proof the hook is dead, and only that combination justifies calling it a leak.
static RAW_KEY_DOWNS: AtomicU64 = AtomicU64::new(0);

/// OS invocations of the *mouse* low-level hook. Both hooks live on the same thread and
/// are installed and re-armed together, so this is the field that separates the last two
/// explanations: if the mouse hook is called during a crossing where the keyboard hook is
/// not, the fault is specific to `WH_KEYBOARD_LL`; if both are zero, nothing on that
/// thread is being called and the fault is thread- or process-wide. Mouse *motion*
/// working proves nothing here — motion comes from Raw Input, not from this hook.
static MOUSE_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(std::mem::zeroed(), code, wparam, lparam);
    }
    KBD_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    let swallow = HOOK_STATE.with(|cell| {
        let mut guard = match cell.try_borrow_mut() {
            Ok(g) => g,
            Err(_) => {
                // Re-entered while the state was already borrowed. Falling through
                // hands the key to the local app — a leak — so make it countable.
                KBD_HOOK_BAILS.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        let Some(state) = guard.as_mut() else {
            // Called on a thread with no hook state (or after teardown). Same leak,
            // different cause; both must be visible.
            KBD_HOOK_BAILS.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        state.on_key(info)
    });
    if swallow {
        1
    } else {
        CallNextHookEx(std::mem::zeroed(), code, wparam, lparam)
    }
}

/// Snapshot physically-held modifiers at the moment suppression engages, so the
/// peer doesn't miss a held Shift/Ctrl/Alt/Super (its down-transition happened
/// while we were still local). Mirrors the X11 `query_keymap` snapshot.
fn emit_held_modifiers(sink: &UnboundedSender<CaptureSignal>) {
    let mods = [
        (vk::LSHIFT, NamedKey::ShiftL),
        (vk::RSHIFT, NamedKey::ShiftR),
        (vk::LCONTROL, NamedKey::CtrlL),
        (vk::RCONTROL, NamedKey::CtrlR),
        (vk::LMENU, NamedKey::AltL),
        (vk::RMENU, NamedKey::AltGr),
        (vk::LWIN, NamedKey::SuperL),
        (vk::RWIN, NamedKey::SuperR),
    ];
    for (v, named) in mods {
        let down = unsafe { GetAsyncKeyState(v as i32) } as u16 & 0x8000 != 0;
        if down {
            let _ = sink.send(CaptureSignal::Input(InputEvent::Key {
                key: Key::Named(named),
                pressed: true,
            }));
        }
    }
}

/// The capture thread: installs both LL hooks and pumps messages so the OS
/// delivers the hook callbacks, reconciling suppression each tick.
fn capture_loop(
    sink: UnboundedSender<CaptureSignal>,
    suppress: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    heartbeat: Arc<AtomicU64>,
    return_vk: u16,
    diag: std::sync::mpsc::Sender<DiagEvent>,
) -> Result<(), InputError> {
    ensure_dpi_aware();

    // Windows delivers low-level hook callbacks on this thread and gives them only
    // `LowLevelHooksTimeout` (~300 ms) to return; miss that and the OS skips the hook,
    // handing the keystroke straight to the local app. This thread therefore must run
    // ahead of everything else in the process — including the GUI's wgpu rendering,
    // which is the one thing here heavy enough to starve it. Time-critical is
    // affordable because the loop is idle-waiting almost always: it blocks in
    // `MsgWaitForMultipleObjectsEx` and does O(1) work per wake.
    unsafe {
        if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) == 0 {
            tracing::warn!("could not raise capture thread priority; hook may be starved");
        }
    }

    HOOK_STATE.with(|cell| {
        *cell.borrow_mut() = Some(HookState {
            suppress: suppress.clone(),
            grabbed: false,
            sink: sink.clone(),
            return_vk,
            last_cursor: None,
            kbd_swallowed: 0,
            kbd_leaked: 0,
            grabbed_at: None,
            calls_at_grab: 0,
            bails_at_grab: 0,
            raw_keys_at_grab: 0,
            mouse_calls_at_grab: 0,
            diag: diag.clone(),
        });
    });

    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let mut h_mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinstance, 0) };
    let mut h_kbd = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinstance, 0) };
    let installed = !is_null_hhook(h_mouse) && !is_null_hhook(h_kbd);
    if !installed {
        if !is_null_hhook(h_mouse) {
            unsafe { UnhookWindowsHookEx(h_mouse) };
        }
        if !is_null_hhook(h_kbd) {
            unsafe { UnhookWindowsHookEx(h_kbd) };
        }
        HOOK_STATE.with(|cell| *cell.borrow_mut() = None);
        let _ = sink.send(CaptureSignal::SuppressRejected);
        return Err(InputError::Backend("SetWindowsHookEx failed".into()));
    }

    // Motion is captured via Raw Input on its OWN thread (device-level deltas), kept off
    // this LL-hook thread so a fast mouse can't starve the keyboard hook's timeout.
    let raw_thread = spawn_raw_input_thread(sink.clone(), suppress.clone(), stop.clone());

    // Pump: wake on input or every ~8ms (so toggles/stop and the heartbeat are
    // honoured even with no input), drain messages so the hooks fire, reconcile.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // Low-level hook callbacks are delivered on THIS thread during this pump. If a single
    // iteration takes longer than Windows' LowLevelHooksTimeout (~300 ms by default), then
    // on Windows 7 and later **the OS silently removes the hook** — it is not merely
    // skipped for that one keystroke, it never fires again. Nothing reports this: the pump
    // afterwards runs perfectly cleanly, `SetWindowsHookEx` gave us a handle that is now
    // meaningless, and there is no API to ask whether a hook is still alive.
    //
    // That is the "keyboard didn't follow the mouse" bug. It is intermittent because it
    // needs one unlucky 300 ms hiccup; it lasts for the rest of the run because removal is
    // permanent; and the mouse keeps crossing because motion comes from Raw Input on a
    // different thread, which is unaffected. Reacting to a *concurrently observed* stall is
    // useless — by the time anything is noticed the hook is long gone.
    //
    // So the hooks are re-installed unconditionally: once at the start of every crossing
    // (a fresh hook per crossing is the property that actually matters), and periodically
    // while suppressing in case one dies mid-crossing. Re-installing costs a single API
    // call and is harmless when the hook was alive.
    let mut last_tick = std::time::Instant::now();
    let mut prev_suppress = false;
    let mut last_rearm = std::time::Instant::now();
    // The most recent foreground window belonging to *another* process, and the window of
    // ours we took foreground from, so it can be handed back on return.
    //
    // Why this exists — the root cause of the "keyboard doesn't follow the mouse" bug:
    // when the foreground window belongs to **this** process, Windows does not deliver
    // `WH_KEYBOARD_LL` callbacks to us. Measured across two runs: every crossing made with
    // another process focused had a live hook (14 and 56 callbacks); every crossing made
    // with our own panel focused had `hook_calls=0` while Raw Input counted up to 93 real
    // keystrokes. The keys then land in our own panel — which is exactly what the user
    // sees, typing into the control panel while the mouse is on the peer.
    //
    // It is invisible to every other check: the hook installs successfully, the pump is
    // healthy, no stall is reported, and re-installing changes nothing, because the hook
    // was never broken — it is simply not being called.
    //
    // The remedy is to stop being the foreground process for the duration of a crossing.
    // Handing foreground to the window the user was last on is also what they'd expect:
    // control has moved to the other machine, so the panel has no reason to hold focus.
    let mut last_foreign_fg: HWND = std::ptr::null_mut();
    let mut last_checked_fg: *mut core::ffi::c_void = std::ptr::null_mut();
    // Our own integrity level, read once: it cannot change while we run.
    let our_integrity = integrity_of(std::process::id()).unwrap_or(0x2000);
    let mut yielded_from: HWND = std::ptr::null_mut();
    // When to check that a requested hand-off actually happened.
    //
    // `SetForegroundWindow` returning non-zero means the request was accepted, not that the
    // foreground has moved — the activation is delivered through the target thread's queue
    // and the system can decline it. So "handed foreground to X" was never evidence that we
    // stopped owning the foreground, and if we didn't, the keyboard hook is still starved
    // while the log claims the fix worked. That is the same shape of mistake as counting
    // `hook_calls=0` as proof: an instrument that cannot see its own failure. Re-check a
    // moment later and say plainly which of the two happened.
    let mut verify_at: Option<std::time::Instant> = None;
    /// How often to re-arm while control is on the peer. Short enough that a mid-crossing
    /// removal costs a couple of keystrokes rather than the whole session.
    const REARM_INTERVAL: Duration = Duration::from_millis(1000);
    'pump: loop {
        unsafe {
            MsgWaitForMultipleObjectsEx(0, std::ptr::null(), 8, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_QUIT {
                break 'pump;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        heartbeat.fetch_add(1, Ordering::Relaxed);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // 150 ms is half the default hook timeout: close enough to matter, far enough
        // above the 8 ms wait that normal scheduling jitter stays quiet. Purely
        // diagnostic now — the repair below no longer waits to catch a stall.
        let gap = last_tick.elapsed();
        let want = suppress.load(Ordering::SeqCst);
        if gap >= Duration::from_millis(150) && want {
            let _ = diag.send(DiagEvent::Stalled {
                ms: gap.as_millis(),
                rearmed: true,
            });
        }
        last_tick = std::time::Instant::now();

        // Re-arm on the rising edge of a crossing, and then once a second for as long as
        // control stays on the peer. The rising edge is the important one: it guarantees
        // every crossing begins with a hook the OS has definitely not removed.
        // Track the last foreground window that isn't ours, so there is somewhere to hand
        // focus back to. Cheap enough for the 8 ms tick: two non-blocking calls.
        let fg = unsafe { GetForegroundWindow() };
        let fg_is_ours = !fg.is_null() && {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(fg, &mut pid) };
            pid == std::process::id()
        };
        if !fg.is_null() && !fg_is_ours {
            last_foreign_fg = fg;
        }

        // Warn once per window when the thing in front runs at a higher integrity level
        // than we do. While it has the foreground Windows will not call our low-level
        // hooks at all, so the pointer cannot cross and nothing else in this program
        // ever finds out why. Checked only when the foreground actually changes —
        // opening a process token per mouse event would be absurd.
        if fg != last_checked_fg {
            last_checked_fg = fg;
            if let Some((what, level)) = foreground_blocks_our_hooks(our_integrity) {
                tracing::warn!(
                    window = %what,
                    their_level = format!("{:#x} ({})", level, integrity_name(level)),
                    our_level = format!("{:#x} ({})", our_integrity, integrity_name(our_integrity)),
                    "CROSSING WILL NOT WORK while this window is in front. It runs at a higher \
                     Windows integrity level than mouse-rs, and Windows does not deliver \
                     low-level mouse or keyboard hooks to a lower-integrity process. Nothing is \
                     broken and there is no workaround inside the program — run mouse-rs as \
                     administrator to match it."
                );
            }
        }

        let crossing_started = want && !prev_suppress;
        if crossing_started && fg_is_ours {
            // Candidates in order of how invisible they are to the user. The first was
            // once the only one, and when it was null — which is the state right after
            // launch, before any other window has been focused — the hand-off was simply
            // abandoned and every keystroke stayed on this machine. There is always a
            // shell window and always a desktop window, so "nothing to hand to" is not a
            // real state; it was only ever a gap in this list.
            let mut handed: Option<(*mut core::ffi::c_void, &'static str)> = None;
            let candidates: [(*mut core::ffi::c_void, &'static str); 3] = [
                (last_foreign_fg, "the window that had focus before us"),
                (unsafe { GetShellWindow() }, "the shell window"),
                (unsafe { GetDesktopWindow() }, "the desktop"),
            ];
            for (hwnd, what) in candidates {
                if hwnd.is_null() {
                    continue;
                }
                if unsafe { SetForegroundWindow(hwnd) } != 0 {
                    handed = Some((hwnd, what));
                    break;
                }
            }
            match handed {
                Some((hwnd, what)) => {
                    yielded_from = fg;
                    // 120 ms: long enough for the target thread to have pumped the
                    // activation, short enough that the answer arrives inside the crossing
                    // it belongs to rather than after it.
                    verify_at = Some(std::time::Instant::now() + Duration::from_millis(120));
                    let _ = diag.send(DiagEvent::FocusHandedOff {
                        ok: true,
                        to: format!("{} ({what})", describe_window(hwnd)),
                    });
                }
                None => {
                    // Everything refused. Minimising ourselves cannot be refused, and a
                    // panel that ducks out of the way is a far smaller problem than a
                    // keyboard that silently stops crossing.
                    unsafe { ShowWindow(fg, SW_MINIMIZE) };
                    yielded_from = std::ptr::null_mut(); // do not restore a window we minimised
                    let _ = diag.send(DiagEvent::FocusHandedOff {
                        ok: true,
                        to: "nothing would take focus, so our own window was minimised".into(),
                    });
                }
            }
        }
        // Did the foreground actually leave? One retry, then report either way — a second
        // refusal is a fact about this machine that belongs in the log, not a silent state
        // the next investigation has to rediscover.
        if verify_at.is_some_and(|due| due <= std::time::Instant::now()) {
            verify_at = None;
            if want && fg_is_ours {
                let retried =
                    !last_foreign_fg.is_null() && unsafe { SetForegroundWindow(last_foreign_fg) } != 0;
                let _ = diag.send(DiagEvent::FocusHandoffIneffective { retried });
            }
        }
        // Control came back: return foreground to whatever of ours had it, so the hand-off
        // is invisible to the user rather than silently rearranging their windows.
        if !want && prev_suppress && !yielded_from.is_null() {
            unsafe { SetForegroundWindow(yielded_from) };
            yielded_from = std::ptr::null_mut();
        }
        if crossing_started || (want && last_rearm.elapsed() >= REARM_INTERVAL) {
            rearm_hooks(hinstance, &mut h_mouse, &mut h_kbd, &diag, crossing_started);
            last_rearm = std::time::Instant::now();
        }
        prev_suppress = want;

        reconcile(&suppress);
    }

    unsafe {
        UnhookWindowsHookEx(h_mouse);
        UnhookWindowsHookEx(h_kbd);
    }
    // Ensure the raw-input thread exits (it polls `stop` every ≤8ms) and reap it, so its
    // window/class are torn down before we return.
    stop.store(true, Ordering::SeqCst);
    let _ = raw_thread.join();
    HOOK_STATE.with(|cell| *cell.borrow_mut() = None);
    Ok(())
}

/// Reconcile the engine's requested suppression against the actual grabbed state,
/// snapshotting held modifiers on the rising edge. No cursor warp is needed: motion
/// comes from Raw Input, which is independent of the (now merely frozen) cursor.
/// Describe the window that currently has keyboard focus: title, class and owning pid.
///
/// Logged when suppression engages because of a field report that keystrokes are not
/// shared *specifically when the mouse-rs panel itself is the focused window* — this is
/// what turns that observation into evidence rather than a hunch. Reading it is safe from
/// the pump thread: all three calls are non-blocking and touch no shared state.
/// Short description of an arbitrary window, for the focus hand-off log.
fn describe_window(hwnd: HWND) -> String {
    unsafe {
        if hwnd.is_null() {
            return "<none>".into();
        }
        let mut title = [0u16; 96];
        let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        format!(
            "'{}' pid={pid}",
            String::from_utf16_lossy(&title[..n.max(0) as usize])
        )
    }
}

fn foreground_desc() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return "<none>".into();
        }
        let mut title = [0u16; 96];
        let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        let title = String::from_utf16_lossy(&title[..n.max(0) as usize]);
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let own = pid == std::process::id();
        format!(
            "'{title}' class={class} pid={pid}{}",
            if own { " (THIS IS OUR OWN PANEL)" } else { "" }
        )
    }
}

fn reconcile(suppress: &Arc<AtomicBool>) {
    let want = suppress.load(Ordering::SeqCst);
    HOOK_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };
        if want {
            // Normally the hook procs have already done this on the first event after
            // the crossing; this covers a crossing with no input at all yet, so the
            // engine still learns the modifier state promptly.
            state.begin_grab();
        } else if state.grabbed {
            state.grabbed = false;
            let ms = state
                .grabbed_at
                .take()
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0);
            // Summary instead of a running counter: enough to spot a leak after the
            // fact, cheap enough to always be on.
            let _ = state.diag.send(DiagEvent::Released {
                swallowed: state.kbd_swallowed,
                leaked: state.kbd_leaked,
                remote_ms: ms,
                hook_calls: KBD_HOOK_CALLS
                    .load(Ordering::Relaxed)
                    .saturating_sub(state.calls_at_grab),
                hook_bails: KBD_HOOK_BAILS
                    .load(Ordering::Relaxed)
                    .saturating_sub(state.bails_at_grab),
                raw_keys: RAW_KEY_DOWNS
                    .load(Ordering::Relaxed)
                    .saturating_sub(state.raw_keys_at_grab),
                mouse_calls: MOUSE_HOOK_CALLS
                    .load(Ordering::Relaxed)
                    .saturating_sub(state.mouse_calls_at_grab),
            });
        }
    });
}

/// Un-freeze watchdog — only sleeps and reads atomics, so it can never itself
/// wedge, and is **non-terminal** (a recovered/starved pump keeps working). It is
/// active only while suppression is engaged and provides two hook-independent
/// safety nets:
///
/// 1. **Hook-independent hotkey escape:** polls the return hotkey via
///    `GetAsyncKeyState`. If it's physically held, releases suppression even when
///    the keyboard hook itself has been removed (so a dead keyboard hook can't trap
///    a frozen cursor — the in-callback self-release would be unreachable then).
/// 2. **Pump-liveness:** if the pump heartbeat hasn't advanced for ~2s while
///    suppressing, the pump has gone dark (the OS already bypasses/removes a wedged
///    LL hook, so input isn't actually frozen) — release suppression and tell the
///    engine to revert to Local. The ~2s threshold avoids tearing down a merely
///    scheduler-starved pump.
///
/// It does NOT unhook cross-thread (not documented-safe) or kill the pump; teardown
/// stays the responsibility of `stop()`/`Drop`.
fn watchdog(
    stop: Arc<AtomicBool>,
    suppress: Arc<AtomicBool>,
    heartbeat: Arc<AtomicU64>,
    sink: UnboundedSender<CaptureSignal>,
    return_vk: u16,
) {
    // ~2s of no pump progress while suppressing before we assume the pump went dark.
    const STALE_TICKS: u32 = 8;
    let mut last_seen = heartbeat.load(Ordering::Relaxed);
    let mut stale = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if !suppress.load(Ordering::SeqCst) {
            last_seen = heartbeat.load(Ordering::Relaxed);
            stale = 0;
            continue;
        }
        // (1) Hook-independent escape: the return hotkey is physically held.
        let hotkey_down = unsafe { GetAsyncKeyState(return_vk as i32) } as u16 & 0x8000 != 0;
        if hotkey_down {
            force_release(&suppress, &sink);
            stale = 0;
            continue;
        }
        // (2) Pump-liveness: heartbeat stalled while suppressing.
        let hb = heartbeat.load(Ordering::Relaxed);
        if hb != last_seen {
            last_seen = hb;
            stale = 0;
        } else {
            stale += 1;
            if stale >= STALE_TICKS {
                force_release(&suppress, &sink);
                stale = 0;
            }
        }
    }
}

/// Re-install both low-level hooks, replacing any the OS has silently removed.
///
/// Must run on the pump thread — a hook belongs to the thread that installed it. The
/// replacement is installed **first** and the old handle dropped only on success, so a
/// failed re-install leaves the existing hook in place rather than leaving us with none.
/// The brief overlap is harmless: the new hook sits at the front of the chain and
/// swallows without calling on, so the old one simply never sees an event.
fn rearm_hooks(
    hinstance: windows_sys::Win32::Foundation::HINSTANCE,
    h_mouse: &mut windows_sys::Win32::UI::WindowsAndMessaging::HHOOK,
    h_kbd: &mut windows_sys::Win32::UI::WindowsAndMessaging::HHOOK,
    diag: &std::sync::mpsc::Sender<DiagEvent>,
    report: bool,
) {
    let mut ok = true;
    let fresh_kbd = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinstance, 0) };
    if is_null_hhook(fresh_kbd) {
        ok = false;
    } else {
        unsafe { UnhookWindowsHookEx(*h_kbd) };
        *h_kbd = fresh_kbd;
    }
    let fresh_mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinstance, 0) };
    if is_null_hhook(fresh_mouse) {
        ok = false;
    } else {
        unsafe { UnhookWindowsHookEx(*h_mouse) };
        *h_mouse = fresh_mouse;
    }
    // Only announce the per-crossing re-arm, not the once-a-second upkeep: one line per
    // crossing is evidence, one line per second is the `kbd_diag` spam we deleted before.
    if report {
        let _ = diag.send(DiagEvent::Rearmed { ok });
    }
}

/// Drain [`DiagEvent`]s and write them to the log. Runs on its own thread so that the
/// file I/O — which can block on the writer's lock, contended with the GUI thread —
/// happens nowhere near the hook pump. Exits when the pump drops its sender.
fn diag_loop(rx: std::sync::mpsc::Receiver<DiagEvent>) {
    while let Ok(evt) = rx.recv() {
        match evt {
            DiagEvent::Grabbed { focus } => {
                tracing::info!(focus = %focus, "keyboard grabbed (control moved to the peer)");
            }
            DiagEvent::Released {
                swallowed,
                leaked,
                remote_ms,
                hook_calls,
                hook_bails,
                raw_keys,
                mouse_calls,
            } => {
                // The three-way verdict the whole investigation turns on. Stated in
                // words, because the counters alone have repeatedly been misread.
                if hook_calls == 0 && raw_keys > 0 {
                    // Raw Input saw real keystrokes while the hook was never invoked.
                    // This is the leak, proven — not inferred from a silent counter.
                    tracing::warn!(
                        swallowed,
                        hook_calls = 0,
                        raw_keys,
                        mouse_calls,
                        remote_ms,
                        "keyboard released — THE OS NEVER CALLED OUR KEYBOARD HOOK. \
                         Raw Input saw {raw_keys} keystrokes that the hook never got; \
                         every one of them went to this machine. mouse_calls={mouse_calls} \
                         (0 = nothing on that thread is being called; >0 = keyboard-specific)"
                    );
                } else if hook_calls == 0 {
                    tracing::info!(
                        remote_ms,
                        "keyboard released (nothing was typed on this crossing)"
                    );
                } else if hook_bails > 0 {
                    tracing::warn!(
                        swallowed,
                        hook_calls,
                        hook_bails,
                        remote_ms,
                        "keyboard released — THE HOOK RAN BUT FELL THROUGH. Our own code let \
                         keystrokes past; this is a bug on this side, not an OS removal."
                    );
                } else if leaked > 0 {
                    tracing::warn!(
                        swallowed,
                        leaked,
                        hook_calls,
                        remote_ms,
                        "keyboard released — SOME KEYS LEAKED to this machine"
                    );
                } else {
                    // raw_keys on the healthy path too, so a *partial* loss is visible:
                    // hook_calls far below raw_keys means some keystrokes were missed even
                    // though the hook was alive, which the total-loss check cannot catch.
                    tracing::info!(
                        swallowed,
                        leaked = 0,
                        hook_calls,
                        raw_keys,
                        mouse_calls,
                        remote_ms,
                        "keyboard released (no leaks)"
                    );
                }
            }
            DiagEvent::Leak { vk, reason, n } => {
                tracing::warn!(
                    vk = format_args!("0x{vk:02x}"),
                    reason,
                    leak_n = n,
                    "KEYBOARD LEAK: key swallowed here but not sent to the peer"
                );
            }
            DiagEvent::FocusHandedOff { ok, to } => {
                if ok {
                    tracing::info!(
                        to = %to,
                        "our own window had focus at the crossing; handed foreground to \
                         another process so the keyboard hook is delivered"
                    );
                } else {
                    tracing::warn!(
                        "our own window had focus at the crossing and foreground could NOT \
                         be handed off — expect keystrokes to stay on this machine"
                    );
                }
            }
            DiagEvent::FocusHandoffIneffective { retried } => {
                tracing::warn!(
                    retried,
                    "the foreground did NOT leave this process after the hand-off — \
                     SetForegroundWindow was accepted but we still own the foreground, so \
                     the keyboard hook stays starved and keystrokes will remain on this \
                     machine. This is the fix being refused, not the bug returning."
                );
            }
            DiagEvent::Rearmed { ok } => {
                if ok {
                    tracing::info!("keyboard + mouse hooks re-armed for this crossing");
                } else {
                    tracing::warn!(
                        "could not re-install a low-level hook — if the OS had already \
                         removed it, input will not be captured this crossing"
                    );
                }
            }
            DiagEvent::Stalled { ms, rearmed } => {
                tracing::warn!(
                    stalled_ms = ms,
                    rearmed,
                    "capture pump stalled while suppressing — Windows may have SKIPPED the \
                     keyboard hook, letting keystrokes reach this machine"
                );
            }
        }
    }
}

/// Drop suppression and tell the engine to revert to Local. Used by the watchdog's
/// failsafe paths; setting the shared flag false also unfreezes input via the pump
/// (or the OS hook-bypass if the pump is dark).
fn force_release(suppress: &Arc<AtomicBool>, sink: &UnboundedSender<CaptureSignal>) {
    suppress.store(false, Ordering::SeqCst);
    let _ = sink.send(CaptureSignal::SuppressRejected);
}

fn is_null_hhook(h: windows_sys::Win32::UI::WindowsAndMessaging::HHOOK) -> bool {
    h.is_null()
}

/// Join `handle`, but never block longer than `timeout`: detach a wedged thread
/// rather than hang the caller (the "Stop never hangs" guarantee). Mirrors the X11
/// backend's bounded join.
fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(timeout).is_err() {
        tracing::warn!("win capture thread did not stop within {timeout:?}; detaching");
    }
}

pub struct WinCapture {
    suppress: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread_id: Arc<AtomicU32>,
    join: Option<JoinHandle<()>>,
    wd_join: Option<JoinHandle<()>>,
}

impl WinCapture {
    pub fn start(
        sink: UnboundedSender<CaptureSignal>,
        suppress: Arc<AtomicBool>,
        return_hotkey: Option<NamedKey>,
    ) -> Result<WinCapture, InputError> {
        ensure_dpi_aware();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(AtomicU32::new(0));
        let heartbeat = Arc::new(AtomicU64::new(0));
        // The return-hotkey VK so the keyboard hook (and the watchdog) can self-release
        // suppression. Falls back to ScrollLock for an unset / non-named hotkey, matching
        // the engine's own default. Note: if a bare modifier (e.g. "shift") is configured,
        // named_to_vk yields one side, so the hook-independent failsafe only recognizes
        // that side (the engine's L/R-collapsing path still handles either) — a non-issue
        // for sane lock keys (ScrollLock/Pause/F-keys), which a modifier is not.
        let return_vk = return_hotkey.map(|n| named_to_vk(n).0).unwrap_or(vk::SCROLL);

        let wd_sink = sink.clone();
        let (s2, st2, hb2, tid2) = (
            suppress.clone(),
            stop.clone(),
            heartbeat.clone(),
            thread_id.clone(),
        );
        // Diagnostics are logged off the pump thread; see `DiagEvent`.
        let (diag_tx, diag_rx) = std::sync::mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("win-capture-diag".into())
            .spawn(move || diag_loop(diag_rx));

        let join = std::thread::Builder::new()
            .name("win-capture".into())
            .spawn(move || {
                tid2.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
                if let Err(e) = capture_loop(sink, s2, st2, hb2, return_vk, diag_tx) {
                    tracing::error!("win capture loop ended: {e}");
                }
            })
            .map_err(|e| InputError::Backend(format!("spawn capture thread: {e}")))?;

        let (st3, s3, hb3) = (stop.clone(), suppress.clone(), heartbeat.clone());
        let wd_join = std::thread::Builder::new()
            .name("win-capture-watchdog".into())
            .spawn(move || watchdog(st3, s3, hb3, wd_sink, return_vk))
            .ok();

        Ok(WinCapture {
            suppress,
            stop,
            thread_id,
            join: Some(join),
            wd_join,
        })
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let tid = self.thread_id.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                PostThreadMessageW(tid, WM_QUIT, 0, 0);
            }
        }
    }
}

impl InputCapture for WinCapture {
    fn set_suppress(&self, on: bool) {
        self.suppress.store(on, Ordering::SeqCst);
    }

    fn stop(mut self: Box<Self>) {
        self.signal_stop();
        if let Some(j) = self.join.take() {
            join_bounded(j, Duration::from_millis(500));
        }
        if let Some(j) = self.wd_join.take() {
            join_bounded(j, Duration::from_millis(500));
        }
    }
}

impl Drop for WinCapture {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(j) = self.join.take() {
            join_bounded(j, Duration::from_millis(500));
        }
        if let Some(j) = self.wd_join.take() {
            join_bounded(j, Duration::from_millis(500));
        }
    }
}

// ----------------------------------------------------------------------------
// Integrity levels — why crossing stops working over an elevated window
// ----------------------------------------------------------------------------

/// Windows integrity level of a process, as a raw RID.
///
/// The values that matter: `0x2000` medium (an ordinary program), `0x3000` high (a
/// program started with "Run as administrator"), `0x4000` system.
///
/// ## Why this is here
///
/// User Interface Privilege Isolation (UIPI) stops a lower-integrity process from
/// interfering with a higher-integrity one, and low-level hooks are part of what it
/// blocks: while an **elevated** window has the foreground, Windows does not call the
/// `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks belonging to a process running at medium
/// integrity. There is no error and no callback — the input simply never reaches us.
///
/// The visible symptom is that the pointer will not cross to the other machine while a
/// game launcher, an installer, or anything else started as administrator is in front,
/// and works again the moment it is not. That reads as a bug in the crossing code, and
/// it is not one: the crossing code is never told the pointer moved.
///
/// It cannot be fixed from inside a medium-integrity process. The only remedy is to run
/// mouse-rs elevated too. So the least this can do is *say so*, rather than leave the
/// user to conclude the software is broken.
fn integrity_of(pid: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, OpenProcessToken};

    // QUERY_LIMITED_INFORMATION rather than QUERY_INFORMATION: the limited right is
    // granted across integrity levels, which is exactly the case being measured.
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    unsafe {
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if proc.is_null() {
            return None;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(proc, TOKEN_QUERY, &mut token) == 0 {
            CloseHandle(proc);
            return None;
        }
        // Two calls: the first learns the size, the second fills the buffer.
        let mut needed = 0u32;
        GetTokenInformation(token, TokenIntegrityLevel, std::ptr::null_mut(), 0, &mut needed);
        let mut buf = vec![0u8; needed.max(1) as usize];
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) != 0;
        let level = if ok {
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            // The integrity level is the last sub-authority of the label's SID.
            let sid = label.Label.Sid;
            if sid.is_null() {
                None
            } else {
                let count = *windows_sys::Win32::Security::GetSidSubAuthorityCount(sid);
                if count == 0 {
                    None
                } else {
                    Some(*windows_sys::Win32::Security::GetSidSubAuthority(
                        sid,
                        u32::from(count - 1),
                    ))
                }
            }
        } else {
            None
        };
        CloseHandle(token);
        CloseHandle(proc);
        level
    }
}

/// This process's privilege level, for the panel to show and act on.
pub(crate) fn process_elevation() -> crate::Elevation {
    match integrity_of(std::process::id()) {
        Some(l) if l >= 0x3000 => crate::Elevation::Elevated,
        // Unreadable is treated as normal: claiming elevation we cannot prove would be
        // worse than the warning being shown once too often.
        _ => crate::Elevation::Normal,
    }
}

/// A human name for an integrity RID, for a log line someone has to act on.
fn integrity_name(level: u32) -> &'static str {
    match level {
        l if l >= 0x4000 => "system",
        l if l >= 0x3000 => "elevated (administrator)",
        l if l >= 0x2000 => "normal",
        _ => "low",
    }
}

/// Is the foreground window owned by a process we cannot receive input for?
///
/// Returns the offending window's description and level when so. Checked on foreground
/// changes rather than continuously: opening a process token is far too expensive to do
/// per mouse event, and the answer only changes when the focused window does.
fn foreground_blocks_our_hooks(our_level: u32) -> Option<(String, u32)> {
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_null() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, &mut pid) };
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    let level = integrity_of(pid)?;
    (level > our_level).then(|| (describe_window(fg), level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_named_roundtrip() {
        for n in [
            NamedKey::Enter,
            NamedKey::Up,
            NamedKey::Delete,
            NamedKey::F(1),
            NamedKey::F(12),
            NamedKey::F(24),
            NamedKey::ShiftL,
            NamedKey::ShiftR,
            NamedKey::CtrlR,
            NamedKey::SuperR,
            NamedKey::ScrollLock,
        ] {
            let (v, ext) = named_to_vk(n);
            assert_eq!(vk_to_named(v, ext), Some(n), "roundtrip {n:?}");
        }
    }

    #[test]
    fn extended_key_flags_are_correct() {
        // Right Shift is NOT extended (it has a separate scancode); right Ctrl/Alt
        // and the navigation cluster ARE — getting this wrong injects malformed keys.
        assert!(!named_to_vk(NamedKey::ShiftR).1, "RShift must not be extended");
        assert!(!named_to_vk(NamedKey::ShiftL).1);
        assert!(named_to_vk(NamedKey::CtrlR).1, "RCtrl is extended");
        assert!(named_to_vk(NamedKey::AltGr).1, "RAlt/AltGr is extended");
        assert!(named_to_vk(NamedKey::Up).1, "arrows are extended");
        assert!(named_to_vk(NamedKey::Delete).1);
        assert!(named_to_vk(NamedKey::Home).1);
        assert!(!named_to_vk(NamedKey::Enter).1, "main Enter is not extended");
        assert!(!named_to_vk(NamedKey::F(5)).1);
    }

    #[test]
    fn altgr_maps_to_and_from_rmenu() {
        let (v, ext) = named_to_vk(NamedKey::AltGr);
        assert_eq!(v, vk::RMENU);
        assert!(ext);
        assert_eq!(vk_to_named(vk::RMENU, true), Some(NamedKey::AltGr));
    }

    #[test]
    fn generic_modifiers_resolve_a_side() {
        assert_eq!(vk_to_named(vk::CONTROL, true), Some(NamedKey::CtrlR));
        assert_eq!(vk_to_named(vk::CONTROL, false), Some(NamedKey::CtrlL));
        assert_eq!(vk_to_named(vk::SHIFT, false), Some(NamedKey::ShiftL));
    }

    #[test]
    fn wheel_delta_decoding() {
        assert_eq!(wheel_delta(WHEEL_DELTA << 16), 1);
        assert_eq!(wheel_delta(((-(WHEEL_DELTA as i32)) as u32) << 16 & 0xFFFF_0000), -1);
        // High-resolution sub-notch still yields a 1-step floor.
        assert_eq!(wheel_delta(40u32 << 16), 1);
        assert_eq!(wheel_delta(0), 0);
    }

    #[test]
    fn unknown_vk_is_not_named() {
        assert_eq!(vk_to_named(0x00, false), None);
    }
}
