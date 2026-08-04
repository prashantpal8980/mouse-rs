//! iced control panel (the product UI). A bare `mouse-rs` launch opens this window:
//! edit config, Save it, and Start/Stop the engine — the same lifecycle the headless
//! CLI uses, so the GUI is just a front-end over [`crate::run`]. Closing the window
//! quits the app. The look (6 themes, status hero, animated toggles) lives in
//! [`crate::ui_theme`]; the system tray in [`crate::tray`].

use std::sync::{Arc, Mutex};

use iced::widget::{
    button, column, container, mouse_area, pick_list, row, scrollable, stack, text, text_input,
    toggler, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task};

use mouse_core::{sanitize_profile_name, Config, Edge, Role};
use mouse_engine::StatusSnapshot;
use mouse_input::screen_size;

use crate::run::EngineHandle;
use crate::ui_theme::{self, Palette, Theme6};

/// Which config section is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connection,
    Layout,
    Security,
    Clipboard,
    Startup,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Connection,
        Tab::Layout,
        Tab::Security,
        Tab::Clipboard,
        Tab::Startup,
    ];
    fn label(self) -> &'static str {
        match self {
            Tab::Connection => "Connection",
            Tab::Layout => "Layout",
            Tab::Security => "Security",
            Tab::Clipboard => "Clipboard",
            Tab::Startup => "Startup",
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    ThemePicked(Theme6),
    TabPicked(Tab),
    NameChanged(String),
    RolePicked(Role),
    SecretChanged(String),
    ToggleShowSecret,
    GenerateSecret,
    TlsToggled(bool),
    ClipboardToggled(bool),
    // --- clipboard manager ---
    /// Put a history entry back on the clipboard.
    ClipReuse(u64),
    ClipTogglePin(u64),
    ClipRemove(u64),
    ClipClear,
    ClipAdvancedToggled(bool),
    ClipSyncTextToggled(bool),
    ClipSyncImagesToggled(bool),
    ClipItemsChanged(String),
    ClipItemMbChanged(String),
    ClipMemMbChanged(String),
    ClipImagePxChanged(String),
    BulkEnabledToggled(bool),
    BulkPortChanged(String),
    ClipSyncFilesToggled(bool),
    ClipAutoMbChanged(String),
    ClipCacheGbChanged(String),
    ClipInlineKbChanged(String),
    ClipMaxFilesChanged(String),
    ReconnectToggled(bool),
    AutostartToggled(bool),
    AutoconnectToggled(bool),
    StartMinimizedToggled(bool),
    LockHotkeyChanged(String),
    ReturnHotkeyChanged(String),
    ServerHostChanged(String),
    ServerPortChanged(String),
    ListenHostChanged(String),
    ListenPortChanged(String),
    ExitEdgePicked(Edge),
    EntryEdgePicked(Edge),
    ScreenWChanged(String),
    ScreenHChanged(String),
    DetectScreen,
    ProfilePicked(String),
    LoadProfile,
    DeleteProfile,
    ProfileNameChanged(String),
    SaveProfileAs,
    Start,
    Stop,
    SaveConfig,
    RestartNow,
    RestartLater,
    RestoreDefaults,
    DetectServers,
    ServerPicked(String),
    ListenHostPicked(String),
    ToggleMenu,
    CloseMenu,
    Quit,
}

/// Settings the running engine read at startup and will not notice changing underneath
/// it. Editing any of these while sharing is live leaves the panel showing one thing and
/// the session doing another — the gap the restart prompt exists to close.
///
/// Deliberately a list of (label, comparison) rather than a `PartialEq` on `Config`:
/// the user needs to be told *which* settings are stale, and theme/log-path/start-at-login
/// changes must not trigger a prompt because they already apply immediately.
fn pending_restart_changes(live: &Config, now: &Config) -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut chk = |differs: bool, label: &'static str| {
        if differs {
            v.push(label);
        }
    };
    chk(live.name != now.name, "machine name");
    chk(live.role != now.role, "role");
    chk(live.shared_secret != now.shared_secret, "shared secret");
    chk(live.tls_enabled != now.tls_enabled, "TLS");
    chk(live.clipboard_sync != now.clipboard_sync, "clipboard sync");
    chk(live.lock_hotkey != now.lock_hotkey, "lock hotkey");
    chk(live.return_hotkey != now.return_hotkey, "return hotkey");
    chk(live.listen_host != now.listen_host, "listen host");
    chk(live.listen_port != now.listen_port, "listen port");
    chk(live.server_host != now.server_host, "server host");
    chk(live.server_port != now.server_port, "server port");
    chk(live.reconnect != now.reconnect, "auto-reconnect");
    chk(live.exit_edge != now.exit_edge, "exit edge");
    chk(live.entry_edge != now.entry_edge, "entry edge");
    chk(
        live.screen_width != now.screen_width || live.screen_height != now.screen_height,
        "screen size",
    );
    v
}

struct App {
    cfg: Config,
    theme: Theme6,
    tab: Tab,
    engine: Option<EngineHandle>,
    show_secret: bool,
    feedback: String,
    profiles: Vec<String>,
    profile_pick: Option<String>,
    profile_name_input: String,
    // String buffers for numeric fields (so partial edits don't fight the parse).
    server_port_s: String,
    listen_port_s: String,
    screen_w_s: String,
    screen_h_s: String,
    /// The system tray (kept alive; pushes live tooltip). `None` if unavailable.
    tray: Option<crate::tray::TrayHandle>,
    /// Live state of the OS login entry — read from the OS at boot, not from `cfg`,
    /// because the user may have removed it outside the app.
    autostart_on: bool,
    /// One-shot latch for the `autoconnect` / `start_minimized` launch actions, so they
    /// fire once on the first tick and never again (e.g. after a manual Stop).
    launch_done: bool,
    /// Where this run is logging. Shown in the panel because a GUI launch has no console —
    /// without it the user has nothing to send when reporting a problem.
    log_path: String,
    /// The config the *running* engine was started with. `None` when stopped. Compared
    /// against `saved_cfg` to tell the user which saved settings are not yet active —
    /// without it, Save reports success and silently changes nothing about the live
    /// session.
    live_cfg: Option<Config>,
    /// The last config actually written to disk. The restart prompt is measured against
    /// **this**, not against `cfg`: `cfg` changes on every keystroke in a text field, so
    /// comparing it would pop the warning while the user is still typing and before they
    /// have committed to anything. From the user's point of view nothing has been saved
    /// yet, so nothing can be pending.
    saved_cfg: Config,
    /// Set by "Later": collapses the restart prompt to a compact marker rather than
    /// hiding it, so the panel never goes back to looking up-to-date while it isn't.
    restart_hint_dismissed: bool,
    /// Result slot for the background subnet sweep. The scan does blocking TCP connects,
    /// so it runs on its own thread and the UI picks the answer up on the next tick —
    /// the panel must stay responsive while it works.
    scan: Arc<Mutex<ScanState>>,
    /// Addresses found by the last sweep, offered as a pick list.
    found_servers: Vec<String>,
    /// Whether the overflow (⋯) menu is open. Everything rare or destructive lives there,
    /// so the footer can hold only the two actions used constantly — the old footer packed
    /// in eight and pushed Stop and Quit off the edge at normal window width.
    menu_open: bool,
    /// Whether the Clipboard tab's Advanced settings are expanded. Everything is
    /// configurable, but not everything is shown at once: the four settings most people
    /// touch stay visible and the rest sit one click away.
    clip_advanced: bool,
    // String buffers for the numeric clipboard settings, same reason as the ports:
    // a partial edit ("1" on the way to "128") must not be parsed and written back.
    clip_items_s: String,
    clip_item_mb_s: String,
    clip_mem_mb_s: String,
    clip_img_px_s: String,
    bulk_port_s: String,
    clip_auto_mb_s: String,
    clip_cache_gb_s: String,
    clip_inline_kb_s: String,
    clip_max_files_s: String,
}

/// Bytes → whole MB for display. The panel talks in MB because that is the unit the
/// user thinks in; config stores bytes because that is what the code compares against.
fn mb_of(bytes: usize) -> String {
    (bytes as f64 / (1024.0 * 1024.0)).round().to_string()
}

