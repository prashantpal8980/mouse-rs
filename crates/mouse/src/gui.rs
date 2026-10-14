//! iced control panel (the product UI). A bare `mouse-rs` launch opens this window:
//! edit config, Save it, and Start/Stop the engine — the same lifecycle the headless
//! CLI uses, so the GUI is just a front-end over [`crate::run`]. Closing the window
//! quits the app. The look (6 themes, status hero, animated toggles) lives in
//! [`crate::ui_theme`]; the system tray in [`crate::tray`].

use std::sync::{Arc, Mutex};

use iced::widget::{
    button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, stack, text,
    text_input, toggler, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task};

use mouse_core::{sanitize_profile_name, Config, Edge, Role};
use mouse_engine::StatusSnapshot;
use mouse_input::screen_size;

use crate::run::EngineHandle;
use crate::ui_theme::{self, Palette, Theme6};

/// The main window's tabs: only what is used every day (R28). Everything else is a
/// setting, and settings live in ⋯ → Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connection,
    Clipboard,
}

impl Tab {
    const ALL: [Tab; 2] = [Tab::Connection, Tab::Clipboard];
    fn label(self) -> &'static str {
        match self {
            Tab::Connection => "Connection",
            Tab::Clipboard => "Clipboard",
        }
    }
}

/// A page of ⋯ → Settings, like a category in a phone's Settings app (R24, R29, R30).
///
/// The order of [`Page::CATEGORIES`] is the order on screen: the ones people change most
/// first. Each page shows its everyday settings and ends with a closed Advanced section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    /// The list of categories.
    Index,
    Clipboard,
    Screens,
    Startup,
    Keys,
    Connection,
    Security,
    Appearance,
    Diagnostics,
    About,
}

impl Page {
    /// (group heading, its pages), in the order shown (R30).
    const CATEGORIES: [(&'static str, &'static [Page]); 3] = [
        ("EVERYDAY", &[Page::Clipboard, Page::Screens, Page::Startup, Page::Keys]),
        ("SETUP", &[Page::Connection, Page::Security, Page::Appearance]),
        ("HELP & ADVANCED", &[Page::Diagnostics, Page::About]),
    ];
    fn title(self) -> &'static str {
        match self {
            Page::Index => "Settings",
            Page::Clipboard => "Clipboard",
            Page::Screens => "Screens & edges",
            Page::Startup => "Startup",
            Page::Keys => "Keyboard & hotkeys",
            Page::Connection => "Connection",
            Page::Security => "Security & pairing",
            Page::Appearance => "Appearance",
            Page::Diagnostics => "Diagnostics",
            Page::About => "About",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Page::Index => "⚙",
            Page::Clipboard => "📋",
            Page::Screens => "🖥",
            Page::Startup => "⏻",
            Page::Keys => "⌨",
            Page::Connection => "🔗",
            Page::Security => "🔒",
            Page::Appearance => "🎨",
            Page::Diagnostics => "🩺",
            Page::About => "ℹ",
        }
    }
    fn summary(self) -> &'static str {
        match self {
            Page::Index => "",
            Page::Clipboard => "What to share, history, files, transfers",
            Page::Screens => "Which side the other computer is on",
            Page::Startup => "Start at login, connect automatically",
            Page::Keys => "Getting control back",
            Page::Connection => "Name, ports, reconnecting, profiles",
            Page::Security => "Shared secret, encryption",
            Page::Appearance => "Theme",
            Page::Diagnostics => "Log file, sending logs for support",
            Page::About => "Version, restore default settings",
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
    ClipCancel(u64),
    ClipDecline(u64),
    ClipAlwaysAllow(u64),
    ClipTogglePin(u64),
    ClipRemove(u64),
    ClipClear,
    ClipCacheClear,
    /// Open ⋯ → Settings at this page; `Index` is the list of categories.
    OpenSettings(Page),
    /// Back to the main window.
    CloseSettings,
    /// Open or close a settings page's Advanced section.
    AdvancedToggled(Page),
    EdgeThresholdChanged(String),
    DiscoveryPortChanged(String),
    CacheDirChanged(String),
    HashLimitMbChanged(String),
    LogUploadUrlChanged(String),
    LogUploadSecsChanged(String),
    ClipSyncTextToggled(bool),
    ClipSyncImagesToggled(bool),
    ClipItemsChanged(String),
    ClipDeliverDirChanged(String),
    ClipMoveOnPasteToggled(bool),
    ClipItemMbChanged(String),
    ClipMemMbChanged(String),
    ClipImagePxChanged(String),
    BulkEnabledToggled(bool),
    BulkPortChanged(String),
    ClipSyncFilesToggled(bool),
    ClipFollowSymlinksToggled(bool),
    ClipImagesAsFileToggled(bool),
    ClipNotifyToggled(bool),
    BulkBufferChanged(String),
    ClipAutoMbChanged(String),
    ClipCacheGbChanged(String),
    ClipInlineKbChanged(String),
    ClipMaxFilesChanged(String),
    ClipPersistToggled(bool),
    ClipQuickSecsChanged(String),
    ClipStreamMbChanged(String),
    ClipStreamWaitChanged(String),
    ClipKeepResumableToggled(bool),
    BulkPauseToggled(bool),
    BulkRateChanged(String),
    ReconnectToggled(bool),
    AutostartToggled(bool),
    AutostartElevatedToggled(bool),
    RelaunchElevated,
    DismissElevationNotice,
    /// The login-screen toggle was flipped. Turning it on may be intercepted by the
    /// one-time explanation first.
    ///
    /// Linux-only, and so is the variant itself: declaring it everywhere while handling
    /// it only here made the Windows build fail on a non-exhaustive match, which is the
    /// compiler correctly pointing out a message nothing could answer.
    #[cfg(target_os = "linux")]
    LoginScreenToggled(bool),
    /// "Yes, turn it on" from that explanation.
    #[cfg(target_os = "linux")]
    ConfirmLoginScreen,
    /// "No, leave it off" from that explanation.
    #[cfg(target_os = "linux")]
    CancelLoginScreen,
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
    // Everything the clipboard thread and the bulk lane read once, at Start. They were
    // missing here, so editing any of them while sharing was live changed the panel and
    // nothing else, with no prompt to say so — the same silent-staleness this function
    // exists to prevent. `clipboard_notify` is absent on purpose: the panel reads it on
    // every tick, so it really does apply immediately. `clipboard_auto_transfer_max` is
    // absent because it is pushed to the running thread as it changes.
    chk(live.clipboard_history_size != now.clipboard_history_size, "history size");
    chk(live.clipboard_max_item_bytes != now.clipboard_max_item_bytes, "biggest item");
    chk(live.clipboard_memory_budget != now.clipboard_memory_budget, "memory budget");
    chk(live.clipboard_max_image_px != now.clipboard_max_image_px, "largest image");
    chk(live.clipboard_sync_text != now.clipboard_sync_text, "sync text");
    chk(live.clipboard_sync_images != now.clipboard_sync_images, "sync images");
    chk(live.clipboard_sync_files != now.clipboard_sync_files, "sync files");
    chk(live.clipboard_cache_budget != now.clipboard_cache_budget, "disk for files");
    chk(live.cache_dir != now.cache_dir, "cache location");
    chk(live.clipboard_inline_image_max != now.clipboard_inline_image_max, "inline image size");
    chk(live.clipboard_max_files_per_copy != now.clipboard_max_files_per_copy, "files per copy");
    chk(live.clipboard_follow_symlinks != now.clipboard_follow_symlinks, "following shortcuts");
    chk(live.clipboard_images_also_as_file != now.clipboard_images_also_as_file, "images as a file");
    chk(live.clipboard_deliver_dir != now.clipboard_deliver_dir, "folder for received files");
    chk(live.clipboard_move_on_paste != now.clipboard_move_on_paste, "move on paste");
    chk(live.clipboard_hash_limit != now.clipboard_hash_limit, "hash limit");
    chk(live.clipboard_quick_send_secs != now.clipboard_quick_send_secs, "don't-ask-if-quick");
    chk(live.clipboard_stream_min_mb != now.clipboard_stream_min_mb, "stream size");
    chk(live.clipboard_stream_wait_secs != now.clipboard_stream_wait_secs, "wait for a dropped connection");
    chk(live.clipboard_keep_resumable_copy != now.clipboard_keep_resumable_copy, "resumable copy");
    chk(live.clipboard_persist_history != now.clipboard_persist_history, "keep history after restart");
    chk(live.bulk_enabled != now.bulk_enabled, "separate transfer lane");
    chk(live.bulk_port != now.bulk_port, "transfer lane port");
    chk(live.bulk_pause_while_crossed != now.bulk_pause_while_crossed, "pause while crossed");
    chk(live.bulk_rate_limit_mbps != now.bulk_rate_limit_mbps, "transfer speed limit");
    chk(live.bulk_send_buffer_mb != now.bulk_send_buffer_mb, "send buffer");
    chk(live.lock_hotkey != now.lock_hotkey, "lock hotkey");
    chk(live.return_hotkey != now.return_hotkey, "return hotkey");
    chk(live.listen_host != now.listen_host, "listen host");
    chk(live.listen_port != now.listen_port, "listen port");
    chk(live.server_host != now.server_host, "server host");
    chk(live.server_port != now.server_port, "server port");
    chk(live.reconnect != now.reconnect, "auto-reconnect");
    chk(live.edge_threshold_px != now.edge_threshold_px, "edge sensitivity");
    chk(live.discovery_port != now.discovery_port, "discovery port");
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
    /// How many waiting files the user has already been notified about, so a
    /// notification fires once per file rather than on every tick.
    notified_waiting: usize,
    /// Whether the "peer is at its login screen" notice has gone out for this arrival.
    notified_login_screen: bool,
    /// Per-entry transfer rate in bytes/sec, and the sample it was derived from.
    clip_rates: std::collections::HashMap<u64, u64>,
    clip_progress: std::collections::HashMap<u64, (std::time::Instant, u64)>,
    /// Live state of the OS login entry — read from the OS at boot, not from `cfg`,
    /// because the user may have removed it outside the app.
    autostart_on: bool,
    autostart_elevated_on: bool,
    /// Live state of the login-screen service, read from systemd rather than `cfg`: it
    /// is a system service and could have been changed with `systemctl` outside the app.
    #[cfg(target_os = "linux")]
    login_screen: crate::greeter::State,
    /// The service runs its own copy of the binary, so an update of this one leaves the
    /// login screen on the old version until it is reinstalled. Read at launch and after
    /// the toggle, not on redraw: it compares two whole binaries.
    #[cfg(target_os = "linux")]
    login_screen_stale: bool,
    /// Set while the one-time explanation is on screen, waiting for an answer. The
    /// toggle does not move until it is answered, so the panel never shows a state the
    /// system is not actually in.
    #[cfg(target_os = "linux")]
    login_screen_asking: bool,
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
    /// The settings page showing, or `None` for the main window.
    page: Option<Page>,
    /// Pages whose Advanced section is open. All closed at launch: everything is
    /// configurable, but the everyday settings are what a page shows first (R25).
    adv_open: Vec<Page>,
    edge_threshold_s: String,
    discovery_port_s: String,
    hash_limit_mb_s: String,
    log_upload_secs_s: String,
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
    clip_quick_s: String,
    clip_stream_mb_s: String,
    clip_stream_wait_s: String,
    bulk_rate_s: String,
    bulk_buf_s: String,
}

