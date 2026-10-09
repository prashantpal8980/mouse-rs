//! The injection side: a dedicated native thread that owns the OS injector and
//! applies events in FIFO order.
//!
//! Why a thread and not `spawn_blocking` per event: X connections are not `Sync`
//! and must stay confined to one thread, and per-event `spawn_blocking` would let
//! events inject out of order. The thread also owns the **pressed-set** and
//! releases everything still down on *any* exit (stop signalled, command channel
//! closed — including an engine panic — or thread teardown). That makes
//! "never leave a key/button stuck on the client" a structural guarantee, the
//! injection-side analogue of `X11Capture`'s ungrab-on-Drop.
//!
//! The release is ultimately guaranteed by [`Injector::shutdown`] (and the `Drop`
//! failsafe) flipping the stop flag, *not* by every [`InjectorTx`] being dropped:
//! the engine task holds a clone of the sender, so the channel doesn't close until
//! that task ends. `main`'s cross-cancel (a Ctrl-C, link death, or engine death
//! cancels the other) is what guarantees `block_on` returns so `shutdown` actually
//! runs — at which point the stop flag fires release-all even with the backlog
//! abandoned. So the failsafe depends on shutdown being reached, which the
//! cross-cancel ensures.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mouse_input::{Button, InputError, InputEvent, InputInjector, Key};

/// A command for the injector thread.
#[derive(Debug, Clone, PartialEq)]
pub enum InjectCmd {
    /// Apply a received input event (move/button/scroll/key).
    Apply(InputEvent),
    /// Warp the local cursor to an absolute position (used on Enter).
    MoveAbs { x: i32, y: i32 },
    /// Release every key/button currently held (clean transition / Leave).
    ReleaseAll,
}

/// The engine's cloneable handle for sending commands to the injector thread.
#[derive(Clone)]
pub struct InjectorTx(Sender<InjectCmd>);

impl InjectorTx {
    /// Enqueue a command. Silently ignored if the injector thread is gone — the
    /// engine is being torn down and will observe the same shortly.
    pub fn send(&self, cmd: InjectCmd) {
        let _ = self.0.send(cmd);
    }

    /// A standalone sender paired with a plain receiver, for driving the engine
    /// in tests without a real injector thread.
    #[cfg(test)]
    pub(crate) fn for_test() -> (InjectorTx, mpsc::Receiver<InjectCmd>) {
        let (tx, rx) = mpsc::channel();
        (InjectorTx(tx), rx)
    }
}

/// Owns the injector thread's lifecycle. The engine gets a [`InjectorTx`]; the
/// caller (e.g. `main`) keeps this handle and calls [`Injector::shutdown`] off
/// the async reactor once the engine has stopped.
pub struct Injector {
    tx: Sender<InjectCmd>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Injector {
    /// Spawn the injection thread around an OS injector. Errors only if the OS
    /// refuses a new thread (resource exhaustion); the caller propagates it rather
    /// than panicking so a failed start is a clean error, not a crash.
    pub fn spawn(inner: Box<dyn InputInjector>) -> Result<Injector, InputError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<InjectCmd>();
        let st = stop.clone();
        let join = std::thread::Builder::new()
            .name("input-injector".into())
            .spawn(move || injector_loop(inner, rx, st))
            .map_err(|e| InputError::Backend(format!("spawn injector thread: {e}")))?;
        Ok(Injector {
            tx,
            stop,
            join: Some(join),
        })
    }

    /// A sender the engine uses to enqueue commands.
    pub fn sender(&self) -> InjectorTx {
        InjectorTx(self.tx.clone())
    }

    /// Stop the thread and join it, bounded so a wedged injector can't hang exit.
    /// Setting the stop flag makes the loop abandon any queued backlog and run
    /// its release-all failsafe immediately. Call this off the async reactor.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Drop our own sender so the thread can also exit via channel-close even
        // if it is blocked in recv (the stop flag is checked on each timeout).
        let (dead_tx, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.tx, dead_tx);
        if let Some(join) = self.join.take() {
            join_bounded(join, Duration::from_millis(500));
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // Failsafe if `shutdown` was not called explicitly (e.g. an early return).
        if let Some(join) = self.join.take() {
            self.stop.store(true, Ordering::SeqCst);
            join_bounded(join, Duration::from_millis(500));
        }
    }
}

/// Join `handle`, but never block longer than `timeout`: if the thread overshoots
/// (a wedged X server), detach it rather than hang the caller. The thread's only
/// remaining work after `stop` is releasing held keys, so detaching is safe.
fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = mpsc::channel();
    // A watcher thread does the actual blocking join and signals completion.
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(timeout).is_err() {
        tracing::warn!("injector thread did not stop within {timeout:?}; detaching");
    }
}