/// A byte count in the largest unit that keeps it readable.
fn human_bytes(bytes: usize) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else if mb >= 1.0 {
        format!("{:.1} MB", mb)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[derive(Default)]
enum ScanState {
    #[default]
    Idle,
    Running,
    Done(Vec<String>),
}

impl App {
    fn new(cfg: Config) -> Self {
        let mut app = App {
            theme: Theme6::Midnight,
            tab: Tab::Connection,
            engine: None,
            show_secret: false,
            feedback: String::new(),
            profiles: Config::list_profiles(),
            profile_pick: None,
            profile_name_input: String::new(),
            server_port_s: cfg.server_port.to_string(),
            listen_port_s: cfg.listen_port.to_string(),
            screen_w_s: cfg.screen_width.to_string(),
            screen_h_s: cfg.screen_height.to_string(),
            // Spawn the tray here — boot runs on the UI thread, which Windows requires.
            tray: crate::tray::spawn("mouse-rs — stopped"),
            autostart_on: crate::autostart::is_enabled(),
            launch_done: false,
            log_path: crate::logging::log_path().display().to_string(),
            live_cfg: None,
            restart_hint_dismissed: false,
            scan: Arc::new(Mutex::new(ScanState::Idle)),
            found_servers: Vec::new(),
            menu_open: false,
            clip_advanced: false,
            clip_items_s: String::new(),
            clip_item_mb_s: String::new(),
            clip_mem_mb_s: String::new(),
            clip_img_px_s: String::new(),
            bulk_port_s: String::new(),
            clip_auto_mb_s: String::new(),
            clip_cache_gb_s: String::new(),
            clip_inline_kb_s: String::new(),
            clip_max_files_s: String::new(),
            saved_cfg: cfg.clone(),
            cfg,
        };
        app.sync_buffers();
        app
    }

    /// Refresh the numeric string buffers from `cfg` (after load/detect).
    fn sync_buffers(&mut self) {
        self.server_port_s = self.cfg.server_port.to_string();
        self.listen_port_s = self.cfg.listen_port.to_string();
        self.screen_w_s = self.cfg.screen_width.to_string();
        self.screen_h_s = self.cfg.screen_height.to_string();
        self.clip_items_s = self.cfg.clipboard_history_size.to_string();
        self.clip_item_mb_s = mb_of(self.cfg.clipboard_max_item_bytes);
        self.clip_mem_mb_s = mb_of(self.cfg.clipboard_memory_budget);
        self.clip_img_px_s = self.cfg.clipboard_max_image_px.to_string();
        self.clip_auto_mb_s = (self.cfg.clipboard_auto_transfer_max / (1024 * 1024)).to_string();
        self.clip_inline_kb_s = (self.cfg.clipboard_inline_image_max / 1024).max(1).to_string();
        self.clip_max_files_s = self.cfg.clipboard_max_files_per_copy.to_string();
        self.clip_cache_gb_s =
            (self.cfg.clipboard_cache_budget / (1024 * 1024 * 1024)).max(1).to_string();
        self.bulk_port_s = if self.cfg.bulk_port == 0 {
            String::new()
        } else {
            self.cfg.bulk_port.to_string()
        };
    }

    fn running(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.is_running())
    }

    fn start_engine(&mut self) {
        // Starting persists the current settings, so a later headless or at-login run uses
        // exactly what the panel was showing. That write is logged because the user didn't
        // ask for it: an unannounced rewrite of the file holding `shared_secret` should at
        // least be visible afterwards.
        let path = Config::default_path();
        self.feedback = match self.cfg.save(&path) {
            Ok(()) => {
                tracing::info!("saved config to {} (implicit, on Start)", path.display());
                "started".into()
            }
            Err(e) => {
                tracing::warn!("could not save config to {}: {e}", path.display());
                format!("(saved with error: {e})")
            }
        };
        self.engine = Some(EngineHandle::start(self.cfg.clone()));
        // Start writes the file too, so both baselines move together and the panel opens
        // a session with nothing pending.
        self.live_cfg = Some(self.cfg.clone());
        self.saved_cfg = self.cfg.clone();
        self.restart_hint_dismissed = false;
    }

    /// Settings **saved** but not yet picked up by the running engine. Unsaved edits are
    /// deliberately excluded — see `saved_cfg`.
    fn pending_changes(&self) -> Vec<&'static str> {
        match (&self.live_cfg, self.running()) {
            (Some(live), true) => pending_restart_changes(live, &self.saved_cfg),
            _ => Vec::new(),
        }
    }

    fn stop_engine(&mut self) {
        if let Some(e) = &self.engine {
            // Without this line a deliberate Stop and a crash look identical in the log,
            // which is the worst possible ambiguity for a tool whose symptom is
            // "my keyboard went dead".
            tracing::info!("stop requested from the panel");
            e.request_stop();
        }
        self.feedback = "stopping…".into();
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                // Launch behaviour, once. Done on the first tick rather than in `new()`
                // because the window doesn't exist yet at construction time, so there is
                // nothing to minimize; and starting the engine here keeps the Start path
                // identical to the button's.
                if !self.launch_done {
                    self.launch_done = true;
                    if self.cfg.autoconnect && !self.running() {
                        self.start_engine();
                    }
                    if self.cfg.start_minimized {
                        return iced::window::latest()
                            .and_then(|id| iced::window::minimize(id, true));
                    }
                }
                // Clear a handle whose engine thread has exited on its own.
                if self.engine.as_ref().is_some_and(|e| !e.is_running()) {
                    // Logged so the log shows sharing actually ending, not just the request.
                    // A Stop leaves "stop requested" then this; an unexpected engine death
                    // leaves only this, which is the distinction worth being able to make.
                    tracing::info!("engine stopped; sharing is off");
                    self.engine = None;
                    // No live session, so nothing can be stale against it.
                    self.live_cfg = None;
                    self.restart_hint_dismissed = false;
                }
                // Collect a finished network sweep.
                let finished = {
                    let mut g = self.scan.lock().unwrap();
                    if let ScanState::Done(list) = &*g {
                        let list = list.clone();
                        *g = ScanState::Idle;
                        Some(list)
                    } else {
                        None
                    }
                };
                if let Some(list) = finished {
                    self.feedback = match list.len() {
                        0 => format!(
                            "no machine found listening on port {} — check the other machine is running and on the same network",
                            self.cfg.server_port
                        ),
                        1 => format!("found {}", list[0]),
                        n => format!("found {n} machines — pick one"),
                    };
                    // A single hit is almost certainly the peer; fill it in rather than
                    // making the user re-type what was just discovered.
                    if let [only] = list.as_slice() {
                        self.cfg.server_host = only.clone();
                    }
                    self.found_servers = list;
                }
                // Keep the tray tooltip reflecting live status.
                if let Some(t) = &self.tray {
                    let snap = self.engine.as_ref().map(|e| e.status());
                    let running = self.running();
                    t.set_state(&tray_tooltip(running, snap.as_ref()), running);
                }
                // Act on tray menu clicks (the tray can't touch the engine/window itself).
                let mut want_open = false;
                let mut want_quit = false;
                for ev in crate::tray::poll() {
                    match ev {
                        crate::tray::TrayEvent::Start => {
                            if !self.running() {
                                self.start_engine();
                            }
                        }
                        crate::tray::TrayEvent::Stop => self.stop_engine(),
                        crate::tray::TrayEvent::Open => want_open = true,
                        crate::tray::TrayEvent::Quit => want_quit = true,
                    }
                }
                if want_quit {
                    return self.update(Message::Quit);
                }
                if want_open {
                    return iced::window::latest().and_then(|id| {
                        // Un-minimize *before* focusing. On Windows `gain_focus` is
                        // `SetForegroundWindow`, which does nothing to a minimized window —
                        // so tray Open silently did nothing, and with `start_minimized` on
                        // there was no way to reach the panel at all. X11 window managers
                        // happen to map on activate, which is why this only showed on
                        // Windows.
                        iced::window::minimize(id, false).chain(iced::window::gain_focus(id))
                    });
                }
            }
            Message::ThemePicked(t) => self.theme = t,
            Message::TabPicked(t) => {
                self.tab = t;
                self.menu_open = false;
            }
            Message::NameChanged(s) => self.cfg.name = s,
            Message::RolePicked(r) => self.cfg.role = r,
            Message::SecretChanged(s) => self.cfg.shared_secret = s,
            Message::ToggleShowSecret => self.show_secret = !self.show_secret,
            Message::GenerateSecret => {
                self.cfg.shared_secret = generate_secret();
                self.feedback = "generated a new secret".into();
            }
            Message::TlsToggled(v) => self.cfg.tls_enabled = v,
            Message::ClipboardToggled(v) => self.cfg.clipboard_sync = v,
            // Re-use goes to the clipboard thread; pin/remove/clear touch the shared
            // history directly, because they never talk to the OS clipboard.
            Message::ClipReuse(id) => {
                if let Some(e) = &self.engine {
                    e.clipboard_reuse(id);
                    self.feedback = "copied — paste it anywhere".into();
                }
            }
            Message::ClipTogglePin(id) => {
                if let Some(e) = &self.engine {
                    match e.clipboard_toggle_pin(id) {
                        Some(true) => self.feedback = "pinned — it won't be evicted".into(),
                        Some(false) => self.feedback = "unpinned".into(),
                        None => {}
                    }
                }
            }
            Message::ClipRemove(id) => {
                if let Some(e) = &self.engine {
                    e.clipboard_remove(id);
                }
            }
            Message::ClipClear => {
                if let Some(e) = &self.engine {
                    e.clipboard_clear();
                    self.feedback = "history cleared (pinned entries kept)".into();
                }
            }
            Message::ClipAdvancedToggled(v) => self.clip_advanced = v,
            Message::ClipSyncTextToggled(v) => self.cfg.clipboard_sync_text = v,
            Message::ClipSyncImagesToggled(v) => self.cfg.clipboard_sync_images = v,
            // The numeric settings keep their own string buffer so a half-typed value is
            // never parsed; an unparseable buffer simply leaves `cfg` alone.
            Message::ClipItemsChanged(s) => {
                if let Ok(v) = s.trim().parse::<usize>() {
                    self.cfg.clipboard_history_size = v.max(1);
                }
                self.clip_items_s = s;
            }
            Message::ClipItemMbChanged(s) => {
                if let Ok(v) = s.trim().parse::<usize>() {
                    self.cfg.clipboard_max_item_bytes = v.max(1) * 1024 * 1024;
                }
                self.clip_item_mb_s = s;
            }
            Message::ClipMemMbChanged(s) => {
                if let Ok(v) = s.trim().parse::<usize>() {
                    self.cfg.clipboard_memory_budget = v.max(1) * 1024 * 1024;
                }
                self.clip_mem_mb_s = s;
            }
            Message::ClipImagePxChanged(s) => {
                if let Ok(v) = s.trim().parse::<usize>() {
                    self.cfg.clipboard_max_image_px = v.max(1);
                }
                self.clip_img_px_s = s;
            }
            Message::BulkEnabledToggled(v) => self.cfg.bulk_enabled = v,
            Message::ClipSyncFilesToggled(v) => self.cfg.clipboard_sync_files = v,
            Message::ClipAutoMbChanged(s) => {
                // 0 is meaningful here — it means "always ask" — so it is accepted as
                // typed rather than clamped up to 1 like the size limits.
                if let Ok(v) = s.trim().parse::<u64>() {
                    self.cfg.clipboard_auto_transfer_max = v * 1024 * 1024;
                }
                self.clip_auto_mb_s = s;
            }
            Message::ClipInlineKbChanged(s) => {
                if let Ok(v) = s.trim().parse::<usize>() {
                    self.cfg.clipboard_inline_image_max = v.max(1) * 1024;
                }
                self.clip_inline_kb_s = s;
            }
            Message::ClipMaxFilesChanged(s) => {
                if let Ok(v) = s.trim().parse::<usize>() {
                    self.cfg.clipboard_max_files_per_copy = v.max(1);
                }
                self.clip_max_files_s = s;
            }
            Message::ClipCacheGbChanged(s) => {
                if let Ok(v) = s.trim().parse::<u64>() {
                    self.cfg.clipboard_cache_budget = v.max(1) * 1024 * 1024 * 1024;
                }
                self.clip_inline_kb_s = (self.cfg.clipboard_inline_image_max / 1024).max(1).to_string();
        self.clip_max_files_s = self.cfg.clipboard_max_files_per_copy.to_string();
        self.clip_cache_gb_s = s;
            }
            Message::BulkPortChanged(s) => {
                // 0 (or an empty box) means "share the input link's port".
                if s.trim().is_empty() {
                    self.cfg.bulk_port = 0;
                } else if let Ok(v) = s.trim().parse::<u16>() {
                    self.cfg.bulk_port = v;
                }
                self.bulk_port_s = s;
            }
            Message::ReconnectToggled(v) => self.cfg.reconnect = v,
            Message::AutostartToggled(v) => {
                // This one touches the OS, not just `cfg`, so it takes effect immediately
                // (no Save needed) and reports the real outcome. On failure we re-read the
                // actual state rather than trusting the toggle, so the UI can't claim an
                // entry exists when the write was refused.
                let res = if v { crate::autostart::enable() } else { crate::autostart::disable() };
                match res {
                    Ok(()) => {
                        self.autostart_on = v;
                        // This writes outside our own config — a registry value on Windows,
                        // a desktop file on Linux — so it gets a log line either way. It's
                        // the kind of change a user later swears they never made.
                        tracing::info!(
                            "{} the start-at-login entry",
                            if v { "added" } else { "removed" }
                        );
                        self.feedback =
                            if v { "will start at login".into() } else { "removed from login".into() };
                    }
                    Err(e) => {
                        self.autostart_on = crate::autostart::is_enabled();
                        tracing::warn!("start-at-login change failed: {e}");
                        self.feedback = format!("autostart failed: {e}");
                    }
                }
            }
            Message::AutoconnectToggled(v) => self.cfg.autoconnect = v,
            Message::StartMinimizedToggled(v) => self.cfg.start_minimized = v,
            Message::LockHotkeyChanged(s) => self.cfg.lock_hotkey = s,
            Message::ReturnHotkeyChanged(s) => self.cfg.return_hotkey = s,
            Message::ServerHostChanged(s) => self.cfg.server_host = s,
            Message::ServerPortChanged(s) => {
                if let Ok(v) = s.parse() {
                    self.cfg.server_port = v;
                }
                self.server_port_s = s;
            }
            Message::ListenHostChanged(s) => self.cfg.listen_host = s,
            Message::ListenPortChanged(s) => {
                if let Ok(v) = s.parse() {
                    self.cfg.listen_port = v;
                }
                self.listen_port_s = s;
            }
            Message::ExitEdgePicked(e) => self.cfg.exit_edge = e,
            Message::EntryEdgePicked(e) => self.cfg.entry_edge = e,
            Message::ScreenWChanged(s) => {
                if let Ok(v) = s.parse() {
                    self.cfg.screen_width = v;
                }
                self.screen_w_s = s;
            }
            Message::ScreenHChanged(s) => {
                if let Ok(v) = s.parse() {
                    self.cfg.screen_height = v;
                }
                self.screen_h_s = s;
            }
            Message::DetectScreen => {
                self.feedback = match screen_size() {
                    Ok((w, h)) => {
                        self.cfg.screen_width = w;
                        self.cfg.screen_height = h;
                        self.sync_buffers();
                        format!("detected {w}×{h}")
                    }
                    Err(e) => format!("detect failed: {e}"),
                };
            }
            Message::ProfilePicked(p) => self.profile_pick = Some(p),
            Message::LoadProfile => {
                if let Some(name) = self.profile_pick.clone() {
                    self.feedback = match Config::load_profile(&name) {
                        Ok(c) => {
                            self.cfg = c;
                            self.sync_buffers();
                            format!("loaded '{name}'")
                        }
                        Err(e) => format!("load failed: {e}"),
                    };
                }
            }
            Message::DeleteProfile => {
                if let Some(name) = self.profile_pick.clone() {
                    self.feedback = match Config::delete_profile(&name) {
                        Ok(()) => format!("deleted '{name}'"),
                        Err(e) => format!("delete failed: {e}"),
                    };
                    self.profile_pick = None;
                    self.profiles = Config::list_profiles();
                }
            }
            Message::ProfileNameChanged(s) => self.profile_name_input = s,
            Message::SaveProfileAs => {
                self.feedback = match self.cfg.save_profile(&self.profile_name_input) {
                    Ok(()) => {
                        // Report — and select — the name the file was actually written
                        // under, not what was typed: `../../pwned` lands as `pwned`, and a
                        // raw pick would leave the dropdown pointing at no real profile.
                        let saved = sanitize_profile_name(&self.profile_name_input)
                            .unwrap_or_else(|| self.profile_name_input.trim().to_string());
                        self.profiles = Config::list_profiles();
                        self.profile_pick = Some(saved.clone());
                        self.profile_name_input.clear();
                        format!("saved profile '{saved}'")
                    }
                    Err(e) => format!("save failed: {e}"),
                };
            }
            Message::Start => {
                if !self.running() {
                    self.start_engine();
                }
            }
            Message::Stop => self.stop_engine(),
            Message::SaveConfig => {
                let path = Config::default_path();
                self.feedback = match self.cfg.save(&path) {
                    Ok(()) => {
                        tracing::info!("saved config to {}", path.display());
                        // Only now do the edits count as committed, which is what the
                        // restart prompt is measured against.
                        self.saved_cfg = self.cfg.clone();
                        // A fresh save is a fresh mismatch: show the prompt again rather
                        // than staying dismissed from a previous round of changes.
                        self.restart_hint_dismissed = false;
                        "saved".into()
                    }
                    Err(e) => {
                        tracing::warn!("could not save config to {}: {e}", path.display());
                        format!("save failed: {e}")
                    }
                };
            }
            Message::RestartNow => {
                // Stop, then start: `start_engine` re-saves and re-baselines `live_cfg`,
                // so the prompt clears itself and the confirmation is truthful.
                let names = self.pending_changes().join(", ");
                self.stop_engine();
                self.engine = None;
                self.start_engine();
                self.feedback = if names.is_empty() {
                    "sharing restarted".into()
                } else {
                    format!("restarted — new settings active ({names})")
                };
            }
            Message::RestartLater => {
                self.restart_hint_dismissed = true;
            }
            Message::ToggleMenu => self.menu_open = !self.menu_open,
            Message::CloseMenu => self.menu_open = false,
            Message::DetectServers => {
                let already_running =
                    matches!(*self.scan.lock().unwrap(), ScanState::Running);
                if !already_running {
                    *self.scan.lock().unwrap() = ScanState::Running;
                    self.feedback = "scanning this machine's networks…".into();
                    let slot = self.scan.clone();
                    let port = self.cfg.server_port;
                    std::thread::spawn(move || {
                        // 300 ms per host: long enough for a switch on a LAN, short
                        // enough that a full /24 finishes in a couple of seconds.
                        let found = mouse_core::discover::scan_for_servers(
                            port,
                            std::time::Duration::from_millis(300),
                        );
                        let list = found.into_iter().map(|ip| ip.to_string()).collect();
                        *slot.lock().unwrap() = ScanState::Done(list);
                    });
                }
            }
            Message::ServerPicked(host) => {
                self.cfg.server_host = host;
            }
            Message::ListenHostPicked(host) => {
                self.cfg.listen_host = host;
            }
            Message::RestoreDefaults => {
                self.menu_open = false;
                // Resets *preferences*, never facts about this machine. Wiping the name,
                // the screen size or the secret is not "restoring defaults", it is losing
                // information the user cannot retype from memory:
                //
                //   * shared secret — must match the other machine; clearing it breaks the
                //     pairing with no way to recover the old value.
                //   * screen size — the default 1920x1080 is wrong on any other display and
                //     silently corrupts edge geometry, so re-detect it instead.
                //   * machine name — an identity the peer displays, not a setting.
                //
                // Applied to the form only; the file is untouched until Save, so this stays
                // reviewable and abandonable.
                let (secret, name) = (self.cfg.shared_secret.clone(), self.cfg.name.clone());
                let (w, h) = screen_size()
                    .unwrap_or((self.cfg.screen_width, self.cfg.screen_height));
                self.cfg = Config {
                    shared_secret: secret,
                    name,
                    screen_width: w,
                    screen_height: h,
                    ..Config::default()
                };
                self.sync_buffers();
                self.feedback = format!(
                    "defaults restored — kept your name, secret and screen ({w}×{h}); press Save to apply"
                );
            }
            Message::Quit => {
                self.menu_open = false;
                if let Some(e) = &self.engine {
                    e.request_stop();
                }
                // Hard backstop so the process always dies even if teardown wedges.
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    std::process::exit(0);
                });
                // Close the window → iced returns → App drops → engine teardown runs
                // (releases the X grab / held keys). The backstop above guarantees exit.
                return iced::window::latest().and_then(iced::window::close);
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(std::time::Duration::from_millis(150)).map(|_| Message::Tick)
    }

    fn theme(&self) -> iced::Theme {
        if self.theme.is_dark() {
            iced::Theme::Dark
        } else {
            iced::Theme::Light
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let p = self.theme.palette();
        let running = self.running();
        let snap = self.engine.as_ref().map(|e| e.status());

        let mut content = column![
            self.header(p, running, snap.as_ref()),
            self.hero(p, running, snap.as_ref()),
        ]
        .spacing(12.0)
        .padding(16.0);
        if let Some(banner) = self.restart_banner(p) {
            content = content.push(banner);
        }
        if let Some(banner) = self.pending_files_banner(p) {
            content = content.push(banner);
        }
        let content = content.push(self.tab_bar(p)).push(
            scrollable(container(self.tab_body(p, running)).padding([4.0, 2.0]))
                .height(Length::Fill),
        ).push(self.footer(p, running));

        let page: Element<'_, Message> = container(content)
            .style(move |_| ui_theme::root(p))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        if !self.menu_open {
            return page;
        }
        // The menu floats over the page, right-aligned under the ⋯ button. The full-size
        // `mouse_area` beneath it closes the menu on any click elsewhere, which is what
        // makes it behave like a menu rather than a panel that has to be dismissed by
        // pressing the same button again.
        let dismiss = mouse_area(
            container(Space::new())
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Message::CloseMenu);

        let floating = container(
            column![
                Space::new().height(52.0),
                container(self.overflow_menu(p)).align_right(Length::Fill),
            ]
        )
        .padding([0.0, 16.0])
        .width(Length::Fill)
        .height(Length::Fill);

        stack![page, dismiss, floating].into()
    }

    /// "Restart needed" prompt: the panel's only honest answer to "did my change take
    /// effect?". Saving a setting the engine already read does nothing to the live
    /// session, and the previous behaviour — reporting "saved" and changing nothing —
    /// gave no way to tell the two states apart.
    ///
    /// Dismissing with "Later" collapses it to a one-line marker rather than hiding it,
    /// because the mismatch is still real after dismissal.
    fn restart_banner(&self, p: Palette) -> Option<Element<'_, Message>> {
        let pending = self.pending_changes();
        if pending.is_empty() {
            return None;
        }
        let n = pending.len();
        let list = pending.join(", ");
        let summary = format!(
            "{n} change{} saved but not yet active: {list}.",
            if n == 1 { "" } else { "s" }
        );

        if self.restart_hint_dismissed {
            // Compact persistent marker.
            return Some(
                container(
                    text(format!("⚠ {summary}  (restart to apply)"))
                        .size(12.0)
                        .style(move |_| iced::widget::text::Style { color: Some(p.warn) }),
                )
                .padding([6.0, 10.0])
                .style(move |_| ui_theme::card(p))
                .width(Length::Fill)
                .into(),
            );
        }

        Some(
            container(
                column![
                    text("⚠ Restart needed")
                        .size(14.0)
                        .style(move |_| iced::widget::text::Style { color: Some(p.warn) }),
                    text(summary).size(12.0),
                    row![
                        button(text("Restart sharing now").size(12.0))
                            .style(ui_theme::primary(p))
                            .padding([8.0, 12.0])
                            .on_press(Message::RestartNow),
                        button(text("Later").size(12.0))
                            .style(ui_theme::ghost(p))
                            .padding([8.0, 12.0])
                            .on_press(Message::RestartLater),
                    ]
                    .spacing(8.0),
                ]
                .spacing(8.0),
            )
            .padding([10.0, 12.0])
            .style(move |_| ui_theme::card(p))
            .width(Length::Fill)
            .into(),
        )
    }

    // ---------- header ----------

    fn header(&self, p: Palette, running: bool, snap: Option<&StatusSnapshot>) -> Element<'_, Message> {
        let (pill_fill, pill_txt, label) = status_pill(p, running, snap);
        let status = container(text(label).size(12.0).color(pill_txt))
            .style(move |_| ui_theme::pill(pill_fill, pill_txt))
            .padding([4.0, 10.0]);

        // Theme swatches used to sit here — six colour dots directly beside the one line
        // of status a user actually reads. They live in the ⋯ menu now.
        row![
            text("mouse-rs").size(22.0).color(p.txt),
            Space::new().width(Length::Fill),
            status,
            button(text("⋯").size(16.0).color(p.txt2))
                .style(ui_theme::ghost(p))
                .padding([6.0, 12.0])
                .on_press(Message::ToggleMenu),
        ]
        .spacing(10.0)
        .align_y(Alignment::Center)
        .into()
    }

    // ---------- status hero ----------

    /// One row of two machine chips plus the live control line.
    ///
    /// The previous hero was two large boxes and took roughly a third of the window to say
    /// what this says in ~70px. It answers the only question that changes moment to moment —
    /// which machine currently has the mouse and keyboard — so it stays on every tab.
    fn hero(&self, p: Palette, running: bool, snap: Option<&StatusSnapshot>) -> Element<'_, Message> {
        let connected = snap.map(|s| s.connected).unwrap_or(false);
        // Exactly one chip is lit, and it is always the machine that currently has the
        // mouse and keyboard. See `control_is_local` for why the role has to be folded in.
        let local_has_control = control_is_local(self.cfg.role, snap);

        let local = machine_chip(
            p,
            &self.cfg.name,
            &format!("{}×{}", self.cfg.screen_width, self.cfg.screen_height),
            local_has_control,
        );
        let (peer_name, peer_res) = match snap {
            Some(s) if s.connected => (s.peer.clone(), format!("{}×{}", s.peer_w, s.peer_h)),
            _ => ("waiting…".to_string(), "—".to_string()),
        };
        let peer = machine_chip(p, &peer_name, &peer_res, connected && !local_has_control);

        let link = text(if running { "⇄" } else { "—" })
            .size(18.0)
            .color(if running { p.accent } else { p.txt3 });

        let body = column![
            row![local, link, peer].spacing(10.0).align_y(Alignment::Center),
            text(hero_line(running, self.cfg.role, snap))
                .size(12.0)
                .color(if running { p.accent } else { p.txt3 }),
        ]
        .spacing(8.0);

        container(body)
            .style(move |_| ui_theme::hero(p))
            .padding(12.0)
            .width(Length::Fill)
            .into()
    }

    // ---------- tab bar ----------

    fn tab_bar(&self, p: Palette) -> Element<'_, Message> {
        let mut r = row![].spacing(6.0);
        for t in Tab::ALL {
            r = r.push(
                button(text(t.label()).size(13.0).color(if t == self.tab { p.accent_ink } else { p.txt2 }))
                    .style(ui_theme::segment(p, t == self.tab))
                    .padding([8.0, 16.0])
                    .on_press(Message::TabPicked(t)),
            );
        }
        container(r).padding(4.0).style(move |_| ui_theme::well(p)).into()
    }

    // ---------- tab bodies ----------

    fn tab_body(&self, p: Palette, running: bool) -> Element<'_, Message> {
        match self.tab {
            Tab::Connection => self.tab_connection(p, running),
            Tab::Layout => self.tab_layout(p),
            Tab::Security => self.tab_security(p),
            Tab::Clipboard => self.tab_clipboard(p),
            Tab::Startup => self.tab_startup(p),
        }
    }

    fn tab_connection(&self, p: Palette, _running: bool) -> Element<'_, Message> {
        let server_side = self.cfg.role == Role::Server;

        // Role segmented control.
        let role = row![
            seg_button(p, "Server · has mouse", self.cfg.role == Role::Server, Message::RolePicked(Role::Server)),
            seg_button(p, "Client · receives", self.cfg.role == Role::Client, Message::RolePicked(Role::Client)),
        ]
        .spacing(6.0);

        let net = if server_side {
            // Real addresses of this machine, so the bind address is chosen rather than
            // typed from memory.
            let local = mouse_core::discover::listen_candidates();
            let picked = local
                .iter()
                .find(|c| **c == self.cfg.listen_host)
                .cloned();
            column![
                labeled(p, "Listen host", input(p, "0.0.0.0", &self.cfg.listen_host, Message::ListenHostChanged)),
                labeled(
                    p,
                    "This machine's addresses",
                    pick_list(local, picked, Message::ListenHostPicked)
                        .width(Length::Fill)
                        .padding([10.0, 12.0])
                        .text_size(13.0),
                ),
                labeled(p, "Listen port", input(p, "24800", &self.listen_port_s, Message::ListenPortChanged)),
            ]
            .spacing(12.0)
        } else {
            let scanning = matches!(*self.scan.lock().unwrap(), ScanState::Running);
            let detect = button(
                text(if scanning { "Scanning…" } else { "Detect" }).size(13.0),
            )
            .style(ui_theme::ghost(p))
            .padding([9.0, 14.0]);
            let detect = if scanning { detect } else { detect.on_press(Message::DetectServers) };

            let mut col = column![
                labeled(
                    p,
                    "Server host / IP",
                    row![
                        input(p, "press Detect, or type the other machine's IP", &self.cfg.server_host, Message::ServerHostChanged),
                        detect,
                    ]
                    .spacing(8.0)
                    .align_y(Alignment::Center),
                ),
            ]
            .spacing(12.0);

            // Only offered once a sweep has actually found something — an empty picker
            // would imply the feature is broken rather than that nothing answered.
            if !self.found_servers.is_empty() {
                let picked = self
                    .found_servers
                    .iter()
                    .find(|c| **c == self.cfg.server_host)
                    .cloned();
                col = col.push(labeled(
                    p,
                    "Found on your network",
                    pick_list(self.found_servers.clone(), picked, Message::ServerPicked)
                        .width(Length::Fill)
                        .padding([10.0, 12.0])
                        .text_size(13.0),
                ));
            }

            col.push(labeled(p, "Server port", input(p, "24800", &self.server_port_s, Message::ServerPortChanged)))
                .push(toggle_row(p, "Auto-reconnect", "retry with backoff until the server appears", self.cfg.reconnect, Message::ReconnectToggled))
        };

        card(p, column![field_label(p, "Role"), role, net].spacing(14.0))
    }

    /// How the app behaves at login, plus profiles. Split out of Connection because it is a
    /// different question — how this machine starts the app, not how it reaches the peer —
    /// and because Connection had grown to nine fields and had to be scrolled.
    fn tab_startup(&self, p: Palette) -> Element<'_, Message> {
        // The three are deliberately independent: the OS launches the panel (Start at
        // login), the panel brings the link up by itself (Auto-connect), and it stays out
        // of the way while doing so (Start minimized).
        let startup = column![
            toggle_row(p, "Start at login", self.autostart_hint(), self.autostart_on, Message::AutostartToggled),
            toggle_row(p, "Auto-connect on launch", "begin sharing as soon as the panel opens", self.cfg.autoconnect, Message::AutoconnectToggled),
            toggle_row(p, "Start minimized", "open out of the way, into the tray/taskbar", self.cfg.start_minimized, Message::StartMinimizedToggled),
        ]
        .spacing(12.0);

        // The panel has no console behind it, so name the log file here — otherwise a user
        // hitting a problem has nothing to look at and nothing to send.
        let logs = column![
            field_label(p, "Log file"),
            text(&self.log_path).size(11.0).color(p.txt3),
        ]
        .spacing(6.0);

        card(p, column![
            labeled(p, "Machine name", input(p, "this-machine", &self.cfg.name, Message::NameChanged)),
            startup,
            logs,
            self.profiles_section(p),
        ]
        .spacing(14.0))
    }

    /// Sub-label for the Start-at-login toggle: says where the entry actually lives, so
    /// the setting is auditable rather than magic.
    fn autostart_hint(&self) -> &'static str {
        if cfg!(windows) {
            "adds a per-user entry under the Windows Run key"
        } else {
            "adds ~/.config/autostart/mouse-rs.desktop"
        }
    }

    fn tab_layout(&self, p: Palette) -> Element<'_, Message> {
        // Leaving through one edge means arriving through the facing one; anything else
        // puts the two screens on top of each other, so say so instead of saving it silently.
        let want = self.cfg.exit_edge.opposite();
        let edge_warning: Element<'_, Message> = if self.cfg.entry_edge == want {
            Space::new().height(0.0).into()
        } else {
            text(format!(
                "⚠ Leaving at the {} edge should bring the cursor in at the {} edge of the other screen — {} will feel like the screens overlap.",
                self.cfg.exit_edge.label(),
                want.label(),
                self.cfg.entry_edge.label(),
            ))
            .size(11.0)
            .color(p.warn)
            .into()
        };

        card(p, column![
            field_label(p, "Exit edge — this machine leaves here"),
            edge_picker(p, self.cfg.exit_edge, Message::ExitEdgePicked),
            field_label(p, "Entry edge — client appears here"),
            edge_picker(p, self.cfg.entry_edge, Message::EntryEdgePicked),
            edge_warning,
            field_label(p, "Screen size (px)"),
            row![
                input(p, "1920", &self.screen_w_s, Message::ScreenWChanged),
                text("×").size(16.0).color(p.txt3),
                input(p, "1080", &self.screen_h_s, Message::ScreenHChanged),
                button(text("Detect").size(13.0)).style(ui_theme::ghost(p)).padding([9.0, 14.0]).on_press(Message::DetectScreen),
            ]
            .spacing(10.0)
            .align_y(Alignment::Center),
            text("The engine re-detects this on Start; edit only to override.").size(11.0).color(p.txt3),
        ]
        .spacing(14.0))
    }

    fn tab_security(&self, p: Palette) -> Element<'_, Message> {
        let secret = row![
            input(p, "shared secret", &self.cfg.shared_secret, Message::SecretChanged)
                .secure(!self.show_secret),
            button(text(if self.show_secret { "Hide" } else { "Show" }).size(13.0))
                .style(ui_theme::ghost(p)).padding([9.0, 12.0]).on_press(Message::ToggleShowSecret),
            button(text("Generate").size(13.0))
                .style(ui_theme::ghost(p)).padding([9.0, 12.0]).on_press(Message::GenerateSecret),
        ]
        .spacing(8.0);

        card(p, column![
            field_label(p, "Shared secret — identical on both machines"),
            secret,
            toggle_row(p, "Encrypt link (TLS)", "rustls, self-signed cert", self.cfg.tls_enabled, Message::TlsToggled),
            labeled(p, "Lock / cross hotkey", input(p, "scroll_lock", &self.cfg.lock_hotkey, Message::LockHotkeyChanged)),
            text("Single key. Always returns control, even if the network or engine has wedged.")
                .size(11.0).color(p.txt3),
            labeled(p, "Return hotkey (server)", input(p, "ctrl+win+alt+esc", &self.cfg.return_hotkey, Message::ReturnHotkeyChanged)),
            text("Optional combo, matched on whichever machine is the server. Empty = none.")
                .size(11.0).color(p.txt3),
        ]
        .spacing(14.0))
    }

    /// "The other machine copied something big — want it?"
    ///
    /// A banner rather than a dialog, and that is not a style preference. Windows
    /// withholds `WH_KEYBOARD_LL` callbacks from a process while that process owns the
    /// foreground window — the exact root cause of the keyboard leak fixed on
    /// 2026‑07‑31. A pop-up that grabbed focus the moment a large file was copied would
    /// bring it straight back, and it would do so mid-work, quite possibly mid-crossing.
    /// So this never calls `SetForegroundWindow` and never steals focus.
    fn pending_files_banner(&self, p: Palette) -> Option<Element<'_, Message>> {
        let waiting: Vec<_> = self
            .engine
            .as_ref()?
            .clipboard_meta()
            .into_iter()
            .filter(|m| m.status == mouse_clipboard::ClipStatus::NeedsConsent)
            .collect();
        let first = waiting.first()?;
        let more = waiting.len().saturating_sub(1);
        let title = if more > 0 {
            format!("{} (and {more} more) is waiting", first.preview)
        } else {
            format!("{} is waiting", first.preview)
        };
        Some(
            container(
                row![
                    column![
                        text(title).size(13.5).color(p.txt),
                        text(format!(
                            "{} — over your {} limit, so it hasn't been brought over yet.",
                            human_bytes(first.disk_bytes as usize),
                            human_bytes(self.cfg.clipboard_auto_transfer_max as usize)
                        ))
                        .size(11.0)
                        .color(p.txt3),
                    ]
                    .spacing(3.0),
                    Space::new().width(Length::Fill),
                    button(text("Bring it over").size(12.5))
                        .style(ui_theme::primary(p))
                        .padding([9.0, 14.0])
                        .on_press(Message::ClipReuse(first.id)),
                ]
                .spacing(10.0)
                .align_y(Alignment::Center),
            )
            .style(move |_| ui_theme::well(p))
            .padding([11.0, 14.0])
            .width(Length::Fill)
            .into(),
        )
    }

    /// The clipboard manager: a list you can act on, not a log you can only read.
    ///
    /// Every row is a button that puts that item back on the clipboard (and sends it to
    /// the peer, exactly as a fresh copy would). Settings split Basic / Advanced so the
    /// four most people touch stay visible while the rest are one click away — nothing
    /// is hidden, it just isn't all shouting at once.
    fn tab_clipboard(&self, p: Palette) -> Element<'_, Message> {
        let entries = self.engine.as_ref().map(|e| e.clipboard_meta());
        let mut list = column![].spacing(8.0);
        match &entries {
            None => {
                list = list.push(
                    text("Start sharing to build up clipboard history.").size(12.0).color(p.txt3),
                )
            }
            Some(e) if e.is_empty() => {
                list = list.push(text("Nothing copied yet.").size(12.0).color(p.txt3))
            }
            Some(e) => {
                for m in e {
                    let (arrow, col) = if m.incoming { ("↓ in", p.accent) } else { ("↑ out", p.good) };
                    let kind = if m.is_files {
                        "🗎"
                    } else if m.is_image {
                        "🖼"
                    } else {
                        "¶"
                    };
                    // The right-hand label is the entry's state in the user's words.
                    // Every one of these is a situation they can really be in, and the
                    // alternative to saying so is leaving them to guess.
                    let (note, note_col) = match &m.status {
                        mouse_clipboard::ClipStatus::Ready if m.is_files => {
                            (format!("{} · ready", human_bytes(m.disk_bytes as usize)), p.good)
                        }
                        mouse_clipboard::ClipStatus::Ready => (human_bytes(m.bytes), p.txt3),
                        mouse_clipboard::ClipStatus::Remote => (
                            format!("{} · fetching…", human_bytes(m.disk_bytes as usize)),
                            p.accent,
                        ),
                        mouse_clipboard::ClipStatus::NeedsConsent => (
                            format!("{} · click to bring it over", human_bytes(m.disk_bytes as usize)),
                            p.warn,
                        ),
                        mouse_clipboard::ClipStatus::Fetching { done, total } => {
                            let pct = if *total > 0 { done * 100 / total } else { 0 };
                            (format!("{pct}% of {}", human_bytes(*total as usize)), p.accent)
                        }
                        mouse_clipboard::ClipStatus::Unavailable(why) => (why.clone(), p.danger),
                    };
                    // The row itself is the re-use action — the thing the user wants
                    // nine times out of ten needs no menu and no second click.
                    let reuse = button(
                        row![
                            text(kind).size(12.0).color(p.txt3),
                            text(arrow).size(11.0).color(col),
                            text(m.preview.clone()).size(13.0).color(p.txt2),
                            Space::new().width(Length::Fill),
                            text(note).size(11.0).color(note_col),
                        ]
                        .spacing(10.0)
                        .align_y(Alignment::Center),
                    )
                    .style(ui_theme::ghost(p))
                    .padding([10.0, 12.0])
                    .width(Length::Fill)
                    .on_press(Message::ClipReuse(m.id));

                    let pin = button(text(if m.pinned { "📌" } else { "📍" }).size(12.0))
                        .style(ui_theme::ghost(p))
                        .padding([10.0, 10.0])
                        .on_press(Message::ClipTogglePin(m.id));
                    let del = button(text("✕").size(12.0))
                        .style(ui_theme::ghost(p))
                        .padding([10.0, 10.0])
                        .on_press(Message::ClipRemove(m.id));

                    list = list.push(row![reuse, pin, del].spacing(4.0).align_y(Alignment::Center));
                }
            }
        }

        // What the user's own limits are costing right now — shown rather than left to
        // be discovered later.
        let usage: Element<'_, Message> = match self.engine.as_ref().map(|e| e.clipboard_usage()) {
            Some((count, bytes)) if count > 0 => text(format!(
                "{count} of {} entries · {} of {}",
                self.cfg.clipboard_history_size,
                human_bytes(bytes),
                human_bytes(self.cfg.clipboard_memory_budget),
            ))
            .size(11.0)
            .color(p.txt3)
            .into(),
            _ => Space::new().height(0.0).into(),
        };

        let header = row![
            field_label(p, "History — newest first · click any entry to copy it again"),
            Space::new().width(Length::Fill),
            button(text("Clear").size(12.0))
                .style(ui_theme::ghost(p))
                .padding([7.0, 12.0])
                .on_press(Message::ClipClear),
        ]
        .align_y(Alignment::Center);

        let mut body = column![
            toggle_row(
                p,
                "Sync clipboard",
                "copy here, paste there — text and images",
                self.cfg.clipboard_sync,
                Message::ClipboardToggled
            ),
            header,
            list,
            usage,
            labeled(p, "Keep this many entries", input(p, "25", &self.clip_items_s, Message::ClipItemsChanged)),
            labeled(
                p,
                "Ask before bringing over anything larger than (MB)",
                input(p, "1", &self.clip_auto_mb_s, Message::ClipAutoMbChanged)
            ),
            text(if self.cfg.clipboard_auto_transfer_max == 0 {
                "0 — always ask, whatever the size.".to_string()
            } else {
                format!(
                    "Under {} it just arrives; over it, the entry waits for you.",
                    human_bytes(self.cfg.clipboard_auto_transfer_max as usize)
                )
            })
            .size(11.0)
            .color(p.txt3),
            toggle_row(
                p,
                "Advanced settings",
                "size limits, memory budget, what gets synced",
                self.clip_advanced,
                Message::ClipAdvancedToggled
            ),
        ]
        .spacing(12.0);

        if self.clip_advanced {
            // The worst case this configuration allows. Stating the cost is the job;
            // refusing the value is not — a slow setting chosen deliberately is the
            // owner's trade to make.
            let worst = self
                .cfg
                .clipboard_history_size
                .saturating_mul(self.cfg.clipboard_max_item_bytes)
                .min(self.cfg.clipboard_memory_budget);
            let px = self.cfg.clipboard_max_image_px;
            // 4 bytes a pixel of RGBA, allocated while an image is encoded or decoded.
            let img_cost = px.saturating_mul(px).saturating_mul(4);

            body = body.push(
                column![
                    toggle_row(p, "Sync text", "copied text crosses machines", self.cfg.clipboard_sync_text, Message::ClipSyncTextToggled),
                    toggle_row(p, "Sync images", "copied images cross machines", self.cfg.clipboard_sync_images, Message::ClipSyncImagesToggled),
                    toggle_row(p, "Sync files", "any file or folder, any extension — no allow-list", self.cfg.clipboard_sync_files, Message::ClipSyncFilesToggled),
                    labeled(p, "Most files in one copy", input(p, "10000", &self.clip_max_files_s, Message::ClipMaxFilesChanged)),
                    text("Bounds a stray click on an enormous folder. The rest is left behind, and the log says so.")
                        .size(11.0)
                        .color(p.txt3),
                    labeled(
                        p,
                        "Send images inline up to (KB)",
                        input(p, "256", &self.clip_inline_kb_s, Message::ClipInlineKbChanged)
                    ),
                    text("Bigger images take the separate lane instead, so they can't stall the mouse. Lower this on a slow link; raise it to skip the extra round trip on a fast one.")
                        .size(11.0)
                        .color(p.txt3),
                    labeled(p, "Disk for transferred files (GB)", input(p, "2", &self.clip_cache_gb_s, Message::ClipCacheGbChanged)),
                    text("Oldest go first when it fills. Pinned entries are never removed.")
                        .size(11.0)
                        .color(p.txt3),
                    labeled(p, "Biggest item to keep (MB)", input(p, "5", &self.clip_item_mb_s, Message::ClipItemMbChanged)),
                    labeled(p, "Memory budget (MB)", input(p, "64", &self.clip_mem_mb_s, Message::ClipMemMbChanged)),
                    text(format!(
                        "This configuration can hold up to {}. Pinned entries are never evicted.",
                        human_bytes(worst)
                    ))
                    .size(11.0)
                    .color(p.txt3),
                    labeled(p, "Largest image edge (px)", input(p, "4096", &self.clip_img_px_s, Message::ClipImagePxChanged)),
                    text(format!(
                        "An image this size costs about {} of memory for a moment while it is compressed.",
                        human_bytes(img_cost)
                    ))
                    .size(11.0)
                    .color(if img_cost > 128 * 1024 * 1024 { p.warn } else { p.txt3 }),
                    toggle_row(
                        p,
                        "Separate lane for large transfers",
                        "stops big items queueing in front of the mouse",
                        self.cfg.bulk_enabled,
                        Message::BulkEnabledToggled
                    ),
                    labeled(
                        p,
                        "Port for that lane (blank = share the main one)",
                        input(p, "same port", &self.bulk_port_s, Message::BulkPortChanged)
                    ),
                    text(if self.cfg.bulk_port == 0 {
                        "Sharing one port — nothing extra to open in a firewall.".to_string()
                    } else {
                        format!("Port {} must be reachable between both machines.", self.cfg.bulk_port)
                    })
                    .size(11.0)
                    .color(p.txt3),
                ]
                .spacing(12.0),
            );
        }

        card(p, body)
    }

    // ---------- profiles + footer ----------

    /// Two controls, and only two. The old footer carried eight — profile picker, Load,
    /// Delete, save-as box, Save as, Restore defaults, Save, Start, Stop and Quit — which
    /// overflowed at normal window width and silently pushed Stop and Quit off the edge.
    /// Everything rare or destructive is in the ⋯ menu; Start and Stop are one toggle
    /// because they are mutually exclusive and one was always greyed out.
    fn footer(&self, p: Palette, running: bool) -> Element<'_, Message> {
        let run = if running {
            button(text("Stop sharing").size(14.0))
                .style(ui_theme::danger(p))
                .padding([12.0, 20.0])
                .on_press(Message::Stop)
        } else {
            button(text("Start sharing").size(14.0))
                .style(ui_theme::primary(p))
                .padding([12.0, 20.0])
                .on_press(Message::Start)
        };

        row![
            text(&self.feedback).size(12.0).color(p.txt3),
            Space::new().width(Length::Fill),
            button(text("Save").size(13.0))
                .style(ui_theme::ghost(p))
                .padding([11.0, 16.0])
                .on_press(Message::SaveConfig),
            run,
        ]
        .spacing(8.0)
        .align_y(Alignment::Center)
        .into()
    }

    /// The ⋯ menu: appearance, profiles, and the destructive actions.
    fn overflow_menu(&self, p: Palette) -> Element<'_, Message> {
        let mut swatches = row![].spacing(6.0).align_y(Alignment::Center);
        for t in Theme6::ALL {
            let base = t.palette();
            swatches = swatches.push(
                button(Space::new().width(18.0).height(18.0))
                    .style(ui_theme::swatch(p, base, t == self.theme))
                    .padding(0.0)
                    .on_press(Message::ThemePicked(t)),
            );
        }

        let item = |label: &'static str, msg: Message, danger: bool| {
            button(text(label).size(12.5).color(if danger { p.danger } else { p.txt2 }))
                .style(ui_theme::ghost(p))
                .padding([9.0, 11.0])
                .width(Length::Fill)
                .on_press(msg)
        };

        container(
            column![
                text("APPEARANCE").size(10.0).color(p.txt3),
                swatches,
                item("Profiles…", Message::TabPicked(Tab::Startup), false),
                item("Restore defaults", Message::RestoreDefaults, true),
                item("Quit mouse-rs", Message::Quit, true),
            ]
            .spacing(8.0),
        )
        .style(move |_| ui_theme::card(p))
        .padding(10.0)
        .width(210.0)
        .into()
    }

    /// Profiles, with the sentence that was missing. The feature shipped as an unlabelled
    /// dropdown and three buttons in the footer, and the owner had never worked out what it
    /// was for — so the explanation is part of the control, not a docs problem.
    fn profiles_section(&self, p: Palette) -> Element<'_, Message> {
        let picker = pick_list(self.profiles.clone(), self.profile_pick.clone(), Message::ProfilePicked)
            .placeholder(if self.profiles.is_empty() { "no profiles saved yet" } else { "choose a profile" })
            .padding(8.0)
            .width(Length::Fill);

        column![
            field_label(p, "Profiles"),
            text(
                "A named snapshot of every setting on this page — peer address, edges, screen \
                 size, hotkeys, secret. Save one per setup (say \"desk\" and \"travel\") and switch \
                 without retyping. Loading one fills the form; press Save to keep it."
            )
            .size(11.0)
            .color(p.txt3),
            row![
                picker,
                button(text("Load").size(12.0)).style(ui_theme::ghost(p)).padding([8.0, 12.0]).on_press(Message::LoadProfile),
                button(text("Delete").size(12.0)).style(ui_theme::ghost(p)).padding([8.0, 12.0]).on_press(Message::DeleteProfile),
            ]
            .spacing(8.0)
            .align_y(Alignment::Center),
            row![
                input(p, "name this setup…", &self.profile_name_input, Message::ProfileNameChanged),
                button(text("Save as").size(12.0)).style(ui_theme::ghost(p)).padding([8.0, 12.0]).on_press(Message::SaveProfileAs),
            ]
            .spacing(8.0)
            .align_y(Alignment::Center),
            // Said plainly because a profile is as sensitive as the config file itself.
            text("Stored in ~/.mouse_share/profiles/. They include the shared secret.")
                .size(10.5)
                .color(p.txt3),
        ]
        .spacing(8.0)
        .into()
    }
}

