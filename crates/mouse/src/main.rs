//! mouse-rs entry point.
//!
//! * `mouse-rs server` / `mouse-rs client` — headless KVM link for that role.
//! * `mouse-rs greeter` — the login-screen instance (Linux): input only, injected
//!   through uinput so it works before anyone has a session. Run by the system service
//!   that `greeter-install` sets up, not by hand.
//! * `mouse-rs` (bare) or `mouse-rs gui` — open the egui control panel (Phase 6),
//!   which edits config and Start/Stops the engine over the same [`run`] lifecycle.
//!
//! The engine lifecycle (link + edge-crossing engine + input backends, torn down
//! with bounded joins so Stop never hangs and input is never left stuck) lives in
//! [`run`]; both the CLI and the GUI drive it.

use mouse_core::{Config, Role};
use tokio_util::sync::CancellationToken;

mod logging;
mod logship;
mod run;
mod update;

#[cfg(target_os = "linux")]
mod greeter;

#[cfg(feature = "gui")]
mod autostart;
#[cfg(feature = "gui")]
mod gui;
#[cfg(feature = "gui")]
mod ui_theme;
#[cfg(feature = "gui")]
mod tray;
#[cfg(feature = "gui")]
mod notify;

fn main() -> anyhow::Result<()> {
    // Before anything else: the path has to be read while it still names this file, and
    // a copy started by an update waits here for the one it replaces to be gone.
    let _ = update::exe_path();
    update::after_restart();

    let sub = std::env::args().nth(1);

    let is_gui = matches!(sub.as_deref(), None | Some("gui"));
    // The login-screen service logs to a file like the GUI does, because the file is
    // what gets shipped — a service that only logged to the journal would be the one
    // part of the system nobody could see from the other machine.
    #[cfg(target_os = "linux")]
    let to_file = is_gui && cfg!(feature = "gui") || sub.as_deref() == Some("greeter");
    #[cfg(not(target_os = "linux"))]
    let to_file = is_gui && cfg!(feature = "gui");

    // On Windows the binary stays a *console* subsystem app so `server`/`client` run in
    // the foreground with logs and Ctrl-C. For a GUI launch we instead drop the console
    // up-front, so the control panel doesn't carry a console window behind it.
    #[cfg(all(windows, feature = "gui"))]
    if is_gui {
        // SAFETY: FreeConsole takes no arguments and only detaches this process from its
        // own console; harmless (returns 0) if there isn't one.
        unsafe { windows_sys::Win32::System::Console::FreeConsole() };
    }

    // Must follow the FreeConsole above: a GUI launch has no console left to log to, so it
    // logs to a file instead (see `logging`). Headless runs keep stdout.
    logging::init(to_file);

    let cfg = Config::load_or_default();

    // A desktop launch is the moment to notice that the login-screen copy has fallen behind.
    #[cfg(target_os = "linux")]
    if matches!(sub.as_deref(), None | Some("gui") | Some("client") | Some("server")) {
        greeter::warn_if_stale();
    }

    // Held for the life of the process: dropping it sends one final copy, which is the
    // most valuable one because it holds whatever happened last. Started here rather
    // than inside the engine so it also covers a run that never manages to connect —
    // exactly the case where the log is the only evidence there is.
    //
    // The login-screen instance has no config under its HOME; its settings are in
    // /etc/mouse-rs. It ships from those, under a name of its own, so its log is neither
    // left unshipped nor written over the desktop instance's copy on the collector.
    #[cfg(target_os = "linux")]
    let (ship_url, ship_name) = if sub.as_deref() == Some("greeter") {
        match greeter::load_config(&greeter::config_path()) {
            Ok(g) => (g.log_upload_url, format!("{}-login-screen", g.name)),
            Err(_) => (String::new(), String::new()),
        }
    } else {
        (cfg.log_upload_url.clone(), cfg.name.clone())
    };
    #[cfg(not(target_os = "linux"))]
    let (ship_url, ship_name) = (cfg.log_upload_url.clone(), cfg.name.clone());
    let _logship = logship::LogShipper::spawn(
        &ship_url,
        &logging::log_path(),
        &ship_name,
        cfg.log_upload_secs,
        mouse_core::auth::log_upload_token(&cfg.shared_secret),
    );

    match sub.as_deref() {
        Some("server") => run::run_blocking(
            force_role(cfg, Role::Server),
            CancellationToken::new(),
            true,
            None,
            None,
            None,
            None,
        ),
        Some("client") => run::run_blocking(
            force_role(cfg, Role::Client),
            CancellationToken::new(),
            true,
            None,
            None,
            None,
            None,
        ),
        Some("gui") | None => launch_gui(cfg),

        // --- the elevated helper behind "Start at login as administrator" (Windows) ---
        // Started by the panel through Windows' consent prompt; makes the change and exits.
        #[cfg(windows)]
        Some("autostart-elevated-on") => autostart::enable_elevated(),
        #[cfg(windows)]
        Some("autostart-elevated-off") => autostart::disable_elevated(),
        #[cfg(windows)]
        Some("firewall-allow") => autostart::firewall_allow(),

        // --- the login-screen instance and its setup (Linux only) ---
        #[cfg(target_os = "linux")]
        Some("greeter") => greeter::run(),
        #[cfg(target_os = "linux")]
        Some("greeter-install") => {
            greeter::install(std::env::args().nth(2).map(std::path::PathBuf::from))
        }
        #[cfg(target_os = "linux")]
        Some("greeter-uninstall") => greeter::uninstall(),
        #[cfg(target_os = "linux")]
        Some("greeter-selftest") => greeter::selftest(),
        #[cfg(target_os = "linux")]
        Some("greeter-status") => greeter::status(),

        Some(other) => {
            eprintln!("unknown argument: {other}");
            eprintln!("usage: mouse-rs [server|client|gui]");
            #[cfg(target_os = "linux")]
            eprintln!(
                "       mouse-rs greeter-install [config]   share input at the login screen\n\
                 \x20      mouse-rs greeter-uninstall          undo that\n\
                 \x20      mouse-rs greeter-selftest           prove uinput works\n\
                 \x20      mouse-rs greeter-status             what it is currently doing"
            );
            std::process::exit(2);
        }
    }
}

/// The CLI subcommand forces the role, overriding the config's `role` field.
fn force_role(mut cfg: Config, role: Role) -> Config {
    cfg.role = role;
    cfg
}

#[cfg(feature = "gui")]
fn launch_gui(cfg: Config) -> anyhow::Result<()> {
    if let Err(e) = gui::launch(cfg) {
        // A headless box (no display) can't open the panel — point at the subcommands.
        tracing::error!(
            "could not open the control panel ({e}); on a headless machine run \
             `mouse-rs server` or `mouse-rs client`"
        );
    }
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn launch_gui(_cfg: Config) -> anyhow::Result<()> {
    tracing::info!("built without the `gui` feature; use `mouse-rs server` or `mouse-rs client`");
    Ok(())
}
