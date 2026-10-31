//! iced control panel (the product UI). A bare `mouse-rs` launch opens this window:
//! edit config (saved as it changes), and Start/Stop the engine — the same lifecycle the headless
//! CLI uses, so the GUI is just a front-end over [`crate::run`]. Closing the window
//! quits the app. The look (R40: six themes, status card, grouped rows) lives in
//! [`crate::ui_theme`]; the system tray in [`crate::tray`].

use std::sync::{Arc, Mutex};

use iced::widget::{
    button, column, container, mouse_area, progress_bar, row, scrollable, stack, text, text_input,
    toggler, Space,
};
use iced::{Alignment, Element, Length, Padding, Subscription, Task};

use mouse_core::{sanitize_profile_name, Config, Edge, Role};
use mouse_engine::StatusSnapshot;
use mouse_input::screen_size;

use crate::run::EngineHandle;
use crate::ui_theme::{self, Palette, Theme6, BOLD, MONO};

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
    /// (group heading, its pages), in the order shown (R40, from the owner's design).
    const CATEGORIES: [(&'static str, &'static [Page]); 3] = [
        ("EVERYDAY", &[Page::Clipboard, Page::Screens, Page::Keys]),
        ("THIS COMPUTER", &[Page::Startup, Page::Connection, Page::Security]),
        ("APP", &[Page::Appearance, Page::Diagnostics, Page::About]),
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
    fn summary(self) -> &'static str {
        match self {
            Page::Index => "",
            Page::Clipboard => "What to share, large files, history",
            Page::Screens => "Where the other computer sits",
            Page::Startup => "Start at login, minimized, auto-connect",
            Page::Keys => "Getting control back",
            Page::Connection => "Name, profiles, ports",
            Page::Security => "PIN, pairing, encryption",
            Page::Appearance => "Theme",
            Page::Diagnostics => "Log file, support logs",
            Page::About => "Version, restore defaults",
        }
    }
}

/// Which hotkey is being recorded (Settings → Keyboard & hotkeys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyField {
    Lock,
    Return,
    /// Entry N of the switch list (R46).
    Switch(usize),
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    ThemePicked(Theme6),
    /// Appearance → "Match system light / dark".
    ThemeFollowSystem(bool),
    /// The system's light/dark setting, at launch and whenever it changes.
    SystemTheme(iced::theme::Mode),
    /// Main window: open or fold the Connection details row.
    ToggleDetails,
    /// The ⇄ button between the two computers: swap this computer's role.
    SwapRoles,
    /// Screens & edges: the side the other computer sits on.
    PeerSidePicked(Edge),
    /// Screens & edges → Advanced: "Arrives on … at: Auto".
    EntryEdgeAuto,
    /// Keyboard & hotkeys: start recording, a key arriving, stop.
    RecordHotkey(HotkeyField),
    KeyEvent(iced::keyboard::Event),
    ClearReturnHotkey,
    /// Diagnostics.
    CopyLogPath,
    OpenLogFolder,
    /// About → Restore…: ask first, then do it.
    AskRestoreDefaults,
    CancelRestoreDefaults,
    TabPicked(Tab),
    NameChanged(String),
    RolePicked(Role),
    /// Settings → Security: first press arms Remove for this device ID, the second removes it.
    RemovePaired(String),
    /// Main window: work with this paired computer from now on (R45).
    ConnectTo(String),
    /// Main window: close the link with the computer in use (stops sharing).
    DisconnectPeer,
    /// The computer with the mouse: move entry N of the switch list up (true) or down (R46).
    MoveSwitch(usize, bool),
    /// Settings: put entry N's shortcut back to its number's default (R46).
    SwitchKeyDefault(usize),
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
    /// Windows: add the firewall rule for mouse-rs, through Windows' Yes/No prompt.
    AllowFirewall,
    /// About → Check now.
    UpdateCheckNow,
    /// The update banner's Update: download, check, swap in.
    UpdateInstall,
    /// The update banner's Later: hidden until the next start.
    UpdateLater,
    /// "Restart mouse-rs" once the new version is in place.
    UpdateRestart,
    UpdatesToggled(bool),
    /// When updating in place is not possible: the download page.
    OpenDownloadPage,
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
    ServerHostChanged(String),
    NetworkPreferencePicked(mouse_core::NetworkPreference),
    ServerPortChanged(String),
    ListenHostChanged(String),
    ListenPortChanged(String),
    EntryEdgePicked(Edge),
    ScreenWChanged(String),
    ScreenHChanged(String),
    DetectScreen,
    /// Load a saved profile and make it the current setup.
    SwitchProfile(String),
    DeleteProfile(String),
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
    /// The window's close button. Treated exactly like Quit.
    CloseRequested,
    /// "Quit anyway" / "Stop anyway" on the transfer-in-progress warning.
    LeaveAnyway,
    /// The elevated relaunch itself, once any transfer warning has been answered.
    RelaunchElevatedNow,
    /// "Keep going" on that warning.
    KeepGoing,
    // --- connection PIN (R35) and pairing with a code ---
    /// Client: the PIN box on the main window.
    PinTyped(String),
    PinSubmit,
    /// Client: the pairing-code box.
    /// A digit typed (or pasted, or deleted) in pairing-code box N.
    PairCodeTyped(usize, String),
    PairWithCode,
    /// Server: show a new pairing code / stop showing it.
    ShowPairCode,
    CancelPairCode,
    /// Server: Settings → Security, PIN section.
    ToggleShowPin,
    PinNewTyped(String),
    PinNew2Typed(String),
    SavePin,
    /// Client: ask for the password while logged in / at the login screen.
    PinSessionToggled(bool),
    /// Linux only: Windows has no login-screen sharing.
    #[cfg(target_os = "linux")]
    PinLoginToggled(bool),
    /// Client: the daily set-a-password reminder.
    PinReminderSet,
    PinReminderLater,
    PinReminderNever,
}

/// What the user asked for when a transfer was still running (R33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leave {
    Quit,
    Stop,
    /// "Restart now", which applies saved settings by stopping and starting sharing.
    Restart,
    /// Windows "Restart as administrator": this copy exits for an elevated one.
    Elevate,
    /// The new version is in place: this copy exits for it.
    Update,
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
    chk(live.switch_order != now.switch_order || live.switch_keys != now.switch_keys, "switch shortcuts");
    chk(
        live.pin_enabled != now.pin_enabled || live.connection_pin != now.connection_pin,
        "connection PIN",
    );
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
    chk(live.network_preference != now.network_preference, "preferred network");
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
    /// The system's light/dark setting, once known; used when the theme follows it.
    system_dark: Option<bool>,
    tab: Tab,
    engine: Option<EngineHandle>,
    /// The paired computer whose Remove was pressed once and waits for the second press.
    remove_armed: Option<String>,
    feedback: String,
    /// Saved profiles with their settings, so the list can mark the one in use.
    profiles: Vec<(String, Config)>,
    profile_name_input: String,
    /// Main window: the Connection details row is open.
    details_open: bool,
    /// Keyboard & hotkeys: the hotkey waiting for a key press.
    recording: Option<HotkeyField>,
    /// About: the "Restore default settings?" question is showing.
    confirm_restore: bool,
    /// When the settings were last edited. They are written once editing pauses, so a
    /// half-typed number is not saved one keystroke at a time (R40: no Save button).
    last_edit: Option<std::time::Instant>,
    /// When the footer message was set; it fades back to "Changes save automatically".
    feedback_at: Option<std::time::Instant>,
    /// The other computer's name and screen from the last connection, for the status card
    /// while sharing is off.
    last_peer: Option<(String, i32, i32)>,
    /// When the clipboard history was last checked for saving, and what it held then (a
    /// fingerprint of ids and pins), so it is written only when it has changed.
    history_mark: (std::time::Instant, u64),
    /// Why the lane port typed was not taken (reserved, or another program has it).
    bulk_port_error: String,
    /// Windows: whether the firewall rule for mouse-rs exists. Read at launch and after
    /// adding it, not per frame: it starts a process.
    firewall_ok: bool,
    /// The update check and install, run on threads of their own (R36).
    update: crate::update::Shared,
    /// When the last check was started, for the once-a-day repeat.
    update_checked_at: Option<std::time::Instant>,
    /// "Later" on the update banner: hidden until the next start.
    update_dismissed: bool,
    /// The first check waits a little after launch, so start-up is not slowed by it.
    opened_at: std::time::Instant,
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
    /// Why "Allow and turn on" did not go ahead (a PIN too short, two that differ).
    #[cfg(target_os = "linux")]
    login_dialog_error: String,
    /// Set while the "a transfer is still running" warning is on screen (R33).
    confirm_leave: Option<Leave>,
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
    /// Client: the connection PIN being typed.
    pin_input: String,
    /// Client: the pairing code being typed, and the attempt in flight.
    pair_code_input: String,
    pairing: Arc<Mutex<PairState>>,
    /// Server: Settings → Security PIN fields.
    show_pin: bool,
    pin_new: String,
    pin_new2: String,
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
            .style(move |_| ui_theme::floating(p))
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

/// A pairing attempt running in the background, and its answer.
#[derive(Default)]
enum PairState {
    #[default]
    Idle,
    Running,
    /// The computer we paired with, or why not.
    Done(Result<mouse_core::config::PairedDevice, String>),
}

#[derive(Default)]
enum ScanState {
    #[default]
    Idle,
    Running,
    Done(Vec<String>),
}