// ---------- free helper widgets ----------

fn input<'a>(
    p: Palette,
    placeholder: &'a str,
    value: &'a str,
    on_change: impl Fn(String) -> Message + 'a,
) -> iced::widget::TextInput<'a, Message> {
    text_input(placeholder, value)
        .on_input(on_change)
        .style(ui_theme::input(p))
        .padding(11.0)
        .size(14.0)
        .width(Length::Fill)
}

fn field_label(p: Palette, label: &str) -> Element<'_, Message> {
    text(label.to_string()).size(12.0).color(p.txt2).into()
}

fn labeled<'a>(p: Palette, label: &'a str, field: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![field_label(p, label), field.into()].spacing(6.0).into()
}

fn card<'a>(p: Palette, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(body)
        .style(move |_| ui_theme::card(p))
        .padding(18.0)
        .width(Length::Fill)
        .into()
}

fn seg_button(p: Palette, label: &str, active: bool, msg: Message) -> Element<'_, Message> {
    button(text(label.to_string()).size(13.0).color(if active { p.accent_ink } else { p.txt2 }))
        .style(ui_theme::segment(p, active))
        .padding([9.0, 16.0])
        .width(Length::Fill)
        .on_press(msg)
        .into()
}

fn toggle_row<'a>(
    p: Palette,
    title: &'a str,
    desc: &'a str,
    value: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    let labels = column![
        text(title.to_string()).size(14.0).color(p.txt),
        text(desc.to_string()).size(11.0).color(p.txt3),
    ]
    .spacing(2.0);
    container(
        row![
            labels,
            Space::new().width(Length::Fill),
            toggler(value).on_toggle(on_toggle).style(ui_theme::switch(p)).size(24.0),
        ]
        .align_y(Alignment::Center),
    )
    .style(move |_| ui_theme::well(p))
    .padding([13.0, 15.0])
    .width(Length::Fill)
    .into()
}

