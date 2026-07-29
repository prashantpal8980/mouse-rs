//! System-tray integration for the iced control panel.
//!
//! `tray-icon` has different threading rules per OS:
//! * **Windows** — the icon must live on the thread that runs the winit event loop
//!   (iced's main thread). We build it there in [`spawn`] (called from the app's boot)
//!   and keep it in a thread-local so the app state stays `Send` and simple.
//! * **Linux** — it needs gtk initialised with a running gtk main loop, which would
//!   fight iced's winit loop, so we give it a dedicated thread that owns gtk entirely.
//!
//! Menu clicks are delivered to a global channel; the iced app drains it each tick via
//! [`poll`] and acts on the [`TrayEvent`]s (the app owns the engine + window).

use std::sync::{Arc, Mutex};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// A tray menu action for the iced app to handle.
#[derive(Clone, Copy, Debug)]
pub enum TrayEvent {
    Open,
    Start,
    Stop,
    Quit,
}

/// The icon plus the two items whose enabled state tracks the engine. `MenuItem` holds a
/// `RefCell` internally, so it is neither `Send` nor `Sync` — these must only ever be
/// touched on the thread that built them (the gtk thread on Linux, the UI thread on
/// Windows), which is what shapes the two branches of [`spawn`].
struct TrayParts {
    icon: TrayIcon,
    start: MenuItem,
    stop: MenuItem,
}

impl TrayParts {
    /// Grey out whichever action is meaningless right now, so the tray agrees with the
    /// panel — which already disables Stop when stopped and shows Start as "Running".
    fn apply(&self, tooltip: &str, running: bool) {
        let _ = self.icon.set_tooltip(Some(tooltip));
        self.start.set_enabled(!running);
        self.stop.set_enabled(running);
    }
}

#[cfg(windows)]
thread_local! {
    /// The Windows tray lives on the UI thread; kept here so app state stays `Send`.
    static WIN_TRAY: std::cell::RefCell<Option<TrayParts>> = const { std::cell::RefCell::new(None) };
}

/// What the app wants the tray to show. Linux's gtk thread polls this; Windows applies it
/// inline from the UI thread.
#[derive(Clone, PartialEq, Eq)]
struct Desired {
    tooltip: String,
    running: bool,
}

/// Kept alive by the app for the tray's lifetime; carries the desired tray state.
pub struct TrayHandle {
    desired: Arc<Mutex<Desired>>,
}

impl TrayHandle {
    /// Push live status to the tray. Cheap to call every tick — it only touches the OS when
    /// something actually changed.
    pub fn set_state(&self, tooltip: &str, running: bool) {
        let next = Desired { tooltip: tooltip.to_owned(), running };
        if let Ok(mut cur) = self.desired.lock() {
            if *cur != next {
                *cur = next;
                #[cfg(windows)]
                WIN_TRAY.with(|t| {
                    if let Some(parts) = t.borrow().as_ref() {
                        parts.apply(tooltip, running);
                    }
                });
            }
        }
    }
}

/// Drain pending tray menu clicks (called from the iced `Tick`).
pub fn poll() -> Vec<TrayEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        out.push(match ev.id.0.as_str() {
            "open" => TrayEvent::Open,
            "start" => TrayEvent::Start,
            "stop" => TrayEvent::Stop,
            "quit" => TrayEvent::Quit,
            _ => continue,
        });
    }
    out
}

/// Build the tray icon + menu on the current thread.
fn build() -> Result<TrayParts, Box<dyn std::error::Error>> {
    let menu = Menu::new();
    // `Start`/`Stop` are held so their enabled state can follow the engine. They start as
    // "not running": nothing is shared until the app says otherwise.
    let start = MenuItem::with_id("start", "Start", true, None);
    let stop = MenuItem::with_id("stop", "Stop", false, None);

    menu.append(&MenuItem::with_id("open", "Open mouse-rs", true, None))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&start)?;
    menu.append(&stop)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id("quit", "Quit", true, None))?;

    let (rgba, w, h) = icon_rgba();
    let icon = tray_icon::Icon::from_rgba(rgba, w, h)?;
    let icon = TrayIconBuilder::new()
        .with_tooltip("mouse-rs")
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .build()?;
    Ok(TrayParts { icon, start, stop })
}

/// Spawn the tray. Returns `None` (and logs) if the platform tray can't be created, in
/// which case the app just runs as a normal window. Must be called on the UI thread.
pub fn spawn(initial_tooltip: &str) -> Option<TrayHandle> {
    let desired = Arc::new(Mutex::new(Desired {
        tooltip: initial_tooltip.to_string(),
        running: false,
    }));

    #[cfg(windows)]
    {
        match build() {
            Ok(parts) => {
                WIN_TRAY.with(|t| *t.borrow_mut() = Some(parts));
                Some(TrayHandle { desired })
            }
            Err(e) => {
                tracing::warn!("tray unavailable: {e}");
                None
            }
        }
    }

    #[cfg(unix)]
    {
        let want = desired.clone();
        let ok = std::thread::Builder::new()
            .name("tray-gtk".into())
            .spawn(move || {
                if gtk::init().is_err() {
                    tracing::warn!("gtk init failed; running without a tray");
                    return;
                }
                match build() {
                    Ok(parts) => {
                        // The icon and menu items may only be touched on this (gtk) thread,
                        // so poll the shared state here. `parts` is moved in and kept alive
                        // for the life of the loop.
                        let mut applied: Option<Desired> = None;
                        gtk::glib::timeout_add_local(
                            std::time::Duration::from_millis(700),
                            move || {
                                if let Ok(g) = want.lock() {
                                    if applied.as_ref() != Some(&*g) {
                                        parts.apply(&g.tooltip, g.running);
                                        applied = Some(g.clone());
                                    }
                                }
                                gtk::glib::ControlFlow::Continue
                            },
                        );
                        gtk::main();
                    }
                    Err(e) => tracing::warn!("tray unavailable: {e}"),
                }
            })
            .is_ok();
        ok.then_some(TrayHandle { desired })
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = desired;
        None
    }
}

/// A simple 32×32 icon — an accent disc with a white centre, built from raw RGBA so
/// there's no image file to ship.
fn icon_rgba() -> (Vec<u8>, u32, u32) {
    const S: i32 = 32;
    let mut px = vec![0u8; (S * S * 4) as usize];
    let (cx, cy, r) = (15.5f32, 15.5f32, 14.0f32);
    for y in 0..S {
        for x in 0..S {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            let d = (dx * dx + dy * dy).sqrt();
            if d <= r {
                let i = ((y * S + x) * 4) as usize;
                let (rr, gg, bb) = if d <= r * 0.45 {
                    (0xff, 0xff, 0xff)
                } else {
                    (0x38, 0xd6, 0xe6)
                };
                px[i] = rr;
                px[i + 1] = gg;
                px[i + 2] = bb;
                px[i + 3] = 0xff;
            }
        }
    }
    (px, S as u32, S as u32)
}
