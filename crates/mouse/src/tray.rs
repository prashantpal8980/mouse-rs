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

#[cfg(windows)]
thread_local! {
    /// The Windows tray icon lives on the UI thread; kept here so app state stays `Send`.
    static WIN_TRAY: std::cell::RefCell<Option<TrayIcon>> = const { std::cell::RefCell::new(None) };
}

/// Kept alive by the app for the tray's lifetime; carries the desired tooltip text.
pub struct TrayHandle {
    /// Shared desired hover text. Linux's gtk thread polls it; Windows applies it inline.
    tooltip: Arc<Mutex<String>>,
}

impl TrayHandle {
    /// Update the tray's hover tooltip. Cheap to call every tick (only acts on change).
    pub fn set_tooltip(&self, text: &str) {
        if let Ok(mut cur) = self.tooltip.lock() {
            if *cur != text {
                *cur = text.to_owned();
                #[cfg(windows)]
                WIN_TRAY.with(|t| {
                    if let Some(icon) = t.borrow().as_ref() {
                        let _ = icon.set_tooltip(Some(text));
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
fn build() -> Result<TrayIcon, Box<dyn std::error::Error>> {
    let menu = Menu::new();
    menu.append(&MenuItem::with_id("open", "Open mouse-rs", true, None))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id("start", "Start", true, None))?;
    menu.append(&MenuItem::with_id("stop", "Stop", true, None))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id("quit", "Quit", true, None))?;

    let (rgba, w, h) = icon_rgba();
    let icon = tray_icon::Icon::from_rgba(rgba, w, h)?;
    Ok(TrayIconBuilder::new()
        .with_tooltip("mouse-rs")
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .build()?)
}

/// Spawn the tray. Returns `None` (and logs) if the platform tray can't be created, in
/// which case the app just runs as a normal window. Must be called on the UI thread.
pub fn spawn(initial_tooltip: &str) -> Option<TrayHandle> {
    let tooltip = Arc::new(Mutex::new(initial_tooltip.to_string()));

    #[cfg(windows)]
    {
        match build() {
            Ok(icon) => {
                WIN_TRAY.with(|t| *t.borrow_mut() = Some(icon));
                Some(TrayHandle { tooltip })
            }
            Err(e) => {
                tracing::warn!("tray unavailable: {e}");
                None
            }
        }
    }

    #[cfg(unix)]
    {
        let tip = tooltip.clone();
        let ok = std::thread::Builder::new()
            .name("tray-gtk".into())
            .spawn(move || {
                if gtk::init().is_err() {
                    tracing::warn!("gtk init failed; running without a tray");
                    return;
                }
                match build() {
                    Ok(icon) => {
                        // The icon must be touched on its owning (gtk) thread, so poll the
                        // shared tooltip here. `icon` is moved in, kept alive for the loop.
                        let mut applied = String::new();
                        gtk::glib::timeout_add_local(
                            std::time::Duration::from_millis(700),
                            move || {
                                if let Ok(g) = tip.lock() {
                                    if *g != applied {
                                        applied = g.clone();
                                        let _ = icon.set_tooltip(Some(applied.as_str()));
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
        ok.then_some(TrayHandle { tooltip })
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = tooltip;
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