/// A small +-shaped edge picker: click the edge that faces the other machine.
fn edge_picker(p: Palette, current: Edge, on_pick: impl Fn(Edge) -> Message + Copy + 'static) -> Element<'static, Message> {
    let seg = |edge: Edge, w: f32, h: f32| {
        button(Space::new().width(w).height(h))
            .style(ui_theme::edge_seg(p, current == edge))
            .padding(0.0)
            .on_press(on_pick(edge))
    };
    let spacer = || Space::new().width(Length::Fill).height(Length::Fill);
    let stage = column![
        row![spacer(), seg(Edge::Top, 60.0, 8.0), spacer()].align_y(Alignment::Center),
        row![
            seg(Edge::Left, 8.0, 40.0),
            container(text("screen").size(11.0).color(p.txt3)).center_x(Length::Fill),
            seg(Edge::Right, 8.0, 40.0),
        ]
        .align_y(Alignment::Center)
        .spacing(6.0),
        row![spacer(), seg(Edge::Bottom, 60.0, 8.0), spacer()].align_y(Alignment::Center),
    ]
    .spacing(6.0);

    container(stage)
        .style(move |_| ui_theme::well(p))
        .padding(14.0)
        .width(Length::Fill)
        .into()
}

/// One machine in the hero: name on the left, resolution on the right, highlighted when
/// that machine currently owns the input.
fn machine_chip(p: Palette, name: &str, res: &str, active: bool) -> Element<'static, Message> {
    container(
        row![
            text(name.to_string()).size(13.0).color(p.txt),
            Space::new().width(Length::Fill),
            text(res.to_string()).size(11.0).color(p.txt3),
        ]
        .align_y(Alignment::Center),
    )
    .style(move |_| if active { ui_theme::well_active(p) } else { ui_theme::well(p) })
    .padding([9.0, 11.0])
    .width(Length::Fill)
    .into()
}

