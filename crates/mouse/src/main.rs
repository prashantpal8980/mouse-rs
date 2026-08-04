//! mouse-rs entry point.
//!
//! * `mouse-rs server` / `mouse-rs client` — headless KVM link for that role.
//! * `mouse-rs` (bare) or `mouse-rs gui` — open the egui control panel (Phase 6),
//!   which edits config and Start/Stops the engine over the same [`run`] lifecycle.
//!
//! The engine lifecycle (link + edge-crossing engine + input backends, torn down
//! with bounded joins so Stop never hangs and input is never left stuck) lives in
//! [`run`]; both the CLI and the GUI drive it.

use mouse_core::{Config, Role};
use tokio_util::sync::CancellationToken;

mod logging;
mod run;

#[cfg(feature = "gui")]
mod autostart;
#[cfg(feature = "gui")]
mod gui;
#[cfg(feature = "gui")]
mod ui_theme;
#[cfg(feature = "gui")]
mod tray;

fn main() -> anyhow::Result<()> {
    let sub = std::env::args().nth(1);
    let is_gui = matches!(sub.as_deref(), None | Some("gui"));

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
    logging::init(is_gui && cfg!(feature = "gui"));

    let cfg = Config::load_or_default();

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
        Some(other) => {
            eprintln!("unknown argument: {other}");
            eprintln!("usage: mouse-rs [server|client|gui]");
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