fn injector_loop(inner: Box<dyn InputInjector>, rx: mpsc::Receiver<InjectCmd>, stop: Arc<AtomicBool>) {
    injector_loop_with(inner, rx, stop, MODIFIER_IDLE_RELEASE)
}

/// As [`injector_loop`], with the idle grace given — so a test can use a short one
/// instead of waiting five seconds.
fn injector_loop_with(
    mut inner: Box<dyn InputInjector>,
    rx: mpsc::Receiver<InjectCmd>,
    stop: Arc<AtomicBool>,
    idle_release: Duration,
) {
    let mut keys: HashSet<Key> = HashSet::new();
    let mut buttons: HashSet<Button> = HashSet::new();
    // Last time anything at all arrived. A modifier held across a long silence is the
    // signature of a lost key-up, not of someone holding it.
    let mut last_input = Instant::now();

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(cmd) => {
                last_input = Instant::now();
                apply(&mut inner, &mut keys, &mut buttons, cmd);
            }
            Err(RecvTimeoutError::Timeout) => {
                // Buttons held means a drag is in progress, and a drag with a modifier
                // down is a real thing someone can hold still mid-gesture.
                if !idle_release.is_zero()
                    && buttons.is_empty()
                    && last_input.elapsed() >= idle_release
                    && keys.iter().any(is_modifier)
                {
                    let freed = release_stuck_modifiers(&mut inner, &mut keys);
                    if !freed.is_empty() {
                        tracing::warn!(
                            released = ?freed,
                            idle_secs = last_input.elapsed().as_secs(),
                            "released modifiers that were still held after a long silence \
                             — the peer almost certainly lost their key-up (B13)"
                        );
                        last_input = Instant::now();
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break, // engine gone (incl. panic)
        }
    }

    // Release everything still down, on every exit path.
    release_all(&mut inner, &mut keys, &mut buttons);
}

fn apply(
    inner: &mut Box<dyn InputInjector>,
    keys: &mut HashSet<Key>,
    buttons: &mut HashSet<Button>,
    cmd: InjectCmd,
) {
    let r: Result<(), InputError> = match cmd {
        InjectCmd::MoveAbs { x, y } => inner.mouse_move_abs(x, y),
        InjectCmd::ReleaseAll => {
            release_all(inner, keys, buttons);
            Ok(())
        }
        InjectCmd::Apply(ev) => {
            // Track press/release so the failsafe knows what to release.
            match &ev {
                InputEvent::Key { key, pressed } => {
                    if *pressed {
                        keys.insert(key.clone());
                    } else {
                        keys.remove(key);
                    }
                }
                InputEvent::MouseButton { button, pressed } => {
                    if *pressed {
                        buttons.insert(*button);
                    } else {
                        buttons.remove(button);
                    }
                }
                _ => {}
            }
            inner.apply(&ev)
        }
    };
    if let Err(e) = r {
        tracing::debug!("inject failed: {e}");
    }
}

/// How long a modifier may stay held with nothing else arriving before we conclude its
/// release was lost in transit.
///
/// Five seconds. The sending side only forwards a key-up it was actually told about, and
/// on Windows it is not always told: the OS stops calling a low-level hook while a
/// higher-integrity window has the foreground, so a release pressed in that window is
/// never captured and never sent (B13). The peer is then left holding Alt for ever, which
/// is what jams the Alt-Tab switcher — holding Alt is what keeps it open.
///
/// Why idleness is the right signal: a human holding Alt is doing something with it, and
/// anything they do — another key, a mouse movement — arrives here. Total silence with a
/// modifier down means nobody is holding it. Five seconds is long enough that reading an
/// open switcher does not trip it, and releasing a modifier we were wrong about costs
/// only what releasing it early costs, which is what the user was trying to do anyway.
pub const MODIFIER_IDLE_RELEASE: Duration = Duration::from_secs(5);

/// Is this a key whose release we are willing to invent?
///
/// Modifiers only. A stuck modifier breaks every subsequent keystroke, so guessing is
/// worth it; a stuck ordinary key types one wrong character and auto-repeat would keep
/// arriving anyway, so there is nothing to detect and nothing to gain.
fn is_modifier(key: &Key) -> bool {
    matches!(key, Key::Named(n) if n.modifier_token().is_some())
}

/// Release modifiers we are probably wrong about still holding.
///
/// Returns the ones released, for the log — a silent correction would hide how often the
/// sending side is losing key-ups, which is the number that says whether B13 is fixed.
fn release_stuck_modifiers(
    inner: &mut Box<dyn InputInjector>,
    keys: &mut HashSet<Key>,
) -> Vec<Key> {
    let stuck: Vec<Key> = keys.iter().filter(|k| is_modifier(k)).cloned().collect();
    for key in &stuck {
        let _ = inner.key(key, false);
        keys.remove(key);
    }
    stuck
}

fn release_all(
    inner: &mut Box<dyn InputInjector>,
    keys: &mut HashSet<Key>,
    buttons: &mut HashSet<Button>,
) {
    for key in keys.drain() {
        let _ = inner.key(&key, false);
    }
    for button in buttons.drain() {
        let _ = inner.mouse_button(button, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mouse_input::NamedKey;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Records every injection so tests can assert on the exact sequence.
    #[derive(Default)]
    struct FakeInjector {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl InputInjector for FakeInjector {
        fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
            self.log.lock().unwrap().push(format!("rel {dx},{dy}"));
            Ok(())
        }
        fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<(), InputError> {
            self.log.lock().unwrap().push(format!("abs {x},{y}"));
            Ok(())
        }
        fn mouse_button(&mut self, button: Button, pressed: bool) -> Result<(), InputError> {
            self.log.lock().unwrap().push(format!("btn {button:?} {pressed}"));
            Ok(())
        }
        fn mouse_scroll(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
            self.log.lock().unwrap().push(format!("scroll {dx},{dy}"));
            Ok(())
        }
        fn key(&mut self, key: &Key, pressed: bool) -> Result<(), InputError> {
            self.log.lock().unwrap().push(format!("key {key} {pressed}"));
            Ok(())
        }
        fn cursor_pos(&mut self) -> Result<(i32, i32), InputError> {
            Ok((0, 0))
        }
    }

    fn wait_for<F: Fn() -> bool>(pred: F) {
        for _ in 0..200 {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition not met in time");
    }

    #[test]
    fn releases_held_keys_and_buttons_when_sender_drops() {
        // Simulates the engine panicking/exiting without an explicit ReleaseAll:
        // the injector thread must still release everything still down.
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Box::new(FakeInjector { log: log.clone() });
        let injector = Injector::spawn(fake).unwrap();
        let tx = injector.sender();

        tx.send(InjectCmd::Apply(InputEvent::Key {
            key: Key::Named(NamedKey::ShiftL),
            pressed: true,
        }));
        tx.send(InjectCmd::Apply(InputEvent::MouseButton {
            button: Button::Left,
            pressed: true,
        }));
        // Wait until both presses have been applied.
        wait_for(|| log.lock().unwrap().len() >= 2);

        // Drop BOTH senders (engine's clone + handle's) — the failsafe must fire.
        drop(tx);
        injector.shutdown();

        let entries = log.lock().unwrap().clone();
        assert!(
            entries.iter().any(|e| e.contains("key k:shift false")),
            "shift must be released; got {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e == "btn Left false"),
            "left button must be released; got {entries:?}"
        );
    }

    #[test]
    fn explicit_release_all_releases_then_keeps_running() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Box::new(FakeInjector { log: log.clone() });
        let injector = Injector::spawn(fake).unwrap();
        let tx = injector.sender();

        tx.send(InjectCmd::Apply(InputEvent::Key {
            key: Key::Char('a'),
            pressed: true,
        }));
        tx.send(InjectCmd::ReleaseAll);
        tx.send(InjectCmd::Apply(InputEvent::MouseMove { dx: 2, dy: 3 }));
        wait_for(|| log.lock().unwrap().iter().any(|e| e == "rel 2,3"));

        let entries = log.lock().unwrap().clone();
        // 'a' down, then released by ReleaseAll, then the move still got applied.
        assert!(entries.iter().any(|e| e == "key c:a true"));
        assert!(entries.iter().any(|e| e == "key c:a false"));
        assert!(entries.iter().any(|e| e == "rel 2,3"));

        injector.shutdown();
    }
    /// Drive `injector_loop_with` directly with a short grace, so the stuck-modifier
    /// path is exercised without a five-second test.
    fn run_loop(
        grace: Duration,
        feed: impl FnOnce(&mpsc::Sender<InjectCmd>),
        settle: Duration,
        after: impl FnOnce(&mpsc::Sender<InjectCmd>),
    ) -> Vec<String> {
        let log = Arc::new(Mutex::new(Vec::new()));
        let inner: Box<dyn InputInjector> = Box::new(FakeInjector { log: log.clone() });
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let h = std::thread::spawn(move || injector_loop_with(inner, rx, s2, grace));
        feed(&tx);
        std::thread::sleep(settle);
        // Anything sent here lands *after* the idle sweep would have run, so its
        // position in the log separates an idle release from the teardown one.
        after(&tx);
        std::thread::sleep(Duration::from_millis(120));
        stop.store(true, Ordering::SeqCst);
        // Dropping the sender would also end it; stop flag is what the engine uses.
        let _ = h.join();
        // Bound rather than returned inline: the guard is a temporary and the tail
        // expression would outlive it.
        let out = log.lock().unwrap().clone();
        out
    }

    #[test]
    fn a_modifier_left_held_through_a_long_silence_is_released() {
        // B13: Windows stops delivering our hook while a higher-integrity window has
        // focus, so an Alt-up pressed in that moment is never captured and never sent.
        // The peer is left holding Alt, which is exactly what jams the Alt-Tab switcher.
        let log = run_loop(
            Duration::from_millis(120),
            |tx| {
                tx.send(InjectCmd::Apply(InputEvent::Key {
                    key: Key::Named(NamedKey::AltL),
                    pressed: true,
                }))
                .unwrap();
                tx.send(InjectCmd::Apply(InputEvent::Key {
                    key: Key::Named(NamedKey::Tab),
                    pressed: true,
                }))
                .unwrap();
                tx.send(InjectCmd::Apply(InputEvent::Key {
                    key: Key::Named(NamedKey::Tab),
                    pressed: false,
                }))
                .unwrap();
                // ...and then nothing. The Alt-up never arrives.
            },
            Duration::from_millis(500),
            |tx| {
                // A marker that lands after the sweep should have fired.
                tx.send(InjectCmd::Apply(InputEvent::Key {
                    key: Key::Named(NamedKey::F(9)),
                    pressed: true,
                }))
                .unwrap();
            },
        );
        let released = log.iter().position(|l| l == "key k:alt false");
        let marker = log.iter().position(|l| l == "key k:f9 true");
        assert!(released.is_some(), "Alt was never released; got {log:?}");
        assert!(
            released < marker,
            "Alt must be released by the idle sweep, not merely at teardown; got {log:?}"
        );
    }

    #[test]
    fn an_ordinary_key_is_not_released_on_a_guess() {
        // Only modifiers are worth inventing a release for. A stuck letter types one
        // wrong character; guessing wrongly on one would eat a real keypress.
        let log = run_loop(
            Duration::from_millis(120),
            |tx| {
                tx.send(InjectCmd::Apply(InputEvent::Key {
                    key: Key::Char('a'),
                    pressed: true,
                }))
                .unwrap();
            },
            Duration::from_millis(400),
            |_| {},
        );
        // Teardown releases it; the sweep must not have done so before that.
        assert_eq!(
            log,
            vec!["key c:a true".to_string(), "key c:a false".to_string()],
            "an ordinary key must only be released at teardown; got {log:?}"
        );
    }

    #[test]
    fn a_modifier_held_during_a_drag_is_left_alone() {
        // Shift-drag is a real gesture and someone can hold still in the middle of it.
        // A held button means a human is demonstrably still there.
        let log = run_loop(
            Duration::from_millis(120),
            |tx| {
                tx.send(InjectCmd::Apply(InputEvent::Key {
                    key: Key::Named(NamedKey::ShiftL),
                    pressed: true,
                }))
                .unwrap();
                tx.send(InjectCmd::Apply(InputEvent::MouseButton {
                    button: Button::Left,
                    pressed: true,
                }))
                .unwrap();
            },
            Duration::from_millis(400),
            |_| {},
        );
        let released_early = log
            .iter()
            .position(|l| l == "key k:shift_l false")
            .zip(log.iter().position(|l| l == "btn Left false"))
            .map(|(shift, btn)| shift < btn)
            .unwrap_or(false);
        assert!(
            !released_early,
            "Shift must survive until the drag ends; got {log:?}"
        );
    }

    #[test]
    fn a_zero_grace_turns_the_sweep_off() {
        let log = run_loop(
            Duration::ZERO,
            |tx| {
                tx.send(InjectCmd::Apply(InputEvent::Key {
                    key: Key::Named(NamedKey::AltL),
                    pressed: true,
                }))
                .unwrap();
            },
            Duration::from_millis(300),
            |_| {},
        );
        // Only the teardown release at the end, never an idle one mid-run.
        assert_eq!(
            log,
            vec!["key k:alt true".to_string(), "key k:alt false".to_string()],
            "with the sweep off, the only release is teardown; got {log:?}"
        );
    }

    #[test]
    fn modifiers_are_identified_and_ordinary_keys_are_not() {
        assert!(is_modifier(&Key::Named(NamedKey::AltL)));
        assert!(is_modifier(&Key::Named(NamedKey::AltGr)));
        assert!(is_modifier(&Key::Named(NamedKey::CtrlR)));
        assert!(is_modifier(&Key::Named(NamedKey::SuperL)));
        assert!(!is_modifier(&Key::Named(NamedKey::Tab)));
        assert!(!is_modifier(&Key::Named(NamedKey::Enter)));
        assert!(!is_modifier(&Key::Char('a')));
        assert!(!is_modifier(&Key::Raw(0x1234)));
    }

}