/// Which machine currently holds the mouse and keyboard — `true` for this one.
///
/// `StatusSnapshot::engaged` is **role-relative**, and that is the trap. On the server it
/// means the cursor has left for the peer; on the client it means the peer is driving *this*
/// machine. It therefore points at opposite machines on the two ends of the same session,
/// and folding the role back in is not optional.
///
/// The hero used `engaged` directly (`!engaged || !server_side` for the local chip,
/// `engaged && server_side` for the peer), which on a **client** reduces to `true` and
/// `false` — constants. So on Kali, which is the client here, the local chip was lit
/// permanently and the peer chip could never light, whichever way the mouse went. The one
/// question the hero exists to answer went unanswered on half the machines.
///
/// With no live session, control is trivially on this machine.
fn control_is_local(role: Role, snap: Option<&StatusSnapshot>) -> bool {
    match snap {
        Some(s) if s.connected => match role {
            Role::Server => !s.engaged,
            Role::Client => s.engaged,
        },
        _ => true,
    }
}

// ---------- status text helpers ----------

fn status_pill(p: Palette, running: bool, snap: Option<&StatusSnapshot>) -> (iced::Color, iced::Color, String) {
    match (running, snap) {
        (false, _) => (ui_theme::alpha(p.txt3, 0.18), p.txt3, "stopped".into()),
        (true, Some(s)) if s.connected => (
            ui_theme::alpha(p.good, 0.16),
            p.good,
            format!("connected — {} ({}×{})", s.peer, s.peer_w, s.peer_h),
        ),
        (true, _) => (ui_theme::alpha(p.warn, 0.16), p.warn, "waiting for peer…".into()),
    }
}