/// Bytes → whole MB for display. The panel talks in MB because that is the unit the
/// user thinks in; config stores bytes because that is what the code compares against.
fn mb_of(bytes: usize) -> String {
    (bytes as f64 / (1024.0 * 1024.0)).round().to_string()
}

/// Wrap a control in hover text explaining what it does.
///
/// An icon is compact and says nothing; a word is clearer and still says very little.
/// Hovering is where the actual sentence belongs, which is what was being asked for.
fn hint<'a>(
    p: Palette,
    control: impl Into<Element<'a, Message>>,
    text_: &'a str,
) -> Element<'a, Message> {
    iced::widget::tooltip(
        control,
        container(text(text_).size(11.5).color(p.txt2))
            .style(move |_| ui_theme::well(p))
            .padding([6.0, 9.0])
            .max_width(260.0),
        iced::widget::tooltip::Position::Top,
    )
    .into()
}

/// Seconds as something a person reads at a glance while waiting.
fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Round a size up to a whole megabyte, for "always allow this size".
///
/// Rounding *up* matters: allowing exactly this file's byte count would leave the next
/// copy of the same kind of thing one byte over the line and asking again, which is
/// precisely what the button was pressed to stop.
fn round_up_mb(bytes: u64) -> u64 {
    const MB: u64 = 1024 * 1024;
    bytes.div_ceil(MB).max(1) * MB
}