impl App {
    fn new(mut cfg: Config) -> Self {
        // The real screen size, so "This computer · 1440×900" is true from the first
        // window, and not the 1920×1080 default until sharing has started once.
        if let Ok((w, h)) = screen_size() {
            cfg.screen_width = w;
            cfg.screen_height = h;
        }
        let mut app = App {
            system_dark: None,
            tab: Tab::Connection,
            engine: None,
            remove_armed: None,
            feedback: String::new(),
            profiles: load_profiles(),
            profile_name_input: String::new(),
            details_open: false,
            recording: None,
            confirm_restore: false,
            last_edit: None,
            feedback_at: None,
            last_peer: None,
            history_mark: (std::time::Instant::now(), 0),
            bulk_port_error: String::new(),
            firewall_ok: crate::autostart::firewall_rule_exists(),
            update: Default::default(),
            update_checked_at: None,
            update_dismissed: false,
            opened_at: std::time::Instant::now(),
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
            #[cfg(target_os = "linux")]
            login_dialog_error: String::new(),
            confirm_leave: None,
            launch_done: false,
            log_path: crate::logging::log_path().display().to_string(),
            live_cfg: None,
            restart_hint_dismissed: false,
            scan: Arc::new(Mutex::new(ScanState::Idle)),
            found_servers: Vec::new(),
            pin_input: String::new(),
            pair_code_input: String::new(),
            pairing: Arc::new(Mutex::new(PairState::Idle)),
            show_pin: false,
            pin_new: String::new(),
            pin_new2: String::new(),
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
            if want == "clipboard-tab" {
                app.tab = Tab::Clipboard;
            }
            if want == "quit-prompt" {
                app.confirm_leave = Some(Leave::Quit);
            }
            if want == "menu" {
                app.menu_open = true;
            }
            if want == "restore-prompt" {
                app.confirm_restore = true;
            }
            #[cfg(target_os = "linux")]
            if want == "login-prompt" {
                app.login_screen_asking = true;
            }
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

    /// After a password setting changed: save, restart sharing so it applies now, and
    /// refresh the login screen's own copy of the settings, which it reads from `/etc`
    /// (that asks for the computer's password through the desktop, not a command).
    fn apply_pin_change(&mut self) -> Task<Message> {
        let t = self.save_and_apply();
        #[cfg(target_os = "linux")]
        if self.login_screen.on() {
            let _ = self.cfg.save(&Config::default_path());
            self.apply_login_screen(true);
        }
        t
    }

    /// Save the settings and, if sharing is on, restart it so they take effect now.
    fn save_and_apply(&mut self) -> Task<Message> {
        if self.running() {
            return self.leave(Leave::Restart);
        }
        let _ = self.update(Message::SaveConfig);
        Task::none()
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
        // Stopped, nothing is being waited for (R45).
        mouse_core::trust::reserve(&self.cfg.device_id, None);
        if let Some(e) = &self.engine {
            // Without this line a deliberate Stop and a crash look identical in the log,
            // which is the worst possible ambiguity for a tool whose symptom is
            // "my keyboard went dead".
            tracing::info!("stop requested from the panel");
            e.request_stop();
        }
        self.feedback = "stopping…".into();
    }

    /// What would be cut off by quitting or stopping now, in words, or `None` when
    /// nothing is moving. Quitting is always the user's choice (R33); this only makes
    /// sure it is an informed one.
    fn transfer_running(&self) -> Option<String> {
        if !self.running() {
            return None;
        }
        let incoming = mouse_clipboard::transfer::receiving_now();
        let outgoing = mouse_clipboard::transfer::sending_now();
        let fetching = self.engine.as_ref().is_some_and(|e| {
            e.clipboard_meta()
                .iter()
                .any(|m| matches!(m.status, mouse_clipboard::ClipStatus::Fetching { .. }))
        });
        let peer = self
            .engine
            .as_ref()
            .map(|e| e.status().peer)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "the other computer".into());
        let files = |n: usize| if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        match (incoming > 0 || fetching, outgoing > 0) {
            (true, true) => Some(format!(
                "Files are still being copied both ways between this computer and {peer}."
            )),
            (true, false) => Some(format!(
                "A paste from {peer} is still coming in{}.",
                if incoming > 0 { format!(" ({})", files(incoming)) } else { String::new() }
            )),
            (false, true) => Some(format!(
                "{peer} is still pasting {} from this computer.",
                files(outgoing)
            )),
            (false, false) => None,
        }
    }

    /// Quit or stop now, or first ask if that would cut a transfer off.
    fn leave(&mut self, how: Leave) -> Task<Message> {
        self.menu_open = false;
        if self.transfer_running().is_some() {
            self.confirm_leave = Some(how);
            // The request may have come from the tray with the panel minimized; the
            // question has to be where the user can see it.
            return iced::window::latest().and_then(|id| {
                iced::window::minimize(id, false).chain(iced::window::gain_focus(id))
            });
        }
        self.leave_now(how)
    }

    fn leave_now(&mut self, how: Leave) -> Task<Message> {
        self.confirm_leave = None;
        match how {
            Leave::Stop => {
                self.stop_engine();
                Task::none()
            }
            Leave::Restart => {
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
                Task::none()
            }
            Leave::Elevate => self.update(Message::RelaunchElevatedNow),
            Leave::Update => match crate::update::relaunch() {
                Ok(()) => {
                    tracing::info!("restarting into the new version");
                    self.leave_now(Leave::Quit)
                }
                Err(e) => {
                    self.feedback = e;
                    Task::none()
                }
            },
            Leave::Quit => {
                // An edit made a moment ago has not been written yet.
                self.autosave();
                if let Some(e) = &self.engine {
                    // Not left to the engine's drop: the hard exit below can beat it.
                    e.save_history();
                    e.request_stop();
                }
                // Hard backstop so the process always dies even if teardown wedges.
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    std::process::exit(0);
                });
                // Close the window → iced returns → App drops → engine teardown runs
                // (releases the X grab / held keys). The backstop above guarantees exit.
                iced::window::latest().and_then(iced::window::close)
            }
        }
    }

    /// Every message goes through here. Anything but the clock counts as an edit, and the
    /// settings are written once editing has paused (R40: changes save automatically).
    fn update(&mut self, message: Message) -> Task<Message> {
        let quiet = matches!(message, Message::Tick | Message::KeyEvent(_) | Message::SystemTheme(_));
        let said = self.feedback.clone();
        let task = self.handle(message);
        if !quiet {
            self.last_edit = Some(std::time::Instant::now());
        }
        if self.feedback != said {
            self.feedback_at = Some(std::time::Instant::now());
        }
        task
    }

    /// Write the settings if they differ from what is on disk.
    fn autosave(&mut self) {
        self.last_edit = None;
        let same = serde_json::to_value(&self.cfg).ok() == serde_json::to_value(&self.saved_cfg).ok();
        if !same {
            let _ = self.handle(Message::SaveConfig);
        }
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                // At start, a little after so launch is not slowed. A test build also
                // checks every minute, so a new test version shows up without a restart.
                let due = match self.update_checked_at {
                    None => self.opened_at.elapsed() >= std::time::Duration::from_secs(8),
                    Some(t) => crate::update::TEST_BUILD && t.elapsed() >= std::time::Duration::from_secs(60),
                };
                if self.cfg.check_updates && due && self.update_idle() {
                    self.update_checked_at = Some(std::time::Instant::now());
                    crate::update::spawn_check(self.update.clone());
                }
                // Save once the last edit is a moment old.
                if self.last_edit.is_some_and(|t| t.elapsed() >= std::time::Duration::from_millis(600)) {
                    self.autosave();
                }
                // A footer message is news for a few seconds, then the footer goes back to
                // saying what is always true.
                if self.feedback_at.is_some_and(|t| t.elapsed() >= std::time::Duration::from_secs(8)) {
                    self.feedback.clear();
                    self.feedback_at = None;
                }
                // The history file is kept current while running, because the app is
                // rarely stopped in a way that runs the save on quit (B45).
                if self.history_mark.0.elapsed() >= std::time::Duration::from_secs(30) {
                    if let Some(e) = &self.engine {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        for m in e.clipboard_meta() {
                            (m.id, m.pinned).hash(&mut h);
                        }
                        let fp = h.finish();
                        if fp != self.history_mark.1 {
                            e.save_history();
                        }
                        self.history_mark = (std::time::Instant::now(), fp);
                    } else {
                        self.history_mark.0 = std::time::Instant::now();
                    }
                }
                // Client: the server decides where the pointer arrives; keep the learned edge
                // as this computer's setting, without asking for a restart (the engine
                // already uses it).
                if self.cfg.role == Role::Client {
                    if let Some(edge) = self.engine.as_ref().and_then(|e| e.status().arrival_edge) {
                        if self.cfg.entry_edge != edge {
                            self.cfg.entry_edge = edge;
                            if let Some(live) = self.live_cfg.as_mut() {
                                live.entry_edge = edge;
                            }
                            self.last_edit = Some(std::time::Instant::now());
                        }
                    }
                }
                if let Some(s) = self.engine.as_ref().map(|e| e.status()).filter(|s| s.connected) {
                    let name = s.peer.trim_end_matches(" (login screen)").to_string();
                    // Remembered, so the status card can name it while sharing is off.
                    if self.cfg.peer_name != name {
                        self.cfg.peer_name = name.clone();
                        self.last_edit = Some(std::time::Instant::now());
                    }
                    self.last_peer = Some((name, s.peer_w, s.peer_h));
                }
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
                // A pairing attempt finished.
                let paired = std::mem::take(&mut *self.pairing.lock().unwrap());
                match paired {
                    PairState::Done(Ok(dev)) => {
                        self.pair_code_input.clear();
                        tracing::info!("paired with {} (device {})", dev.name, dev.id.get(..8).unwrap_or(&dev.id));
                        let server = dev.name.clone();
                        self.save_pairings();
                        let t = self.save_and_apply();
                        if !self.running() {
                            self.start_engine();
                        }
                        self.feedback = format!("paired with {server}");
                        return Task::batch([t, self.refresh_login_copy()]);
                    }
                    PairState::Done(Err(why)) => {
                        tracing::warn!("pairing failed: {why}");
                        self.feedback = pair_error_text(&why);
                    }
                    other => *self.pairing.lock().unwrap() = other,
                }
                if let Some(client) = mouse_core::pairing::take_last_paired() {
                    self.feedback = format!("paired with {client}");
                }
                // A computer paired with this one while it showed a code: keep it.
                if self.save_pairings() {
                    // Its switch shortcut is registered when sharing starts, so start again
                    // now: the new computer's Ctrl+Alt+Shift+number works straight away,
                    // without "Restart sharing". The others reconnect by themselves.
                    if self.cfg.role == Role::Server && self.running() {
                        tracing::info!("new pairing: restarting sharing so its switch shortcut works");
                        self.stop_engine();
                        self.engine = None;
                        self.start_engine();
                    }
                    return self.refresh_login_copy();
                }
                // Remember the computer this one is now with, for the next start (R45).
                if let Some(id) = mouse_core::trust::take_last(&self.cfg.device_id) {
                    if self.cfg.connect_to != id {
                        self.cfg.connect_to = id.clone();
                        self.saved_cfg.connect_to = id.clone();
                        if let Some(live) = self.live_cfg.as_mut() {
                            live.connect_to = id;
                        }
                        let _ = self.cfg.save(&Config::default_path());
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
                let mut want_stop = false;
                for ev in crate::tray::poll() {
                    match ev {
                        crate::tray::TrayEvent::Start => {
                            if !self.running() {
                                self.start_engine();
                            }
                        }
                        crate::tray::TrayEvent::Stop => want_stop = true,
                        crate::tray::TrayEvent::Open => want_open = true,
                        crate::tray::TrayEvent::Quit => want_quit = true,
                    }
                }
                if want_quit {
                    return self.update(Message::Quit);
                }
                if want_stop {
                    return self.update(Message::Stop);
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
            Message::ThemePicked(t) => {
                self.cfg.theme = t.key().to_string();
                self.cfg.theme_follow_system = false;
            }
            Message::ThemeFollowSystem(on) => {
                self.cfg.theme_follow_system = on;
                if on {
                    return iced::system::theme().map(Message::SystemTheme);
                }
            }
            Message::SystemTheme(mode) => {
                self.system_dark = match mode {
                    iced::theme::Mode::Dark => Some(true),
                    iced::theme::Mode::Light => Some(false),
                    iced::theme::Mode::None => None,
                };
            }
            Message::ToggleDetails => self.details_open = !self.details_open,
            Message::SwapRoles => {
                self.cfg.role = match self.cfg.role {
                    Role::Server => Role::Client,
                    Role::Client => Role::Server,
                };
            }
            Message::PeerSidePicked(side) => match self.cfg.role {
                // The server leaves through the side the other computer is on, and the
                // pointer arrives on the facing edge over there.
                Role::Server => {
                    self.cfg.exit_edge = side;
                    self.cfg.entry_edge = side.opposite();
                }
                // The client only sees where the pointer arrives: the side facing the server.
                Role::Client => self.cfg.entry_edge = side,
            },
            Message::EntryEdgeAuto => self.cfg.entry_edge = self.cfg.exit_edge.opposite(),
            Message::RecordHotkey(which) => {
                self.recording = if self.recording == Some(which) { None } else { Some(which) };
            }
            Message::KeyEvent(ev) => {
                if let (Some(which), iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = (self.recording, ev) {
                    let plain = !(modifiers.control() || modifiers.alt() || modifiers.shift() || modifiers.logo());
                    if plain && key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) {
                        self.recording = None;
                        self.feedback = "kept the old key".into();
                    } else if let Some(tok) = key_token(&key) {
                        let combo = match which {
                            // One key: modifiers held with it are not part of it.
                            HotkeyField::Lock => tok,
                            HotkeyField::Return | HotkeyField::Switch(_) => {
                                let mut parts: Vec<&str> = Vec::new();
                                if modifiers.control() { parts.push("ctrl"); }
                                if modifiers.shift() { parts.push("shift"); }
                                if modifiers.logo() { parts.push("win"); }
                                if modifiers.alt() { parts.push("alt"); }
                                parts.push(&tok);
                                parts.join("+")
                            }
                        };
                        match which {
                            HotkeyField::Lock => self.cfg.lock_hotkey = combo.clone(),
                            HotkeyField::Return => self.cfg.return_hotkey = combo.clone(),
                            HotkeyField::Switch(i) => {
                                if let Some(t) = self.cfg.switch_targets().get(i) {
                                    self.cfg.switch_keys.insert(t.id.clone(), combo.clone());
                                }
                            }
                        }
                        self.recording = None;
                        self.last_edit = Some(std::time::Instant::now());
                        self.feedback = format!("set to {}", key_label(&combo));
                        self.feedback_at = Some(std::time::Instant::now());
                    }
                }
            }
            Message::ClearReturnHotkey => self.cfg.return_hotkey.clear(),
            Message::CopyLogPath => {
                self.feedback = "log path copied".into();
                return iced::clipboard::write(self.log_path.clone());
            }
            Message::OpenLogFolder => {
                let dir = std::path::Path::new(&self.log_path)
                    .parent()
                    .map(|d| d.to_path_buf())
                    .unwrap_or_default();
                let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
                self.feedback = match std::process::Command::new(opener).arg(&dir).spawn() {
                    Ok(_) => format!("opened {}", dir.display()),
                    Err(e) => format!("could not open {}: {e}", dir.display()),
                };
            }
            Message::AskRestoreDefaults => self.confirm_restore = true,
            Message::CancelRestoreDefaults => self.confirm_restore = false,
            Message::TabPicked(t) => {
                self.tab = t;
                self.menu_open = false;
            }
            Message::NameChanged(s) => self.cfg.name = s,
            Message::RolePicked(r) => self.cfg.role = r,
            Message::ConnectTo(id) => {
                let Some(name) = self.cfg.paired.iter().find(|d| d.id == id).map(|d| d.name.clone()) else {
                    return Task::none();
                };
                let own = self.cfg.device_id.clone();
                // Controlled: connect to this one only. With the mouse: let only this one in
                // until it arrives, even if another keeps asking (R45).
                mouse_core::trust::set_choice(&own, Some(&id));
                if self.cfg.role == Role::Server {
                    // Connected to all at once (R46): switching is instant, nothing restarts.
                    mouse_core::trust::reserve(&own, Some(&id));
                    self.feedback = format!("using {name}");
                    if !self.running() {
                        self.start_engine();
                    }
                    return Task::none();
                }
                self.cfg.connect_to = id;
                self.saved_cfg.connect_to = self.cfg.connect_to.clone();
                if let Some(live) = self.live_cfg.as_mut() {
                    live.connect_to = self.cfg.connect_to.clone();
                }
                let _ = self.cfg.save(&Config::default_path());
                tracing::info!("switching to {name}, as the user chose");
                self.feedback = format!("switching to {name}…");
                // Close the current link so the switch happens now.
                if self.running() {
                    return self.leave(Leave::Restart);
                }
                self.start_engine();
            }
            Message::MoveSwitch(i, up) => {
                let mut ids: Vec<String> = self.cfg.switch_targets().into_iter().map(|t| t.id).collect();
                let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < ids.len()) };
                if let Some(j) = j {
                    ids.swap(i, j);
                    // A shortcut that was the position's default moves with the position:
                    // only shortcuts the user typed stay with their computer.
                    self.cfg.switch_order = ids;
                }
            }
            Message::SwitchKeyDefault(i) => {
                if let Some(t) = self.cfg.switch_targets().get(i) {
                    self.cfg.switch_keys.remove(&t.id);
                }
            }
            Message::DisconnectPeer => {
                // Disconnect means stop working with it now: close the link, wait for no one.
                mouse_core::trust::reserve(&self.cfg.device_id, None);
                return self.leave(Leave::Stop);
            }
            Message::RemovePaired(id) => {
                if self.remove_armed.as_deref() != Some(id.as_str()) {
                    self.remove_armed = Some(id);
                    return Task::none();
                }
                self.remove_armed = None;
                let name = self.cfg.paired.iter().find(|d| d.id == id).map(|d| d.name.clone()).unwrap_or_default();
                // Refused from this moment, without restarting sharing (R44).
                mouse_core::trust::remove(&self.cfg.device_id, &id);
                self.cfg.paired.retain(|d| d.id != id);
                if self.cfg.connect_to == id {
                    self.cfg.connect_to.clear();
                    mouse_core::trust::set_choice(&self.cfg.device_id, None);
                }
                if mouse_core::trust::reserved(&self.cfg.device_id).as_deref() == Some(id.as_str()) {
                    mouse_core::trust::reserve(&self.cfg.device_id, None);
                }
                tracing::info!("removed the pairing with {name}");
                self.feedback = format!("removed {name}; it needs a new code to connect again");
                let _ = self.update(Message::SaveConfig);
                return self.refresh_login_copy();
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
            // One Clear for the whole history: the entries, and the received-file copies
            // only they were keeping. Two buttons with the same name doing different
            // things was a UI bug (owner, 2026-10-15). History first, so the cache sweep
            // keeps exactly what is still referenced: pinned entries and live transfers.
            Message::ClipClear => {
                if let Some(e) = &self.engine {
                    e.clipboard_clear();
                    let freed = e.cache_clear();
                    self.feedback = if freed == 0 {
                        "history cleared (pinned entries kept)".into()
                    } else {
                        format!("history cleared and {} of received files deleted (pinned kept)", human_bytes(freed as usize))
                    };
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
                self.clip_cache_gb_s = s;
            }
            Message::BulkPortChanged(s) => {
                // 0 (or an empty box) means "share the input link's port". A port is only
                // taken once it is one that can work; until then the old one stays.
                self.bulk_port_error.clear();
                match s.trim().parse::<u16>() {
                    _ if s.trim().is_empty() => self.cfg.bulk_port = 0,
                    Ok(0) => self.cfg.bulk_port = 0,
                    // Still typing: "2" on the way to "24802" is not an error yet.
                    Ok(_) if s.trim().len() < 4 => {}
                    Ok(v) => match lane_port_problem(v, &self.cfg, self.live_cfg.as_ref()) {
                        Some(why) => self.bulk_port_error = why,
                        None => self.cfg.bulk_port = v,
                    },
                    Err(_) => self.bulk_port_error = "A port is a number from 1024 to 65535, e.g. 24802.".into(),
                }
                self.bulk_port_s = s;
            }
            Message::UpdateCheckNow => {
                if self.update_idle() {
                    self.update_checked_at = Some(std::time::Instant::now());
                    crate::update::spawn_check(self.update.clone());
                }
            }
            Message::UpdateInstall => {
                if let crate::update::State::Available(m) = crate::update::get(&self.update) {
                    crate::update::spawn_install(self.update.clone(), m);
                }
            }
            Message::UpdateLater => self.update_dismissed = true,
            Message::UpdateRestart => return self.leave(Leave::Update),
            Message::UpdatesToggled(on) => self.cfg.check_updates = on,
            Message::OpenDownloadPage => {
                let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
                self.feedback = match std::process::Command::new(opener).arg(crate::update::DOWNLOAD_PAGE).spawn() {
                    Ok(_) => "opened the download page".into(),
                    Err(e) => format!("could not open {}: {e}", crate::update::DOWNLOAD_PAGE),
                };
            }
            Message::AllowFirewall => {
                self.feedback = match crate::autostart::firewall_allow_via_prompt() {
                    Ok(true) => {
                        self.firewall_ok = crate::autostart::firewall_rule_exists();
                        if self.firewall_ok {
                            "mouse-rs is allowed through Windows Firewall on every port".into()
                        } else {
                            "the firewall rule did not appear — see the log".into()
                        }
                    }
                    Ok(false) => "Windows prompt declined — nothing was changed".into(),
                    Err(e) => format!("{e}"),
                };
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
                // Turning it on always asks (owner, 28 Oct): what it costs, the PIN that will
                // guard the login screen, and then the system's own password prompt. Turning
                // it off needs no explanation.
                if v {
                    self.pin_new.clear();
                    self.pin_new2.clear();
                    self.login_dialog_error.clear();
                    self.login_screen_asking = true;
                } else {
                    self.apply_login_screen(false);
                }
            }
            #[cfg(target_os = "linux")]
            Message::ConfirmLoginScreen => {
                // A PIN first: the service copies the settings when it is installed, so the
                // PIN has to exist before the system prompt, or the login screen would be
                // open to anyone paired until the next reinstall.
                let has = !self.cfg.connection_pin.is_empty();
                if !has || !self.pin_new.is_empty() {
                    if !mouse_core::pin::is_valid(&self.pin_new) {
                        self.login_dialog_error = format!(
                            "The PIN needs {} to {} characters.",
                            mouse_core::pin::MIN_LEN,
                            mouse_core::pin::MAX_LEN
                        );
                        return Task::none();
                    }
                    if self.pin_new != self.pin_new2 {
                        self.login_dialog_error = "The two PINs differ.".into();
                        return Task::none();
                    }
                    self.cfg.connection_pin = std::mem::take(&mut self.pin_new);
                    self.pin_new2.clear();
                    tracing::info!("connection PIN set while turning on login-screen sharing");
                }
                self.cfg.login_pin_enabled = true;
                self.cfg.login_screen_notice_seen = true;
                self.login_dialog_error.clear();
                self.login_screen_asking = false;
                // Written before the install, which copies this file.
                let t = self.save_and_apply();
                self.apply_login_screen(true);
                return t;
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
            Message::RelaunchElevated => return self.leave(Leave::Elevate),
            Message::RelaunchElevatedNow => {
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
                // From a normal window this goes through Windows' own Yes/No prompt: the
                // owner asked for elevation to be granted the way other software asks for
                // it, not refused with "restart as administrator first".
                let res = if !mouse_input::elevation().is_limited() {
                    if v { crate::autostart::enable_elevated() } else { crate::autostart::disable_elevated() }
                } else {
                    self.feedback = "approve the Windows prompt to continue…".into();
                    match crate::autostart::set_elevated_via_prompt(v) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err(anyhow::anyhow!("Windows prompt declined — nothing was changed")),
                        Err(e) => Err(e),
                    }
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
            Message::SwitchProfile(name) => {
                self.menu_open = false;
                self.feedback = match Config::load_profile(&name) {
                    Ok(mut c) => {
                        // A profile holds preferences. Whom this computer is paired with,
                        // and who it is, stay as they are now: a profile saved before a
                        // pairing must not quietly undo it (R44).
                        c.device_id = self.cfg.device_id.clone();
                        c.paired = self.cfg.paired.clone();
                        c.connect_to = self.cfg.connect_to.clone();
                        c.screen_width = self.cfg.screen_width;
                        c.screen_height = self.cfg.screen_height;
                        self.cfg = c;
                        self.sync_buffers();
                        format!("switched to '{name}'")
                    }
                    Err(e) => format!("could not switch: {e}"),
                };
            }
            Message::DeleteProfile(name) => {
                self.feedback = match Config::delete_profile(&name) {
                    Ok(()) => format!("deleted '{name}'"),
                    Err(e) => format!("delete failed: {e}"),
                };
                self.profiles = load_profiles();
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
                        self.profiles = load_profiles();
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
            Message::Stop => return self.leave(Leave::Stop),
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
            Message::PinTyped(v) => self.pin_input = v.chars().take(mouse_core::pin::MAX_LEN).collect(),
            Message::PinSubmit => match mouse_core::pin::prompt() {
                Some((client, _)) if mouse_core::pin::is_valid(&self.pin_input) => {
                    mouse_core::pin::enter(std::mem::take(&mut self.pin_input));
                    self.feedback = format!("sent; {client} connects in a moment if the PIN is right");
                }
                Some(_) => {
                    self.feedback = format!("a PIN has at least {} characters", mouse_core::pin::MIN_LEN);
                }
                None => {}
            },
            Message::PairCodeTyped(i, v) => {
                let (code, next) = code_after_typing(&self.pair_code_input, i, &v, mouse_core::pairing::CODE_DIGITS);
                self.pair_code_input = code;
                if let Some(j) = next {
                    return Task::batch([
                        iced::widget::operation::focus(code_box_id(j)),
                        iced::widget::operation::select_all(code_box_id(j)),
                    ]);
                }
            }
            Message::PairWithCode => {
                if !mouse_core::pairing::is_valid_code(&self.pair_code_input) {
                    self.feedback = format!("the code is {} digits", mouse_core::pairing::CODE_DIGITS);
                    return Task::none();
                }
                if matches!(*self.pairing.lock().unwrap(), PairState::Running) {
                    return Task::none();
                }
                *self.pairing.lock().unwrap() = PairState::Running;
                self.feedback = "pairing…".into();
                let (cfg, code, slot) = (self.cfg.clone(), self.pair_code_input.clone(), self.pairing.clone());
                std::thread::spawn(move || {
                    let answer = pair_in_background(&cfg, &code);
                    *slot.lock().unwrap() = PairState::Done(answer);
                });
            }
            Message::ShowPairCode => {
                // Each pairing makes its own key (R44), so there is nothing to prepare.
                mouse_core::pairing::start_code();
                self.feedback = "pairing code shown; type it on the other computer".into();
                if !self.running() {
                    self.start_engine();
                }
            }
            Message::CancelPairCode => mouse_core::pairing::cancel_code(),
            Message::ToggleShowPin => self.show_pin = !self.show_pin,
            Message::PinNewTyped(v) => self.pin_new = v.chars().take(mouse_core::pin::MAX_LEN).collect(),
            Message::PinNew2Typed(v) => self.pin_new2 = v.chars().take(mouse_core::pin::MAX_LEN).collect(),
            Message::SavePin => {
                // No old password is asked for: being at this computer is the proof (owner,
                // 27 Oct). It is also how a forgotten password is replaced.
                if !mouse_core::pin::is_valid(&self.pin_new) {
                    self.feedback = format!(
                        "the password needs {} to {} characters; letters, numbers and symbols are all fine",
                        mouse_core::pin::MIN_LEN,
                        mouse_core::pin::MAX_LEN
                    );
                    return Task::none();
                }
                if self.pin_new != self.pin_new2 {
                    self.feedback = "the two passwords differ".into();
                    return Task::none();
                }
                let first = self.cfg.connection_pin.is_empty();
                self.cfg.connection_pin = std::mem::take(&mut self.pin_new);
                self.pin_new2.clear();
                // Setting a password for the first time is asking for it to be used.
                if first {
                    self.cfg.pin_enabled = true;
                }
                tracing::info!("connection password changed");
                let t = self.apply_pin_change();
                self.feedback = "password saved".into();
                return t;
            }
            Message::PinSessionToggled(on) => {
                if on && self.cfg.connection_pin.is_empty() {
                    self.feedback = "set a password below first".into();
                    return Task::none();
                }
                self.cfg.pin_enabled = on;
                tracing::info!(on, "password while logged in changed");
                return self.apply_pin_change();
            }
            #[cfg(target_os = "linux")]
            Message::PinLoginToggled(on) => {
                self.cfg.login_pin_enabled = on;
                tracing::info!(on, "password at the login screen changed");
                return self.apply_pin_change();
            }
            Message::PinReminderSet => {
                self.page = Some(Page::Security);
            }
            Message::PinReminderLater => {
                self.cfg.pin_reminder_day = today();
                let _ = self.cfg.save(&Config::default_path());
            }
            Message::PinReminderNever => {
                self.cfg.pin_reminder_off = true;
                let _ = self.cfg.save(&Config::default_path());
            }
            Message::NetworkPreferencePicked(v) => self.cfg.network_preference = v,
            Message::RestartNow => return self.leave(Leave::Restart),
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
                self.confirm_restore = false;
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
                // Asked for first (About → Restore…), then saved like any other change.
                let (secret, name, peer) =
                    (self.cfg.shared_secret.clone(), self.cfg.name.clone(), self.cfg.peer_name.clone());
                let (device_id, paired, connect_to) =
                    (self.cfg.device_id.clone(), self.cfg.paired.clone(), self.cfg.connect_to.clone());
                let (w, h) = screen_size()
                    .unwrap_or((self.cfg.screen_width, self.cfg.screen_height));
                self.cfg = Config {
                    shared_secret: secret,
                    // Who this computer is and whom it is paired with are not settings.
                    device_id,
                    paired,
                    connect_to,
                    name,
                    peer_name: peer,
                    screen_width: w,
                    screen_height: h,
                    ..Config::default()
                };
                self.sync_buffers();
                self.feedback = format!("defaults restored — kept your name, pairings and screen ({w}×{h})");
            }
            Message::Quit | Message::CloseRequested => return self.leave(Leave::Quit),
            Message::LeaveAnyway => {
                if let Some(how) = self.confirm_leave {
                    tracing::info!(?how, "left while a transfer was running, as the user chose");
                    return self.leave_now(how);
                }
            }
            Message::KeepGoing => self.confirm_leave = None,
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            iced::time::every(std::time::Duration::from_millis(150)).map(|_| Message::Tick),
            // The close button asks first, like Quit, so it can warn about a transfer.
            iced::window::close_requests().map(|_| Message::CloseRequested),
        ];
        if self.cfg.theme_follow_system {
            subs.push(iced::system::theme_changes().map(Message::SystemTheme));
        }
        if self.recording.is_some() {
            subs.push(iced::keyboard::listen().map(Message::KeyEvent));
        }
        Subscription::batch(subs)
    }

    /// The theme in use: the saved one, or the system's light/dark pick (R40).
    fn theme6(&self) -> Theme6 {
        match (self.cfg.theme_follow_system, self.system_dark) {
            (true, Some(true)) => Theme6::Graphite,
            (true, Some(false)) => Theme6::Porcelain,
            _ => Theme6::from_key(&self.cfg.theme),
        }
    }

    fn theme(&self) -> iced::Theme {
        if self.theme6().is_dark() {
            iced::Theme::Dark
        } else {
            iced::Theme::Light
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let th = self.theme6();
        let p = th.palette();
        let h = th.hero();
        let running = self.running();
        let snap = self.engine.as_ref().map(|e| e.status());

        // The top band: header, and on the main window the status card. On a mixed theme it
        // is painted in the dark top palette.
        let mut top = column![self.header(h, running, snap.as_ref())].spacing(14.0);
        if self.page.is_none() {
            top = top.push(self.hero(h, running, snap.as_ref()));
        }
        let top = container(top)
            .padding(Padding { top: 14.0, right: 20.0, bottom: 16.0, left: 20.0 })
            .width(Length::Fill)
            .style(move |_| ui_theme::root(h));

        let body: Element<'_, Message> = match self.page {
            None => {
                let mut col = column![].spacing(10.0);
                let banners = self.banners(p);
                if !banners.is_empty() {
                    let mut stack_col = column![].spacing(8.0);
                    for (i, (full, compact)) in banners.into_iter().enumerate() {
                        stack_col = stack_col.push(if i == 0 { full } else { compact });
                    }
                    col = col.push(container(stack_col).padding([0.0, 20.0]));
                }
                col.push(self.tab_bar(p))
                    .push(scroll(container(self.tab_body(p, running)).padding(Padding {
                        top: 14.0,
                        right: 20.0,
                        bottom: 16.0,
                        left: 20.0,
                    })))
                    .into()
            }
            Some(pg) => column![
                container(self.settings_bar(p, pg)).padding(Padding { top: 14.0, right: 20.0, bottom: 4.0, left: 20.0 }),
                scroll(container(self.settings_body(p, pg)).padding(Padding {
                    top: 4.0,
                    right: 20.0,
                    bottom: 16.0,
                    left: 20.0,
                })),
            ]
            .into(),
        };

        let page: Element<'_, Message> = container(
            column![top, container(body).height(Length::Fill), self.footer(p, running)],
        )
        .style(move |_| ui_theme::root(p))
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

        if let Some(how) = self.confirm_leave {
            return stack![page, self.leave_prompt(p, how)].into();
        }
        if self.confirm_restore {
            return stack![page, self.restore_prompt(p)].into();
        }
        #[cfg(target_os = "linux")]
        if self.login_screen_asking {
            return stack![page, self.login_screen_prompt(p)].into();
        }
        if !self.menu_open {
            return page;
        }
        // The menu floats over the page, right-aligned under the ⋯ button. The full-size
        // `mouse_area` beneath it closes the menu on any click elsewhere, which is what
        // makes it behave like a menu rather than a panel that has to be dismissed by
        // pressing the same button again.
        let dismiss = mouse_area(container(Space::new()).width(Length::Fill).height(Length::Fill))
            .on_press(Message::CloseMenu);
        let floating = container(column![
            Space::new().height(50.0),
            container(self.overflow_menu(p)).align_right(Length::Fill),
        ])
        .padding([0.0, 20.0])
        .width(Length::Fill)
        .height(Length::Fill);

        stack![page, dismiss, floating].into()
    }

    // ---------- dialogs ----------

    /// R33: quitting or stopping during a transfer asks once. Either answer is final:
    /// "Quit anyway" quits at once, wherever the transfer is; "Keep going" changes nothing.
    fn leave_prompt(&self, p: Palette, how: Leave) -> Element<'_, Message> {
        let what = self
            .transfer_running()
            .unwrap_or_else(|| "The transfer has just finished.".into());
        let (title, go, verb) = match how {
            Leave::Quit => ("Quit while a transfer is running?", "Quit anyway", "Quitting"),
            Leave::Stop => ("Stop sharing while a transfer is running?", "Stop anyway", "Stopping"),
            Leave::Restart => ("Restart sharing while a transfer is running?", "Restart anyway", "Restarting"),
            Leave::Elevate => ("Restart as administrator while a transfer is running?", "Restart anyway", "Restarting"),
            Leave::Update => ("Restart into the new version while a transfer is running?", "Restart anyway", "Restarting"),
        };
        dialog(
            p,
            column![
                text(title).size(16.0).font(BOLD).color(p.txt),
                text(format!("{what} {verb} stops it, and a half-pasted file stays incomplete."))
                    .size(13.0)
                    .color(p.txt2),
                row![
                    Space::new().width(Length::Fill),
                    btn_secondary(p, "Keep going", Some(Message::KeepGoing)),
                    btn_danger(p, go, Some(Message::LeaveAnyway)),
                ]
                .spacing(8.0),
            ]
            .spacing(12.0),
        )
    }

    /// About → Restore…: say what is kept before doing it.
    fn restore_prompt(&self, p: Palette) -> Element<'_, Message> {
        dialog(
            p,
            column![
                text("Restore default settings?").size(16.0).font(BOLD).color(p.txt),
                text("Every preference goes back to how it was on install. This computer's name, shared secret and screen size are kept.")
                    .size(13.0)
                    .color(p.txt2),
                row![
                    Space::new().width(Length::Fill),
                    btn_secondary(p, "Cancel", Some(Message::CancelRestoreDefaults)),
                    btn_danger(p, "Restore", Some(Message::RestoreDefaults)),
                ]
                .spacing(8.0),
            ]
            .spacing(12.0),
        )
    }

    /// The one-time explanation, shown only when turning the login-screen service on for
    /// the first time (D2). It appears at the moment the decision is being made, which is
    /// when the information is worth something; a permanent warning about a thing nobody
    /// asked for would be noise.
    #[cfg(target_os = "linux")]
    fn login_screen_prompt(&self, p: Palette) -> Element<'_, Message> {
        let peer = self.peer_name();
        let has = !self.cfg.connection_pin.is_empty();
        let pin: Element<'_, Message> = if has {
            note(p, p.good, "This computer's PIN is set. Whoever drives the login screen types it first.")
        } else {
            column![
                text("Set this computer's PIN").size(13.5).font(BOLD).color(p.txt),
                row![
                    input(p, "New PIN", &self.pin_new, Message::PinNewTyped).secure(true),
                    input(p, "Again", &self.pin_new2, Message::PinNew2Typed)
                        .secure(true)
                        .on_submit(Message::ConfirmLoginScreen),
                ]
                .spacing(8.0),
                text(format!(
                    "{peer} must type it before its keyboard reaches the login screen. 3 wrong tries lock it out for an hour."
                ))
                .size(11.5)
                .color(p.txt3),
            ]
            .spacing(8.0)
            .into()
        };
        let mut body = column![
            text("Share the keyboard before anyone logs in?").size(16.0).font(BOLD).color(p.txt),
            text(format!(
                "This installs a small system service that starts at boot, so {peer}'s keyboard \
                 works at the password screen and you no longer need a second one plugged in here."
            ))
            .size(13.0)
            .color(p.txt2),
            note(
                p,
                p.warn,
                "This computer will accept keystrokes over the network before anyone signs in. \
                 The shared secret and the PIN are what protect this login.",
            ),
            pin,
        ]
        .spacing(12.0);
        if !self.login_dialog_error.is_empty() {
            body = body.push(text(self.login_dialog_error.clone()).size(12.0).color(p.danger));
        }
        body = body
            .push(text("Next, your computer asks for your password to install it.").size(11.5).color(p.txt3))
            .push(
                row![
                    Space::new().width(Length::Fill),
                    btn_secondary(p, "Leave it off", Some(Message::CancelLoginScreen)),
                    btn_primary(p, if has { "Allow and turn on" } else { "Save PIN and turn on" }, Some(Message::ConfirmLoginScreen)),
                ]
                .spacing(8.0),
            );
        dialog(p, body)
    }

    // ---------- banners ----------

    /// The messages above the tabs (B1), in the agreed order. Each comes as a full banner
    /// and a one-line form: only the first is shown in full, so a pile of them never pushes
    /// the tabs off the window.
    fn banners(&self, p: Palette) -> Vec<(Element<'_, Message>, Element<'_, Message>)> {
        let mut v = Vec::new();
        if let Some(b) = self.restart_banner(p) {
            v.push(b);
        }
        if let Some(b) = self.pending_files_banner(p) {
            v.push(b);
        }
        if let Some(b) = self.elevation_notice(p) {
            v.push(b);
        }
        if let Some(b) = self.pin_reminder(p) {
            v.push(b);
        }
        if let Some(b) = self.pin_waiting_banner(p) {
            v.push(b);
        }
        if let Some(b) = self.update_banner(p) {
            v.push(b);
        }
        v
    }

    /// Whether a new check may start: not while one runs, and never over a download or
    /// a version already waiting for a restart.
    fn update_idle(&self) -> bool {
        use crate::update::State as U;
        matches!(crate::update::get(&self.update), U::Idle | U::UpToDate | U::CheckFailed(_) | U::Ready(_) | U::Failed(_))
    }

    /// "Version x.y is available" (R36). A failed check shows nothing: a network blip
    /// is not news. A failed install is, with the download page as the way out.
    fn update_banner(&self, p: Palette) -> Option<(Element<'_, Message>, Element<'_, Message>)> {
        use crate::update::State as U;
        match crate::update::get(&self.update) {
            U::Available(m) if !self.update_dismissed => {
                let title = format!("Version {} is available", m.version);
                let body = if m.notes.trim().is_empty() {
                    format!("You have {}.", crate::update::VERSION)
                } else {
                    format!("{} You have {}.", m.notes.trim(), crate::update::VERSION)
                };
                let compact = banner_line(p, p.accent, title.clone(), Some(("Update", Message::UpdateInstall)));
                let full = banner(
                    p,
                    p.accent,
                    title,
                    body,
                    row![
                        btn_primary(p, "Update", Some(Message::UpdateInstall)),
                        btn_secondary(p, "Later", Some(Message::UpdateLater)),
                    ]
                    .spacing(8.0)
                    .into(),
                );
                Some((full, compact))
            }
            U::Downloading { version, done, total } => {
                let pct = if total > 0 { done * 100 / total } else { 0 };
                let title = format!("Downloading version {version}… {pct}%");
                Some((
                    banner_line(p, p.accent, title.clone(), None),
                    banner_line(p, p.accent, title, None),
                ))
            }
            U::Ready(version) => {
                let title = format!("Version {version} is installed");
                let compact = banner_line(p, p.good, title.clone(), Some(("Restart mouse-rs", Message::UpdateRestart)));
                let full = banner(
                    p,
                    p.good,
                    title,
                    "It starts the next time mouse-rs starts. Restarting mouse-rs now takes a few \
                     seconds and only restarts this app, not the computer; the other computer \
                     reconnects by itself."
                        .into(),
                    btn_primary(p, "Restart mouse-rs", Some(Message::UpdateRestart)).into(),
                );
                Some((full, compact))
            }
            U::Failed(why) if !self.update_dismissed => {
                let compact = banner_line(p, p.warn, "The update did not install".into(), Some(("Download", Message::OpenDownloadPage)));
                let full = banner(
                    p,
                    p.warn,
                    "The update did not install",
                    format!("{why}. This version keeps working. You can download the new one from the website instead."),
                    row![
                        btn_primary(p, "Open download page", Some(Message::OpenDownloadPage)),
                        btn_secondary(p, "Later", Some(Message::UpdateLater)),
                    ]
                    .spacing(8.0)
                    .into(),
                );
                Some((full, compact))
            }
            _ => None,
        }
    }

    /// "Restart needed": the panel's only honest answer to "did my change take effect?".
    /// Saving a setting the engine already read does nothing to the live session.
    ///
    /// "Later" collapses it to the one-line form rather than hiding it, because the
    /// mismatch is still real after dismissal.
    fn restart_banner(&self, p: Palette) -> Option<(Element<'_, Message>, Element<'_, Message>)> {
        let pending = self.pending_changes();
        if pending.is_empty() {
            return None;
        }
        let n = pending.len();
        let plural = if n == 1 { "" } else { "s" };
        let compact = banner_line(
            p,
            p.warn,
            format!("{n} change{plural} wait for a restart"),
            Some(("Restart", Message::RestartNow)),
        );
        if self.restart_hint_dismissed {
            let again = banner_line(
                p,
                p.warn,
                format!("{n} change{plural} wait for a restart"),
                Some(("Restart", Message::RestartNow)),
            );
            return Some((again, compact));
        }
        let full = banner(
            p,
            p.warn,
            "Restart needed",
            format!("{n} saved change{plural} apply after a restart: {}.", pending.join(", ")),
            row![
                btn_primary(p, "Restart sharing now", Some(Message::RestartNow)),
                btn_secondary(p, "Later", Some(Message::RestartLater)),
            ]
            .spacing(8.0)
            .into(),
        );
        Some((full, compact))
    }

    /// "The other machine copied something big — want it?"
    ///
    /// A banner rather than a dialog, and that is not a style preference. Windows
    /// withholds `WH_KEYBOARD_LL` callbacks from a process while that process owns the
    /// foreground window — the exact root cause of the keyboard leak fixed on
    /// 2026‑07‑31. A pop-up that grabbed focus the moment a large file was copied would
    /// bring it straight back, mid-work, quite possibly mid-crossing. So this never calls
    /// `SetForegroundWindow` and never steals focus.
    fn pending_files_banner(&self, p: Palette) -> Option<(Element<'_, Message>, Element<'_, Message>)> {
        // Only while it is *waiting*. Once fetching — "Bring it over" pressed, or a paste
        // started it — its job is done; the history row carries the progress.
        let waiting: Vec<_> = self
            .engine
            .as_ref()?
            .clipboard_meta()
            .into_iter()
            .filter(|m| m.status == mouse_clipboard::ClipStatus::NeedsConsent)
            .collect();
        let first = waiting.first()?;
        let more = waiting.len().saturating_sub(1);
        let title = match more {
            0 => format!("{} is ready to paste", first.preview),
            n => format!("{} (and {n} more) are ready to paste", first.preview),
        };
        let body = format!(
            "{}, over your {} limit, so it only comes over when you paste it. {}",
            human_bytes(first.disk_bytes as usize),
            human_bytes(self.cfg.clipboard_auto_transfer_max as usize),
            // Linux offers it from the streaming folder, so a paste of any size starts at
            // once. Elsewhere the paste waits for the whole transfer.
            if cfg!(target_os = "linux") {
                "Pasting works now; this is only a heads-up."
            } else {
                "If it is large, bring it over first: a paste waits for the whole file."
            }
        );
        let compact = banner_line(p, p.accent, title.clone(), Some(("Bring it over", Message::ClipReuse(first.id))));
        // Three answers, because "yes" and "no" are not the only ones: the third says "stop
        // asking me about files this size", which is the real request behind repeatedly
        // pressing the first.
        let full = banner(
            p,
            p.accent,
            title,
            body,
            row![
                btn_primary(p, "Bring it over", Some(Message::ClipReuse(first.id))),
                btn_secondary(p, "Not now", Some(Message::ClipDecline(first.id))),
                btn_link(p, format!("Always allow up to {}", mb_ceil_label(first.disk_bytes)), Message::ClipAlwaysAllow(first.id)),
            ]
            .spacing(8.0)
            .align_y(Alignment::Center)
            .into(),
        );
        Some((full, compact))
    }

    /// Told once, then never again. Running without administrator rights is a perfectly
    /// reasonable way to use this, so it says the limitation plainly, offers the two ways
    /// to act on it, and goes away for good.
    fn elevation_notice(&self, p: Palette) -> Option<(Element<'_, Message>, Element<'_, Message>)> {
        if self.cfg.elevation_notice_seen || !mouse_input::elevation().is_limited() {
            return None;
        }
        let full = banner(
            p,
            p.txt3,
            "Running with normal permissions",
            "Fine for everyday use. While an administrator window is in front (an installer or a \
             permission prompt), Windows hides the mouse from mouse-rs, so the pointer can't cross \
             until you click away."
                .to_string(),
            row![
                btn_secondary(p, "Restart as administrator", Some(Message::RelaunchElevated)),
                btn_link(p, "Keep normal", Message::DismissElevationNotice),
            ]
            .spacing(8.0)
            .align_y(Alignment::Center)
            .into(),
        );
        let compact = banner_line(
            p,
            p.txt3,
            "Running with normal permissions".to_string(),
            Some(("Restart as administrator", Message::RelaunchElevated)),
        );
        Some((full, compact))
    }

    /// Client, main window, at most once a day: a nudge to set a PIN, until one is in use
    /// or the user says never (owner, 27 Oct).
    fn pin_reminder(&self, p: Palette) -> Option<(Element<'_, Message>, Element<'_, Message>)> {
        if self.cfg.role != Role::Client
            || self.cfg.pin_reminder_off
            || (self.cfg.pin_enabled && !self.cfg.connection_pin.is_empty())
            || self.cfg.pin_reminder_day == today()
            || self.page.is_some()
        {
            return None;
        }
        let full = banner(
            p,
            p.accent,
            "Protect this computer with a PIN",
            "Anyone on this network who has paired with this computer can control it. With a PIN, \
             they also need the PIN, and 3 wrong tries lock them out for an hour."
                .to_string(),
            row![
                btn_primary(p, "Set a PIN", Some(Message::PinReminderSet)),
                btn_secondary(p, "Not now", Some(Message::PinReminderLater)),
                btn_link(p, "Don't ask again", Message::PinReminderNever),
            ]
            .spacing(8.0)
            .align_y(Alignment::Center)
            .into(),
        );
        let compact = banner_line(
            p,
            p.accent,
            "Protect this computer with a PIN".to_string(),
            Some(("Set a PIN", Message::PinReminderSet)),
        );
        Some((full, compact))
    }

    /// Client: while a server is being refused over the PIN, say so.
    fn pin_waiting_banner(&self, p: Palette) -> Option<(Element<'_, Message>, Element<'_, Message>)> {
        if self.cfg.role != Role::Client || !self.running() || !mouse_core::pin::awaiting() {
            return None;
        }
        let full = banner(
            p,
            p.warn,
            "A computer is asking to control this one",
            "Type this computer's PIN on it to allow it.".to_string(),
            Space::new().height(0.0).into(),
        );
        let compact = banner_line(p, p.warn, "A computer is asking to control this one".to_string(), None);
        Some((full, compact))
    }

    // ---------- header and status card ----------

    fn header(&self, h: Palette, running: bool, snap: Option<&StatusSnapshot>) -> Element<'_, Message> {
        let (colour, label) = self.status_label(h, running, snap);
        let status = container(
            row![dot(colour, 7.0), text(label).size(12.0).color(h.txt2)]
                .spacing(7.0)
                .align_y(Alignment::Center),
        )
        .style(move |_| ui_theme::pill(h))
        .padding([4.0, 11.0]);

        row![
            text("mouse-rs").size(15.0).font(BOLD).color(h.txt).width(Length::Fill),
            status,
            button(text("⋯").size(16.0).color(h.txt2))
                .style(ui_theme::icon_button(h, self.menu_open))
                .padding([3.0, 10.0])
                .on_press(Message::ToggleMenu),
        ]
        .spacing(10.0)
        .align_y(Alignment::Center)
        .into()
    }

    /// The pill in the header: one word or two for the state of the link.
    fn status_label(&self, h: Palette, running: bool, snap: Option<&StatusSnapshot>) -> (iced::Color, String) {
        if !running {
            return if self.paired() { (h.txt3, "Stopped".into()) } else { (h.txt3, "Not set up".into()) };
        }
        if self.server_pin_prompt().is_some() {
            return (h.warn, "PIN needed".into());
        }
        match snap {
            Some(s) if s.connected => (h.good, "Connected".into()),
            Some(StatusSnapshot { refused: Some(_), .. }) => (h.danger, "Refused".into()),
            _ => (h.warn, format!("Waiting for {}…", self.peer_name())),
        }
    }

    /// Has this computer been paired with anyone?
    fn paired(&self) -> bool {
        !self.cfg.paired.is_empty()
    }

    /// Save pairings the link made (on either side) that are not in the file yet. True if
    /// there were any.
    fn save_pairings(&mut self) -> bool {
        let new = mouse_core::trust::take_unsaved(&self.cfg.device_id);
        if new.is_empty() {
            return false;
        }
        for dev in new {
            self.cfg.paired.retain(|d| d.id != dev.id);
            self.cfg.paired.push(dev.clone());
            // A newly paired computer becomes the one in use (R45).
            self.cfg.connect_to = dev.id.clone();
            mouse_core::trust::set_choice(&self.cfg.device_id, Some(&dev.id));
            self.saved_cfg.connect_to = dev.id.clone();
            if let Some(live) = self.live_cfg.as_mut() {
                live.connect_to = dev.id.clone();
            }
            // Keep `saved_cfg` and `live_cfg` in step too: a pairing is not a setting that
            // waits for a restart.
            self.saved_cfg.paired = self.cfg.paired.clone();
            if let Some(live) = self.live_cfg.as_mut() {
                live.paired = self.cfg.paired.clone();
            }
        }
        // Once paired, sharing starts by itself whenever mouse-rs starts, as a new user
        // expects: a restart no longer needs "Start sharing" on both computers. It can be
        // turned off in Settings.
        if !self.cfg.autoconnect {
            tracing::info!("paired: sharing now starts by itself when mouse-rs starts");
            self.cfg.autoconnect = true;
            self.saved_cfg.autoconnect = true;
        }
        if let Err(e) = self.cfg.save(&Config::default_path()) {
            tracing::warn!("could not save the new pairing: {e}");
        }
        true
    }

    /// The login screen keeps its own copy of the settings, pairings included. After a
    /// pairing is added or removed, refresh it if that service is on (it asks for the
    /// computer's password through the desktop).
    fn refresh_login_copy(&mut self) -> Task<Message> {
        #[cfg(target_os = "linux")]
        if self.login_screen.on() {
            self.apply_login_screen(true);
        }
        Task::none()
    }

    /// The client asking this server for its PIN, if any.
    fn server_pin_prompt(&self) -> Option<(String, mouse_core::pin::Prompt)> {
        if self.cfg.role != Role::Server || !self.running() {
            return None;
        }
        mouse_core::pin::prompt()
    }

    /// The other computer's name as best known: live, else from the last connection, else
    /// the address typed for it, else a plain description.
    fn peer_name(&self) -> String {
        if let Some(s) = self.engine.as_ref().map(|e| e.status()).filter(|s| s.connected) {
            return s.peer.trim_end_matches(" (login screen)").to_string();
        }
        if let Some((name, _, _)) = &self.last_peer {
            return name.clone();
        }
        if !self.cfg.peer_name.trim().is_empty() {
            return self.cfg.peer_name.trim().to_string();
        }
        if self.cfg.role == Role::Client && !self.cfg.server_host.trim().is_empty() {
            return self.cfg.server_host.trim().to_string();
        }
        "the other computer".into()
    }

    /// The status card's sentence: a title and one line under it.
    fn hero_text(&self, running: bool, snap: Option<&StatusSnapshot>) -> (String, String) {
        let enc = if self.cfg.tls_enabled { "encrypted" } else { "not encrypted" };
        if !self.paired() && !running {
            return ("Let's connect a second computer".into(), "Takes about a minute, once".into());
        }
        if !running {
            let peer = self.peer_name();
            return ("Sharing is off".into(), format!("Start sharing to connect to {peer} again"));
        }
        if let Some((client, _)) = self.server_pin_prompt() {
            return (format!("{client} wants its PIN"), format!("Type it below to start controlling {client}"));
        }
        match snap {
            Some(s) if s.connected => {
                let peer = s.peer.trim_end_matches(" (login screen)").to_string();
                let login = if s.peer_at_login_screen { " · at its login screen" } else { "" };
                match (self.cfg.role, s.engaged) {
                    (Role::Server, true) => (
                        format!("Controlling {peer}"),
                        format!("You're on {peer} now · {} brings you back · {enc}{login}", key_label(&self.cfg.lock_hotkey)),
                    ),
                    (Role::Server, false) => (
                        format!("Controlling {peer}"),
                        format!("Push past the {} edge to use {peer} · {enc}{login}", self.cfg.exit_edge.label()),
                    ),
                    (Role::Client, true) => {
                        (format!("Connected to {peer}"), format!("{peer} is controlling this computer · {enc}"))
                    }
                    (Role::Client, false) => (
                        format!("Connected to {peer}"),
                        format!("{peer} takes over when its pointer crosses · {enc}"),
                    ),
                }
            }
            Some(StatusSnapshot { refused: Some(why), .. }) => ("Couldn't connect".into(), refusal_text(why)),
            _ => {
                let peer = self.peer_name();
                (format!("Waiting for {peer}"), format!("It will connect as soon as {peer} is reachable"))
            }
        }
    }

    /// The two computers, the one with the mouse outlined (R31, R40). It answers the only
    /// question that changes moment to moment — which machine has the mouse and keyboard.
    fn hero(&self, h: Palette, running: bool, snap: Option<&StatusSnapshot>) -> Element<'_, Message> {
        // Exactly one tile is lit, and it is always the machine that currently has the
        // mouse and keyboard. See `control_is_local` for why the role has to be folded in.
        let local_has_control = control_is_local(self.cfg.role, snap);
        let (title, sub) = self.hero_text(running, snap);

        let local = machine_tile(
            h,
            &self.cfg.name,
            &format!("This computer · {}×{}", self.cfg.screen_width, self.cfg.screen_height),
            local_has_control,
            true,
        );
        let peer = match (snap, self.server_pin_prompt()) {
            (Some(s), _) if s.connected => machine_tile(
                h,
                s.peer.trim_end_matches(" (login screen)"),
                &format!("{}×{}", s.peer_w, s.peer_h),
                !local_has_control,
                true,
            ),
            (_, Some((client, _))) => machine_tile(h, &client, "Asking for PIN", false, false),
            _ if !self.paired() => machine_tile(h, "Waiting…", "Not connected", false, false),
            _ => machine_tile(h, &self.peer_label(), "Not connected", false, false),
        };
        let swap = hint(
            h,
            button(container(text("⇄").size(14.0).color(h.txt2)).center(Length::Fixed(18.0)))
                .style(ui_theme::round_button(h))
                .padding(6.0)
                .on_press(Message::SwapRoles),
            "Swap roles: this computer shares its mouse, or is controlled. The other computer needs the opposite.",
        );

        container(
            column![
                column![
                    text(title).size(21.0).font(BOLD).color(h.txt),
                    text(sub).size(12.5).color(h.txt2),
                ]
                .spacing(3.0),
                row![local, swap, peer].spacing(10.0).align_y(Alignment::Center),
            ]
            .spacing(14.0),
        )
        .style(move |_| ui_theme::hero(h))
        .padding(16.0)
        .width(Length::Fill)
        .into()
    }

    // ---------- tabs ----------

    fn tab_bar(&self, p: Palette) -> Element<'_, Message> {
        let count = self
            .engine
            .as_ref()
            .map(|e| e.clipboard_usage().0)
            .filter(|n| *n > 0);
        let tab = |t: Tab, count: Option<usize>| -> Element<'_, Message> {
            let active = t == self.tab;
            let mut label = row![text(t.label()).size(13.5).font(if active { BOLD } else { iced::Font::DEFAULT })]
                .spacing(6.0)
                .align_y(Alignment::Center);
            // Wide enough for the label: a Fill underline inside a Shrink column would take
            // the whole row.
            let mut w = t.label().len() as f32 * 7.6;
            if let Some(n) = count {
                label = label.push(text(n.to_string()).size(11.5).color(p.txt3).font(BOLD));
                w += 8.0 + n.to_string().len() as f32 * 7.0;
            }
            column![
                button(label).style(ui_theme::tab(p, active)).padding([10.0, 0.0]).on_press(Message::TabPicked(t)),
                container(Space::new().height(2.0))
                    .width(w)
                    .height(2.0)
                    .style(move |_| ui_theme::rule(if active { p.accent } else { iced::Color::TRANSPARENT })),
            ]
            .into()
        };
        column![
            container(
                Tab::ALL
                    .into_iter()
                    .fold(row![].spacing(22.0), |r, t| r.push(tab(t, if t == Tab::Clipboard { count } else { None }))),
            )
            .padding([0.0, 20.0]),
            hairline(p),
        ]
        .into()
    }

    fn tab_body(&self, p: Palette, running: bool) -> Element<'_, Message> {
        match self.tab {
            Tab::Connection if !self.paired() => self.first_run(p, running),
            Tab::Connection => self.tab_connection(p, running),
            Tab::Clipboard => self.tab_clipboard(p),
        }
    }

    /// The main window's Connection tab once paired (M1–M3): who this computer is, how the
    /// two find each other, and the way back. Everything else is in ⋯ → Settings.
    fn tab_connection(&self, p: Palette, _running: bool) -> Element<'_, Message> {
        let server_side = self.cfg.role == Role::Server;
        let connected_peer = self
            .engine
            .as_ref()
            .map(|e| e.status())
            .filter(|s| s.connected)
            .map(|s| s.peer.trim_end_matches(" (login screen)").to_string());

        let mut col = column![].spacing(12.0);
        // First, while a client is waiting for its PIN: it is the one thing to do now.
        if let Some(pin) = self.pin_prompt_view(p) {
            col = col.push(pin);
        }

        let role_sub = match (server_side, &connected_peer) {
            (true, Some(peer)) => format!("Server · its mouse drives {peer}"),
            (true, None) => "Server".to_string(),
            (false, Some(peer)) => format!("Client · driven by {peer}"),
            (false, None) => "Client".to_string(),
        };
        let mut rows: Vec<Element<'_, Message>> = vec![srow(p, "This computer", role_sub, self.role_switch(p))];

        // Connection details: folded to one line, opened in place.
        let (summary_value, summary_sub): (Element<'_, Message>, String) = if server_side {
            let local = own_addresses();
            let v = match local.len() {
                0 => text("no address").size(12.5).color(p.txt3).into(),
                1 => mono(p, local[0].clone()),
                n => row![mono(p, local[0].clone()), text(format!("+{}", n - 1)).size(11.5).color(p.txt3)]
                    .spacing(6.0)
                    .align_y(Alignment::Center)
                    .into(),
            };
            (v, "Addresses and pairing".into())
        } else {
            let v = if self.cfg.server_host.trim().is_empty() {
                text("found automatically").size(12.5).color(p.txt2).into()
            } else {
                mono(p, self.cfg.server_host.trim().to_string())
            };
            (v, format!("Address and pairing · paired with {}", self.peer_name()))
        };
        rows.push(
            button(
                row![
                    labels(p, "Connection details", if self.details_open { "Open".to_string() } else { summary_sub }),
                    if self.details_open { Space::new().width(0.0).into() } else { summary_value },
                    text(if self.details_open { "⌄" } else { "›" }).size(15.0).color(p.txt3),
                ]
                .spacing(10.0)
                .align_y(Alignment::Center),
            )
            .style(ui_theme::row_button(p))
            .padding([12.0, 14.0])
            .width(Length::Fill)
            .on_press(Message::ToggleDetails)
            .into(),
        );
        if self.details_open {
            if server_side {
                rows.push(self.addresses_row(p));
                rows.push(self.pair_server_row(p));
            } else {
                rows.push(self.server_address_row(p));
                rows.push(self.network_preference_row(p));
                rows.push(stacked(p, "Pair with a new code", self.pair_client_view(p), ""));
            }
        }
        if !server_side {
            let has = !self.cfg.connection_pin.is_empty();
            let value = match (has, self.cfg.pin_enabled, self.cfg.login_pin_enabled && cfg!(target_os = "linux")) {
                (false, _, _) => "Not set",
                (true, true, _) => "On · asked when connecting",
                (true, false, true) => "At the login screen only",
                (true, false, false) => "Off",
            };
            rows.push(nav(p, "Connection PIN", "", value_text(p, value), Message::OpenSettings(Page::Security)));
        }
        rows.push(nav(p, "Take control back", "", keycaps(p, &self.cfg.lock_hotkey), Message::OpenSettings(Page::Keys)));
        col = col.push(group(p, rows));

        // Which paired computer to work with, like choosing a Bluetooth device (R45).
        if !self.cfg.paired.is_empty() {
            let heading = if server_side { "Computers · switch with Ctrl+Alt+Shift+number" } else { "Paired computers" };
            col = col
                .push(text(heading).size(12.5).font(BOLD).color(p.txt2))
                .push(group(p, self.paired_switch_rows(p, connected_peer.is_some())));
        }

        // No recent-clipboard list here: the Clipboard tab beside this one has it (R47).
        col.into()
    }

    /// "Shares mouse | Is controlled".
    fn role_switch(&self, p: Palette) -> Element<'_, Message> {
        segmented(
            p,
            vec![
                ("Shares mouse".into(), self.cfg.role == Role::Server, Message::RolePicked(Role::Server)),
                ("Is controlled".into(), self.cfg.role == Role::Client, Message::RolePicked(Role::Client)),
            ],
        )
    }

    /// Server: the addresses the other computer can use.
    fn addresses_row(&self, p: Palette) -> Element<'_, Message> {
        let local = own_addresses();
        let chips: Element<'_, Message> = if local.is_empty() {
            text("none found").size(12.5).color(p.txt3).into()
        } else {
            let mut r = row![].spacing(6.0);
            for a in local {
                r = r.push(container(text(a).size(12.5).font(MONO).color(p.txt)).padding([4.0, 9.0]).style(move |_| ui_theme::chip(p)));
            }
            r.wrap().vertical_spacing(6.0).into()
        };
        stacked(
            p,
            "This computer's addresses",
            chips,
            "The other computer finds this one by itself. Type one of these there only if it doesn't.",
        )
    }

    /// Client: the address of the computer with the mouse, Detect, and what Detect found.
    fn server_address_row(&self, p: Palette) -> Element<'_, Message> {
        let scanning = matches!(*self.scan.lock().unwrap(), ScanState::Running);
        let detect = hint(
            p,
            btn_secondary(p, if scanning { "Scanning…" } else { "Detect" }, (!scanning).then_some(Message::DetectServers)),
            "Searches the networks this computer is on for the other computer running \
             mouse-rs as the server, and fills in its address. Takes a few seconds.",
        );
        let mut col = column![
            row![
                input(p, "Press Detect, or type its IP", &self.cfg.server_host, Message::ServerHostChanged).font(MONO),
                detect,
            ]
            .spacing(8.0)
            .align_y(Alignment::Center),
        ]
        .spacing(8.0);
        // Only offered once a sweep has actually found something — an empty list would imply
        // the feature is broken rather than that nothing answered.
        for found in &self.found_servers {
            let used = *found == self.cfg.server_host;
            col = col.push(
                container(
                    row![
                        dot(p.good, 7.0),
                        text(found.clone()).size(12.5).font(MONO).color(p.txt).width(Length::Fill),
                        if used {
                            Element::from(text("In use").size(12.0).color(p.txt3))
                        } else {
                            btn_link(p, "Use", Message::ServerPicked(found.clone()))
                        },
                    ]
                    .spacing(10.0)
                    .align_y(Alignment::Center),
                )
                .padding([6.0, 12.0])
                .style(move |_| ui_theme::note(p))
                .width(Length::Fill),
            );
        }
        stacked(
            p,
            "The computer with the mouse",
            col.into(),
            "Usually found by itself. Press Detect to search your network, or type its IP address.",
        )
    }

    /// Client: which network to use when the server answers on both (R39).
    fn network_preference_row(&self, p: Palette) -> Element<'_, Message> {
        use mouse_core::NetworkPreference as N;
        let pick = self.cfg.network_preference;
        srow(
            p,
            "Preferred network",
            "When both work, use this one; the other takes over if it fails",
            segmented(
                p,
                vec![
                    ("Cable".into(), pick == N::Cable, Message::NetworkPreferencePicked(N::Cable)),
                    ("Wi-Fi".into(), pick == N::Wifi, Message::NetworkPreferencePicked(N::Wifi)),
                    ("Fastest".into(), pick == N::Fastest, Message::NetworkPreferencePicked(N::Fastest)),
                ],
            ),
        )
    }

    /// Server: "Pair a new device", or the code being shown.
    fn pair_server_row(&self, p: Palette) -> Element<'_, Message> {
        match mouse_core::pairing::current_code() {
            Some(_) => container(self.pair_server_view(p)).padding([12.0, 14.0]).into(),
            None => srow(
                p,
                "Pair a new device",
                "Shows a 6-digit code to type on the new computer",
                btn_secondary(p, "Pair a new device", Some(Message::ShowPairCode)),
            ),
        }
    }

    /// The code itself, big, with how long it lasts.
    fn pair_server_view(&self, p: Palette) -> Element<'_, Message> {
        match mouse_core::pairing::current_code() {
            Some((code, left)) => column![
                text("Type this code on the new computer, in its main window:").size(12.5).color(p.txt2),
                text(format!("{}  {}", &code[..3], &code[3..])).size(38.0).font(MONO).color(p.accent),
                row![
                    text(format!("Valid for {}:{:02} · works once", left.as_secs() / 60, left.as_secs() % 60))
                        .size(11.5)
                        .color(p.txt3)
                        .width(Length::Fill),
                    btn_secondary(p, "Cancel", Some(Message::CancelPairCode)),
                ]
                .align_y(Alignment::Center),
            ]
            .spacing(8.0)
            .into(),
            None => row![
                text("Only for setting up a new computer. Press it, then type the 6-digit code into that computer's mouse-rs window.")
                    .size(12.0)
                    .color(p.txt3)
                    .width(Length::Fill),
                btn_primary(p, "Pair a new device", Some(Message::ShowPairCode)),
            ]
            .spacing(12.0)
            .align_y(Alignment::Center)
            .into(),
        }
    }

    /// Client: the six code boxes and Pair.
    fn pair_client_view(&self, p: Palette) -> Element<'_, Message> {
        let busy = matches!(*self.pairing.lock().unwrap(), PairState::Running);
        row![
            code_boxes(p, &self.pair_code_input, mouse_core::pairing::CODE_DIGITS, Message::PairCodeTyped, Message::PairWithCode),
            Space::new().width(Length::Fill),
            btn_primary(p, if busy { "Pairing…" } else { "Pair" }, (!busy).then_some(Message::PairWithCode)),
        ]
        .spacing(10.0)
        .align_y(Alignment::Center)
        .into()
    }

    /// Before pairing, the Connection tab is a 3-step setup (M4). After pairing it folds
    /// into the compact list.
    fn first_run(&self, p: Palette, _running: bool) -> Element<'_, Message> {
        let server_side = self.cfg.role == Role::Server;
        let choice = |title: &'static str, desc: &'static str, role: Role| -> Element<'_, Message> {
            button(
                column![
                    text(title).size(13.5).font(BOLD).color(p.txt),
                    text(desc).size(11.5).color(p.txt3),
                ]
                .spacing(4.0),
            )
            .style(ui_theme::choice(p, self.cfg.role == role))
            .padding([12.0, 14.0])
            .width(Length::Fill)
            .on_press(Message::RolePicked(role))
            .into()
        };
        let one = step(
            p,
            1,
            "What does this computer do?",
            row![
                choice("Shares its mouse", "Server. Its mouse and keyboard drive the other one.", Role::Server),
                choice("Is controlled", "Client. Uses the other computer's mouse.", Role::Client),
            ]
            .spacing(10.0)
            .into(),
        );
        let (two, three) = if server_side {
            (
                step(
                    p,
                    2,
                    "Show a pairing code",
                    column![self.pair_server_view(p)].spacing(8.0).into(),
                ),
                step(
                    p,
                    3,
                    "Type it on the other computer",
                    text("In mouse-rs on the other computer, choose \"Is controlled\" and type the code under \"Pair with its code\". The two then find each other by themselves.")
                        .size(12.0)
                        .color(p.txt3)
                        .into(),
                ),
            )
        } else {
            let mut find = column![self.server_address_inline(p)].spacing(8.0);
            for found in &self.found_servers {
                find = find.push(
                    container(
                        row![
                            dot(p.good, 7.0),
                            text(found.clone()).size(12.5).font(MONO).color(p.txt).width(Length::Fill),
                            btn_link(p, "Use", Message::ServerPicked(found.clone())),
                        ]
                        .spacing(10.0)
                        .align_y(Alignment::Center),
                    )
                    .padding([6.0, 12.0])
                    .style(move |_| ui_theme::note(p))
                    .width(Length::Fill),
                );
            }
            (
                step(p, 2, "Find the computer with the mouse", find.into()),
                step(
                    p,
                    3,
                    "Pair with its code",
                    column![
                        self.pair_client_view(p),
                        text("On the computer with the mouse, press \"Pair a new device\", then type the code it shows here.")
                            .size(11.5)
                            .color(p.txt3),
                    ]
                    .spacing(8.0)
                    .into(),
                ),
            )
        };
        column![one, two, three].spacing(18.0).into()
    }

    /// The address box and Detect, without a row around them (first run).
    fn server_address_inline(&self, p: Palette) -> Element<'_, Message> {
        let scanning = matches!(*self.scan.lock().unwrap(), ScanState::Running);
        row![
            input(p, "Press Detect, or type its IP", &self.cfg.server_host, Message::ServerHostChanged).font(MONO),
            btn_secondary(p, if scanning { "Scanning…" } else { "Detect" }, (!scanning).then_some(Message::DetectServers)),
        ]
        .spacing(8.0)
        .align_y(Alignment::Center)
        .into()
    }

    /// Server, main window: the box for the client's PIN, while a client is asking for it (M3).
    fn pin_prompt_view(&self, p: Palette) -> Option<Element<'_, Message>> {
        use mouse_core::pin::Prompt;
        let (client, why) = self.server_pin_prompt()?;
        let (line, colour) = match why {
            Prompt::Needed => (
                format!("It's shown on {client} under Settings › Security & pairing."),
                p.txt2,
            ),
            Prompt::Wrong(left) => (format!("Wrong PIN. {left} tries left before {client} stops accepting PINs for a while."), p.danger),
            Prompt::Locked(until) => {
                let secs = until.saturating_duration_since(std::time::Instant::now()).as_secs();
                (format!("Too many wrong PINs. {client} accepts none for {} min.", secs.div_ceil(60).max(1)), p.danger)
            }
        };
        Some(
            container(
                column![
                    text(format!("{client} is protected with a PIN")).size(15.0).font(BOLD).color(p.txt),
                    text(line).size(12.0).color(colour),
                    row![
                        input(p, "PIN", &self.pin_input, Message::PinTyped)
                            .secure(true)
                            .on_submit(Message::PinSubmit),
                        btn_primary(p, "Connect", Some(Message::PinSubmit)),
                    ]
                    .spacing(10.0)
                    .align_y(Alignment::Center),
                ]
                .spacing(8.0),
            )
            .padding(16.0)
            .style(move |_| ui_theme::group_accent(p))
            .width(Length::Fill)
            .into(),
        )
    }

    // ---------- clipboard tab ----------

    /// The clipboard manager: a list you can act on, not a log you can only read (M5).
    ///
    /// Every row is a button that puts that item back on the clipboard (and sends it to
    /// the peer, exactly as a fresh copy would).
    fn tab_clipboard(&self, p: Palette) -> Element<'_, Message> {
        let entries = self.engine.as_ref().map(|e| e.clipboard_meta()).unwrap_or_default();
        if entries.is_empty() {
            return container(
                column![
                    text("No clipboard history yet").size(14.0).font(BOLD).color(p.txt),
                    text(if self.running() {
                        "Copy something on either computer."
                    } else {
                        "Copy something on either computer once sharing is on."
                    })
                    .size(12.0)
                    .color(p.txt3),
                ]
                .spacing(6.0)
                .align_x(Alignment::Center),
            )
            .center_x(Length::Fill)
            .padding([90.0, 0.0])
            .into();
        }

        // What the user's own limits are costing right now — shown rather than left to be
        // discovered later.
        let usage = match self.engine.as_ref().map(|e| e.clipboard_usage()) {
            Some((_, bytes)) => format!(
                "Click an item to copy it again · {} of {}",
                human_bytes(bytes),
                human_bytes(self.cfg.clipboard_memory_budget)
            ),
            None => "Click an item to copy it again".into(),
        };
        let header = row![
            column![text("History").size(14.0).font(BOLD).color(p.txt), text(usage).size(11.5).color(p.txt3)]
                .spacing(2.0)
                .width(Length::Fill),
            hint(
                p,
                btn_secondary(p, "Clear", Some(Message::ClipClear)),
                "Clear the history and delete mouse-rs's own copies of received files. \
                 Pinned entries and transfers still running are kept; files you have \
                 pasted somewhere are never touched.",
            ),
        ]
        .align_y(Alignment::Center);

        let mut list = column![].spacing(8.0);
        for m in &entries {
            list = list.push(self.clip_row(p, m));
        }

        // What the transferred files are costing on disk, so "is this program eating my
        // drive?" has an answer.
        let disk: Element<'_, Message> = match self.engine.as_ref().and_then(|e| e.cache_usage()) {
            Some((bytes, count)) if count > 0 => text(format!(
                "Received files kept by mouse-rs: {} in {count} file{}",
                human_bytes(bytes as usize),
                if count == 1 { "" } else { "s" }
            ))
            .size(11.5)
            .color(if bytes > self.cfg.clipboard_cache_budget { p.warn } else { p.txt3 })
            .into(),
            _ => Space::new().height(0.0).into(),
        };

        column![
            header,
            list,
            disk,
            btn_link(p, "Clipboard settings ›", Message::OpenSettings(Page::Clipboard)),
        ]
        .spacing(12.0)
        .into()
    }

    /// One history entry: kind, name, its state in the user's words, and Stop/Pin/×.
    fn clip_row(&self, p: Palette, m: &mouse_clipboard::ClipMeta) -> Element<'_, Message> {
        let what = if m.incoming { "Received" } else { "Sent" };
        // The status line is the entry's state in the user's words. Every one of these is a
        // situation they can really be in, and the alternative is leaving them to guess.
        let (note_text, note_col) = match &m.status {
            mouse_clipboard::ClipStatus::Ready if m.is_files => {
                (format!("{what} · {} · ready", human_bytes(m.disk_bytes as usize)), p.txt3)
            }
            mouse_clipboard::ClipStatus::Ready => (format!("{what} · {}", human_bytes(m.bytes)), p.txt3),
            // A small file being prefetched, or a large one that streams in when pasted.
            // Either way, pasting works now.
            mouse_clipboard::ClipStatus::Remote => (
                format!("{} · on the other computer · paste any time", human_bytes(m.disk_bytes as usize)),
                p.accent,
            ),
            mouse_clipboard::ClipStatus::NeedsConsent | mouse_clipboard::ClipStatus::Declined => (
                format!("{} · paste it anywhere", human_bytes(m.disk_bytes as usize)),
                p.txt3,
            ),
            mouse_clipboard::ClipStatus::Fetching { done, total } => {
                let pct = if *total > 0 { done * 100 / total } else { 0 };
                // Speed and time left, measured from this entry's own progress between
                // ticks: "22% of 6.8 GB" answers neither question being asked while
                // watching a transfer — how fast, and how much longer.
                let tail = match self.clip_rates.get(&m.id).copied() {
                    Some(bps) if bps > 0 => format!(
                        " · {}/s · {} left",
                        human_bytes(bps as usize),
                        human_duration(total.saturating_sub(*done) / bps.max(1))
                    ),
                    _ => String::new(),
                };
                (format!("{pct}% of {}{tail}", human_bytes(*total as usize)), p.accent)
            }
            mouse_clipboard::ClipStatus::Unavailable(why) => (why.clone(), p.danger),
        };
        let mut info = column![
            text(m.preview.clone())
                .size(13.5)
                .font(if m.is_files || m.is_image { iced::Font::DEFAULT } else { MONO })
                .color(p.txt)
                .width(Length::Fill)
                // A file name has no spaces to break at, so it may break anywhere.
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
            text(note_text)
                .size(11.5)
                .color(note_col)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(3.0)
        .width(Length::Fill);
        let fetching = matches!(m.status, mouse_clipboard::ClipStatus::Fetching { .. });
        if let mouse_clipboard::ClipStatus::Fetching { done, total } = &m.status {
            let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
            info = info.push(progress_bar(0.0..=1.0, frac).girth(3.0).style(ui_theme::progress(p)));
        }
        // The row itself is the re-use action — the thing wanted nine times out of ten.
        let reuse = button(row![kind_badge(p, m), info].spacing(12.0).align_y(Alignment::Center))
            .style(ui_theme::row_button(p))
            .padding(0.0)
            .width(Length::Fill)
            .on_press(Message::ClipReuse(m.id));

        let mut controls = row![reuse].spacing(8.0).align_y(Alignment::Center);
        // Stop only exists while there is something to stop. What has already arrived is
        // kept, so pressing it is cheap to undo.
        if fetching {
            controls = controls.push(hint(
                p,
                button(text("Stop").size(11.5)).style(ui_theme::secondary(p)).padding([4.0, 9.0]).on_press(Message::ClipCancel(m.id)),
                "Stop this transfer. What has arrived is kept, so clicking the entry resumes it.",
            ));
        }
        controls = controls.push(hint(
            p,
            button(text(if m.pinned { "Pinned" } else { "Pin" }).size(11.5).font(BOLD).color(if m.pinned { p.accent } else { p.txt3 }))
                .style(ui_theme::row_button(p))
                .padding([4.0, 6.0])
                .on_press(Message::ClipTogglePin(m.id)),
            if m.pinned {
                "Pinned — kept when the history fills up or is cleared. Click to unpin."
            } else {
                "Pin this, so it is kept when the history fills up or is cleared."
            },
        ));
        controls = controls.push(hint(
            p,
            button(text("×").size(14.0).color(p.txt3)).style(ui_theme::row_button(p)).padding([2.0, 6.0]).on_press(Message::ClipRemove(m.id)),
            "Remove this entry. Files already transferred stay on your disk.",
        ));
        let pinned = m.pinned;
        container(controls)
            .padding([10.0, 14.0])
            .style(move |_| if pinned { ui_theme::group_accent(p) } else { ui_theme::group(p) })
            .width(Length::Fill)
            .into()
    }

    // ---------- footer and menu ----------

    /// One line of news and one action. Settings save themselves, so there is no Save
    /// (R40); Start and Stop are one button because they are mutually exclusive.
    fn footer(&self, p: Palette, running: bool) -> Element<'_, Message> {
        let pending = self.pending_changes().len();
        let left = if !self.feedback.is_empty() {
            self.feedback.clone()
        } else if pending > 0 {
            format!("{pending} change{} wait for a restart", if pending == 1 { "" } else { "s" })
        } else if !running && !self.paired() {
            "Not connected yet".to_string()
        } else {
            "Changes save automatically".to_string()
        };
        let run = if running {
            btn_danger(p, "Stop sharing", Some(Message::Stop))
        } else {
            btn_primary(p, "Start sharing", Some(Message::Start))
        };
        column![
            hairline(p),
            container({
                // Shown on every page, Settings included: that is where changes are made.
                let mut r = row![text(left).size(12.0).color(if pending > 0 && self.feedback.is_empty() { p.warn } else { p.txt3 }).width(Length::Fill)]
                    .spacing(10.0)
                    .align_y(Alignment::Center);
                if pending > 0 {
                    r = r.push(btn_secondary(p, "Restart", Some(Message::RestartNow)));
                }
                r.push(run)
            })
            .padding([12.0, 20.0]),
        ]
        .into()
    }

    /// The ⋯ menu: Settings, the profiles to switch between, and Quit (M6).
    fn overflow_menu(&self, p: Palette) -> Element<'_, Message> {
        let item = |label: String, colour: iced::Color, msg: Message| -> Element<'_, Message> {
            button(text(label).size(13.0).color(colour))
                .style(ui_theme::row_button(p))
                .padding([8.0, 12.0])
                .width(Length::Fill)
                .on_press(msg)
                .into()
        };
        let mut col = column![item("Settings".into(), p.txt, Message::OpenSettings(Page::Index)), hairline(p)].spacing(2.0);
        if !self.profiles.is_empty() {
            col = col.push(container(text("PROFILE").size(10.5).font(BOLD).color(p.txt3)).padding([6.0, 12.0]));
            for (name, saved) in &self.profiles {
                let active = same_settings(saved, &self.cfg);
                col = col.push(
                    button(
                        row![
                            text(name.clone()).size(13.0).color(p.txt).width(Length::Fill),
                            text(if active { "✓" } else { "" }).size(13.0).color(p.accent),
                        ]
                        .align_y(Alignment::Center),
                    )
                    .style(ui_theme::segment(p, active))
                    .padding([8.0, 12.0])
                    .width(Length::Fill)
                    .on_press(Message::SwitchProfile(name.clone())),
                );
            }
        }
        col = col
            .push(item(
                if self.profiles.is_empty() { "Profiles…".into() } else { "Manage profiles…".into() },
                p.txt2,
                Message::OpenSettings(Page::Connection),
            ))
            .push(hairline(p))
            .push(item("Quit mouse-rs".into(), p.danger, Message::Quit));

        container(col)
            .style(move |_| ui_theme::floating(p))
            .padding(6.0)
            .width(234.0)
            .into()
    }

    // ---------- ⋯ → Settings ----------

    /// The back button and the page's title. Back from a category goes to the list; back
    /// from the list goes to the main window.
    fn settings_bar(&self, p: Palette, pg: Page) -> Element<'_, Message> {
        let back = if pg == Page::Index { Message::CloseSettings } else { Message::OpenSettings(Page::Index) };
        row![
            button(text("←").size(14.0).color(p.txt2))
                .style(ui_theme::icon_button(p, true))
                .padding([4.0, 10.0])
                .on_press(back),
            text(pg.title()).size(20.0).font(BOLD).color(p.txt).width(Length::Fill),
        ]
        .spacing(12.0)
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

    /// The list of categories (S0). Each row shows its current value, so a setting can be
    /// checked without opening it.
    fn settings_index(&self, p: Palette) -> Element<'_, Message> {
        let mut col = column![].spacing(0.0);
        for (heading, pages) in Page::CATEGORIES {
            col = col.push(section(p, heading));
            let rows = pages
                .iter()
                .map(|&pg| nav(p, pg.title(), pg.summary(), self.page_value(p, pg), Message::OpenSettings(pg)))
                .collect();
            col = col.push(group(p, rows));
        }
        col.into()
    }

    /// What a settings row shows on the right of the index.
    fn page_value(&self, p: Palette, pg: Page) -> Element<'_, Message> {
        let c = &self.cfg;
        let v = match pg {
            Page::Clipboard => {
                let on: Vec<&str> = [
                    (c.clipboard_sync_text, "text"),
                    (c.clipboard_sync_images, "images"),
                    (c.clipboard_sync_files, "files"),
                ]
                .into_iter()
                .filter_map(|(on, what)| on.then_some(what))
                .collect();
                if !c.clipboard_sync || on.is_empty() {
                    "Off".to_string()
                } else {
                    let s = on.join(", ");
                    s[..1].to_uppercase() + &s[1..]
                }
            }
            Page::Screens => format!("{} {}", self.peer_label(), side_phrase(self.peer_side())),
            Page::Keys => return keycaps(p, &c.lock_hotkey),
            Page::Startup => if self.autostart_on { "At login" } else { "Manual" }.to_string(),
            Page::Connection => c.name.clone(),
            Page::Security => match c.role {
                Role::Client if !c.connection_pin.is_empty() && (c.pin_enabled || c.login_pin_enabled) => "PIN on".into(),
                Role::Client => "PIN off".into(),
                Role::Server if self.paired() => "Paired".into(),
                Role::Server => "Not paired".into(),
            },
            Page::Appearance => {
                if c.theme_follow_system { "Match system".to_string() } else { Theme6::from_key(&c.theme).label().to_string() }
            }
            Page::About => format!("Version {}", crate::update::VERSION),
            Page::Index | Page::Diagnostics => String::new(),
        };
        value_text(p, v)
    }

    /// The other computer's name for sentences, capitalised when it is the generic one.
    fn peer_label(&self) -> String {
        let n = self.peer_name();
        if n == "the other computer" { "Other computer".into() } else { n }
    }

    /// The side the other computer sits on, as this role stores it: the server leaves
    /// through it, the client's pointer arrives through it.
    fn peer_side(&self) -> Edge {
        match self.cfg.role {
            Role::Server => self.cfg.exit_edge,
            Role::Client => self.cfg.entry_edge,
        }
    }

    /// A page's Advanced section: at the bottom, closed until opened (R25, R30).
    fn advanced<'a>(&'a self, p: Palette, pg: Page, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        let open = self.adv_open.contains(&pg);
        let head = button(
            row![
                text(if open { "▾" } else { "▸" }).size(12.0).color(p.txt2),
                text("Advanced").size(13.0).color(p.txt2),
                container(Space::new().height(1.0)).width(Length::Fill).height(1.0).style(move |_| ui_theme::rule(p.line)),
            ]
            .spacing(10.0)
            .align_y(Alignment::Center),
        )
        .style(ui_theme::tab(p, false))
        .padding([14.0, 0.0])
        .width(Length::Fill)
        .on_press(Message::AdvancedToggled(pg));
        if open {
            column![head, body.into()].into()
        } else {
            head.into()
        }
    }

    /// Settings → Clipboard (S1): the everyday options at the top, the technical transfer
    /// and memory options under Advanced. Every control the page had is still here.
    fn page_clipboard(&self, p: Palette) -> Element<'_, Message> {
        let c = &self.cfg;
        let what = row![
            chip_toggle(p, "Text", c.clipboard_sync_text, Message::ClipSyncTextToggled(!c.clipboard_sync_text)),
            chip_toggle(p, "Images", c.clipboard_sync_images, Message::ClipSyncImagesToggled(!c.clipboard_sync_images)),
            chip_toggle(p, "Files", c.clipboard_sync_files, Message::ClipSyncFilesToggled(!c.clipboard_sync_files)),
        ]
        .spacing(6.0);
        let ask_sub = if c.clipboard_auto_transfer_max == u64::MAX {
            "Never asks: anything you copy just comes over".to_string()
        } else if c.clipboard_auto_transfer_max == 0 {
            "0 — always ask, whatever the size".to_string()
        } else {
            "Smaller ones arrive straight away".to_string()
        };
        let deliver_hint = if c.clipboard_deliver_dir.trim().is_empty() {
            "Leave as default to paste wherever you like. Pick a folder only if files should arrive automatically.".to_string()
        } else {
            format!(
                "Files land in {} as they arrive — nothing to paste. They are linked, not copied, so a 40 GB file uses 40 GB and not 80.",
                c.clipboard_deliver_dir.trim()
            )
        };

        // The worst case this configuration allows. Stating the cost is the job; refusing
        // the value is not.
        let worst = c.clipboard_history_size.saturating_mul(c.clipboard_max_item_bytes).min(c.clipboard_memory_budget);
        let px = c.clipboard_max_image_px;
        // 4 bytes a pixel of RGBA, allocated while an image is encoded or decoded.
        let img_cost = px.saturating_mul(px).saturating_mul(4);
        let quick_sub = if c.clipboard_quick_send_secs == 0 {
            "0 — the size limit alone decides".to_string()
        } else {
            match self.engine.as_ref().and_then(|e| e.measured_throughput()) {
                Some(bps) => format!("Measured from real transfers · now ~{}/s", human_bytes(bps as usize)),
                None => "Measured from real transfers · nothing measured yet, so it asks".to_string(),
            }
        };

        let adv = column![
            section(p, "Files"),
            group(p, vec![
                num(p, "Most files in one copy", "Guards against a stray copy of a huge folder", &self.clip_max_files_s, "", Message::ClipMaxFilesChanged),
                tog(p, "Also copy images as a .png file", "So images paste into file managers too", c.clipboard_images_also_as_file, Message::ClipImagesAsFileToggled),
                tog(p, "Follow shortcuts inside copied folders", "Off: avoids copying files you never picked", c.clipboard_follow_symlinks, Message::ClipFollowSymlinksToggled),
                tog(p, "Pasting a received file moves it", "Off: copies it, briefly using twice the space", c.clipboard_move_on_paste, Message::ClipMoveOnPasteToggled),
            ]),
            section(p, "Transfers"),
            group(p, vec![
                num(p, "Don't ask if it takes under", quick_sub, &self.clip_quick_s, "s", Message::ClipQuickSecsChanged),
                num(
                    p,
                    "Stream files bigger than",
                    if c.clipboard_keep_resumable_copy {
                        "Off while a resumable copy is kept".to_string()
                    } else {
                        "Straight from the network to your folder".to_string()
                    },
                    &self.clip_stream_mb_s,
                    "MB",
                    Message::ClipStreamMbChanged,
                ),
                num(p, "Wait for a dropped connection", "Then resume from the same byte", &self.clip_stream_wait_s, "s", Message::ClipStreamWaitChanged),
                tog(p, "Keep a resumable copy while pasting", "Uses the file's size again on disk", c.clipboard_keep_resumable_copy, Message::ClipKeepResumableToggled),
                tog(p, "Slow transfers while I'm on the other computer", "Only if the pointer starts to lag", c.bulk_pause_while_crossed, Message::BulkPauseToggled),
                num(p, "Speed limit", "0 = automatic", &self.bulk_rate_s, "MB/s", Message::BulkRateChanged),
                num(p, "Send buffer", "Max file data held in memory", &self.bulk_buf_s, "MB", Message::BulkBufferChanged),
                tog(p, "Separate lane for large transfers", "Keeps big items from delaying the mouse", c.bulk_enabled, Message::BulkEnabledToggled),
                self.lane_port_row(p),
                num(p, "Fingerprint files up to", "Bigger files are matched by size", &self.hash_limit_mb_s, "MB", Message::HashLimitMbChanged),
            ]),
            section(p, "Storage & memory"),
            group(p, vec![
                num(p, "Disk for received files", "Oldest go first; pinned never", &self.clip_cache_gb_s, "GB", Message::ClipCacheGbChanged),
                stacked(
                    p,
                    "Cache folder",
                    input(p, "~/.mouse_share/cache", &self.cfg.cache_dir, Message::CacheDirChanged).font(MONO).into(),
                    "",
                ),
                num(p, "Biggest item to keep", "", &self.clip_item_mb_s, "MB", Message::ClipItemMbChanged),
                num(p, "Memory budget", format!("Can hold up to {}; pinned entries are never evicted", human_bytes(worst)), &self.clip_mem_mb_s, "MB", Message::ClipMemMbChanged),
                num(p, "Largest image edge", format!("About {} of memory while one is compressed", human_bytes(img_cost)), &self.clip_img_px_s, "px", Message::ClipImagePxChanged),
                num(p, "Send images inline up to", "Bigger ones take the separate lane", &self.clip_inline_kb_s, "KB", Message::ClipInlineKbChanged),
            ]),
        ];

        column![
            section(p, "Sharing"),
            group(p, vec![
                tog(p, "Sync clipboard", "Copy here, paste there", c.clipboard_sync, Message::ClipboardToggled),
                srow(p, "What to share", "", what),
                tog(p, "Tell me when a file is waiting", "A desktop notification, never a pop-up", c.clipboard_notify, Message::ClipNotifyToggled),
            ]),
            section(p, "Large files"),
            group(p, vec![
                num(p, "Ask before bringing over files larger than", ask_sub, &self.clip_auto_mb_s, "MB", Message::ClipAutoMbChanged),
                stacked(
                    p,
                    "Save received files to",
                    input(p, "Wherever I paste (default)", &self.cfg.clipboard_deliver_dir, Message::ClipDeliverDirChanged).into(),
                    deliver_hint,
                ),
            ]),
            section(p, "History"),
            group(p, vec![
                num(p, "Keep", "", &self.clip_items_s, "items", Message::ClipItemsChanged),
                tog(p, "Keep history after restart", "Writes clipboard contents, including private ones, to disk", c.clipboard_persist_history, Message::ClipPersistToggled),
            ]),
            self.advanced(p, Page::Clipboard, adv),
        ]
        .into()
    }

    /// The lane port: an example in the box, the reason when a port is refused, and on a
    /// Windows server a way to let the port through the firewall.
    fn lane_port_row(&self, p: Palette) -> Element<'_, Message> {
        let c = &self.cfg;
        let (sub, colour) = if !self.bulk_port_error.is_empty() {
            (self.bulk_port_error.clone(), p.danger)
        } else if c.bulk_port == 0 {
            ("Blank = share the main port. Set the same port on both computers.".to_string(), p.txt3)
        } else {
            (format!("Port {} · set the same port on both computers", c.bulk_port), p.txt3)
        };
        let field = text_input("e.g. 24802", &self.bulk_port_s)
            .on_input(Message::BulkPortChanged)
            .style(ui_theme::input(p))
            .padding([7.0, 10.0])
            .size(13.0)
            .font(MONO)
            .width(104.0);
        let mut col = column![row![
            column![
                text("Port for that lane").size(13.5).font(BOLD).color(p.txt),
                text(sub).size(11.5).color(colour),
            ]
            .spacing(2.0)
            .width(Length::Fill),
            field,
        ]
        .spacing(12.0)
        .align_y(Alignment::Center)]
        .spacing(10.0);
        // Only the computer that listens needs an inbound rule.
        if c.bulk_port != 0 && c.role == Role::Server && !self.firewall_ok {
            col = col.push(
                row![
                    text("Windows Firewall may block this port. Allow mouse-rs through it once and every port it uses works.")
                        .size(11.5)
                        .color(p.warn)
                        .width(Length::Fill),
                    btn_secondary(p, "Allow…", Some(Message::AllowFirewall)),
                ]
                .spacing(10.0)
                .align_y(Alignment::Center),
            );
        }
        container(col).padding([12.0, 14.0]).width(Length::Fill).into()
    }

    /// Settings → Screens & edges (S2).
    ///
    /// Only the computer with the mouse decides the arrangement: it leaves through one of
    /// its edges and places the pointer on the facing edge of the other screen. So on that
    /// computer this is a picture you click; on the controlled one it shows what the other
    /// computer set, and the way back follows it by itself (owner, 28 Oct).
    fn page_screens(&self, p: Palette) -> Element<'_, Message> {
        let peer = self.peer_label();
        let peer_in_text = self.peer_name();
        let server = shows_exit_edge(self.cfg.role);

        let mut col = column![
            Space::new().height(6.0),
            container(self.screens_picture(p)).style(move |_| ui_theme::group(p)).width(Length::Fill),
            Space::new().height(10.0),
        ];
        if server {
            col = col.push(note(
                p,
                p.accent,
                format!(
                    "Leaves {} at its {} edge, appears on {}'s {} edge. Click an edge of {} for where it leaves, and an edge of {} for where it appears.",
                    self.cfg.name,
                    self.cfg.exit_edge.label(),
                    peer_in_text,
                    self.cfg.entry_edge.label(),
                    self.cfg.name,
                    peer_in_text
                ),
            ));
            // Leaving through one edge means arriving through the facing one; anything else
            // feels as if the screens overlap.
            if self.cfg.entry_edge != self.cfg.exit_edge.opposite() {
                col = col.push(Space::new().height(8.0)).push(note(
                    p,
                    p.warn,
                    format!(
                        "Your choice: leaving at the {} edge would usually appear at the {} edge of {}. With {} it works, and the way back is the same edge — it just doesn't match where the screens sit on your desk. Choosing a {} edge here resets it.",
                        self.cfg.exit_edge.label(),
                        self.cfg.exit_edge.opposite().label(),
                        peer_in_text,
                        self.cfg.entry_edge.label(),
                        self.cfg.name
                    ),
                ));
            }
        } else {
            col = col.push(note(
                p,
                p.accent,
                format!(
                    "Set on {peer_in_text}, the computer with the mouse — change it there. The pointer arrives at this screen's {} edge, and pushing back through it returns to {peer_in_text}. This computer follows by itself.",
                    self.cfg.entry_edge.label()
                ),
            ));
        }

        let mut adv_rows: Vec<Element<'_, Message>> = Vec::new();
        if server {
            let auto = self.cfg.entry_edge == self.cfg.exit_edge.opposite();
            let e = |edge: Edge, label: &str| (label.to_string(), !auto && self.cfg.entry_edge == edge, Message::EntryEdgePicked(edge));
            adv_rows.push(srow(
                p,
                format!("Appears on {peer} at"),
                "Auto matches the edge you picked",
                segmented(p, vec![
                    ("Auto".into(), auto, Message::EntryEdgeAuto),
                    e(Edge::Left, "Left"),
                    e(Edge::Right, "Right"),
                    e(Edge::Top, "Top"),
                    e(Edge::Bottom, "Bottom"),
                ]),
            ));
        }
        adv_rows.push(srow(
            p,
            "This screen",
            "Detected each time sharing starts",
            row![
                small_input(p, "1920", &self.screen_w_s, Message::ScreenWChanged),
                text("×").size(13.0).color(p.txt3),
                small_input(p, "1080", &self.screen_h_s, Message::ScreenHChanged),
                btn_secondary(p, "Detect", Some(Message::DetectScreen)),
            ]
            .spacing(6.0)
            .align_y(Alignment::Center),
        ));
        adv_rows.push(num(
            p,
            "Edge sensitivity",
            "How close to the edge before it crosses · 0 = last pixel",
            &self.edge_threshold_s,
            "px",
            Message::EdgeThresholdChanged,
        ));

        col.push(self.advanced(p, Page::Screens, group(p, adv_rows))).into()
    }

    /// The two screens, in proportion. This computer has an edge strip on each side; the
    /// lit one is where the pointer leaves, and on the other screen the lit strip is where
    /// it appears. On the computer with the mouse the strips are buttons.
    fn screens_picture(&self, p: Palette) -> Element<'_, Message> {
        let server = shows_exit_edge(self.cfg.role);
        // Server: it leaves here through `exit_edge`, and appears over there at `entry_edge`.
        // Client: it arrives here at `entry_edge` from the computer on that side.
        let (here_edge, there_edge) = if server {
            (self.cfg.exit_edge, self.cfg.entry_edge)
        } else {
            (self.cfg.entry_edge, self.cfg.entry_edge.opposite())
        };
        let (lw, lh) = (self.cfg.screen_width.max(1) as f32, self.cfg.screen_height.max(1) as f32);
        let (pw, ph, peer_res) = match &self.last_peer {
            Some((_, w, h)) if *w > 0 && *h > 0 => (*w as f32, *h as f32, format!("{w}×{h}")),
            _ => (1920.0, 1080.0, String::new()),
        };
        // Both at one scale, the larger screen 150 px wide.
        let scale = 150.0 / lw.max(pw);
        const BAR: f32 = 6.0;

        // `pick`: what clicking an edge strip does — choose where the pointer leaves this
        // screen, or where it appears on the other one. `None` draws the strips only.
        let screen = |name: String, line1: String, line2: String, w: f32, h: f32, lit: Edge, pick: Option<(fn(Edge) -> Message, &'static str)>, me: bool| -> Element<'_, Message> {
            let (bw, bh) = (w * scale, (h * scale).max(64.0));
            let face = container(
                column![
                    Space::new().height(Length::Fill),
                    text(name).size(12.0).font(BOLD).color(p.txt),
                    text(line1).size(10.5).color(if me { p.accent } else { p.txt2 }),
                    text(line2).size(10.0).font(MONO).color(p.txt3),
                ]
                .spacing(1.0),
            )
            .padding([6.0, 8.0])
            .width(bw)
            .height(bh)
            .style(move |_| ui_theme::tile(p, me, true));
            let strip = |edge: Edge, w: f32, h: f32| -> Element<'_, Message> {
                let on = edge == lit;
                if let Some((msg, tip)) = pick {
                    hint(
                        p,
                        button(Space::new().width(w).height(h))
                            .style(ui_theme::edge_strip(p, on))
                            .padding(0.0)
                            .on_press(msg(edge)),
                        tip,
                    )
                } else {
                    container(Space::new().width(w).height(h))
                        .style(move |_| ui_theme::fill(if on { p.accent } else { iced::Color::TRANSPARENT }, 3.0))
                        .into()
                }
            };
            column![
                row![Space::new().width(BAR), strip(Edge::Top, bw, BAR), Space::new().width(BAR)].spacing(3.0),
                row![strip(Edge::Left, BAR, bh), face, strip(Edge::Right, BAR, bh)].spacing(3.0).align_y(Alignment::Center),
                row![Space::new().width(BAR), strip(Edge::Bottom, bw, BAR), Space::new().width(BAR)].spacing(3.0),
            ]
            .spacing(3.0)
            .into()
        };

        let me = screen(
            self.cfg.name.clone(),
            if server { format!("leaves: {}", here_edge.label()) } else { format!("arrives: {}", here_edge.label()) },
            format!("this · {}×{}", self.cfg.screen_width, self.cfg.screen_height),
            lw,
            lh,
            here_edge,
            server.then_some((Message::PeerSidePicked as fn(Edge) -> Message, "The pointer leaves through this edge")),
            true,
        );
        let other = screen(
            self.peer_label(),
            if server { format!("appears: {}", there_edge.label()) } else { "has the mouse".to_string() },
            peer_res,
            pw,
            ph,
            there_edge,
            server.then_some((Message::EntryEdgePicked as fn(Edge) -> Message, "The pointer appears on this edge")),
            false,
        );
        let arrow = |s: &'static str| text(s).size(18.0).color(p.accent);
        // The other screen sits on the side this one is left through (or entered from).
        let arrangement: Element<'_, Message> = match (here_edge, server) {
            (Edge::Right, true) => row![me, arrow("→"), other].spacing(8.0).align_y(Alignment::Center).into(),
            (Edge::Right, false) => row![me, arrow("←"), other].spacing(8.0).align_y(Alignment::Center).into(),
            (Edge::Left, true) => row![other, arrow("←"), me].spacing(8.0).align_y(Alignment::Center).into(),
            (Edge::Left, false) => row![other, arrow("→"), me].spacing(8.0).align_y(Alignment::Center).into(),
            (Edge::Top, true) => column![other, arrow("↑"), me].spacing(4.0).align_x(Alignment::Center).into(),
            (Edge::Top, false) => column![other, arrow("↓"), me].spacing(4.0).align_x(Alignment::Center).into(),
            (Edge::Bottom, true) => column![me, arrow("↓"), other].spacing(4.0).align_x(Alignment::Center).into(),
            (Edge::Bottom, false) => column![me, arrow("↑"), other].spacing(4.0).align_x(Alignment::Center).into(),
        };
        let vertical = matches!(here_edge, Edge::Top | Edge::Bottom);
        container(container(arrangement).center(Length::Fill))
            .height(if vertical { 290.0 } else { 190.0 })
            .width(Length::Fill)
            .padding(12.0)
            .style(move |_| ui_theme::stage(p))
            .into()
    }

    /// Settings → Startup (S3). Start minimized is near the top, since many people never
    /// open the window; each OS shows only its own group.
    fn page_startup(&self, p: Palette) -> Element<'_, Message> {
        // The three are deliberately independent: the OS launches the panel (Start at
        // login), the panel brings the link up by itself (Start sharing automatically), and
        // it stays out of the way while doing so (Start minimized).
        let mut col = column![
            section(p, "When I log in"),
            group(p, vec![
                tog(p, "Start at login", self.autostart_hint(), self.autostart_on, Message::AutostartToggled),
                tog(p, "Start minimized", "Opens in the tray; click the tray icon to show it", self.cfg.start_minimized, Message::StartMinimizedToggled),
                tog(p, "Start sharing automatically", "Connects as soon as mouse-rs opens", self.cfg.autoconnect, Message::AutoconnectToggled),
            ]),
        ];

        // Linux: what this machine does before anyone has signed in.
        #[cfg(target_os = "linux")]
        {
            let mut rows = vec![tog(
                p,
                "Share input at the login screen",
                "The other keyboard can type your login password. Installs a system service.",
                self.login_screen.on(),
                Message::LoginScreenToggled,
            )];
            // Only worth a line when it says something the toggle does not. Installed but not
            // enabled is the state a stray `systemctl disable` leaves behind, and it would
            // otherwise look identical to off.
            let warn = if self.login_screen.installed && !self.login_screen.enabled {
                Some("Installed but disabled in systemd. Turn this off and on to fix it.")
            } else if self.login_screen.on() && self.login_screen_stale {
                Some("The login screen runs an older version. Turn this off and on to update it.")
            } else {
                None
            };
            if let Some(w) = warn {
                rows.push(container(note(p, p.warn, w)).padding([10.0, 14.0]).into());
            }
            col = col.push(section(p, "Linux only")).push(group(p, rows));
        }

        // Windows: which privilege level this runs at is a fact worth stating permanently;
        // the explanation of what it costs is a one-time banner.
        if mouse_input::elevation() != mouse_input::Elevation::NotApplicable {
            let limited = mouse_input::elevation().is_limited();
            col = col.push(section(p, "Windows only")).push(group(p, vec![
                srow(
                    p,
                    "Permissions",
                    if limited { "Normal · some admin windows block crossing" } else { "Administrator" },
                    btn_secondary(p, if limited { "Restart as administrator" } else { "Restart as normal user" }, Some(Message::RelaunchElevated)),
                ),
                tog(
                    p,
                    "Start at login as administrator",
                    "Keeps administrator rights after a reboot. Needs mouse-rs running as administrator now.",
                    self.autostart_elevated_on,
                    Message::AutostartElevatedToggled,
                ),
            ]));
        }
        col.into()
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

    /// Sub-label for the Start-at-login toggle: says where the entry actually lives, so
    /// the setting is auditable rather than magic.
    fn autostart_hint(&self) -> &'static str {
        if cfg!(windows) {
            "Adds a per-user entry under the Windows Run key"
        } else {
            "Adds ~/.config/autostart/mouse-rs.desktop"
        }
    }

    /// Settings → Keyboard & hotkeys (S4). Hotkeys are recorded, not typed: press Change,
    /// then press the key.
    fn page_keys(&self, p: Palette) -> Element<'_, Message> {
        let cap = |field: HotkeyField, value: &str| -> Element<'_, Message> {
            if self.recording == Some(field) {
                container(text("Press a key…").size(12.0).color(p.accent))
                    .padding([3.0, 8.0])
                    .style(move |_| ui_theme::keycap(p))
                    .into()
            } else {
                keycaps(p, value)
            }
        };
        let change = |field: HotkeyField| {
            btn_secondary(p, if self.recording == Some(field) { "Cancel" } else { "Change" }, Some(Message::RecordHotkey(field)))
        };
        let mut col = column![
            section(p, "Getting control back"),
            group(p, vec![srow(
                p,
                "Take control back",
                "One key. Always works, even if the network drops.",
                row![cap(HotkeyField::Lock, &self.cfg.lock_hotkey), change(HotkeyField::Lock)]
                    .spacing(10.0)
                    .align_y(Alignment::Center),
            )]),
        ];
        if self.recording.is_some() {
            col = col.push(Space::new().height(8.0)).push(note(
                p,
                p.accent,
                "Press the key now, holding any modifiers for the return combo. Esc keeps the old one.",
            ));
        }
        let mut ret = row![cap(HotkeyField::Return, &self.cfg.return_hotkey), change(HotkeyField::Return)]
            .spacing(10.0)
            .align_y(Alignment::Center);
        if !self.cfg.return_hotkey.trim().is_empty() && self.recording != Some(HotkeyField::Return) {
            ret = ret.push(btn_link(p, "Clear", Message::ClearReturnHotkey));
        }
        let adv = group(p, vec![srow(p, "Return combo", "Optional · works on the computer with the mouse", ret)]);
        // R46: the switch shortcuts, on the computer with the mouse.
        if self.cfg.role == Role::Server {
            let targets = self.cfg.switch_targets();
            let mut rows: Vec<Element<'_, Message>> = Vec::new();
            for (i, t) in targets.iter().enumerate() {
                let field = HotkeyField::Switch(i);
                let mut r = row![cap(field, &t.hotkey), change(field)].spacing(10.0).align_y(Alignment::Center);
                if self.cfg.switch_keys.contains_key(&t.id) && self.recording != Some(field) {
                    r = r.push(btn_link(p, "Default", Message::SwitchKeyDefault(i)));
                }
                let sub = if t.is_self { "this computer: brings control back here".to_string() } else { "jumps into it".to_string() };
                rows.push(srow(p, format!("{}. {}", i + 1, t.name), sub, r));
            }
            let dupes: Vec<&str> = targets
                .iter()
                .enumerate()
                .filter(|(i, t)| !t.hotkey.is_empty() && targets[..*i].iter().any(|o| o.hotkey.eq_ignore_ascii_case(&t.hotkey)))
                .map(|(_, t)| t.name.as_str())
                .collect();
            col = col.push(section(p, "Switch between computers")).push(group(p, rows));
            if !dupes.is_empty() {
                col = col.push(note(p, p.warn, &format!("Two entries share a shortcut; the first one wins. Change the one for {}.", dupes.join(", "))));
            }
            col = col.push(text("The order is set on the main window with ▲▼. Changes apply when sharing restarts.").size(11.5).color(p.txt3));
        }
        col.push(self.advanced(p, Page::Keys, adv)).into()
    }

    /// Settings → Connection (S5): this computer's name, staying connected and profiles;
    /// the ports under Advanced. Role and the other computer's address are on the main
    /// window.
    fn page_connection(&self, p: Palette) -> Element<'_, Message> {
        let server_side = self.cfg.role == Role::Server;
        let mut basic = vec![stacked(
            p,
            "Name",
            input(p, "this-computer", &self.cfg.name, Message::NameChanged).into(),
            "How it appears on the other computer",
        )];
        if !server_side {
            basic.push(tog(
                p,
                "Reconnect automatically",
                format!("Keep retrying until {} is back", self.peer_name()),
                self.cfg.reconnect,
                Message::ReconnectToggled,
            ));
            basic.push(self.network_preference_row(p));
        }

        // Profiles: listed by name, the one in use marked (R40).
        let mut prof: Vec<Element<'_, Message>> = Vec::new();
        for (name, saved) in &self.profiles {
            let active = same_settings(saved, &self.cfg);
            let right: Element<'_, Message> = if active {
                container(text("Active").size(11.5).font(BOLD).color(p.accent))
                    .padding([3.0, 8.0])
                    .style(move |_| ui_theme::chip(p))
                    .into()
            } else {
                row![
                    btn_secondary(p, "Switch", Some(Message::SwitchProfile(name.clone()))),
                    button(text("×").size(15.0).color(p.txt3))
                        .style(ui_theme::row_button(p))
                        .padding([2.0, 8.0])
                        .on_press(Message::DeleteProfile(name.clone())),
                ]
                .spacing(6.0)
                .align_y(Alignment::Center)
                .into()
            };
            prof.push(srow(p, name.clone(), profile_summary(saved), right));
        }
        prof.push(stacked(
            p,
            "Save current setup as",
            row![
                input(p, "Name, e.g. home", &self.profile_name_input, Message::ProfileNameChanged)
                    .on_submit(Message::SaveProfileAs),
                btn_secondary(p, "Save", Some(Message::SaveProfileAs)),
            ]
            .spacing(8.0)
            .align_y(Alignment::Center)
            .into(),
            // Said plainly because a profile is as sensitive as the config file itself.
            "A named copy of all your settings, to switch between setups like \"desk\" and \"travel\". Pairings are not part of a profile · stored in ~/.mouse_share/profiles/",
        ));

        let mut ports: Vec<Element<'_, Message>> = Vec::new();
        if server_side {
            // Real addresses of this machine, so the bind address is chosen rather than
            // typed from memory.
            let mut chips = row![].spacing(6.0);
            for a in mouse_core::discover::listen_candidates() {
                let on = a == self.cfg.listen_host;
                chips = chips.push(
                    button(text(a.clone()).size(12.0).font(MONO))
                        .style(ui_theme::chip_toggle(p, on))
                        .padding([3.0, 9.0])
                        .on_press(Message::ListenHostPicked(a)),
                );
            }
            ports.push(stacked(
                p,
                "Listen host",
                column![
                    input(p, "0.0.0.0", &self.cfg.listen_host, Message::ListenHostChanged).font(MONO),
                    chips.wrap().vertical_spacing(6.0),
                ]
                .spacing(8.0)
                .into(),
                "0.0.0.0 accepts on every address; pick one to accept only there",
            ));
            ports.push(num(p, "Listen port", "", &self.listen_port_s, "", Message::ListenPortChanged));
        } else {
            ports.push(num(p, "Server port", "", &self.server_port_s, "", Message::ServerPortChanged));
        }
        ports.push(num(
            p,
            "Discovery port",
            "UDP · same on both computers · 0 = off",
            &self.discovery_port_s,
            "",
            Message::DiscoveryPortChanged,
        ));

        column![
            section(p, "This computer"),
            group(p, basic),
            section(p, "Profiles"),
            group(p, prof),
            self.advanced(p, Page::Connection, column![section(p, "Ports"), group(p, ports)]),
        ]
        .into()
    }

    /// Settings → Security & pairing (S6 on the controlled computer, S7 on the one with
    /// the mouse). The PIN is set on the computer being controlled, so it lives there.
    fn page_security(&self, p: Palette) -> Element<'_, Message> {
        let mut col = column![];
        if self.cfg.role == Role::Client {
            col = col
                .push(section(p, "Connection PIN"))
                .push(group(p, self.pin_settings_rows(p)))
                .push(section(p, "Pairing"));
            let mut pairing = Vec::new();
            if self.paired() {
                let names: Vec<&str> = self.cfg.paired.iter().map(|d| d.name.as_str()).collect();
                pairing.push(srow(p, format!("Paired with {}", names.join(", ")), "", dot(p.good, 8.0)));
            }
            pairing.push(stacked(p, if self.paired() { "Pair with a new code" } else { "Pair with a code" }, self.pair_client_view(p), ""));
            col = col.push(group(p, pairing));
        } else {
            col = col
                .push(section(p, "Pairing"))
                .push(group(p, vec![self.pair_server_row(p)]))
                .push(section(p, "Connection PIN"))
                .push(group(p, vec![srow(
                    p,
                    "Set on the computer you control",
                    format!(
                        "When {} asks for its PIN, a box appears on this computer's main window. It's asked once each time mouse-rs starts, not after a Wi-Fi drop.",
                        self.peer_name()
                    ),
                    Space::new().width(0.0),
                )]));
        }

        // Every computer this one is paired with, each with its own key (R44).
        col = col.push(section(p, "Paired computers")).push(group(p, self.paired_rows(p)));

        let adv = group(p, vec![
            tog(p, "Encrypt the connection (TLS)", "rustls, self-signed certificate", self.cfg.tls_enabled, Message::TlsToggled),
        ]);
        col.push(self.advanced(p, Page::Security, adv)).into()
    }

    /// Main window: each paired computer. Connect on the others; on the one in use, its
    /// state (connecting, or connected with the row tinted) and Disconnect. With sharing
    /// stopped nothing is waiting, so every row offers Connect.
    fn paired_switch_rows(&self, p: Palette, connected: bool) -> Vec<Element<'_, Message>> {
        if self.cfg.role == Role::Server {
            return self.switch_list_rows(p);
        }
        let own = &self.cfg.device_id;
        let running = self.running();
        let current = mouse_core::trust::choice(own).unwrap_or_else(|| self.cfg.connect_to.clone());
        let mut list = self.cfg.paired.clone();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        list.into_iter()
            .map(|d| {
                let in_use = running && d.id == current;
                let state = |color, label: &'static str| {
                    row![dot(color, 8.0), text(label).size(12.0).color(p.txt2)].spacing(6.0).align_y(Alignment::Center)
                };
                let right: Element<'_, Message> = if in_use {
                    let (c, l) = if connected { (p.good, "connected") } else { (p.warn, "connecting…") };
                    row![state(c, l), btn_secondary(p, "Disconnect", Some(Message::DisconnectPeer))]
                        .spacing(12.0)
                        .align_y(Alignment::Center)
                        .into()
                } else {
                    btn_secondary(p, "Connect", Some(Message::ConnectTo(d.id.clone())))
                };
                let sub = match (in_use, connected) {
                    (true, true) => "in use".to_string(),
                    (true, false) => "not connected yet".to_string(),
                    _ if d.id == current => "last used".to_string(),
                    _ => String::new(),
                };
                let r = srow(p, d.name.clone(), sub, right);
                if in_use && connected {
                    container(r).style(move |_| ui_theme::row_connected(p)).width(Length::Fill).into()
                } else {
                    r
                }
            })
            .collect()
    }

    /// The computer with the mouse (R46): the switch list in order, this computer included.
    /// Each row: its number's shortcut, whether it is connected or in use, Use for a
    /// connected one, and ▲▼ to change its number.
    fn switch_list_rows(&self, p: Palette) -> Vec<Element<'_, Message>> {
        let own = &self.cfg.device_id;
        let running = self.running();
        let connected = if running { mouse_core::trust::connected(own) } else { Vec::new() };
        let in_use = if running { mouse_core::trust::input_peer(own) } else { None };
        let targets = self.cfg.switch_targets();
        let last = targets.len().saturating_sub(1);
        targets
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let is_connected = connected.contains(&t.id);
                let using = in_use.as_deref() == Some(t.id.as_str()) && is_connected;
                let state = |color, label: &'static str| -> Element<'_, Message> {
                    row![dot(color, 8.0), text(label).size(12.0).color(p.txt2)].spacing(6.0).align_y(Alignment::Center).into()
                };
                let status: Element<'_, Message> = if t.is_self {
                    text("").into()
                } else if using {
                    state(p.good, "in use")
                } else if is_connected {
                    row![state(p.good, "connected"), btn_secondary(p, "Use", Some(Message::ConnectTo(t.id.clone())))]
                        .spacing(10.0)
                        .align_y(Alignment::Center)
                        .into()
                } else {
                    state(p.txt3, "not connected")
                };
                let arrows = row![
                    btn_secondary(p, "▲", (i > 0).then_some(Message::MoveSwitch(i, true))),
                    btn_secondary(p, "▼", (i < last).then_some(Message::MoveSwitch(i, false))),
                ]
                .spacing(4.0);
                let right = row![status, keycaps(p, &t.hotkey), arrows].spacing(12.0).align_y(Alignment::Center);
                let title = format!("{}. {}", i + 1, t.name);
                let sub = if t.is_self { "this computer" } else { "" };
                let r = srow(p, title, sub, right);
                if using {
                    container(r).style(move |_| ui_theme::row_connected(p)).width(Length::Fill).into()
                } else {
                    r
                }
            })
            .collect()
    }

    /// One row per paired computer, with Remove (pressed twice, so a slip does not undo a
    /// pairing). Removing it refuses that computer at once.
    fn paired_rows(&self, p: Palette) -> Vec<Element<'_, Message>> {
        if self.cfg.paired.is_empty() {
            return vec![srow(p, "None yet", "Pair with a code above. Each pairing gets its own key.", Space::new().width(0.0))];
        }
        let mut list = self.cfg.paired.clone();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        list.into_iter()
            .map(|d| {
                let when = if d.paired_at > 0 { format!("paired {}", day_text(d.paired_at)) } else { "paired".into() };
                let sub = format!("{when} · its own key · device {}", d.id.get(..8).unwrap_or(&d.id));
                let armed = self.remove_armed.as_deref() == Some(d.id.as_str());
                let button = if armed {
                    btn_danger(p, "Remove for good", Some(Message::RemovePaired(d.id.clone())))
                } else {
                    btn_secondary(p, "Remove", Some(Message::RemovePaired(d.id.clone())))
                };
                srow(p, d.name.clone(), sub, button)
            })
            .collect()
    }

    /// Client: the PIN rows of Security & pairing.
    fn pin_settings_rows(&self, p: Palette) -> Vec<Element<'_, Message>> {
        let has = !self.cfg.connection_pin.is_empty();
        let status: Element<'_, Message> = if has {
            let mut r = row![].spacing(10.0).align_y(Alignment::Center);
            if self.show_pin {
                r = r.push(mono(p, self.cfg.connection_pin.clone()));
            }
            r.push(btn_secondary(p, if self.show_pin { "Hide" } else { "Show" }, Some(Message::ToggleShowPin))).into()
        } else {
            Space::new().width(0.0).into()
        };
        let mut rows = vec![
            srow(
                p,
                if has { "PIN is set" } else { "No PIN set" },
                if has {
                    "The other computer must type it before it's allowed in"
                } else {
                    "Any computer paired with this one can control it"
                },
                status,
            ),
            tog(p, "Ask when someone is logged in here", "", self.cfg.pin_enabled, Message::PinSessionToggled),
        ];
        #[cfg(target_os = "linux")]
        rows.push(tog(
            p,
            "Ask at the login screen",
            "Only if login-screen sharing is on",
            self.cfg.login_pin_enabled,
            Message::PinLoginToggled,
        ));
        rows.push(stacked(
            p,
            if has { "New PIN" } else { "Set a PIN" },
            row![
                input(p, "New PIN", &self.pin_new, Message::PinNewTyped).secure(true),
                input(p, "Again", &self.pin_new2, Message::PinNew2Typed).secure(true).on_submit(Message::SavePin),
                btn_primary(p, "Save", Some(Message::SavePin)),
            ]
            .spacing(8.0)
            .align_y(Alignment::Center)
            .into(),
            "3 wrong tries lock others out for 1 hour, then 2, 4, 8… Forgot it? Set a new one; being at this computer is enough.",
        ));
        rows
    }

    /// Settings → Appearance (S8): click a card to switch the theme for the whole window.
    fn page_appearance(&self, p: Palette) -> Element<'_, Message> {
        let current = self.theme6();
        let card = |t: Theme6| -> Element<'_, Message> {
            let (top, body) = (t.hero(), t.palette());
            let on = t == current && !self.cfg.theme_follow_system;
            let preview = column![
                container(
                    row![
                        container(Space::new()).width(40.0).height(5.0).style(move |_| ui_theme::fill(ui_theme::alpha(top.txt, 0.7), 3.0)),
                        Space::new().width(Length::Fill),
                        container(Space::new()).width(6.0).height(6.0).style(move |_| ui_theme::dot(top.good)),
                    ]
                    .align_y(Alignment::Center),
                )
                .padding([0.0, 10.0])
                .height(28.0)
                .center_y(28.0)
                .width(Length::Fill)
                .style(move |_| ui_theme::fill(top.bg, 0.0)),
                container(
                    column![
                        container(Space::new()).width(Length::FillPortion(4)).height(8.0).style(move |_| container::Style {
                            background: Some(body.surf.into()),
                            border: iced::Border { color: body.line, width: 1.0, radius: 3.0.into() },
                            ..container::Style::default()
                        }),
                        container(Space::new()).width(38.0).height(9.0).style(move |_| ui_theme::fill(body.accent, 3.0)),
                    ]
                    .spacing(6.0),
                )
                .padding([8.0, 10.0])
                .height(44.0)
                .width(Length::Fill)
                .style(move |_| ui_theme::fill(body.bg, 0.0)),
            ];
            let foot = row![
                container(Space::new()).width(12.0).height(12.0).style(move |_| ui_theme::radio(p, on)),
                column![
                    text(t.label()).size(13.0).font(BOLD).color(p.txt),
                    text(t.desc()).size(11.0).color(p.txt3),
                ]
                .spacing(1.0),
            ]
            .spacing(9.0)
            .align_y(Alignment::Center);
            button(column![preview, container(foot).padding([9.0, 11.0])])
                .style(ui_theme::choice(p, on))
                .padding(1.0)
                .width(Length::Fill)
                .on_press(Message::ThemePicked(t))
                .into()
        };
        let mut grid = column![].spacing(10.0);
        for pair in Theme6::ALL.chunks(2) {
            grid = grid.push(row![card(pair[0]), card(pair[1])].spacing(10.0));
        }
        column![
            Space::new().height(8.0),
            grid,
            Space::new().height(12.0),
            group(p, vec![tog(
                p,
                "Match system light / dark",
                "Dark: Graphite · Light: Porcelain",
                self.cfg.theme_follow_system,
                Message::ThemeFollowSystem,
            )]),
        ]
        .into()
    }

    /// Settings → Diagnostics (S9): where the log is, and (Advanced) sending it for support.
    fn page_diagnostics(&self, p: Palette) -> Element<'_, Message> {
        // The panel has no console behind it, so name the log file here — otherwise a user
        // hitting a problem has nothing to look at and nothing to send.
        let log = container(
            column![
                container(text(self.log_path.clone()).size(12.0).font(MONO).color(p.txt))
                    .padding([7.0, 10.0])
                    .width(Length::Fill)
                    .style(move |_| ui_theme::keycap(p)),
                row![
                    Space::new().width(Length::Fill),
                    btn_secondary(p, "Copy path", Some(Message::CopyLogPath)),
                    btn_secondary(p, "Open folder", Some(Message::OpenLogFolder)),
                ]
                .spacing(8.0),
            ]
            .spacing(12.0),
        )
        .padding(14.0);

        let adv = column![
            section(p, "Send logs for support"),
            group(p, vec![
                stacked(
                    p,
                    "Upload address",
                    input(p, "Blank = never sent", &self.cfg.log_upload_url, Message::LogUploadUrlChanged).font(MONO).into(),
                    "",
                ),
                num(p, "Send every", "", &self.log_upload_secs_s, "s", Message::LogUploadSecsChanged),
            ]),
            Space::new().height(10.0),
            note(p, p.txt3, "Nothing leaves this computer unless an address is set. Takes effect next time mouse-rs starts."),
        ];

        column![section(p, "Log file"), group(p, vec![log.into()]), self.advanced(p, Page::Diagnostics, adv)].into()
    }

    /// Settings → About (S10): the version, and Restore defaults, which asks first.
    /// The About page's line on updates.
    fn update_status(&self) -> String {
        use crate::update::State as U;
        match crate::update::get(&self.update) {
            U::Idle => "Not checked yet".into(),
            U::Checking => "Checking…".into(),
            U::UpToDate => "This is the newest version".into(),
            U::CheckFailed(e) => format!("Could not check: {e}"),
            U::Available(m) => format!("Version {} is available", m.version),
            U::Downloading { version, .. } => format!("Downloading version {version}…"),
            U::Ready(v) => format!("Version {v} is installed; it starts at the next restart"),
            U::Failed(e) => format!("The update did not install: {e}"),
        }
    }

    /// The button beside the update status on About: whatever the next step is, right
    /// there, so a "Check now" that finds a version can be installed without going back to
    /// the main window (owner, 30 Oct).
    fn update_action(&self, p: Palette) -> Element<'_, Message> {
        use crate::update::State as U;
        match crate::update::get(&self.update) {
            U::Available(_) => btn_primary(p, "Update now", Some(Message::UpdateInstall)),
            U::Downloading { done, total, .. } => {
                let pct = if total > 0 { done * 100 / total } else { 0 };
                text(format!("{pct}%")).size(12.5).color(p.txt2).into()
            }
            // Installed and waiting for a restart; a newer one can still be looked for.
            U::Ready(_) => row![
                btn_secondary(p, "Check now", Some(Message::UpdateCheckNow)),
                btn_primary(p, "Restart mouse-rs", Some(Message::UpdateRestart)),
            ]
            .spacing(8.0)
            .into(),
            U::Failed(_) => btn_secondary(p, "Try again", Some(Message::UpdateCheckNow)),
            _ => btn_secondary(p, "Check now", self.update_idle().then_some(Message::UpdateCheckNow)),
        }
    }

    fn page_about(&self, p: Palette) -> Element<'_, Message> {
        column![
            Space::new().height(8.0),
            group(p, vec![
                srow(p, "mouse-rs", format!("Version {}", crate::update::VERSION), Space::new().width(0.0)),
                srow(p, "Updates", self.update_status(), self.update_action(p)),
                tog(
                    p,
                    "Check for updates automatically",
                    if crate::update::TEST_BUILD { "Test build: at start and every minute" } else { "Each time mouse-rs starts" },
                    self.cfg.check_updates,
                    Message::UpdatesToggled,
                ),
                srow(
                    p,
                    "Restore default settings",
                    "Keeps this computer's name, its pairings and screen size",
                    btn_danger(p, "Restore…", Some(Message::AskRestoreDefaults)),
                ),
            ]),
        ]
        .into()
    }
}

