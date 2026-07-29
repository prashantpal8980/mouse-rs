//! iced control panel (the product UI). A bare `mouse-rs` launch opens this window:
//! edit config, Save it, and Start/Stop the engine — the same lifecycle the headless
//! CLI uses, so the GUI is just a front-end over [`crate::run`]. Closing the window
//! quits the app. The look (6 themes, status hero, animated toggles) lives in
//! [`crate::ui_theme`]; the system tray in [`crate::tray`].

use iced::widget::{
    button, column, container, pick_list, row, scrollable, text, text_input, toggler, Space,
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
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Connection, Tab::Layout, Tab::Security, Tab::Clipboard];
    fn label(self) -> &'static str {
        match self {
            Tab::Connection => "Connection",
            Tab::Layout => "Layout",
            Tab::Security => "Security",
            Tab::Clipboard => "Clipboard",
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
    Quit,
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
            Message::TabPicked(t) => self.tab = t,
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
                        "saved".into()
                    }
                    Err(e) => {
                        tracing::warn!("could not save config to {}: {e}", path.display());
                        format!("save failed: {e}")
                    }
                };
            }
            Message::Quit => {
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

        let content = column![
            self.header(p, running, snap.as_ref()),
            self.hero(p, running, snap.as_ref()),
            self.tab_bar(p),
            scrollable(container(self.tab_body(p, running)).padding([4.0, 2.0]))
                .height(Length::Fill),
            self.footer(p, running),
        ]
        .spacing(14.0)
        .padding(18.0);

        container(content)
            .style(move |_| ui_theme::root(p))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    // ---------- header ----------

    fn header(&self, p: Palette, running: bool, snap: Option<&StatusSnapshot>) -> Element<'_, Message> {
        let mut swatches = row![].spacing(6.0).align_y(Alignment::Center);
        swatches = swatches.push(text("THEME").size(10.0).color(p.txt3));
        for t in Theme6::ALL {
            let base = t.palette();
            swatches = swatches.push(
                button(Space::new().width(18.0).height(18.0))
                    .style(ui_theme::swatch(p, base, t == self.theme))
                    .padding(0.0)
                    .on_press(Message::ThemePicked(t)),
            );
        }

        let (pill_fill, pill_txt, label) = status_pill(p, running, snap);
        let status = container(text(label).size(12.0).color(pill_txt))
            .style(move |_| ui_theme::pill(pill_fill, pill_txt))
            .padding([4.0, 10.0]);

        row![
            text("mouse-rs").size(22.0).color(p.txt),
            Space::new().width(Length::Fill),
            swatches,
            status,
        ]
        .spacing(12.0)
        .align_y(Alignment::Center)
        .into()
    }

    // ---------- status hero ----------

    fn hero(&self, p: Palette, running: bool, snap: Option<&StatusSnapshot>) -> Element<'_, Message> {
        let engaged = snap.map(|s| s.connected && s.engaged).unwrap_or(false);
        let server_side = self.cfg.role == Role::Server;

        // Local machine card; the peer card from the live snapshot.
        let local = machine_card(
            p,
            &self.cfg.name,
            if server_side { "server" } else { "client" },
            &format!("{}×{}", self.cfg.screen_width, self.cfg.screen_height),
            // active = we currently hold control locally (not engaged yet, or we're the
            // client being controlled means the server side is the active one instead)
            !engaged || !server_side,
        );
        let (peer_name, peer_res) = match snap {
            Some(s) if s.connected => (s.peer.clone(), format!("{}×{}", s.peer_w, s.peer_h)),
            _ => ("waiting…".to_string(), "—".to_string()),
        };
        let peer = machine_card(
            p,
            &peer_name,
            if server_side { "client" } else { "server" },
            &peer_res,
            engaged && server_side,
        );

        let link = text(if running { "⇄" } else { "—" })
            .size(24.0)
            .color(if running { p.accent } else { p.txt3 });

        let badge_txt = hero_line(running, self.cfg.role, snap);
        let badge = text(badge_txt).size(12.0).color(if running { p.good } else { p.txt3 });

        let body = column![
            badge,
            row![local, container(link).center_y(Length::Fill), peer]
                .spacing(14.0)
                .align_y(Alignment::Center),
        ]
        .spacing(14.0);

        container(body)
            .style(move |_| ui_theme::hero(p))
            .padding(20.0)
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
            column![
                labeled(p, "Listen host", input(p, "0.0.0.0", &self.cfg.listen_host, Message::ListenHostChanged)),
                labeled(p, "Listen port", input(p, "24800", &self.listen_port_s, Message::ListenPortChanged)),
            ]
            .spacing(12.0)
        } else {
            column![
                labeled(p, "Server host / IP", input(p, "192.168.1.50", &self.cfg.server_host, Message::ServerHostChanged)),
                labeled(p, "Server port", input(p, "24800", &self.server_port_s, Message::ServerPortChanged)),
                toggle_row(p, "Auto-reconnect", "retry with backoff until the server appears", self.cfg.reconnect, Message::ReconnectToggled),
            ]
            .spacing(12.0)
        };

        // Hands-off start at login. The three are deliberately independent: the OS
        // launches the panel (Start at login), the panel brings the link up by itself
        // (Auto-connect), and it stays out of the way while doing so (Start minimized).
        let startup = column![
            field_label(p, "Startup"),
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
            field_label(p, "Role"),
            role,
            net,
            startup,
            logs,
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
            toggle_row(p, "Sync clipboard", "copy here, paste there", self.cfg.clipboard_sync, Message::ClipboardToggled),
            labeled(p, "Return hotkey (server)", input(p, "ctrl+win+alt+esc", &self.cfg.return_hotkey, Message::ReturnHotkeyChanged)),
            labeled(p, "Lock / cross hotkey", input(p, "scroll_lock", &self.cfg.lock_hotkey, Message::LockHotkeyChanged)),
        ]
        .spacing(14.0))
    }

    fn tab_clipboard(&self, p: Palette) -> Element<'_, Message> {
        let history = self.engine.as_ref().map(|e| e.clipboard_history());
        let mut list = column![].spacing(8.0);
        match history {
            None => list = list.push(text("Start sharing to see synced clipboard text here.").size(12.0).color(p.txt3)),
            Some(entries) if entries.is_empty() => {
                list = list.push(text("Nothing synced yet.").size(12.0).color(p.txt3))
            }
            Some(entries) => {
                for e in entries {
                    let (arrow, col) = if e.incoming { ("↓ in", p.accent) } else { ("↑ out", p.good) };
                    list = list.push(
                        container(row![
                            text(arrow).size(11.0).color(col),
                            text(preview(&e.text)).size(13.0).color(p.txt2),
                        ].spacing(10.0))
                        .style(move |_| ui_theme::well(p))
                        .padding([10.0, 12.0])
                        .width(Length::Fill),
                    );
                }
            }
        }
        card(p, column![field_label(p, "Clipboard history — newest first"), list].spacing(12.0))
    }

    // ---------- profiles + footer ----------

    fn footer(&self, p: Palette, running: bool) -> Element<'_, Message> {
        let start = button(text(if running { "Running" } else { "Start" }).size(14.0))
            .style(ui_theme::primary(p))
            .padding([12.0, 22.0]);
        let start = if running { start } else { start.on_press(Message::Start) };

        let stop = button(text("Stop").size(14.0))
            .style(ui_theme::danger(p))
            .padding([12.0, 18.0]);
        let stop = if running { stop.on_press(Message::Stop) } else { stop };

        let save = button(text("Save").size(13.0))
            .style(ui_theme::ghost(p))
            .padding([11.0, 14.0])
            .on_press(Message::SaveConfig);

        let quit = button(text("Quit").size(13.0))
            .style(ui_theme::danger(p))
            .padding([11.0, 14.0])
            .on_press(Message::Quit);

        column![
            self.profiles_row(p),
            row![
                text(&self.feedback).size(12.0).color(p.txt3),
                Space::new().width(Length::Fill),
                save,
                start,
                stop,
                quit,
            ]
            .spacing(8.0)
            .align_y(Alignment::Center),
        ]
        .spacing(10.0)
        .into()
    }

    fn profiles_row(&self, p: Palette) -> Element<'_, Message> {
        let picker = pick_list(self.profiles.clone(), self.profile_pick.clone(), Message::ProfilePicked)
            .placeholder("profile")
            .padding(8.0)
            .width(150.0);
        row![
            picker,
            button(text("Load").size(12.0)).style(ui_theme::ghost(p)).padding([8.0, 12.0]).on_press(Message::LoadProfile),
            button(text("Delete").size(12.0)).style(ui_theme::ghost(p)).padding([8.0, 12.0]).on_press(Message::DeleteProfile),
            input(p, "save as…", &self.profile_name_input, Message::ProfileNameChanged).width(140.0),
            button(text("Save as").size(12.0)).style(ui_theme::ghost(p)).padding([8.0, 12.0]).on_press(Message::SaveProfileAs),
        ]
        .spacing(8.0)
        .align_y(Alignment::Center)
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

fn machine_card(p: Palette, name: &str, role: &str, res: &str, active: bool) -> Element<'static, Message> {
    let screen = container(text(role.to_uppercase()).size(10.0).color(p.txt3))
        .style(move |_| if active { ui_theme::well_active(p) } else { ui_theme::well(p) })
        .center_x(Length::Fill)
        .center_y(70.0)
        .width(Length::Fill);
    column![
        screen,
        text(name.to_string()).size(14.0).color(p.txt),
        text(format!("{role} · {res}")).size(11.0).color(p.txt3),
    ]
    .spacing(6.0)
    .align_x(Alignment::Center)
    .width(Length::Fill)
    .into()
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

/// A single-line, length-capped preview of clipboard text.
fn preview(text: &str) -> String {
    const MAX: usize = 58;
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > MAX {
        format!("{}…", one_line.chars().take(MAX).collect::<String>())
    } else {
        one_line
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
        .window_size(iced::Size::new(600.0, 840.0))
        .run()
        .map_err(|e| anyhow::anyhow!("control panel: {e}"))
}