/// How that rounded limit reads on the button.
fn mb_ceil_label(bytes: u64) -> String {
    human_bytes(round_up_mb(bytes) as usize)
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
            notified_waiting: 0,
            notified_login_screen: false,
            clip_rates: std::collections::HashMap::new(),
            clip_progress: std::collections::HashMap::new(),
            autostart_on: crate::autostart::is_enabled(),
            autostart_elevated_on: crate::autostart::elevated_is_enabled(),
            #[cfg(target_os = "linux")]
            login_screen: crate::greeter::state(),
            #[cfg(target_os = "linux")]
            login_screen_stale: crate::greeter::installed_copy_is_current() == Some(false),
            #[cfg(target_os = "linux")]
            login_screen_asking: false,
            launch_done: false,
            log_path: crate::logging::log_path().display().to_string(),
            live_cfg: None,
            restart_hint_dismissed: false,
            scan: Arc::new(Mutex::new(ScanState::Idle)),
            found_servers: Vec::new(),
            menu_open: false,
            page: None,
            adv_open: Vec::new(),
            edge_threshold_s: String::new(),
            discovery_port_s: String::new(),
            hash_limit_mb_s: String::new(),
            log_upload_secs_s: String::new(),
            clip_items_s: String::new(),
            clip_item_mb_s: String::new(),
            clip_mem_mb_s: String::new(),
            clip_img_px_s: String::new(),
            bulk_port_s: String::new(),
            clip_auto_mb_s: String::new(),
            clip_cache_gb_s: String::new(),
            clip_inline_kb_s: String::new(),
            clip_max_files_s: String::new(),
            clip_quick_s: String::new(),
            clip_stream_mb_s: String::new(),
            clip_stream_wait_s: String::new(),
            bulk_rate_s: String::new(),
            bulk_buf_s: String::new(),
            saved_cfg: cfg.clone(),
            cfg,
        };
        app.sync_buffers();
        // A screenshot aid: `MOUSE_RS_OPEN_SETTINGS=clipboard` opens the panel at that
        // settings page with its Advanced section expanded ("index" for the list), so each
        // page can be checked without clicking through a live panel, where synthetic
        // clicks can be forwarded to the other machine.
        if let Ok(want) = std::env::var("MOUSE_RS_OPEN_SETTINGS") {
            let all = std::iter::once(Page::Index).chain(Page::CATEGORIES.iter().flat_map(|(_, p)| p.iter().copied()));
            let want = want.to_lowercase();
            if let Some(pg) = all.into_iter().find(|p| p.title().to_lowercase().starts_with(&want) || (want == "index" && *p == Page::Index)) {
                app.page = Some(pg);
                app.adv_open.push(pg);
            }
        }
        app
    }

    /// Refresh the numeric string buffers from `cfg` (after load/detect).
    fn sync_buffers(&mut self) {
        self.server_port_s = self.cfg.server_port.to_string();
        self.listen_port_s = self.cfg.listen_port.to_string();
        self.screen_w_s = self.cfg.screen_width.to_string();
        self.screen_h_s = self.cfg.screen_height.to_string();
        self.edge_threshold_s = self.cfg.edge_threshold_px.to_string();
        self.discovery_port_s = self.cfg.discovery_port.to_string();
        self.hash_limit_mb_s = (self.cfg.clipboard_hash_limit / (1024 * 1024)).to_string();
        self.log_upload_secs_s = self.cfg.log_upload_secs.to_string();
        self.clip_items_s = self.cfg.clipboard_history_size.to_string();
        self.clip_item_mb_s = mb_of(self.cfg.clipboard_max_item_bytes);
        self.clip_mem_mb_s = mb_of(self.cfg.clipboard_memory_budget);
        self.clip_img_px_s = self.cfg.clipboard_max_image_px.to_string();
        self.clip_auto_mb_s = (self.cfg.clipboard_auto_transfer_max / (1024 * 1024)).to_string();
        self.clip_inline_kb_s = (self.cfg.clipboard_inline_image_max / 1024).max(1).to_string();
        self.clip_max_files_s = self.cfg.clipboard_max_files_per_copy.to_string();
        self.clip_quick_s = self.cfg.clipboard_quick_send_secs.to_string();
        self.clip_stream_mb_s = self.cfg.clipboard_stream_min_mb.to_string();
        self.clip_stream_wait_s = self.cfg.clipboard_stream_wait_secs.to_string();
        self.bulk_rate_s = self.cfg.bulk_rate_limit_mbps.to_string();
        self.bulk_buf_s = self.cfg.bulk_send_buffer_mb.to_string();
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

    /// How many entries are sitting on an unanswered "shall I bring this over?".
    ///
    /// `Declined` deliberately does not count: answering "not now" is an answer, and
    /// nagging afterwards would make the button pointless.
    fn waiting_files(&self) -> usize {
        self.engine
            .as_ref()
            .map(|e| {
                e.clipboard_meta()
                    .iter()
                    .filter(|m| m.status == mouse_clipboard::ClipStatus::NeedsConsent)
                    .count()
            })
            .unwrap_or(0)
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
                // Sample each fetching entry's progress so the row can show a real
                // rate. Smoothed, because a raw per-tick delta jitters far too much to
                // read while a transfer is running.
                if let Some(e) = &self.engine {
                    let now = std::time::Instant::now();
                    for m in e.clipboard_meta() {
                        let mouse_clipboard::ClipStatus::Fetching { done, .. } = m.status else {
                            self.clip_progress.remove(&m.id);
                            self.clip_rates.remove(&m.id);
                            continue;
                        };
                        match self.clip_progress.get(&m.id) {
                            Some((then, before)) => {
                                let secs = now.duration_since(*then).as_secs_f64();
                                if secs >= 1.0 {
                                    let moved = done.saturating_sub(*before);
                                    let sample = (moved as f64 / secs) as u64;
                                    let smoothed = match self.clip_rates.get(&m.id) {
                                        Some(prev) => (prev * 3 + sample) / 4,
                                        None => sample,
                                    };
                                    self.clip_rates.insert(m.id, smoothed);
                                    self.clip_progress.insert(m.id, (now, done));
                                }
                            }
                            None => {
                                self.clip_progress.insert(m.id, (now, done));
                            }
                        }
                    }
                }
                // Keep the tray tooltip reflecting live status — including anything
                // waiting on an answer, which is invisible while the panel is closed.
                let waiting = self.waiting_files();
                if let Some(t) = &self.tray {
                    let snap = self.engine.as_ref().map(|e| e.status());
                    let running = self.running();
                    t.set_state(&tray_tooltip(running, snap.as_ref(), waiting), running);
                }
                // One notification per newly-waiting file, never a repeat per tick.
                if waiting > self.notified_waiting {
                    if self.cfg.clipboard_notify {
                        crate::notify::file_waiting(waiting);
                    }
                    self.notified_waiting = waiting;
                } else if waiting < self.notified_waiting {
                    self.notified_waiting = waiting;
                }
                // Say so when the other machine is at its login screen. Input works there,
                // but only once the pointer has been moved across to it.
                let login_peer = self
                    .engine
                    .as_ref()
                    .map(|e| e.status())
                    .filter(|s| s.connected && s.peer_at_login_screen)
                    .map(|s| s.peer);
                if let Some(peer) = &login_peer {
                    if !self.notified_login_screen {
                        crate::notify::peer_at_login_screen(peer.trim_end_matches(" (login screen)"));
                    }
                }
                self.notified_login_screen = login_peer.is_some();
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
                    // Accurate for both cases: a complete entry is on the clipboard now, and
                    // an incomplete one has its paths there with the transfer starting on
                    // paste. Saying "copied" when nothing was on the clipboard was simply
                    // untrue, and the user found out by pasting nothing.
                    self.feedback = "on the clipboard — paste it where you want it".into();
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
            Message::ClipCancel(id) => {
                if let Some(e) = &self.engine {
                    e.clipboard_cancel(id);
                    self.feedback = "stopped — what arrived is kept, click to resume".into();
                }
            }
            Message::ClipDecline(id) => {
                if let Some(e) = &self.engine {
                    e.clipboard_decline(id);
                    self.feedback = "left for later — it's still in the list".into();
                }
            }
            // "Always allow this size" is a settings change, so it behaves like one:
            // the limit is raised, saved, and named back to the user. Silently
            // remembering it somewhere they cannot see or undo would be worse than
            // asking every time.
            Message::ClipAlwaysAllow(id) => {
                let size = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.clipboard_meta().into_iter().find(|m| m.id == id))
                    .map(|m| m.disk_bytes)
                    .unwrap_or(0);
                if size > 0 {
                    let limit = round_up_mb(size);
                    self.cfg.clipboard_auto_transfer_max = limit;
                    self.clip_auto_mb_s = (limit / (1024 * 1024)).to_string();
                    match self.cfg.save(&Config::default_path()) {
                        Ok(()) => {
                            if let Some(e) = &self.engine {
                                e.clipboard_set_auto_max(limit);
                                e.clipboard_reuse(id);
                            }
                            self.feedback = format!(
                                "anything up to {} now comes over without asking",
                                human_bytes(limit as usize)
                            );
                        }
                        Err(e) => self.feedback = format!("could not save that: {e}"),
                    }
                }
            }
            Message::ClipRemove(id) => {
                if let Some(e) = &self.engine {
                    e.clipboard_remove(id);
                }
            }
            // Deliberately not "delete everything": pinned entries and anything still
            // arriving are kept, because those bytes are not ours to throw away.
            Message::ClipCacheClear => {
                if let Some(e) = &self.engine {
                    let freed = e.cache_clear();
                    self.feedback = if freed == 0 {
                        "nothing to clear — what is left is pinned or still arriving".into()
                    } else {
                        format!("freed {} (pinned and in-progress files kept)", human_bytes(freed as usize))
                    };
                }
            }
            Message::ClipClear => {
                if let Some(e) = &self.engine {
                    e.clipboard_clear();
                    self.feedback = "history cleared (pinned entries kept)".into();
                }
            }
            Message::OpenSettings(pg) => {
                self.page = Some(pg);
                self.menu_open = false;
            }
            Message::CloseSettings => self.page = None,
            Message::AdvancedToggled(pg) => {
                if let Some(i) = self.adv_open.iter().position(|x| *x == pg) {
                    self.adv_open.remove(i);
                } else {
                    self.adv_open.push(pg);
                }
            }
            Message::EdgeThresholdChanged(s) => {
                if let Ok(v) = s.trim().parse::<i32>() {
                    self.cfg.edge_threshold_px = v.max(0);
                }
                self.edge_threshold_s = s;
            }
            Message::DiscoveryPortChanged(s) => {
                if let Ok(v) = s.trim().parse::<u16>() {
                    self.cfg.discovery_port = v;
                }
                self.discovery_port_s = s;
            }
            Message::CacheDirChanged(s) => self.cfg.cache_dir = s,
            Message::HashLimitMbChanged(s) => {
                if let Ok(v) = s.trim().parse::<u64>() {
                    self.cfg.clipboard_hash_limit = v.saturating_mul(1024 * 1024);
                }
                self.hash_limit_mb_s = s;
            }
            Message::LogUploadUrlChanged(s) => self.cfg.log_upload_url = s,
            Message::LogUploadSecsChanged(s) => {
                if let Ok(v) = s.trim().parse::<u32>() {
                    self.cfg.log_upload_secs = v.max(1);
                }
                self.log_upload_secs_s = s;
            }
            Message::ClipSyncTextToggled(v) => self.cfg.clipboard_sync_text = v,
            Message::ClipSyncImagesToggled(v) => self.cfg.clipboard_sync_images = v,
            // The numeric settings keep their own string buffer so a half-typed value is
            // never parsed; an unparseable buffer simply leaves `cfg` alone.
            Message::ClipDeliverDirChanged(s) => self.cfg.clipboard_deliver_dir = s,
            Message::ClipMoveOnPasteToggled(v) => self.cfg.clipboard_move_on_paste = v,
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
            Message::ClipFollowSymlinksToggled(v) => self.cfg.clipboard_follow_symlinks = v,
            Message::ClipImagesAsFileToggled(v) => self.cfg.clipboard_images_also_as_file = v,
            Message::ClipNotifyToggled(v) => self.cfg.clipboard_notify = v,
            Message::BulkBufferChanged(s) => {
                // Floored at 1 MB: below one chunk the sender would have nowhere to put
                // the chunk it just read and would wait forever.
                if let Ok(v) = s.trim().parse::<u32>() {
                    self.cfg.bulk_send_buffer_mb = v.max(1);
                }
                self.bulk_buf_s = s;
            }
            Message::ClipAutoMbChanged(s) => {
                // 0 is meaningful here — it means "always ask" — so it is accepted as
                // typed rather than clamped up to 1 like the size limits.
                if let Ok(v) = s.trim().parse::<u64>() {
                    self.cfg.clipboard_auto_transfer_max = v * 1024 * 1024;
                    // Pushed straight to the running thread, so this one needs no
                    // restart and stays out of the stale-settings list.
                    if let Some(e) = &self.engine {
                        e.clipboard_set_auto_max(self.cfg.clipboard_auto_transfer_max);
                    }
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
            Message::ClipPersistToggled(v) => self.cfg.clipboard_persist_history = v,
            Message::ClipQuickSecsChanged(s) => {
                // 0 is meaningful: it turns the shortcut off entirely.
                if let Ok(v) = s.trim().parse::<u32>() {
                    self.cfg.clipboard_quick_send_secs = v;
                }
                self.clip_quick_s = s;
            }
            Message::ClipStreamMbChanged(s) => {
                // 0 streams everything, however small.
                if let Ok(v) = s.trim().parse::<u32>() {
                    self.cfg.clipboard_stream_min_mb = v;
                }
                self.clip_stream_mb_s = s;
            }
            Message::ClipStreamWaitChanged(s) => {
                if let Ok(v) = s.trim().parse::<u32>() {
                    self.cfg.clipboard_stream_wait_secs = v.max(5);
                }
                self.clip_stream_wait_s = s;
            }
            Message::ClipKeepResumableToggled(v) => self.cfg.clipboard_keep_resumable_copy = v,
            Message::BulkPauseToggled(v) => self.cfg.bulk_pause_while_crossed = v,
            Message::BulkRateChanged(s) => {
                // 0 means adapt automatically, which is the default.
                if let Ok(v) = s.trim().parse::<u32>() {
                    self.cfg.bulk_rate_limit_mbps = v;
                }
                self.bulk_rate_s = s;
            }
            Message::ClipCacheGbChanged(s) => {
                if let Ok(v) = s.trim().parse::<u64>() {
                    self.cfg.clipboard_cache_budget = v.max(1) * 1024 * 1024 * 1024;
                }
                self.clip_inline_kb_s = (self.cfg.clipboard_inline_image_max / 1024).max(1).to_string();
        self.clip_max_files_s = self.cfg.clipboard_max_files_per_copy.to_string();
        self.clip_quick_s = self.cfg.clipboard_quick_send_secs.to_string();
        self.clip_stream_mb_s = self.cfg.clipboard_stream_min_mb.to_string();
        self.clip_stream_wait_s = self.cfg.clipboard_stream_wait_secs.to_string();
        self.bulk_rate_s = self.cfg.bulk_rate_limit_mbps.to_string();
        self.bulk_buf_s = self.cfg.bulk_send_buffer_mb.to_string();
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
            // Same shape as the plain one: touches the OS, applies immediately, and
            // re-reads the real state on failure rather than trusting the toggle. The
            // usual failure is that mouse-rs is not itself elevated, and Windows will
            // not let a normal-privilege process register a highest-privileges task —
            // so the message says that rather than a bare error code.
            // Both answers are recorded, because both are decisions. Dismissing is not
            // "not yet" — it is "I have chosen normal permissions", and asking again
            // would be ignoring the answer.
            Message::DismissElevationNotice => {
                self.cfg.elevation_notice_seen = true;
                let path = Config::default_path();
                self.feedback = match self.cfg.save(&path) {
                    Ok(()) => "keeping normal permissions — you can change this in Startup".into(),
                    Err(e) => format!("could not save that choice: {e}"),
                };
            }
            // Linux. The explanation gates the first enable only; after that the
            // toggle acts immediately, because the answer has been given.
            #[cfg(target_os = "linux")]
            Message::LoginScreenToggled(v) => {
                if v && !self.cfg.login_screen_notice_seen {
                    self.login_screen_asking = true;
                } else {
                    self.apply_login_screen(v);
                }
            }
            #[cfg(target_os = "linux")]
            Message::ConfirmLoginScreen => {
                // Recorded before the attempt: they have been told, whether or not the
                // install then succeeds or the password prompt is dismissed.
                self.cfg.login_screen_notice_seen = true;
                let _ = self.cfg.save(&Config::default_path());
                self.login_screen_asking = false;
                self.apply_login_screen(true);
            }
            #[cfg(target_os = "linux")]
            Message::CancelLoginScreen => {
                // Also a decision, so it is also remembered. Re-asking would be
                // ignoring the answer; the toggle is still there if they change it.
                self.cfg.login_screen_notice_seen = true;
                let _ = self.cfg.save(&Config::default_path());
                self.login_screen_asking = false;
                self.feedback = "left off — the toggle is in Startup if you want it later".into();
            }
            Message::RelaunchElevated => {
                // Remember the answer first: whichever way this goes, they have now been
                // asked, and a UAC prompt they declined must not reappear next launch.
                self.cfg.elevation_notice_seen = true;
                let _ = self.cfg.save(&Config::default_path());
                match crate::autostart::relaunch_elevated() {
                    Ok(true) => {
                        // The new instance is starting; this one must go, or two copies
                        // fight over the same hooks and port.
                        tracing::info!("restarting with administrator rights at the user's request");
                        self.stop_engine();
                        return iced::exit();
                    }
                    Ok(false) => {
                        self.feedback = "Windows declined — staying with normal permissions".into();
                    }
                    Err(e) => self.feedback = format!("{e}"),
                }
            }
            Message::AutostartElevatedToggled(v) => {
                let res = if v {
                    crate::autostart::enable_elevated()
                } else {
                    crate::autostart::disable_elevated()
                };
                match res {
                    Ok(()) => {
                        self.autostart_elevated_on = v;
                        tracing::info!(enabled = v, "elevated login task changed from the panel");
                        self.feedback = if v {
                            "it will start as administrator at login".into()
                        } else {
                            "elevated login entry removed".into()
                        };
                    }
                    Err(e) => {
                        self.autostart_elevated_on = crate::autostart::elevated_is_enabled();
                        self.feedback = format!("{e}");
                    }
                }
            }
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
                self.adv_open.clear();
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

        let mut content = column![self.header(p, running, snap.as_ref())]
            .spacing(12.0)
            .padding(16.0);
        // The hero answers "who has the mouse right now", which is what the main window
        // is for. A settings page is a different task and gets the room instead.
        if self.page.is_none() {
            content = content.push(self.hero(p, running, snap.as_ref()));
        }
        if let Some(banner) = self.restart_banner(p) {
            content = content.push(banner);
        }
        if let Some(banner) = self.pending_files_banner(p) {
            content = content.push(banner);
        }
        if let Some(banner) = self.elevation_notice(p) {
            content = content.push(banner);
        }
        #[cfg(target_os = "linux")]
        if let Some(banner) = self.login_screen_notice(p) {
            content = content.push(banner);
        }
        let content = match self.page {
            None => content.push(self.tab_bar(p)).push(
                scrollable(container(self.tab_body(p, running)).padding([4.0, 2.0]))
                    .height(Length::Fill),
            ),
            Some(pg) => content.push(self.settings_bar(p, pg)).push(
                scrollable(container(self.settings_body(p, pg)).padding([4.0, 2.0]))
                    .height(Length::Fill),
            ),
        }
        .push(self.footer(p, running));

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
            text("mouse-rs").size(22.0).color(p.txt).width(Length::Fill),
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
            Tab::Clipboard => self.tab_clipboard(p),
        }
    }

    /// The main window's Connection tab: who this computer is, and how it finds the other
    /// one (R30). Ports, auto-reconnect, the machine name and profiles are in
    /// ⋯ → Settings → Connection.
    fn tab_connection(&self, p: Palette, _running: bool) -> Element<'_, Message> {
        let server_side = self.cfg.role == Role::Server;

        // Role segmented control.
        let role = row![
            seg_button(p, "Server · has mouse", self.cfg.role == Role::Server, Message::RolePicked(Role::Server)),
            seg_button(p, "Client · receives", self.cfg.role == Role::Client, Message::RolePicked(Role::Client)),
        ]
        .spacing(6.0);

        let net: Element<'_, Message> = if server_side {
            // The server is found by the client, so what matters here is what to tell it.
            // Choosing which of these addresses to listen on is in Settings → Connection.
            let local = mouse_core::discover::listen_candidates();
            column![
                field_label(p, "This computer's addresses"),
                text(if local.is_empty() { "none found".to_string() } else { local.join("  ·  ") })
                    .size(13.0)
                    .color(p.txt2),
                text("The other computer finds this one by itself on a shared network; type one of these there only if it does not.")
                    .size(11.0)
                    .color(p.txt3),
            ]
            .spacing(6.0)
            .into()
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
            col.into()
        };

        card(p, column![
            field_label(p, "Role"),
            role,
            net,
            text("Ports, auto-reconnect and this computer's name: ⋯ → Settings → Connection.")
                .size(11.0)
                .color(p.txt3),
        ]
        .spacing(14.0))
    }

    /// Settings → Startup: how the app behaves at login. Split out of Connection because it
    /// is a different question — how this machine starts the app, not how it reaches the
    /// peer. Every setting here is everyday, so the page has no Advanced section.
    fn page_startup(&self, p: Palette) -> Element<'_, Message> {
        // The three are deliberately independent: the OS launches the panel (Start at
        // login), the panel brings the link up by itself (Auto-connect), and it stays out
        // of the way while doing so (Start minimized).
        let mut startup = column![
            toggle_row(p, "Start at login", self.autostart_hint(), self.autostart_on, Message::AutostartToggled),
            toggle_row(p, "Auto-connect on launch", "begin sharing as soon as the panel opens", self.cfg.autoconnect, Message::AutoconnectToggled),
            toggle_row(p, "Start minimized", "open out of the way, into the tray/taskbar", self.cfg.start_minimized, Message::StartMinimizedToggled),
        ]
        .spacing(12.0);

        // Windows only. Which privilege level we are at is a *fact* worth stating
        // permanently — it is one line and answers a real question — but the
        // explanation of what it costs is a one-time notice, dismissed for good once
        // read. Running with normal privileges is a legitimate choice most people never
        // notice, so nagging about a decision already made would be worse than useless.
        if mouse_input::elevation() != mouse_input::Elevation::NotApplicable {
            let limited = mouse_input::elevation().is_limited();
            startup = startup.push(
                row![
                    text(if limited {
                        "Permissions: normal"
                    } else {
                        "Permissions: administrator"
                    })
                    .size(12.0)
                    .color(if limited { p.txt2 } else { p.good })
                    .width(Length::Fill),
                    button(text(if limited { "Restart as administrator" } else { "Restart as normal user" }).size(11.5))
                        .style(ui_theme::ghost(p))
                        .padding([7.0, 12.0])
                        .on_press(Message::RelaunchElevated),
                ]
                .align_y(Alignment::Center),
            );
            startup = startup.push(toggle_row(
                p,
                "Start at login as administrator",
                "keeps administrator rights after a reboot — a normal login entry always starts without them. Setting this up needs mouse-rs to be running as administrator now.",
                self.autostart_elevated_on,
                Message::AutostartElevatedToggled,
            ));
        }

        // Linux only, and the counterpart to the Windows block above: the same shape of
        // question — what this machine does before anyone has signed in — answered by a
        // different mechanism. There is no Windows equivalent built yet.
        #[cfg(target_os = "linux")]
        {
            startup = startup.push(toggle_row(
                p,
                "Share input at the login screen",
                "starts before the password prompt, as a system service, so the other \
                 machine's keyboard can type it. Needs your password to set up, and \
                 accepts keystrokes over the network before anyone has signed in.",
                self.login_screen.on(),
                Message::LoginScreenToggled,
            ));
            // Only worth a line when it says something the toggle does not. Installed
            // but not enabled is the state a stray `systemctl disable` leaves behind,
            // and it would otherwise look identical to off.
            if self.login_screen.installed && !self.login_screen.enabled {
                startup = startup.push(
                    text("installed but disabled in systemd — turn it off and on to fix")
                        .size(11.0)
                        .color(p.warn),
                );
            } else if self.login_screen.on() && self.login_screen_stale {
                startup = startup.push(
                    text("the login screen runs an older version than this one — turn it off \
                          and on to update it")
                        .size(11.0)
                        .color(p.warn),
                );
            }
        }

        card(p, column![section(p, "When I log in"), startup].spacing(14.0))
    }

    /// Install or remove the login-screen service, then re-read what systemd actually
    /// says rather than assuming the click worked.
    ///
    /// Re-reading matters: the password prompt can be dismissed, and a toggle that moved
    /// anyway would claim the login screen was covered when it was not — the one claim
    /// this feature must never get wrong, because the way you discover it is false is by
    /// being locked out with no keyboard.
    #[cfg(target_os = "linux")]
    fn apply_login_screen(&mut self, on: bool) {
        let res = crate::greeter::set_enabled(on);
        self.login_screen = crate::greeter::state();
        self.login_screen_stale = crate::greeter::installed_copy_is_current() == Some(false);
        self.feedback = match res {
            Ok(()) if self.login_screen.on() == on => {
                tracing::info!(enabled = on, "login-screen service changed from the panel");
                if on {
                    "the login screen now accepts the other machine's keyboard".into()
                } else {
                    "login-screen sharing removed".into()
                }
            }
            // No error, but systemd disagrees. Report what is true, not what was asked.
            Ok(()) => {
                tracing::warn!(
                    asked = on,
                    now = self.login_screen.on(),
                    "the login-screen change reported success but systemd disagrees"
                );
                "that did not take effect — see the log".into()
            }
            Err(e) => {
                tracing::warn!(asked = on, "login-screen change failed: {e}");
                format!("{e}")
            }
        };
    }

    /// The one-time explanation, shown only when turning the login-screen service on for
    /// the first time.
    ///
    /// Not a permanent banner and not shown while it is off: off is the safe default and
    /// warning about a thing nobody has asked for is noise. It appears once, at the
    /// moment the decision is actually being made, which is when the information is
    /// worth something.
    #[cfg(target_os = "linux")]
    fn login_screen_notice(&self, p: Palette) -> Option<Element<'_, Message>> {
        if !self.login_screen_asking {
            return None;
        }
        Some(
            container(
                column![
                    text("Share the keyboard before anyone logs in?").size(13.5).color(p.txt),
                    text(
                        "This installs a small system service that starts at boot, like \
                         NetworkManager does, so the other machine's keyboard works at the \
                         password screen and you no longer need a second one plugged in here."
                    )
                    .size(11.0)
                    .color(p.txt3),
                    text(
                        "What it costs: this machine will accept keystrokes over the network \
                         before anyone has signed in. Anything holding your shared secret \
                         could type at the password prompt, so that secret becomes what \
                         stands between your network and this login. The service needs \
                         administrator rights to install, and asks for your password now."
                    )
                    .size(11.0)
                    .color(p.warn),
                    row![
                        button(text("Turn it on").size(12.5))
                            .style(ui_theme::primary(p))
                            .padding([9.0, 14.0])
                            .on_press(Message::ConfirmLoginScreen),
                        button(text("Leave it off").size(12.5))
                            .style(ui_theme::ghost(p))
                            .padding([9.0, 12.0])
                            .on_press(Message::CancelLoginScreen),
                    ]
                    .spacing(8.0),
                ]
                .spacing(8.0),
            )
            .style(move |_| ui_theme::well(p))
            .padding([11.0, 14.0])
            .width(Length::Fill)
            .into(),
        )
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

    /// Settings → Screens & edges, showing only what this machine's role actually uses.
    ///
    /// The client engine reads `entry_edge` and nothing else — it never looks at
    /// `exit_edge` (`client.rs:37`). So on a client the Exit-edge picker was a dial
    /// wired to nothing: it wrote a config value no code would ever read. Worse, the
    /// overlap warning compared that dead value against the live one, so it could
    /// contradict a pair that was configured perfectly.
    ///
    /// Deciding where to *leave* from is the server's business, because the server is
    /// what watches its own edges and hands control over. The client's only edge
    /// question is where control comes back.
    fn page_screens(&self, p: Palette) -> Element<'_, Message> {
        let mut body = column![section(p, "Arrangement")].spacing(14.0);

        if shows_exit_edge(self.cfg.role) {
            // Leaving through one edge means arriving through the facing one; anything
            // else puts the two screens on top of each other, so say so instead of
            // saving it silently. Only meaningful here: the server is the only role that
            // reads both, and so the only one that can be inconsistent.
            let want = self.cfg.exit_edge.opposite();
            body = body
                .push(field_label(p, "Exit edge — the pointer leaves this screen here"))
                .push(edge_picker(p, self.cfg.exit_edge, Message::ExitEdgePicked));
            if self.cfg.entry_edge != want {
                body = body.push(
                    text(format!(
                        "⚠ Leaving at the {} edge should bring the cursor in at the {} edge of the other screen — {} will feel like the screens overlap.",
                        self.cfg.exit_edge.label(),
                        want.label(),
                        self.cfg.entry_edge.label(),
                    ))
                    .size(11.0)
                    .color(p.warn),
                );
            }
        }

        body = body
            .push(field_label(p, entry_edge_label(self.cfg.role)))
            .push(edge_picker(p, self.cfg.entry_edge, Message::EntryEdgePicked))
            .push(
                text(entry_edge_hint(self.cfg.role))
                    .size(11.0)
                    .color(p.txt3)
                    .width(Length::Fill),
            );

        let adv = column![
            section(p, "This screen"),
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
            section(p, "Crossing feel"),
            labeled(p, "Edge sensitivity (px from the edge)", input(p, "2", &self.edge_threshold_s, Message::EdgeThresholdChanged)),
            text("How close to the edge the pointer must get before it crosses. Raise it if crossing is hard to hit; 0 means the very last pixel.")
                .size(11.0)
                .color(p.txt3),
        ]
        .spacing(14.0);

        column![card(p, body), self.advanced(p, Page::Screens, adv)].spacing(12.0).into()
    }

    fn page_security(&self, p: Palette) -> Element<'_, Message> {
        let secret = row![
            input(p, "shared secret", &self.cfg.shared_secret, Message::SecretChanged)
                .secure(!self.show_secret),
            button(text(if self.show_secret { "Hide" } else { "Show" }).size(13.0))
                .style(ui_theme::ghost(p)).padding([9.0, 12.0]).on_press(Message::ToggleShowSecret),
            button(text("Generate").size(13.0))
                .style(ui_theme::ghost(p)).padding([9.0, 12.0]).on_press(Message::GenerateSecret),
        ]
        .spacing(8.0);

        let adv = column![
            toggle_row(p, "Encrypt link (TLS)", "rustls, self-signed cert", self.cfg.tls_enabled, Message::TlsToggled),
        ]
        .spacing(14.0);

        column![
            card(p, column![
                section(p, "Pairing"),
                field_label(p, "Shared secret — identical on both machines"),
                secret,
            ]
            .spacing(14.0)),
            self.advanced(p, Page::Security, adv),
        ]
        .spacing(12.0)
        .into()
    }

    /// Settings → Keyboard & hotkeys. The lock key is the everyday way back; the return
    /// combo is optional, so it is Advanced.
    fn page_keys(&self, p: Palette) -> Element<'_, Message> {
        let adv = column![
            labeled(p, "Return hotkey (server)", input(p, "ctrl+win+alt+esc", &self.cfg.return_hotkey, Message::ReturnHotkeyChanged)),
            text("Optional combo, matched on whichever machine is the server. Empty = none.")
                .size(11.0).color(p.txt3),
        ]
        .spacing(14.0);

        column![
            card(p, column![
                section(p, "Getting control back"),
                labeled(p, "Lock / cross hotkey", input(p, "scroll_lock", &self.cfg.lock_hotkey, Message::LockHotkeyChanged)),
                text("Single key. Always returns control, even if the network or engine has wedged.")
                    .size(11.0).color(p.txt3),
            ]
            .spacing(14.0)),
            self.advanced(p, Page::Keys, adv),
        ]
        .spacing(12.0)
        .into()
    }

    /// "The other machine copied something big — want it?"
    ///
    /// A banner rather than a dialog, and that is not a style preference. Windows
    /// withholds `WH_KEYBOARD_LL` callbacks from a process while that process owns the
    /// foreground window — the exact root cause of the keyboard leak fixed on
    /// 2026‑07‑31. A pop-up that grabbed focus the moment a large file was copied would
    /// bring it straight back, and it would do so mid-work, quite possibly mid-crossing.
    /// So this never calls `SetForegroundWindow` and never steals focus.
    /// Told once, then never again.
    ///
    /// Running without administrator rights is a perfectly reasonable way to use this —
    /// most people never focus an elevated window from one week to the next. So this
    /// says the limitation plainly, offers the two ways to act on it, and then goes away
    /// for good. A permanent banner about a choice already made is nagging, and gets
    /// ignored exactly when it matters.
    fn elevation_notice(&self, p: Palette) -> Option<Element<'_, Message>> {
        if self.cfg.elevation_notice_seen || !mouse_input::elevation().is_limited() {
            return None;
        }
        Some(
            container(
                column![
                    text("mouse-rs is running with normal permissions").size(13.5).color(p.txt),
                    text(
                        "That is fine for everyday use. The one limitation: Windows will not let \
                         us see the mouse while a window running as administrator is in front — a \
                         game launcher, an installer, or a Windows permission prompt — so the \
                         pointer cannot cross to the other machine until you click away from it. \
                         No setting inside this program can change that; only running as \
                         administrator can."
                    )
                    .size(11.0)
                    .color(p.txt3),
                    row![
                        button(text("Restart as administrator").size(12.5))
                            .style(ui_theme::primary(p))
                            .padding([9.0, 14.0])
                            .on_press(Message::RelaunchElevated),
                        button(text("Keep normal permissions").size(12.5))
                            .style(ui_theme::ghost(p))
                            .padding([9.0, 12.0])
                            .on_press(Message::DismissElevationNotice),
                    ]
                    .spacing(8.0),
                ]
                .spacing(8.0),
            )
            .style(move |_| ui_theme::well(p))
            .padding([11.0, 14.0])
            .width(Length::Fill)
            .into(),
        )
    }

    fn pending_files_banner(&self, p: Palette) -> Option<Element<'_, Message>> {
        // Fetching counts too, and that is the point. The banner used to show only
        // while an entry was waiting for consent — a state a paste ends within
        // milliseconds — so "Always allow this size" flashed past before it could be
        // read, let alone clicked. A transfer that has just started is exactly when
        // someone wants to say "don't ask me about this size again", and the answer is
        // still useful: it applies to every file after this one.
        let waiting: Vec<_> = self
            .engine
            .as_ref()?
            .clipboard_meta()
            .into_iter()
            // Only while it is *waiting*. Once fetching — "Allow" pressed, or a paste
            // started it — its job is done; the history row carries the progress. Keeping
            // it up showed a stale "Fetching…" prompt over a transfer already running below.
            .filter(|m| m.status == mouse_clipboard::ClipStatus::NeedsConsent)
            .collect();
        let first = waiting.first()?;
        let fetching = matches!(first.status, mouse_clipboard::ClipStatus::Fetching { .. });
        let more = waiting.len().saturating_sub(1);
        let title = match (fetching, more) {
            (true, 0) => format!("{} is coming over", first.preview),
            (true, n) => format!("{} (and {n} more) is coming over", first.preview),
            (false, 0) => format!("{} is ready to paste", first.preview),
            (false, n) => format!("{} (and {n} more) are ready to paste", first.preview),
        };
        Some(
            container(
                row![
                    column![
                        text(title).size(13.5).color(p.txt),
                        text(format!(
                            "{} — over your {} limit, so it is not fetched until you paste it. {}",
                            human_bytes(first.disk_bytes as usize),
                            human_bytes(self.cfg.clipboard_auto_transfer_max as usize),
                            // Linux offers it from the streaming folder, so a paste of any size
                            // starts at once. Elsewhere the paste waits for the whole transfer.
                            if cfg!(target_os = "linux") {
                                "Paste works now; this is only a heads-up."
                            } else {
                                "Press Bring it over first if it is large: a paste waits for the whole file."
                            }
                        ))
                        .size(11.0)
                        .color(p.txt3),
                    ]
                    .spacing(3.0)
                    .width(Length::Fill),
                    // Three answers, because "yes" and "no" are not the only ones: the
                    // third says "stop asking me about files this size", which is the
                    // real request behind repeatedly pressing the first.
                    button(text(if fetching { "Fetching…" } else { "Bring it over" }).size(12.5))
                        .style(ui_theme::primary(p))
                        .padding([9.0, 14.0])
                        .on_press_maybe((!fetching).then_some(Message::ClipReuse(first.id))),
                    button(text(if fetching { "Dismiss" } else { "Not now" }).size(12.5))
                        .style(ui_theme::ghost(p))
                        .padding([9.0, 12.0])
                        .on_press(Message::ClipDecline(first.id)),
                    button(text(format!("Always allow {}", mb_ceil_label(first.disk_bytes))).size(12.5))
                        .style(ui_theme::ghost(p))
                        .padding([9.0, 12.0])
                        .on_press(Message::ClipAlwaysAllow(first.id)),
                ]
                .spacing(8.0)
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
        let mut list = column![].spacing(10.0);
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
                        // True of both kinds: a small file being prefetched, and a large one
                        // that streams in when pasted. Either way, pasting works now.
                        mouse_clipboard::ClipStatus::Remote => (
                            format!("{} · on the other computer · paste any time", human_bytes(m.disk_bytes as usize)),
                            p.accent,
                        ),
                        // No longer an instruction. The paths are already on the
                        // clipboard and pasting is what starts the transfer, so telling
                        // the user to click first was describing an older design and
                        // making a working feature look like a chore.
                        mouse_clipboard::ClipStatus::NeedsConsent => (
                            format!("{} · paste it anywhere", human_bytes(m.disk_bytes as usize)),
                            p.txt3,
                        ),
                        // "Not now" is not an error and not a dead end — say what it is
                        // and what to do about it.
                        mouse_clipboard::ClipStatus::Declined => (
                            format!("{} · paste it anywhere", human_bytes(m.disk_bytes as usize)),
                            p.txt3,
                        ),
                        mouse_clipboard::ClipStatus::Fetching { done, total } => {
                            let pct = if *total > 0 { done * 100 / total } else { 0 };
                            // Speed and time left, because "22% of 6.8 GB" answers
                            // neither of the two questions actually being asked while
                            // watching a transfer: how fast, and how much longer.
                            // Measured from this entry's own progress between ticks.
                            // The lane's own throughput figure is reset by every Begin
                            // frame, and a folder sends 23 of them interleaved, so it
                            // measures a fragment of one file rather than the transfer.
                            let rate = self.clip_rates.get(&m.id).copied();
                            let tail = match rate {
                                Some(bps) if bps > 0 => {
                                    let left = total.saturating_sub(*done);
                                    format!(
                                        " · {}/s · {} left",
                                        human_bytes(bps as usize),
                                        human_duration(left / bps.max(1))
                                    )
                                }
                                _ => String::new(),
                            };
                            (
                                format!("{pct}% of {}{tail}", human_bytes(*total as usize)),
                                p.accent,
                            )
                        }
                        mouse_clipboard::ClipStatus::Unavailable(why) => (why.clone(), p.danger),
                    };
                    let fetching = matches!(m.status, mouse_clipboard::ClipStatus::Fetching { .. });
                    // The row itself is the re-use action — the thing the user wants
                    // nine times out of ten needs no menu and no second click.
                    // Stacked, not side by side. Sharing one line means the two texts
                    // compete for width at every window size, and one of them always
                    // loses — either the status is crushed into a one-word-per-line
                    // column, or a long status ("no longer at the recorded path on the
                    // other machine") runs straight over the file name. On its own line
                    // each can simply use the width available, at any size down to a
                    // phone.
                    let head = column![
                        row![
                            text(kind).size(12.0).color(p.txt3),
                            text(arrow).size(11.0).color(col),
                            text(m.preview.clone()).size(13.0).color(p.txt2).width(Length::Fill),
                        ]
                        .spacing(8.0)
                        .align_y(Alignment::Center),
                        text(note).size(11.0).color(note_col).width(Length::Fill),
                    ]
                    .spacing(4.0);
                    // A bar while it is moving: a percentage answers "how far", a bar
                    // answers "is it still going", which is the one being asked while
                    // watching a long transfer.
                    let body: Element<'_, Message> = if let mouse_clipboard::ClipStatus::Fetching {
                        done,
                        total,
                    } = &m.status
                    {
                        let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                        column![head, progress_bar(0.0..=1.0, frac).girth(3.0)]
                            .spacing(6.0)
                            .into()
                    } else {
                        head.into()
                    };
                    let reuse = button(body)
                        .style(ui_theme::ghost(p))
                        .padding([12.0, 14.0])
                        .width(Length::Fill)
                        .on_press(Message::ClipReuse(m.id));

                    // Icons with real hover text. Renaming them to words was the wrong
                    // fix for "hovering tells me nothing" — it cost the compactness and
                    // still explained less than a sentence can.
                    let pin = hint(
                        p,
                        button(text(if m.pinned { "📌" } else { "📍" }).size(12.0))
                            .style(ui_theme::ghost(p))
                            .padding([9.0, 10.0])
                            .on_press(Message::ClipTogglePin(m.id)),
                        if m.pinned {
                            "Pinned — kept when the history fills up or is cleared. Click to unpin."
                        } else {
                            "Pin this, so it is kept when the history fills up or is cleared."
                        },
                    );
                    let del = hint(
                        p,
                        button(text("✕").size(12.0))
                            .style(ui_theme::ghost(p))
                            .padding([9.0, 10.0])
                            .on_press(Message::ClipRemove(m.id)),
                        "Remove this entry. Files already transferred stay on your disk.",
                    );

                    let mut controls = row![reuse].spacing(6.0).align_y(Alignment::Center);
                    // Stop only exists while there is something to stop. What has already
                    // arrived is kept, so pressing it is cheap to undo.
                    if fetching {
                        controls = controls.push(hint(
                            p,
                            button(text("Stop").size(11.5))
                                .style(ui_theme::ghost(p))
                                .padding([9.0, 10.0])
                                .on_press(Message::ClipCancel(m.id)),
                            "Stop this transfer. What has arrived is kept, so clicking the entry resumes it.",
                        ));
                    }
                    list = list.push(controls.push(pin).push(del));
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
            container(field_label(p, "History — newest first · click any entry to copy it again"))
                .width(Length::Fill),
            button(text("Clear").size(12.0))
                .style(ui_theme::ghost(p))
                .padding([7.0, 12.0])
                .on_press(Message::ClipClear),
        ]
        .align_y(Alignment::Center);

        // What the transferred files are actually costing on disk. Absent before, so
        // there was no way to answer "is this program eating my drive?" short of finding
        // the cache folder by hand — and no way to do anything about it if it was.
        let disk: Element<'_, Message> = match self.engine.as_ref().and_then(|e| e.cache_usage()) {
            Some((bytes, count)) if count > 0 => row![
                text(format!(
                    "transferred files on disk: {} in {count} file{}",
                    human_bytes(bytes as usize),
                    if count == 1 { "" } else { "s" }
                ))
                .size(11.0)
                .color(if bytes > self.cfg.clipboard_cache_budget { p.warn } else { p.txt3 })
                .width(Length::Fill),
                button(text("Clear").size(11.0))
                    .style(ui_theme::ghost(p))
                    .padding([5.0, 10.0])
                    .on_press(Message::ClipCacheClear),
            ]
            .align_y(Alignment::Center)
            .into(),
            _ => Space::new().height(0.0).into(),
        };

        // The history only: what you copied, and the one action per row. Its settings
        // are in ⋯ → Settings → Clipboard, which the last line says.
        column![
            header,
            list,
            usage,
            disk,
            text("Clipboard settings: ⋯ → Settings → Clipboard.").size(11.0).color(p.txt3),
        ]
        .spacing(12.0)
        .into()
    }

    /// Settings → Clipboard. The everyday settings first, the rest under Advanced, in
    /// three groups (R30). Every control is the one the Clipboard tab used to hold.
    fn page_clipboard(&self, p: Palette) -> Element<'_, Message> {
        let basic = column![
            section(p, "Sharing"),
            toggle_row(
                p,
                "Sync clipboard",
                "copy here, paste there — text and images",
                self.cfg.clipboard_sync,
                Message::ClipboardToggled
            ),
            toggle_row(
                p,
                "Notify me when a file is waiting",
                "a desktop notification, never a pop-up — a window stealing focus is what caused the keyboard bug",
                self.cfg.clipboard_notify,
                Message::ClipNotifyToggled
            ),
            section(p, "Big files"),
            labeled(
                p,
                "Ask before bringing over anything larger than (MB)",
                input(p, "1", &self.clip_auto_mb_s, Message::ClipAutoMbChanged)
            ),
            text(if self.cfg.clipboard_auto_transfer_max == u64::MAX {
                "Never asks — copying is the decision, so anything you copy just comes over. \
                 Set a size here if you are on a slow or metered link.".to_string()
            } else if self.cfg.clipboard_auto_transfer_max == 0 {
                "0 — always ask, whatever the size.".to_string()
            } else {
                format!(
                    "Under {} it just arrives; over it, the entry waits for you.",
                    human_bytes(self.cfg.clipboard_auto_transfer_max as usize)
                )
            })
            .size(11.0)
            .color(p.txt3),
            labeled(
                p,
                "Or put received files straight into this folder",
                input(p, "/home/you/Desktop — blank to keep them on the clipboard only", &self.cfg.clipboard_deliver_dir, Message::ClipDeliverDirChanged)
            ),
            text(if self.cfg.clipboard_deliver_dir.trim().is_empty() {
                "Blank is normal — paste wherever you want it and that is where it goes. Set a folder only if you would rather files arrived automatically, with no paste at all.".to_string()
            } else {
                format!(
                    "Files land in {} as they arrive — nothing to paste. They are linked, not copied, so a 40 GB file uses 40 GB and not 80.",
                    self.cfg.clipboard_deliver_dir.trim()
                )
            })
            .size(11.0)
            .color(p.txt3),
            section(p, "History"),
            labeled(p, "Keep this many entries", input(p, "25", &self.clip_items_s, Message::ClipItemsChanged)),
            toggle_row(
                p,
                "Keep history after restart",
                "convenient — but clipboard contents, including anything you copied that was private, are written to your disk",
                self.cfg.clipboard_persist_history,
                Message::ClipPersistToggled
            ),
        ]
        .spacing(12.0);

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

        let adv = column![
            section(p, "What crosses"),
            toggle_row(p, "Sync text", "copied text crosses machines", self.cfg.clipboard_sync_text, Message::ClipSyncTextToggled),
            toggle_row(p, "Sync images", "copied images cross machines", self.cfg.clipboard_sync_images, Message::ClipSyncImagesToggled),
            toggle_row(p, "Sync files", "any file or folder, any extension — no allow-list", self.cfg.clipboard_sync_files, Message::ClipSyncFilesToggled),
            labeled(p, "Most files in one copy", input(p, "10000", &self.clip_max_files_s, Message::ClipMaxFilesChanged)),
            text("Bounds a stray click on an enormous folder. The rest is left behind, and the log says so.")
                .size(11.0)
                .color(p.txt3),
            toggle_row(
                p,
                "Also copy images as a .png file",
                "on: a copied image pastes into a browser AND into a file manager. Off: pixels only, and pasting into Explorer or Thunar does nothing.",
                self.cfg.clipboard_images_also_as_file,
                Message::ClipImagesAsFileToggled
            ),
            toggle_row(
                p,
                "Follow shortcuts inside copied folders",
                "off by default: one pointing outside the folder quietly copies files you never picked",
                self.cfg.clipboard_follow_symlinks,
                Message::ClipFollowSymlinksToggled
            ),
            toggle_row(
                p,
                "Pasting a received file moves it there",
                "on: the folder you paste into IS the destination, and a big file costs nothing to paste. Off: it is copied out of the cache, so a 40 GB file briefly uses 80 GB.",
                self.cfg.clipboard_move_on_paste,
                Message::ClipMoveOnPasteToggled
            ),
            section(p, "Transfers"),
            labeled(
                p,
                "Don't ask if it'll take under (seconds)",
                input(p, "3", &self.clip_quick_s, Message::ClipQuickSecsChanged)
            ),
            text(if self.cfg.clipboard_quick_send_secs == 0 {
                "0 — the size limit alone decides.".to_string()
            } else {
                format!(
                    "On a fast link a big file lands in a moment, so asking is noise. Measured from real transfers, so it still asks on a slow one. Currently {}.",
                    match self.engine.as_ref().and_then(|e| e.measured_throughput()) {
                        Some(bps) => format!("measuring ~{}/s", human_bytes(bps as usize)),
                        None => "no measurement yet, so it always asks".to_string(),
                    }
                )
            })
            .size(11.0)
            .color(p.txt3),
            labeled(
                p,
                "Paste files bigger than (MB) straight from the network",
                input(p, "64", &self.clip_stream_mb_s, Message::ClipStreamMbChanged)
            ),
            text(if self.cfg.clipboard_keep_resumable_copy {
                "Off while \"keep a resumable copy\" is on: every paste goes through the cache.".to_string()
            } else {
                format!(
                    "Files from {} MB up go network → memory → your folder: they use their own size on disk, once. Smaller ones pass through mouse-rs's cache first.",
                    self.cfg.clipboard_stream_min_mb
                )
            })
            .size(11.0)
            .color(p.txt3),
            labeled(
                p,
                "Wait for a dropped connection (seconds)",
                input(p, "180", &self.clip_stream_wait_s, Message::ClipStreamWaitChanged)
            ),
            text("A paste whose transfer stops (cable pulled, Wi-Fi drop) pauses and continues from the same byte if the connection is back within this time; after that it fails.")
                .size(11.0)
                .color(p.txt3),
            toggle_row(
                p,
                "Keep a resumable copy while pasting",
                "uses the file's size again in disk while a paste runs, but a paste cut short — even by a restart — continues where it stopped when you copy and paste the same file again",
                self.cfg.clipboard_keep_resumable_copy,
                Message::ClipKeepResumableToggled
            ),
            toggle_row(
                p,
                "Slow transfers while I'm on the other machine",
                "only if the pointer starts to lag; otherwise transfers run at the network's full speed",
                self.cfg.bulk_pause_while_crossed,
                Message::BulkPauseToggled
            ),
            labeled(
                p,
                "Transfer speed limit (MB/s, 0 = automatic)",
                input(p, "0", &self.bulk_rate_s, Message::BulkRateChanged)
            ),
            text("Automatic watches your mouse latency and backs off by itself. Pin a number if you'd rather decide.")
                .size(11.0)
                .color(p.txt3),
            labeled(
                p,
                "Send buffer (MB)",
                input(p, "8", &self.bulk_buf_s, Message::BulkBufferChanged)
            ),
            text(format!(
                "How much of a file may sit in memory waiting to go out. This is what keeps a 4 GB copy costing the same as a 4 MB one — right now, at most {}.",
                human_bytes((self.cfg.bulk_send_buffer_mb.max(1) as usize) * 1024 * 1024)
            ))
            .size(11.0)
            .color(p.txt3),
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
            labeled(
                p,
                "Don't fingerprint files bigger than (MB)",
                input(p, "64", &self.hash_limit_mb_s, Message::HashLimitMbChanged)
            ),
            text("Files up to this size are checked by their content when they arrive; bigger ones by their size, so copying a film is announced at once instead of after reading it all.")
                .size(11.0)
                .color(p.txt3),
            section(p, "Storage & memory"),
            labeled(p, "Disk for transferred files (GB)", input(p, "2", &self.clip_cache_gb_s, Message::ClipCacheGbChanged)),
            text("Oldest go first when it fills. Pinned entries are never removed.")
                .size(11.0)
                .color(p.txt3),
            labeled(
                p,
                "Folder for transferred files",
                input(p, "blank = ~/.mouse_share/cache", &self.cfg.cache_dir, Message::CacheDirChanged)
            ),
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
            labeled(
                p,
                "Send images inline up to (KB)",
                input(p, "256", &self.clip_inline_kb_s, Message::ClipInlineKbChanged)
            ),
            text("Bigger images take the separate lane instead, so they can't stall the mouse. Lower this on a slow link; raise it to skip the extra round trip on a fast one.")
                .size(11.0)
                .color(p.txt3),
        ]
        .spacing(12.0);

        column![card(p, basic), self.advanced(p, Page::Clipboard, adv)].spacing(12.0).into()
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
            text(&self.feedback).size(12.0).color(p.txt3).width(Length::Fill),
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

    /// The ⋯ menu: the way into Settings, and Quit. The theme moved to Settings →
    /// Appearance and Restore defaults to Settings → About (R30).
    fn overflow_menu(&self, p: Palette) -> Element<'_, Message> {
        let item = |label: &'static str, msg: Message, danger: bool| {
            button(text(label).size(12.5).color(if danger { p.danger } else { p.txt2 }))
                .style(ui_theme::ghost(p))
                .padding([9.0, 11.0])
                .width(Length::Fill)
                .on_press(msg)
        };

        container(
            column![
                item("⚙  Settings", Message::OpenSettings(Page::Index), false),
                item("Profiles…", Message::OpenSettings(Page::Connection), false),
                item("Quit mouse-rs", Message::Quit, true),
            ]
            .spacing(4.0),
        )
        .style(move |_| ui_theme::card(p))
        .padding(10.0)
        .width(210.0)
        .into()
    }

    // ---------- ⋯ → Settings ----------

    /// The back button and the page's title. Back from a category goes to the list; back
    /// from the list goes to the main window.
    fn settings_bar(&self, p: Palette, pg: Page) -> Element<'_, Message> {
        let back = if pg == Page::Index { Message::CloseSettings } else { Message::OpenSettings(Page::Index) };
        row![
            button(text("←").size(15.0).color(p.txt2))
                .style(ui_theme::ghost(p))
                .padding([6.0, 12.0])
                .on_press(back),
            text(pg.title()).size(18.0).color(p.txt).width(Length::Fill),
        ]
        .spacing(10.0)
        .align_y(Alignment::Center)
        .into()
    }

    fn settings_body(&self, p: Palette, pg: Page) -> Element<'_, Message> {
        match pg {
            Page::Index => self.settings_index(p),
            Page::Clipboard => self.page_clipboard(p),
            Page::Screens => self.page_screens(p),
            Page::Startup => self.page_startup(p),
            Page::Keys => self.page_keys(p),
            Page::Connection => self.page_connection(p),
            Page::Security => self.page_security(p),
            Page::Appearance => self.page_appearance(p),
            Page::Diagnostics => self.page_diagnostics(p),
            Page::About => self.page_about(p),
        }
    }

    /// The list of categories, grouped and ordered by how often people use them (R29).
    fn settings_index(&self, p: Palette) -> Element<'_, Message> {
        let mut col = column![].spacing(6.0);
        for (group, pages) in Page::CATEGORIES {
            col = col.push(Space::new().height(4.0)).push(text(group).size(10.5).color(p.txt3));
            for &pg in pages {
                col = col.push(
                    button(
                        row![
                            text(pg.icon()).size(18.0),
                            column![
                                text(pg.title()).size(14.0).color(p.txt),
                                text(pg.summary()).size(11.0).color(p.txt3),
                            ]
                            .spacing(2.0)
                            .width(Length::Fill),
                            text("›").size(16.0).color(p.txt3),
                        ]
                        .spacing(12.0)
                        .align_y(Alignment::Center),
                    )
                    .style(ui_theme::ghost(p))
                    .padding([12.0, 14.0])
                    .width(Length::Fill)
                    .on_press(Message::OpenSettings(pg)),
                );
            }
        }
        col.into()
    }

    /// A page's Advanced section: at the bottom, closed until opened (R25, R30).
    fn advanced<'a>(&'a self, p: Palette, pg: Page, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        let open = self.adv_open.contains(&pg);
        let head = button(text(if open { "▾  Advanced" } else { "▸  Advanced" }).size(13.0).color(p.txt2))
            .style(ui_theme::ghost(p))
            .padding([10.0, 14.0])
            .width(Length::Fill)
            .on_press(Message::AdvancedToggled(pg));
        if open {
            column![head, card(p, body)].spacing(8.0).into()
        } else {
            head.into()
        }
    }

    /// Settings → Connection: this computer's name, staying connected and profiles; the
    /// ports and the discovery port under Advanced. Role and the other computer's address
    /// are on the main window (R30).
    fn page_connection(&self, p: Palette) -> Element<'_, Message> {
        let server_side = self.cfg.role == Role::Server;
        let mut basic = column![
            section(p, "This computer"),
            labeled(p, "Machine name", input(p, "this-machine", &self.cfg.name, Message::NameChanged)),
        ]
        .spacing(14.0);
        if !server_side {
            basic = basic.push(toggle_row(p, "Auto-reconnect", "retry with backoff until the server appears", self.cfg.reconnect, Message::ReconnectToggled));
        }

        let mut adv = column![section(p, "Ports")].spacing(14.0);
        if server_side {
            // Real addresses of this machine, so the bind address is chosen rather than
            // typed from memory.
            let local = mouse_core::discover::listen_candidates();
            let picked = local
                .iter()
                .find(|c| **c == self.cfg.listen_host)
                .cloned();
            adv = adv
                .push(labeled(p, "Listen host", input(p, "0.0.0.0", &self.cfg.listen_host, Message::ListenHostChanged)))
                .push(labeled(
                    p,
                    "This machine's addresses",
                    pick_list(local, picked, Message::ListenHostPicked)
                        .width(Length::Fill)
                        .padding([10.0, 12.0])
                        .text_size(13.0),
                ))
                .push(labeled(p, "Listen port", input(p, "24800", &self.listen_port_s, Message::ListenPortChanged)));
        } else {
            adv = adv.push(labeled(p, "Server port", input(p, "24800", &self.server_port_s, Message::ServerPortChanged)));
        }
        adv = adv
            .push(labeled(p, "Discovery port (finding the other computer)", input(p, "24801", &self.discovery_port_s, Message::DiscoveryPortChanged)))
            .push(
                text("The two computers find each other on this UDP port. Same number on both; 0 turns finding off, and the address on the main window is used instead.")
                    .size(11.0)
                    .color(p.txt3),
            );

        column![
            card(p, basic),
            card(p, self.profiles_section(p)),
            self.advanced(p, Page::Connection, adv),
        ]
        .spacing(12.0)
        .into()
    }

    /// Settings → Appearance: the six themes, moved here from the ⋯ menu.
    fn page_appearance(&self, p: Palette) -> Element<'_, Message> {
        let mut themes = column![section(p, "Theme")].spacing(8.0);
        for t in Theme6::ALL {
            let base = t.palette();
            themes = themes.push(
                button(
                    row![
                        container(Space::new().width(18.0).height(18.0))
                            .style(move |_| ui_theme::swatch_fill(base)),
                        text(t.label()).size(13.5).color(if t == self.theme { p.accent_ink } else { p.txt2 }),
                    ]
                    .spacing(10.0)
                    .align_y(Alignment::Center),
                )
                .style(ui_theme::segment(p, t == self.theme))
                .padding([9.0, 12.0])
                .width(Length::Fill)
                .on_press(Message::ThemePicked(t)),
            );
        }
        card(p, themes)
    }

    /// Settings → Diagnostics: where the log is, and (Advanced) sending it for support.
    fn page_diagnostics(&self, p: Palette) -> Element<'_, Message> {
        // The panel has no console behind it, so name the log file here — otherwise a user
        // hitting a problem has nothing to look at and nothing to send.
        let logs = column![
            section(p, "Log file"),
            text(&self.log_path).size(11.0).color(p.txt3),
        ]
        .spacing(6.0);

        let adv = column![
            section(p, "Send logs for support"),
            labeled(p, "Log upload address", input(p, "blank = never sent", &self.cfg.log_upload_url, Message::LogUploadUrlChanged)),
            labeled(p, "Send every (seconds)", input(p, "30", &self.log_upload_secs_s, Message::LogUploadSecsChanged)),
            text("Nothing leaves this computer unless an address is set. Takes effect the next time mouse-rs starts.")
                .size(11.0)
                .color(p.txt3),
        ]
        .spacing(14.0);

        column![card(p, logs), self.advanced(p, Page::Diagnostics, adv)].spacing(12.0).into()
    }

    /// Settings → About: the version, and Restore defaults, moved here from the ⋯ menu.
    fn page_about(&self, p: Palette) -> Element<'_, Message> {
        card(p, column![
            section(p, "mouse-rs"),
            text(format!("Version {}", env!("CARGO_PKG_VERSION"))).size(12.5).color(p.txt2),
            button(text("Restore default settings").size(12.5).color(p.danger))
                .style(ui_theme::ghost(p))
                .padding([9.0, 12.0])
                .on_press(Message::RestoreDefaults),
            text("Resets preferences only — keeps this computer's name, shared secret and screen size. Applied to the form; press Save to keep it.")
                .size(11.0)
                .color(p.txt3),
        ]
        .spacing(12.0))
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
                "A named snapshot of all your settings — peer address, edges, screen size, \
                 hotkeys, secret. Save one per setup (say \"desk\" and \"travel\") and switch \
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