// ---------- free helper widgets ----------

fn scroll<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    scrollable(content)
        .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4.0).scroller_width(4.0)))
        .height(Length::Fill)
        .into()
}

fn input<'a>(
    p: Palette,
    placeholder: &'a str,
    value: &'a str,
    on_change: impl Fn(String) -> Message + 'a,
) -> iced::widget::TextInput<'a, Message> {
    text_input(placeholder, value)
        .on_input(on_change)
        .style(ui_theme::input(p))
        .padding([8.0, 11.0])
        .size(13.5)
        .width(Length::Fill)
}

/// A short number box (sizes, ports, screen width).
fn small_input<'a>(
    p: Palette,
    placeholder: &'a str,
    value: &'a str,
    on_change: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    text_input(placeholder, value)
        .on_input(on_change)
        .style(ui_theme::input(p))
        .padding([7.0, 10.0])
        .size(13.0)
        .font(MONO)
        .align_x(iced::alignment::Horizontal::Right)
        .width(72.0)
        .into()
}

/// A row's title and the grey line under it.
fn labels<'a>(p: Palette, title: impl Into<String>, sub: impl Into<String>) -> Element<'a, Message> {
    let sub = sub.into();
    let mut c = column![text(title.into()).size(13.5).font(BOLD).color(p.txt)]
        .spacing(2.0)
        .width(Length::Fill);
    if !sub.is_empty() {
        c = c.push(text(sub).size(11.5).color(p.txt3));
    }
    c.into()
}