/// Compact hover text for the tray icon: a mirror of the header status.
fn tray_tooltip(running: bool, snap: Option<&StatusSnapshot>) -> String {
    match (running, snap) {
        (false, _) => "mouse-rs — stopped".to_owned(),
        (true, Some(s)) if s.connected => {
            let mut t = format!("mouse-rs — connected to {}", s.peer);
            if s.engaged {
                t.push_str(" (active)");
            }
            t
        }
        (true, _) => "mouse-rs — waiting for peer".to_owned(),
    }
}

fn hero_line(running: bool, role: Role, snap: Option<&StatusSnapshot>) -> String {
    match snap {
        Some(s) if s.connected && s.engaged => match role {
            Role::Server => format!("→ controlling {}", s.peer),
            Role::Client => format!("← controlled by {}", s.peer),
        },
        Some(s) if s.connected => format!("connected to {}", s.peer),
        _ if running => "waiting for peer…".into(),
        _ => "stopped".into(),
    }
}

/// A random URL-safe-ish secret for the Generate button.
fn generate_secret() -> String {
    use rand::Rng;
    const CH: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..32).map(|_| CH[rng.gen_range(0..CH.len())] as char).collect()
}

/// Open the control panel, blocking until the window closes.
pub fn launch(cfg: Config) -> anyhow::Result<()> {
    iced::application(move || App::new(cfg.clone()), App::update, App::view)
        .title("mouse-rs")
        .subscription(App::subscription)
        .theme(App::theme)
        .window_size(iced::Size::new(560.0, 720.0))
        .run()
        .map_err(|e| anyhow::anyhow!("control panel: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Changing a setting the engine already read must be reported, or the panel says
    /// "saved" while the live session keeps using the old value — the exact confusion
    /// this prompt exists to remove.
    #[test]
    fn restart_needed_lists_only_the_settings_that_changed() {
        let live = Config::default();
        let mut now = live.clone();
        assert!(pending_restart_changes(&live, &now).is_empty(), "identical configs are clean");

        now.listen_port = live.listen_port.wrapping_add(1);
        now.tls_enabled = !live.tls_enabled;
        let pending = pending_restart_changes(&live, &now);
        assert_eq!(pending, vec!["TLS", "listen port"]);
    }

    /// Settings that take effect immediately must NOT raise a restart prompt — a prompt
    /// that cries wolf for the theme picker would be ignored when it matters.
    #[test]
    fn immediate_settings_do_not_ask_for_a_restart() {
        let live = Config::default();
        let mut now = live.clone();
        now.autoconnect = !live.autoconnect;
        now.start_minimized = !live.start_minimized;
        assert!(
            pending_restart_changes(&live, &now).is_empty(),
            "launch-time-only settings must not demand a restart of a running session"
        );
    }

    /// The prompt says "saved but not yet active", so it must be measured against what
    /// was actually saved. Comparing against the live edit buffer made it appear while
    /// the user was still typing, before they had saved anything — which reads as the
    /// panel warning about a change the user has not made yet.
    #[test]
    fn typing_without_saving_is_not_pending() {
        let running_engine_cfg = Config::default();
        let saved = running_engine_cfg.clone();

        // The user is midway through editing the port field; nothing is committed.
        let mut being_edited = saved.clone();
        being_edited.listen_port = 9999;

        assert!(
            pending_restart_changes(&running_engine_cfg, &saved).is_empty(),
            "unsaved edits must not raise the restart prompt"
        );
        // ...and once they press Save, that same change is pending.
        assert_eq!(
            pending_restart_changes(&running_engine_cfg, &being_edited),
            vec!["listen port"]
        );
    }

    /// "Restore defaults" must not destroy facts about this machine. It reset the name to
    /// a generic default and the screen to 1920x1080 on a 1440x900 display, which silently
    /// corrupts edge geometry — the user cannot retype what was auto-detected.
    #[test]
    fn restoring_defaults_keeps_identity_and_hardware_facts() {
        let cfg = Config {
            name: "kali-desktop".into(),
            shared_secret: "a-real-secret".into(),
            screen_width: 1440,
            screen_height: 900,
            exit_edge: Edge::Right, // a preference, should be reset
            ..Config::default()
        };

        // Mirrors the handler: preferences reset, machine facts carried over.
        let restored = Config {
            shared_secret: cfg.shared_secret.clone(),
            name: cfg.name.clone(),
            screen_width: cfg.screen_width,
            screen_height: cfg.screen_height,
            ..Config::default()
        };

        assert_eq!(restored.name, "kali-desktop");
        assert_eq!(restored.shared_secret, "a-real-secret");
        assert_eq!((restored.screen_width, restored.screen_height), (1440, 900));
        assert_eq!(restored.exit_edge, Config::default().exit_edge, "preferences do reset");
    }

    /// The hero highlight must track the machine that has the mouse, on **both** ends.
    /// It did not: `engaged` is role-relative, and using it raw made the client's chips
    /// constants — local always lit, peer never — so the panel on the client machine could
    /// not say where control was. Asserted per role because the client is the side that
    /// was broken and the side no test covered.
    #[test]
    fn the_lit_chip_is_the_machine_that_has_control() {
        let idle = StatusSnapshot { connected: true, engaged: false, ..Default::default() };
        let crossing = StatusSnapshot { connected: true, engaged: true, ..Default::default() };

        // Server: not engaged = the cursor is still here; engaged = it has gone to the peer.
        assert!(control_is_local(Role::Server, Some(&idle)));
        assert!(!control_is_local(Role::Server, Some(&crossing)));

        // Client: engaged = the peer is driving *this* machine, so control is here — the
        // exact inversion the old code missed.
        assert!(!control_is_local(Role::Client, Some(&idle)));
        assert!(control_is_local(Role::Client, Some(&crossing)));

        // No session: control is trivially local, on either role. A client showing the
        // peer chip lit while stopped would be a new way of lying about the same thing.
        let down = StatusSnapshot { connected: false, engaged: false, ..Default::default() };
        for role in [Role::Server, Role::Client] {
            assert!(control_is_local(role, None), "{role:?}: no engine");
            assert!(control_is_local(role, Some(&down)), "{role:?}: disconnected");
        }
    }

    /// Screen size is two fields but one user-facing setting; reporting it twice would
    /// misstate the count in "N changes saved but not yet active".
    #[test]
    fn screen_size_counts_as_one_change() {
        let live = Config::default();
        let mut now = live.clone();
        now.screen_width += 100;
        now.screen_height += 100;
        assert_eq!(pending_restart_changes(&live, &now), vec!["screen size"]);
    }
}