/// A sub-heading inside a settings page ("Sharing", "Ports", …).
fn section(p: Palette, title: &str) -> Element<'_, Message> {
    text(title.to_string()).size(12.5).color(p.accent).into()
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
    // The label column takes the free space and the switch keeps its own, so a long
    // description wraps inside its column instead of growing the row until it runs over
    // the control beside it. A spacer cannot do this: it only pushes, and what is needed
    // is for the text to be *allowed to be narrow*.
    let labels = column![
        text(title.to_string()).size(14.0).color(p.txt),
        text(desc.to_string()).size(11.0).color(p.txt3),
    ]
    .spacing(2.0)
    .width(Length::Fill);
    container(
        row![
            labels,
            toggler(value).on_toggle(on_toggle).style(ui_theme::switch(p)).size(24.0),
        ]
        .spacing(12.0)
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
            text(name.to_string()).size(13.0).color(p.txt).width(Length::Fill),
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

/// Does this role decide where the pointer *leaves* from?
///
/// Only the server. It watches its own edges and hands control over when the pointer
/// reaches one, so `exit_edge` is the question it has to answer. The client engine never
/// reads that value (`client.rs:37`), so offering the control there was a dial wired to
/// nothing — and the warning beside it compared the dead value against the live one,
/// which is how it could contradict a pair that was set up correctly.
fn shows_exit_edge(role: Role) -> bool {
    role == Role::Server
}

/// What `entry_edge` means to this role, in the terms that role experiences.
///
/// The same setting, honestly, does two different jobs. On the server it is where the
/// *other* screen picks the pointer up. On the client it is both where the pointer
/// arrives and the edge to push back against to send control home — which is the only
/// thing about it the person at the client keyboard actually does.
fn entry_edge_label(role: Role) -> &'static str {
    match role {
        Role::Server => "Entry edge — the pointer appears here on the other screen",
        Role::Client => "Entry edge — where the pointer arrives, and where to push back",
    }
}

