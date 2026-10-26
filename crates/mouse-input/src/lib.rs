//! OS input capture and injection for mouse-rs, behind OS-neutral traits.
//!
//! Two capabilities, each implemented per-OS so *either* machine can be the
//! server or the client (Linux↔Linux, Linux↔Windows, Windows↔Linux, Windows↔
//! Windows):
//!
//! * [`InputInjector`] — apply an [`InputEvent`] to this machine (client side,
//!   and the server uses it to recenter/warp its own cursor).
//! * [`InputCapture`] — capture local input and, while suppressed, swallow it so
//!   it doesn't reach local apps (server side). Runs on its own native thread
//!   (Windows hooks need a message pump; X11 grabs need a dedicated connection),
//!   talking to the async engine over a channel — so a stuck hook can never
//!   block the network task.

pub mod event;
#[cfg(any(target_os = "windows", test))]
mod held;
pub mod keys;

pub use event::InputEvent;
pub use keys::{Key, NamedKey};
pub use mouse_core::protocol::Button;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod motion;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub mod uinput;

#[cfg(target_os = "windows")]
mod windows;

/// A cheap, cloneable handle to toggle a running capture's suppression, separate
/// from the capture's teardown handle. This lets the async engine flip
/// suppression while ownership of the capture (and its blocking thread join)
/// stays with the caller, so teardown can happen off the async reactor.
#[derive(Clone)]
pub struct Suppressor(Arc<AtomicBool>);

impl Suppressor {
    /// A detached suppressor not wired to any capture — useful in tests that
    /// drive the engine without a real X grab.
    pub fn new() -> Suppressor {
        Suppressor(Arc::new(AtomicBool::new(false)))
    }
    /// The shared flag the capture backend reconciles against. Only consumed by a
    /// platform capture backend; dead on targets whose `start_capture` is still the
    /// `Unsupported` stub.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "windows")),
        allow(dead_code)
    )]
    fn shared(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
    /// Turn suppression on/off. Honoured by the capture thread within its poll
    /// window (a few ms), so the engine must not assume it took effect instantly.
    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
    /// Current requested suppression state (used in tests).
    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Default for Suppressor {
    fn default() -> Self {
        Suppressor::new()
    }
}

/// What the capture thread reports to the engine. The variant is decoupled from
/// the engine's notion of state on purpose: the suppression toggle is honoured
/// asynchronously by the capture thread, so for a brief window the "wrong" kind
/// of signal can still arrive. The engine routes by **its own state**, ignoring
/// `Input` while local and `Cursor` while remote, which makes that race benign.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureSignal {
    /// Absolute pointer position, reported while **not** suppressing, so the
    /// engine can watch for the cursor reaching the crossing edge.
    Cursor { x: i32, y: i32 },
    /// A captured input event, reported while suppressing, to forward to the peer.
    /// On entering suppression the capture also emits synthetic `Key` presses for
    /// any modifier physically held at the moment of crossing, so the peer doesn't
    /// miss a held Shift/Ctrl/Alt/Super.
    Input(InputEvent),
    /// The capture backend asked to grab (suppress) but the grab was refused —
    /// another client holds an active grab (a menu, drag-and-drop, or a screen
    /// locker). The engine should revert to Local rather than strand the user in a
    /// pseudo-Remote state with input still reaching local apps.
    SuppressRejected,
}

/// Errors from the platform input backends.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("no input backend for this platform")]
    Unsupported,
    #[error("input backend error: {0}")]
    Backend(String),
}

/// Inject input events into the local machine.
pub trait InputInjector: Send {
    /// Move the pointer by a relative delta.
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InputError>;
    /// Move the pointer to an absolute screen position (used for warp/recenter).
    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<(), InputError>;
    /// Press or release a mouse button.
    fn mouse_button(&mut self, button: Button, pressed: bool) -> Result<(), InputError>;
    /// Scroll. Positive `dy` = up, positive `dx` = right.
    fn mouse_scroll(&mut self, dx: i32, dy: i32) -> Result<(), InputError>;
    /// Press or release a key.
    fn key(&mut self, key: &Key, pressed: bool) -> Result<(), InputError>;
    /// Current absolute pointer position.
    fn cursor_pos(&mut self) -> Result<(i32, i32), InputError>;

    /// Apply a captured/received [`InputEvent`]. Default dispatch over the
    /// primitives above so backends only implement the primitives.
    fn apply(&mut self, ev: &InputEvent) -> Result<(), InputError> {
        match ev {
            InputEvent::MouseMove { dx, dy } => self.mouse_move_rel(*dx, *dy),
            InputEvent::MouseButton { button, pressed } => self.mouse_button(*button, *pressed),
            InputEvent::MouseScroll { dx, dy } => self.mouse_scroll(*dx, *dy),
            InputEvent::Key { key, pressed } => self.key(key, *pressed),
        }
    }
}