/// A settings row: title and sub-line on the left, a control on the right.
fn srow<'a>(
    p: Palette,
    title: impl Into<String>,
    sub: impl Into<String>,
    right: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(row![labels(p, title, sub), right.into()].spacing(12.0).align_y(Alignment::Center))
        .padding([12.0, 14.0])
        .width(Length::Fill)
        .into()
}

/// A settings row with a switch.
fn tog<'a>(
    p: Palette,
    title: impl Into<String>,
    sub: impl Into<String>,
    on: bool,
    f: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    srow(p, title, sub, toggler(on).on_toggle(f).style(ui_theme::switch(p)).size(22.0))
}

/// A settings row with a number box and its unit.
fn num<'a>(
    p: Palette,
    title: impl Into<String>,
    sub: impl Into<String>,
    buf: &'a str,
    unit: &'static str,
    f: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    srow(
        p,
        title,
        sub,
        row![small_input(p, "", buf, f), text(unit).size(11.5).color(p.txt3).width(34.0)]
            .spacing(8.0)
            .align_y(Alignment::Center),
    )
}

/// A row whose control needs the full width: title above, the control, a hint below.
fn stacked<'a>(
    p: Palette,
    title: impl Into<String>,
    body: Element<'a, Message>,
    hint_text: impl Into<String>,
) -> Element<'a, Message> {
    let hint_text = hint_text.into();
    let mut c = column![text(title.into()).size(13.5).font(BOLD).color(p.txt), body].spacing(8.0);
    if !hint_text.is_empty() {
        c = c.push(text(hint_text).size(11.5).color(p.txt3));
    }
    container(c).padding([12.0, 14.0]).width(Length::Fill).into()
}