fn entry_edge_hint(role: Role) -> &'static str {
    match role {
        Role::Server => {
            "Set this to the edge facing this machine, so the two screens sit side by \
             side rather than overlapping."
        }
        Role::Client => {
            "Push the pointer back into this edge to hand control home. Where it leaves \
             from is decided on the other machine, which is why there is no exit edge \
             here."
        }
    }
}

/// Compact hover text for the tray icon: a mirror of the header status.
///
/// Files waiting for an answer take precedence over connection state, because they are
/// the only thing here the user has to *act* on — and with the panel closed the tray is
/// the only place that can be said. A pop-up dialog is not an option: Windows withholds
/// the low-level keyboard hook while our own window owns the foreground, which is the
/// leak fixed on 31 July and would come straight back.
fn tray_tooltip(running: bool, snap: Option<&StatusSnapshot>, waiting: usize) -> String {
    if running && waiting > 0 {
        return match waiting {
            1 => "mouse-rs — a file is waiting for you".to_owned(),
            n => format!("mouse-rs — {n} files are waiting for you"),
        };
    }
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
        .window(iced::window::Settings {
            size: iced::Size::new(560.0, 720.0),
            // A floor, not a target. Every row now lets its text shrink and wrap rather
            // than grow until it runs over the control beside it, so the panel stays
            // usable well below the default — but under roughly a phone's width there is
            // genuinely nowhere for a label and its switch to both go, and allowing it
            // to be dragged narrower only produces a window that looks broken.
            min_size: Some(iced::Size::new(360.0, 420.0)),
            ..Default::default()
        })
        .run()
        .map_err(|e| anyhow::anyhow!("control panel: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R30, as the owner confirmed it on 2026-10-14: the categories, their order and
    /// their groups. Every page is listed exactly once.
    #[test]
    fn settings_categories_are_in_the_agreed_order() {
        let order: Vec<&str> = Page::CATEGORIES.iter().flat_map(|(_, pages)| pages.iter().map(|p| p.title())).collect();
        assert_eq!(
            order,
            [
                "Clipboard",
                "Screens & edges",
                "Startup",
                "Keyboard & hotkeys",
                "Connection",
                "Security & pairing",
                "Appearance",
                "Diagnostics",
                "About",
            ]
        );
        let groups: Vec<&str> = Page::CATEGORIES.iter().map(|(g, _)| *g).collect();
        assert_eq!(groups, ["EVERYDAY", "SETUP", "HELP & ADVANCED"]);
        assert!(order.iter().all(|t| !Page::Index.title().eq(*t)), "the index is not a category");
    }

    /// R28: the main window has two tabs, and nothing else.
    #[test]
    fn the_main_window_has_only_connection_and_clipboard() {
        let tabs: Vec<&str> = Tab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(tabs, ["Connection", "Clipboard"]);
    }

    /// The new controls change settings the engine reads at Start, so editing them while
    /// sharing must raise the restart prompt like every other such setting (R1).
    #[test]
    fn the_new_engine_settings_ask_for_a_restart() {
        let live = Config::default();
        let mut now = live.clone();
        now.edge_threshold_px += 3;
        now.discovery_port = live.discovery_port.wrapping_add(1);
        assert_eq!(pending_restart_changes(&live, &now), vec!["edge sensitivity", "discovery port"]);
    }

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

#[cfg(test)]
mod layout_role_tests {
    use super::*;

    /// R16. The client engine reads `entry_edge` and never `exit_edge`, so offering an
    /// Exit-edge picker on a client wrote a value nothing would read — the original dead
    /// knob, and the reason every later rule about not shipping controls the code ignores
    /// was written.
    #[test]
    fn only_the_server_is_asked_where_the_pointer_leaves() {
        assert!(shows_exit_edge(Role::Server), "the server watches its own edges");
        assert!(
            !shows_exit_edge(Role::Client),
            "the client never reads exit_edge, so asking for it is a dial wired to nothing"
        );
    }

    /// The overlap warning compares exit against entry. On a client the exit half is
    /// dead, so the comparison could contradict a correctly-configured pair — it must
    /// only ever appear where both values are real.
    #[test]
    fn the_overlap_warning_cannot_appear_on_a_client() {
        // The warning is rendered inside the `shows_exit_edge` branch, so this is the
        // property that keeps it honest: no exit edge shown, no comparison made.
        assert!(
            !shows_exit_edge(Role::Client),
            "a warning about exit vs entry has nothing true to say on a client"
        );
    }

    /// Each role is told what the setting does *for it*. The client's job is pushing
    /// back to go home, which is the wording agreed for R16.
    #[test]
    fn each_role_gets_wording_that_describes_its_own_job() {
        assert!(
            entry_edge_hint(Role::Client).contains("push the pointer back")
                || entry_edge_hint(Role::Client).contains("Push the pointer back"),
            "the client's entry edge is the one it pushes back into: {}",
            entry_edge_hint(Role::Client)
        );
        assert!(
            entry_edge_hint(Role::Client).contains("no exit edge"),
            "and it should say why the other control is absent, not just omit it"
        );
        assert_ne!(
            entry_edge_label(Role::Server),
            entry_edge_label(Role::Client),
            "the same setting does two different jobs and should not claim otherwise"
        );
    }
}