/// A running input capture. Dropping or calling [`InputCapture::stop`] tears down
/// the capture thread and releases any active grab/suppression.
pub trait InputCapture: Send {
    /// Turn suppression on/off. When on, local input is swallowed (it does not
    /// reach local apps) and is reported to the sink as [`InputEvent`]s instead.
    /// When off, input flows to local apps normally and is not reported.
    fn set_suppress(&self, on: bool);
    /// Stop capturing and release everything.
    fn stop(self: Box<Self>);
}

/// Create an injector that works with no desktop session of its own.
///
/// Only meaningful on Linux, where the normal backend is bound to one X server and so
/// cannot reach a display-manager greeter. `w`/`h` are the screen bounds the virtual
/// pointer is declared over; they are passed in because the whole point of this backend
/// is that there is no X server available to ask.
///
/// Requires privilege — see [`uinput`].
#[cfg(target_os = "linux")]
pub fn new_sessionless_injector(w: i32, h: i32) -> Result<Box<dyn InputInjector>, InputError> {
    Ok(Box::new(uinput::UinputInjector::new(w, h)?))
}

/// Create the injector for the current platform.
pub fn new_injector() -> Result<Box<dyn InputInjector>, InputError> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::X11Injector::new()?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WinInjector::new()?))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(InputError::Unsupported)
    }
}

/// Start capturing local input on the current platform. While suppression is on,
/// captured input is sent to `sink` as [`CaptureSignal::Input`]; while it is off,
/// the pointer position is sent as [`CaptureSignal::Cursor`]. Suppression starts
/// **off**. The caller owns the receiving half of `sink`, which is what makes the
/// engine unit-testable: a test can feed synthetic signals through a channel it
/// created and pair them with a fake [`InputCapture`].
/// `return_hotkey` is the configured lock/return key. On Windows the capture
/// backend uses it for its in-callback + watchdog failsafe self-release (so the
/// user can always escape suppression); on Linux the engine handles the hotkey, so
/// it is ignored there.
pub fn start_capture(
    sink: UnboundedSender<CaptureSignal>,
    return_hotkey: Option<NamedKey>,
) -> Result<(Box<dyn InputCapture>, Suppressor), InputError> {
    let suppressor = Suppressor::new();
    #[cfg(target_os = "linux")]
    {
        let _ = return_hotkey; // the engine handles the hotkey on Linux
        let cap = linux::X11Capture::start(sink, suppressor.shared())?;
        Ok((Box::new(cap), suppressor))
    }
    #[cfg(target_os = "windows")]
    {
        let cap = windows::WinCapture::start(sink, suppressor.shared(), return_hotkey)?;
        Ok((Box::new(cap), suppressor))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = sink;
        let _ = suppressor;
        let _ = return_hotkey;
        Err(InputError::Unsupported)
    }
}

/// The local screen size in pixels, queried from the OS. Used so edge geometry
/// matches the surface the OS actually clamps the cursor against, rather than
/// trusting possibly-stale config values.
pub fn screen_size() -> Result<(i32, i32), InputError> {
    #[cfg(target_os = "linux")]
    {
        linux::screen_size()
    }
    #[cfg(target_os = "windows")]
    {
        windows::screen_size()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(InputError::Unsupported)
    }
}

/// Whether this process can receive input while an elevated window is in front.
///
/// Windows only. User Interface Privilege Isolation stops the OS calling a
/// lower-integrity process's low-level hooks while a higher-integrity window has the
/// foreground — so a normal-privilege mouse-rs simply cannot see the pointer over a game
/// launcher, an installer or a UAC prompt, and the crossing never fires. Running elevated
/// is the only remedy, so the panel needs to be able to say which state it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Elevated: input is delivered over other elevated windows too.
    Elevated,
    /// Normal privileges: crossing will not work while an elevated window is focused.
    Normal,
    /// No such restriction on this platform.
    NotApplicable,
}

impl Elevation {
    /// True when this platform has the restriction and we are *not* clear of it.
    pub fn is_limited(self) -> bool {
        self == Elevation::Normal
    }
}

/// What privilege level this process is running at.
pub fn elevation() -> Elevation {
    #[cfg(windows)]
    {
        windows::process_elevation()
    }
    #[cfg(not(windows))]
    {
        // X11 has no equivalent: a client can read input regardless of what owns the
        // focused window, so there is nothing here to warn about.
        Elevation::NotApplicable
    }
}