/// A row that opens another page: title, sub-line, its current value, and ›.
fn nav<'a>(
    p: Palette,
    title: impl Into<String>,
    sub: impl Into<String>,
    value: Element<'a, Message>,
    msg: Message,
) -> Element<'a, Message> {
    button(
        row![labels(p, title, sub), value, text("›").size(15.0).color(p.txt3)]
            .spacing(10.0)
            .align_y(Alignment::Center),
    )
    .style(ui_theme::row_button(p))
    .padding([12.0, 14.0])
    .width(Length::Fill)
    .on_press(msg)
    .into()
}

fn value_text<'a>(p: Palette, s: impl Into<String>) -> Element<'a, Message> {
    text(s.into()).size(12.5).color(p.txt2).into()
}

fn mono<'a>(p: Palette, s: String) -> Element<'a, Message> {
    text(s).size(12.5).font(MONO).color(p.txt).into()
}

/// Rows in one card, divided by hairlines.
fn group<'a>(p: Palette, rows: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let n = rows.len();
    let mut col = column![];
    for (i, r) in rows.into_iter().enumerate() {
        col = col.push(r);
        if i + 1 < n {
            col = col.push(hairline(p));
        }
    }
    container(col).style(move |_| ui_theme::group(p)).width(Length::Fill).into()
}

fn hairline<'a>(p: Palette) -> Element<'a, Message> {
    container(Space::new().height(1.0))
        .width(Length::Fill)
        .height(1.0)
        .style(move |_| ui_theme::rule(p.line))
        .into()
}

/// A heading over a group ("SHARING").
fn section(p: Palette, title: &str) -> Element<'static, Message> {
    container(text(title.to_uppercase()).size(11.0).font(BOLD).color(p.txt3))
        .padding(Padding { top: 16.0, right: 2.0, bottom: 7.0, left: 2.0 })
        .into()
}

fn dot<'a>(c: iced::Color, size: f32) -> Element<'a, Message> {
    container(Space::new()).width(size).height(size).style(move |_| ui_theme::dot(c)).into()
}

/// A quiet line with a coloured dot in front.
fn note<'a>(p: Palette, c: iced::Color, s: impl Into<String>) -> Element<'a, Message> {
    container(
        row![dot(c, 7.0), text(s.into()).size(12.0).color(p.txt2).width(Length::Fill)]
            .spacing(9.0)
            .align_y(Alignment::Center),
    )
    .padding([10.0, 14.0])
    .width(Length::Fill)
    .style(move |_| ui_theme::note(p))
    .into()
}

/// A full banner above the tabs (B1).
fn banner<'a>(
    p: Palette,
    c: iced::Color,
    title: impl Into<String>,
    body: String,
    actions: Element<'a, Message>,
) -> Element<'a, Message> {
    container(
        column![
            row![dot(c, 7.0), text(title.into()).size(13.5).font(BOLD).color(p.txt)]
                .spacing(9.0)
                .align_y(Alignment::Center),
            text(body).size(12.0).color(p.txt2),
            actions,
        ]
        .spacing(8.0),
    )
    .padding([13.0, 15.0])
    .width(Length::Fill)
    .style(move |_| ui_theme::group(p))
    .into()
}

/// The one-line form of a banner.
fn banner_line<'a>(p: Palette, c: iced::Color, title: String, action: Option<(&'static str, Message)>) -> Element<'a, Message> {
    let mut r = row![dot(c, 7.0), text(title).size(12.0).color(p.txt2).width(Length::Fill)]
        .spacing(9.0)
        .align_y(Alignment::Center);
    if let Some((label, msg)) = action {
        r = r.push(btn_link(p, label, msg));
    }
    container(r).padding([4.0, 14.0]).width(Length::Fill).style(move |_| ui_theme::group(p)).into()
}

/// A centred dialog over a dimmed window. The dim layer swallows clicks, so the page
/// behind cannot be used while the question is open.
fn dialog<'a>(p: Palette, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    mouse_area(
        container(container(body).style(move |_| ui_theme::floating(p)).padding(20.0).max_width(440.0))
            .center(Length::Fill)
            .padding(20.0)
            .style(|_| ui_theme::scrim()),
    )
    .on_press(Message::Tick)
    .into()
}

/// A numbered step of the first-run setup.
fn step<'a>(p: Palette, n: u8, title: &'static str, body: Element<'a, Message>) -> Element<'a, Message> {
    row![
        container(text(n.to_string()).size(11.5).font(BOLD).color(p.txt2))
            .center(Length::Fixed(22.0))
            .style(move |_| ui_theme::fill(p.surf2, 999.0)),
        column![text(title).size(13.5).font(BOLD).color(p.txt), body].spacing(10.0).width(Length::Fill),
    ]
    .spacing(12.0)
    .into()
}

/// One computer in the status card: its screen, name, size, and — on the one with the
/// mouse right now — "● Has the mouse".
fn machine_tile(p: Palette, name: &str, detail: &str, active: bool, present: bool) -> Element<'static, Message> {
    let name_colour = if present { p.txt } else { p.txt2 };
    container(
        column![
            row![
                container(Space::new()).width(16.0).height(11.0).style(move |_| ui_theme::screen_glyph(name_colour)),
                text(name.to_string()).size(14.0).font(BOLD).color(name_colour).width(Length::Fill),
            ]
            .spacing(9.0)
            .align_y(Alignment::Center),
            text(detail.to_string()).size(11.5).color(p.txt3),
            // Both tiles get the line, blank on the other one, so they stay the same height
            // as control moves back and forth.
            text(if active { "• Has the mouse" } else { "\u{a0}" }).size(11.5).font(BOLD).color(p.accent),
        ]
        .spacing(4.0),
    )
    .style(move |_| ui_theme::tile(p, active, present))
    .padding([11.0, 13.0])
    .width(Length::Fill)
    .into()
}

/// The kind of a history entry as a small badge.
fn kind_badge<'a>(p: Palette, m: &mouse_clipboard::ClipMeta) -> Element<'a, Message> {
    let kind = if m.is_files {
        "FILE"
    } else if m.is_image {
        "IMG"
    } else {
        "TXT"
    };
    container(text(kind).size(10.0).font(MONO).color(p.txt2))
        .padding([4.0, 6.0])
        .style(move |_| ui_theme::chip(p))
        .into()
}

/// A hotkey as key caps: "ctrl+win+alt+esc" → [Ctrl] [Win] [Alt] [Esc].
fn keycaps<'a>(p: Palette, combo: &str) -> Element<'a, Message> {
    if combo.trim().is_empty() {
        return text("None").size(12.5).color(p.txt3).into();
    }
    let mut r = row![].spacing(5.0);
    for part in combo.split('+').filter(|s| !s.trim().is_empty()) {
        r = r.push(
            container(text(key_label(part)).size(12.0).font(MONO).color(p.txt2))
                .padding([3.0, 8.0])
                .style(move |_| ui_theme::keycap(p)),
        );
    }
    r.into()
}

/// A segmented choice: (label, picked, message) for each option.
fn segmented<'a>(p: Palette, opts: Vec<(String, bool, Message)>) -> Element<'a, Message> {
    let mut r = row![].spacing(2.0);
    for (label, active, msg) in opts {
        r = r.push(
            button(text(label).size(12.0).font(if active { BOLD } else { iced::Font::DEFAULT }))
                .style(ui_theme::segment(p, active))
                .padding([5.0, 10.0])
                .on_press(msg),
        );
    }
    container(r).padding(3.0).style(move |_| ui_theme::seg_track(p)).into()
}

fn chip_toggle<'a>(p: Palette, label: &'static str, on: bool, msg: Message) -> Element<'a, Message> {
    button(text(if on { format!("✓ {label}") } else { label.to_string() }).size(12.0).font(BOLD))
        .style(ui_theme::chip_toggle(p, on))
        .padding([4.0, 10.0])
        .on_press(msg)
        .into()
}

/// The code being typed, drawn as one box per digit. An invisible input on top takes the
/// typing, so the boxes behave like one field (paste works too).
/// The pairing code as one box per digit, each a real input: it shows the blinking cursor
/// and a highlighted border when selected, any box can be clicked to change just that
/// digit, and typing moves on to the next box (owner, 30 Oct). Pasting a whole code into
/// any box fills the boxes from there.
fn code_boxes<'a>(
    p: Palette,
    value: &'a str,
    n: usize,
    on_digit: impl Fn(usize, String) -> Message + Clone + 'a,
    on_submit: Message,
) -> Element<'a, Message> {
    let typed: Vec<char> = value.chars().collect();
    let mut boxes = row![].spacing(7.0);
    for i in 0..n {
        let ch = typed.get(i).filter(|c| c.is_ascii_digit()).map(|c| c.to_string()).unwrap_or_default();
        let f = on_digit.clone();
        boxes = boxes.push(
            text_input("", &ch)
                .id(code_box_id(i))
                .on_input(move |v| f(i, v))
                .on_submit(on_submit.clone())
                .style(ui_theme::input(p))
                .align_x(Alignment::Center)
                .font(MONO)
                .size(18.0)
                .padding([9.0, 2.0])
                .width(36.0),
        );
    }
    boxes.into()
}

/// The widget ID of pairing-code box `i`, so typing can move the cursor between boxes.
fn code_box_id(i: usize) -> iced::widget::Id {
    iced::widget::Id::from(format!("pair-digit-{i}"))
}

/// The code after `typed` arrived in box `i` of `old` (one char per box, ' ' for empty),
/// and the box to put the cursor in next.
fn code_after_typing(old: &str, i: usize, typed: &str, n: usize) -> (String, Option<usize>) {
    let mut slots: Vec<char> = old.chars().chain(std::iter::repeat(' ')).take(n).collect();
    let digits: Vec<char> = typed.chars().filter(char::is_ascii_digit).collect();
    let before = slots[i];
    let next = match digits.len() {
        // The box was emptied: go back one, like Backspace in a single field.
        0 => {
            slots[i] = ' ';
            i.checked_sub(1)
        }
        1 => {
            slots[i] = digits[0];
            Some(i + 1).filter(|j| *j < n)
        }
        // One new digit typed beside the old one: the new one replaces it.
        2 if digits.contains(&before) => {
            slots[i] = if digits[0] == before { digits[1] } else { digits[0] };
            Some(i + 1).filter(|j| *j < n)
        }
        // A pasted code: fill from this box on.
        _ => {
            let mut j = i;
            for d in digits {
                if j >= n {
                    break;
                }
                slots[j] = d;
                j += 1;
            }
            Some(j.min(n - 1))
        }
    };
    let code: String = slots.into_iter().collect();
    (code.trim_end().to_string(), next)
}

fn btn_primary<'a>(p: Palette, label: impl Into<String>, msg: Option<Message>) -> Element<'a, Message> {
    button(text(label.into()).size(13.0).font(BOLD))
        .style(ui_theme::primary(p))
        .padding([8.0, 15.0])
        .on_press_maybe(msg)
        .into()
}

fn btn_secondary<'a>(p: Palette, label: impl Into<String>, msg: Option<Message>) -> Element<'a, Message> {
    button(text(label.into()).size(13.0).font(BOLD))
        .style(ui_theme::secondary(p))
        .padding([8.0, 14.0])
        .on_press_maybe(msg)
        .into()
}

fn btn_danger<'a>(p: Palette, label: impl Into<String>, msg: Option<Message>) -> Element<'a, Message> {
    button(text(label.into()).size(13.0).font(BOLD))
        .style(ui_theme::danger(p))
        .padding([8.0, 15.0])
        .on_press_maybe(msg)
        .into()
}

fn btn_link<'a>(p: Palette, label: impl Into<String>, msg: Message) -> Element<'a, Message> {
    button(text(label.into()).size(12.5).font(BOLD))
        .style(ui_theme::link(p))
        .padding([6.0, 4.0])
        .on_press(msg)
        .into()
}

/// Why a lane port cannot be used, or `None` when it can.
///
/// Below 1024 are the system's own services (22 ssh, 80/443 web…). On the computer that
/// listens, a port another program already holds would leave file copying silently dead,
/// so it is tried here first; a port this session's own lane holds is of course fine.
fn lane_port_problem(port: u16, cfg: &Config, live: Option<&Config>) -> Option<String> {
    if port < 1024 {
        return Some(format!(
            "{port} is reserved for system services (like 22, 80, 443). Use 1024–65535, e.g. 24802."
        ));
    }
    if cfg.role != Role::Server {
        return None;
    }
    let main = cfg.listen_port;
    let ours = live.is_some_and(|l| {
        let lane = if l.bulk_port == 0 { l.listen_port } else { l.bulk_port };
        lane == port || l.listen_port == port
    });
    if port == main || ours {
        return None;
    }
    match std::net::TcpListener::bind(("0.0.0.0", port)) {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            Some(format!("{port} is already used by another program here. Try another, e.g. 24802."))
        }
        Err(_) => Some(format!("This computer won't let mouse-rs use {port}. Try another, e.g. 24802.")),
    }
}

/// This computer's addresses another computer could type, without the 0.0.0.0 wildcard
/// the listen-host list starts with.
fn own_addresses() -> Vec<String> {
    mouse_core::discover::listen_candidates().into_iter().filter(|a| a != "0.0.0.0").collect()
}

/// "on the left", "above": where the other computer sits, as a phrase.
fn side_phrase(e: Edge) -> &'static str {
    match e {
        Edge::Left => "on the left",
        Edge::Right => "on the right",
        Edge::Top => "above",
        Edge::Bottom => "below",
    }
}

/// The saved profiles with their settings.
fn load_profiles() -> Vec<(String, Config)> {
    Config::list_profiles()
        .into_iter()
        .filter_map(|n| Config::load_profile(&n).ok().map(|c| (n, c)))
        .collect()
}

/// Whether two configs hold the same settings — how a profile is known to be the one in use.
fn same_settings(a: &Config, b: &Config) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// A profile in one line: who it talks to, the side, the way back.
fn profile_summary(c: &Config) -> String {
    let who = match c.role {
        Role::Server => "shares its mouse".to_string(),
        Role::Client if !c.server_host.trim().is_empty() => c.server_host.trim().to_string(),
        Role::Client => "is controlled".to_string(),
    };
    let side = match c.role {
        Role::Server => c.exit_edge,
        Role::Client => c.entry_edge,
    };
    format!("{who} · {} edge · {}", side.label(), key_label(&c.lock_hotkey))
}

/// A key the recorder heard, as the hotkey parser spells it. `None` for a modifier on its
/// own — the recorder waits for the key that goes with it.
fn key_token(key: &iced::keyboard::Key) -> Option<String> {
    use iced::keyboard::key::Named as N;
    use iced::keyboard::Key;
    match key {
        Key::Named(n) => {
            let s = match n {
                N::ScrollLock => "scroll_lock",
                N::Pause => "pause",
                N::PrintScreen => "print_screen",
                N::Insert => "insert",
                N::Delete => "delete",
                N::Home => "home",
                N::End => "end",
                N::PageUp => "page_up",
                N::PageDown => "page_down",
                N::CapsLock => "caps_lock",
                N::NumLock => "num_lock",
                N::Escape => "esc",
                N::Enter => "enter",
                N::Tab => "tab",
                N::Space => "space",
                N::Backspace => "backspace",
                N::ArrowUp => "up",
                N::ArrowDown => "down",
                N::ArrowLeft => "left",
                N::ArrowRight => "right",
                N::ContextMenu => "menu",
                N::F1 => "f1",
                N::F2 => "f2",
                N::F3 => "f3",
                N::F4 => "f4",
                N::F5 => "f5",
                N::F6 => "f6",
                N::F7 => "f7",
                N::F8 => "f8",
                N::F9 => "f9",
                N::F10 => "f10",
                N::F11 => "f11",
                N::F12 => "f12",
                N::F13 => "f13",
                N::F14 => "f14",
                N::F15 => "f15",
                N::F16 => "f16",
                N::F17 => "f17",
                N::F18 => "f18",
                N::F19 => "f19",
                N::F20 => "f20",
                N::F21 => "f21",
                N::F22 => "f22",
                N::F23 => "f23",
                N::F24 => "f24",
                _ => return None,
            };
            Some(s.to_string())
        }
        Key::Character(c) => {
            let mut it = c.chars();
            let ch = it.next()?;
            it.next().is_none().then(|| ch.to_lowercase().to_string())
        }
        Key::Unidentified => None,
    }
}

/// A hotkey as people say it: "scroll_lock" reads as "Scroll Lock", "ctrl+alt+f12" as
/// "Ctrl + Alt + F12".
fn key_label(key: &str) -> String {
    if key.trim().is_empty() {
        return "none set".to_string();
    }
    key.split('+')
        .map(|k| {
            k.trim()
                .split('_')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" + ")
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
        (true, Some(StatusSnapshot { refused: Some(why), .. })) => format!("mouse-rs — refused: {}", refusal_text(why)),
        (true, _) => "mouse-rs — waiting for peer".to_owned(),
    }
}

/// Pair with the server using `code`. With no server address set, every machine on this
/// computer's networks that answers on the server port is tried; only the one showing the
/// code can pair. Blocking: runs on its own thread.
fn pair_in_background(cfg: &Config, code: &str) -> Result<mouse_core::config::PairedDevice, String> {
    let hosts: Vec<String> = if cfg.server_host.trim().is_empty() {
        mouse_core::discover::scan_for_servers(cfg.server_port, std::time::Duration::from_millis(300))
            .into_iter()
            .map(|ip| ip.to_string())
            .collect()
    } else {
        vec![cfg.server_host.trim().to_owned()]
    };
    if hosts.is_empty() {
        return Err("no_server".into());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let mut last = Err("no_code".to_string());
    for host in hosts {
        match rt.block_on(mouse_core::net::pair_with_server(cfg, &host, code, std::time::Duration::from_secs(5))) {
            Ok(v) => return Ok(v),
            // The machine showing the code says something other than "no code": keep that.
            Err(e) if e == "no_code" => {}
            Err(e) => last = Err(e),
        }
    }
    last
}

/// A moment (seconds since 1970, UTC) as a date, like "30 Oct 2026".
fn day_text(secs: u64) -> String {
    // Days to a civil date (Howard Hinnant's algorithm), so no date library is needed.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
}

/// Today, as days since 1970 (UTC): enough to show the reminder at most once a day.
fn today() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// Pairing failures in words.
fn pair_error_text(why: &str) -> String {
    match why {
        "no_code" => "no server is showing a pairing code; press \"Pair a new device\" on it first".into(),
        "wrong_code" => format!(
            "wrong code; after {} wrong codes the server needs a new one",
            mouse_core::pairing::MAX_FAILS
        ),
        "no_server" => "no server found on this computer's networks; type its address and try again".into(),
        "old_version" => "the server runs an older mouse-rs; update it".into(),
        "that is this computer" => "that code is showing on this computer; type it on the other one".into(),
        other => format!("pairing failed: {other}"),
    }
}

/// Why a connection was refused, in words. Reasons from the other computer arrive as
/// short codes; reasons this computer wrote itself are already sentences.
fn refusal_text(why: &str) -> String {
    match why {
        busy if busy.starts_with("busy:") => format!(
            "the other computer is working with {} right now; on it, click Connect next to this computer to switch",
            &busy[5..]
        ),
        "not_paired_server" | "not_chosen_server" => {
            "a computer answered that isn't the one chosen here; it was ignored (Connect picks another)".into()
        }
        "not_paired" => "the other computer is not paired with this one (or removed it); pair them again with a code".into(),
        "server_not_verified" => "the other computer could not prove it is the one you paired with; nothing was sent to it".into(),
        "bad_secret" => "the pairing keys don't match; pair the two computers again with a code".into(),
        "old_version" => "the other computer runs an older mouse-rs; update both".into(),
        other => other.into(),
    }
}

/// Open the control panel, blocking until the window closes.
pub fn launch(cfg: Config) -> anyhow::Result<()> {
    iced::application(
        move || (App::new(cfg.clone()), iced::system::theme().map(Message::SystemTheme)),
        App::update,
        App::view,
    )
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
            // Closing still quits; `close_requests` routes it through Quit's check.
            exit_on_close_request: false,
            ..Default::default()
        })
        .run()
        .map_err(|e| anyhow::anyhow!("control panel: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_into_the_code_boxes() {
        // Typing moves on box by box.
        assert_eq!(code_after_typing("", 0, "4", 6), ("4".into(), Some(1)));
        assert_eq!(code_after_typing("4", 1, "7", 6), ("47".into(), Some(2)));
        // Fixing just the third digit of a full code.
        assert_eq!(code_after_typing("123456", 2, "39", 6), ("129456".into(), Some(3)));
        assert_eq!(code_after_typing("123456", 2, "93", 6), ("129456".into(), Some(3)));
        // Deleting a digit empties that box only and steps back.
        assert_eq!(code_after_typing("123456", 2, "", 6), ("12 456".into(), Some(1)));
        // A whole code pasted into the first box fills all six.
        assert_eq!(code_after_typing("", 0, "654321", 6), ("654321".into(), Some(5)));
        // Letters are ignored.
        assert_eq!(code_after_typing("1", 1, "a", 6), ("1".into(), Some(0)));
    }

    #[test]
    fn pairing_dates_read_as_dates() {
        assert_eq!(day_text(0), "1 Jan 1970");
        assert_eq!(day_text(1_793_330_000), "30 Oct 2026");
        assert_eq!(day_text(951_782_400), "29 Feb 2000");
    }

    /// R40, from the owner's design (28 Oct; R30 had Startup under Everyday): the
    /// categories, their order and their groups. Every page is listed exactly once.
    #[test]
    fn settings_categories_are_in_the_agreed_order() {
        let order: Vec<&str> = Page::CATEGORIES.iter().flat_map(|(_, pages)| pages.iter().map(|p| p.title())).collect();
        assert_eq!(
            order,
            [
                "Clipboard",
                "Screens & edges",
                "Keyboard & hotkeys",
                "Startup",
                "Connection",
                "Security & pairing",
                "Appearance",
                "Diagnostics",
                "About",
            ]
        );
        let groups: Vec<&str> = Page::CATEGORIES.iter().map(|(g, _)| *g).collect();
        assert_eq!(groups, ["EVERYDAY", "THIS COMPUTER", "APP"]);
        assert!(order.iter().all(|t| !Page::Index.title().eq(*t)), "the index is not a category");
    }

    /// "At a glance" names the return key the way people say it.
    #[test]
    fn hotkeys_read_the_way_people_say_them() {
        assert_eq!(key_label("scroll_lock"), "Scroll Lock");
        assert_eq!(key_label("ctrl+win+alt+esc"), "Ctrl + Win + Alt + Esc");
        assert_eq!(key_label("  "), "none set");
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
    /// R40: the six themes, saved by key; anything unknown opens as Amber.
    #[test]
    fn themes_round_trip_through_their_config_key() {
        for t in Theme6::ALL {
            assert_eq!(Theme6::from_key(t.key()), t);
        }
        assert_eq!(Theme6::from_key("midnight"), Theme6::Ember, "an old build's theme falls back");
        assert_eq!(Theme6::from_key(&Config::default().theme), Theme6::Ember, "Amber is the default");
        // The mixed themes have a dark top over a light body; the rest are one tone.
        for t in [Theme6::Plum, Theme6::Forest, Theme6::Slate] {
            assert!(t.hero().dark && !t.palette().dark, "{t:?}");
        }
        for t in [Theme6::Graphite, Theme6::Ember, Theme6::Porcelain] {
            assert_eq!(t.hero().bg, t.palette().bg, "{t:?}");
        }
    }

    /// Recorded hotkeys are spelled the way the hotkey parser reads them.
    #[test]
    fn recorded_keys_use_the_parsers_spelling() {
        use iced::keyboard::{key::Named, Key};
        assert_eq!(key_token(&Key::Named(Named::ScrollLock)).as_deref(), Some("scroll_lock"));
        assert_eq!(key_token(&Key::Named(Named::Escape)).as_deref(), Some("esc"));
        assert_eq!(key_token(&Key::Named(Named::F12)).as_deref(), Some("f12"));
        assert_eq!(key_token(&Key::Character("K".into())).as_deref(), Some("k"));
        assert_eq!(key_token(&Key::Named(Named::Control)), None, "a modifier alone waits for its key");
        for tok in ["scroll_lock", "esc", "f12", "page_up", "print_screen"] {
            assert!(mouse_input::NamedKey::from_name(tok).is_some(), "{tok} must parse");
        }
    }

    /// A profile is "Active" when it holds exactly the settings in use.
    #[test]
    fn the_active_profile_is_the_one_with_the_same_settings() {
        let a = Config::default();
        let mut b = a.clone();
        assert!(same_settings(&a, &b));
        b.exit_edge = b.exit_edge.opposite();
        assert!(!same_settings(&a, &b));
        assert!(profile_summary(&a).contains("Scroll Lock"));
    }

    /// Reserved ports are refused with a reason; a port this computer is already using
    /// for something else is refused; a free high port is taken.
    #[test]
    fn the_lane_port_must_be_one_that_can_work() {
        let mut cfg = Config::default();
        cfg.role = Role::Server;
        for reserved in [20, 22, 80, 443, 1023] {
            assert!(lane_port_problem(reserved, &cfg, None).unwrap().contains("reserved"));
        }
        let held = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let busy = held.local_addr().unwrap().port();
        assert!(lane_port_problem(busy, &cfg, None).unwrap().contains("already used"));
        let free = { let l = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap(); l.local_addr().unwrap().port() };
        assert_eq!(lane_port_problem(free, &cfg, None), None);
        assert_eq!(lane_port_problem(cfg.listen_port, &cfg, None), None, "the main port is the shared default");
        cfg.role = Role::Client;
        assert_eq!(lane_port_problem(busy, &cfg, None), None, "a client does not listen");
    }
}
