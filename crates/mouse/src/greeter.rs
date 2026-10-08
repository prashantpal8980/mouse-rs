//! The login-screen instance: share the keyboard and mouse *before* anyone has logged in.
//!
//! The problem this solves is small to describe and annoying to live with. Autostart is
//! an XDG desktop entry, so mouse-rs starts when the desktop session starts — which is
//! after the display manager has already taken a password. Typing that password needs a
//! keyboard physically attached to *this* machine, even though there is a perfectly good
//! one attached to the other one.
//!
//! So this mode runs as a system service from boot, acting as a client with nothing but
//! the input link, and stops as soon as a real session takes over.
//!
//! Three things make it different from a normal client run:
//!
//! * **Injection goes through uinput**, not X11 — there is no X server of ours to talk to.
//!   See [`mouse_input::uinput`].
//! * **No clipboard.** File and text sync own an X11 selection, which needs a session.
//!   There is also nowhere sensible to paste at a login prompt.
//! * **Config comes from `/etc`, not `$HOME`.** A service that runs before login must not
//!   depend on a user's home directory being present, mounted or decrypted.
//!
//! ## Handing over at login
//!
//! Once someone logs in, their own instance starts and connects. Two clients injecting the
//! same stream means every keystroke arrives twice, so exactly one must be running: this
//! one stops the moment logind reports an active graphical session. `Restart=always` in
//! the unit brings it back when that session ends, which is when the login screen returns.
//!
//! ## What this costs, stated plainly
//!
//! It is a network-driven keystroke injector running before anyone has authenticated.
//! A peer that holds `shared_secret` can type at the login prompt — that is the feature,
//! not a flaw in it, but it means the secret is now what stands between the network and
//! this machine's login. Hence: off unless installed deliberately, and a config readable
//! only by root.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mouse_core::{Config, Role};
use tokio_util::sync::CancellationToken;

use crate::run::{self, Session};

/// Where the login-screen instance reads its settings.
///
/// Deliberately not the user's `~/.mouse_share/config.json`: at the point this runs,
/// `/home` may not be mounted, and a service running as root should not be reaching into
/// a user's home for a secret anyway.
pub const SYSTEM_CONFIG: &str = "/etc/mouse-rs/config.json";

/// How often to check whether a session has taken over.
///
/// Two seconds: a login is not a race worth winning quickly, and the check is two or
/// three small file reads.
const POLL: Duration = Duration::from_secs(2);

/// logind's per-session state directory.
const SESSIONS: &str = "/run/systemd/sessions";

pub fn config_path() -> PathBuf {
    PathBuf::from(SYSTEM_CONFIG)
}

/// Does this session record describe a logged-in human at a graphical seat?
///
/// Split out from the directory walk so the parsing is testable without logind.
/// The greeter's own session is `CLASS=greeter`, which is exactly what must *not* match
/// — otherwise this would see the login screen it is there to serve and stop immediately.
fn is_graphical_user_session(contents: &str) -> bool {
    let mut class = "";
    let mut ty = "";
    let mut active = false;
    for line in contents.lines() {
        if let Some(v) = line.strip_prefix("CLASS=") {
            class = v.trim();
        } else if let Some(v) = line.strip_prefix("TYPE=") {
            ty = v.trim();
        } else if let Some(v) = line.strip_prefix("ACTIVE=") {
            active = v.trim() == "1";
        }
    }
    class == "user" && active && matches!(ty, "x11" | "wayland" | "mir")
}

/// Is a human logged into a graphical session right now?
///
/// False when logind is absent or unreadable. That is the deliberate direction to fail:
/// a wrong "nobody is logged in" leaves this instance running alongside a user's, which
/// shows up immediately as doubled keystrokes; a wrong "somebody is logged in" makes the
/// login screen silently unusable, which is the bug this whole mode exists to fix.
pub fn graphical_user_session_active() -> bool {
    let Ok(entries) = std::fs::read_dir(SESSIONS) else {
        return false;
    };
    for e in entries.filter_map(Result::ok) {
        let path = e.path();
        // logind keeps `<id>.ref` pipes alongside the state files; only the latter parse.
        if path.extension().is_some() {
            continue;
        }
        if let Ok(c) = std::fs::read_to_string(&path) {
            if is_graphical_user_session(&c) {
                return true;
            }
        }
    }
    false
}

/// Load the system config, shaped for a login-screen run.
///
/// Forces client role and no clipboard rather than trusting the file: both are
/// requirements of the mode, not preferences, and a copied-in config will usually say
/// otherwise simply because it was copied from a desktop one.
pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    let mut cfg = Config::load(path).map_err(|e| {
        anyhow::anyhow!(
            "cannot read {}: {e}. Run `mouse-rs greeter-install` as root to create it \
             from your own config.",
            path.display()
        )
    })?;
    if cfg.role != Role::Client {
        tracing::info!("forcing client role: the login-screen instance only receives input");
        cfg.role = Role::Client;
    }
    cfg.clipboard_sync = false;
    // Nothing is watching a log file at the login screen, and the upload is how the
    // owner sees what happened — leave whatever the config says about shipping alone.
    Ok(cfg)
}

/// Run until a graphical session takes over, then return.
///
/// Returns `Ok(())` on a clean hand-over as well as on a clean config-less exit: both are
/// normal outcomes and neither should make systemd report a failed unit.
pub fn run() -> anyhow::Result<()> {
    let path = config_path();
    let cfg = load_config(&path)?;

    // Someone is already logged in — their instance owns the link. Nothing to do, and
    // starting anyway would double every keystroke.
    if graphical_user_session_active() {
        tracing::info!("a graphical session is already active; the login screen is not showing");
        return Ok(());
    }

    tracing::info!(
        peer = %cfg.server_host,
        "serving the login screen: input only, injecting through uinput"
    );

    let cancel = CancellationToken::new();
    let engine_cancel = cancel.clone();
    let engine = std::thread::Builder::new()
        .name("greeter-engine".into())
        .spawn(move || {
            if let Err(e) = run::run_blocking_in(
                cfg,
                engine_cancel,
                false,
                None,
                None,
                None,
                None,
                Session::Greeter,
            ) {
                tracing::error!("login-screen engine exited with error: {e}");
            }
        })?;

    while !graphical_user_session_active() {
        if engine.is_finished() {
            // The engine gave up on its own (no uinput permission, say). Let the unit
            // restart us rather than spinning here with nothing running.
            tracing::warn!("the engine stopped before any session took over");
            break;
        }
        std::thread::sleep(POLL);
    }

    tracing::info!("a session has taken over; standing down so input is not injected twice");
    cancel.cancel();
    let _ = engine.join();
    Ok(())
}

// --------------------------------------------------------------- installation

/// Where the unit lives. `/etc`, not `/usr/lib`: this is local configuration, not
/// something a package shipped.
const UNIT_PATH: &str = "/etc/systemd/system/mouse-rs-greeter.service";

/// Where the service's copy of the binary lives.
///
/// The running executable is usually somewhere under `target/`, and a system service
/// must not depend on a build tree: the next `cargo clean` would leave a unit pointing
/// at nothing, and the failure would show up as a login screen that quietly stopped
/// working. So the binary is copied somewhere stable and the unit names that.
const INSTALLED_EXE: &str = "/usr/local/bin/mouse-rs";

fn require_root(what: &str) -> anyhow::Result<()> {
    // SAFETY: geteuid takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!(
            "{what} needs root: it creates a root-owned config and a system service. \
             Re-run with sudo."
        );
    }
    Ok(())
}

/// The config to copy settings from, when the caller did not name one.
///
/// Guessed from `SUDO_USER` rather than `$HOME`, which under sudo is root's. A guess is
/// fine here because being wrong is immediately visible and the remedy is to pass the
/// path explicitly.
fn guess_source_config() -> Option<PathBuf> {
    let user = std::env::var("SUDO_USER").ok()?;
    let p = PathBuf::from(format!("/home/{user}/.mouse_share/config.json"));
    p.is_file().then_some(p)
}

fn unit_text(exe: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=mouse-rs at the login screen\n\
         # Needs a route to the peer, and logind to notice when a session takes over.\n\
         After=network-online.target systemd-logind.service\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} greeter\n\
         # The process stands down when someone logs in; this is what brings it back\n\
         # when they log out and the login screen returns.\n\
         Restart=always\n\
         RestartSec=3\n\
         \n\
         # Creating a virtual input device is root-only on a stock system.\n\
         User=root\n\
         # Gives the log a home under /var/lib/mouse-rs so it can still be shipped.\n\
         Environment=HOME=/var/lib/mouse-rs\n\
         StateDirectory=mouse-rs\n\
         \n\
         # It needs /etc/mouse-rs to read, its state directory to write, /dev/uinput to\n\
         # inject into, and the network. Everything else is closed off.\n\
         NoNewPrivileges=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         ProtectKernelTunables=yes\n\
         ProtectKernelModules=yes\n\
         ProtectControlGroups=yes\n\
         RestrictNamespaces=yes\n\
         RestrictRealtime=yes\n\
         LockPersonality=yes\n\
         RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n\
         SystemCallArchitectures=native\n\
         SystemCallFilter=@system-service\n\
         # Named explicitly because PrivateDevices=yes would hide the one device the\n\
         # whole service exists to open.\n\
         DeviceAllow=/dev/uinput rw\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe = exe.display()
    )
}

/// Install the login-screen service: root-owned config, unit, enable.
///
/// `source` is the desktop config to take the peer address and shared secret from.
pub fn install(source: Option<PathBuf>) -> anyhow::Result<()> {
    require_root("installing the login-screen service")?;

    let source = source
        .or_else(guess_source_config)
        .ok_or_else(|| anyhow::anyhow!(
            "could not find your config to copy settings from. Pass it explicitly:\n  \
             sudo mouse-rs greeter-install /home/<you>/.mouse_share/config.json"
        ))?;
    let mut cfg = Config::load(&source)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", source.display()))?;

    if cfg.shared_secret.is_empty() {
        anyhow::bail!(
            "{} has no shared secret, so the login-screen instance could not \
             authenticate to the peer. Pair the two machines normally first.",
            source.display()
        );
    }

    // The mode's own requirements, not the user's preferences.
    cfg.role = Role::Client;
    cfg.clipboard_sync = false;
    // A login screen has no tray and nothing to autostart into.
    cfg.autoconnect = true;

    let dest = config_path();
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
        // The directory holds a secret that authorises keystroke injection.
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    // `Config::save` already writes 0600 inside a 0700 directory for exactly this reason.
    cfg.save(&dest)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", dest.display()))?;

    // Copy ourselves somewhere the service can rely on, unless we are already there.
    let running = std::env::current_exe()?;
    let exe = PathBuf::from(INSTALLED_EXE);
    if running != exe {
        std::fs::copy(&running, &exe).map_err(|e| {
            anyhow::anyhow!("cannot install the binary to {}: {e}", exe.display())
        })?;
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    }
    std::fs::write(UNIT_PATH, unit_text(&exe))?;

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "mouse-rs-greeter.service"])?;

    println!("Installed.");
    println!("  config  {}  (root-only; holds a copy of the shared secret)", dest.display());
    println!("  service {UNIT_PATH}");
    println!("  binary  {}  (copied from {})", exe.display(), running.display());
    println!();
    println!("It starts at the next boot. To start it now without rebooting:");
    println!("  sudo systemctl start mouse-rs-greeter");
    println!();
    println!("Re-run this after changing the shared secret or the peer address — the");
    println!("copy in /etc does not follow later edits to your own config.");
    Ok(())
}

/// Remove the service and the root-owned config.
pub fn uninstall() -> anyhow::Result<()> {
    require_root("removing the login-screen service")?;
    // Best effort in order: a half-installed state must still clean up.
    let _ = run_systemctl(&["disable", "--now", "mouse-rs-greeter.service"]);
    let mut removed = Vec::new();
    if std::fs::remove_file(UNIT_PATH).is_ok() {
        removed.push(UNIT_PATH.to_string());
    }
    let cfg = config_path();
    if std::fs::remove_file(&cfg).is_ok() {
        removed.push(cfg.display().to_string());
    }
    // Only the copy this command made; never a mouse-rs the user installed themselves,
    // which is why the path is fixed rather than "whatever is on PATH".
    if std::fs::remove_file(INSTALLED_EXE).is_ok() {
        removed.push(INSTALLED_EXE.to_string());
    }
    let _ = run_systemctl(&["daemon-reload"]);
    if removed.is_empty() {
        println!("Nothing to remove.");
    } else {
        for r in removed {
            println!("removed {r}");
        }
    }
    Ok(())
}

fn run_systemctl(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("systemctl").args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "systemctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Prove the virtual devices work, without involving the network or the login screen.
///
/// This exists because the thing being tested is otherwise only observable at a login
/// prompt, where there is no terminal to read an error from. Run it from a normal
/// session, watch the characters appear, and the uinput half is accounted for — leaving
/// only the service wiring if the real thing still misbehaves.
pub fn selftest() -> anyhow::Result<()> {
    require_root("the self-test")?;
    let (w, h) = (1920, 1080);
    let mut inj = mouse_input::new_sessionless_injector(w, h)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("Created the virtual keyboard and pointer.");
    println!("Focus a text editor within 5 seconds — it will type `mouse-rs ok`.");
    std::thread::sleep(Duration::from_secs(5));

    for c in "mouse-rs ok".chars() {
        let key = mouse_input::Key::Char(c);
        inj.key(&key, true).map_err(|e| anyhow::anyhow!("{e}"))?;
        std::thread::sleep(Duration::from_millis(12));
        inj.key(&key, false).map_err(|e| anyhow::anyhow!("{e}"))?;
        std::thread::sleep(Duration::from_millis(12));
    }
    println!("Typed. If nothing appeared, the devices were created but nothing is");
    println!("reading them — check that no other instance is already running.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logged_in_graphical_session_is_recognised() {
        // The exact shape logind writes, taken from a live /run/systemd/sessions file.
        let s = "ACTIVE=1\nSTATE=active\nREMOTE=0\nUSER=kalip\nTYPE=x11\nCLASS=user\n";
        assert!(is_graphical_user_session(s));
    }

    #[test]
    fn the_greeters_own_session_does_not_count_as_a_login() {
        // This is the one that matters: the login screen itself runs as a logind session.
        // Matching it would make this mode stop the instant it started, which looks
        // exactly like the feature not working at all.
        let s = "ACTIVE=1\nSTATE=active\nREMOTE=0\nUSER=sddm\nTYPE=x11\nCLASS=greeter\n";
        assert!(!is_graphical_user_session(s));
    }

    #[test]
    fn the_user_manager_session_does_not_count() {
        // Observed on a live system: logind keeps a second `CLASS=manager` session per
        // user alongside the real one. It is active and it is theirs, but it is not a
        // seat anyone is typing at.
        let s = "ACTIVE=1\nSTATE=active\nUSER=kalip\nTYPE=unspecified\nCLASS=manager\n";
        assert!(!is_graphical_user_session(s));
    }

    #[test]
    fn a_text_console_login_does_not_count() {
        // Someone on tty3 has not taken over the graphical seat, so the login screen is
        // still there to serve.
        let s = "ACTIVE=1\nSTATE=active\nUSER=kalip\nTYPE=tty\nCLASS=user\n";
        assert!(!is_graphical_user_session(s));
    }

    #[test]
    fn an_inactive_session_does_not_count() {
        // A switched-away session is still a session; the seat in front may well be
        // showing the login screen.
        let s = "ACTIVE=0\nSTATE=online\nUSER=kalip\nTYPE=x11\nCLASS=user\n";
        assert!(!is_graphical_user_session(s));
    }

    #[test]
    fn wayland_counts_too() {
        let s = "ACTIVE=1\nUSER=kalip\nTYPE=wayland\nCLASS=user\n";
        assert!(is_graphical_user_session(s));
    }

    #[test]
    fn an_empty_or_partial_record_does_not_count() {
        assert!(!is_graphical_user_session(""));
        assert!(!is_graphical_user_session("CLASS=user\n"), "no TYPE, no ACTIVE");
        assert!(!is_graphical_user_session("TYPE=x11\nACTIVE=1\n"), "no CLASS");
    }

    #[test]
    fn the_generated_unit_is_valid_systemd() {
        // A unit with a typo'd directive is accepted by the file system and rejected by
        // systemd at boot, where nobody is watching. `systemd-analyze verify` is the
        // only thing that actually reads it the way systemd will.
        let text = unit_text(Path::new("/usr/local/bin/mouse-rs"));
        let dir = std::env::temp_dir().join(format!("mouse-rs-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mouse-rs-greeter.service");
        std::fs::write(&path, &text).unwrap();

        let out = std::process::Command::new("systemd-analyze")
            .arg("verify")
            .arg(&path)
            .output();
        let _ = std::fs::remove_dir_all(&dir);

        let Ok(out) = out else {
            return; // no systemd-analyze here; the directive checks below still apply
        };
        let err = String::from_utf8_lossy(&out.stderr);
        // It cannot find the binary (we never installed one for a test) or the unit's
        // dependencies in a non-booted context; everything else is a real complaint.
        let real: Vec<&str> = err
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| !l.contains("Command") && !l.contains("not found"))
            .filter(|l| !l.contains("Unit") || !l.contains("not found"))
            .collect();
        assert!(real.is_empty(), "systemd-analyze objected:\n{}", real.join("\n"));
    }

    #[test]
    fn the_unit_keeps_the_directives_the_mode_depends_on() {
        let text = unit_text(Path::new("/usr/local/bin/mouse-rs"));
        // PrivateDevices would hide /dev/uinput, which is the one device this exists to
        // open — so the allow has to be explicit and the hide must never appear.
        assert!(text.contains("DeviceAllow=/dev/uinput rw"));
        // Must be absent as a *directive*; the unit mentions it in a comment explaining
        // precisely why it is not set, so a plain substring search sees it either way.
        let directives: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .collect();
        assert!(
            !directives.iter().any(|l| l.starts_with("PrivateDevices")),
            "PrivateDevices would hide /dev/uinput"
        );
        // Without this the service would not come back when the user logs out, so the
        // login screen would only work on the first boot.
        assert!(text.contains("Restart=always"));
        // The log has to land somewhere shippable, which means a writable HOME.
        assert!(text.contains("StateDirectory=mouse-rs"));
        assert!(text.contains("Environment=HOME=/var/lib/mouse-rs"));
        assert!(text.contains("ExecStart=/usr/local/bin/mouse-rs greeter"));
    }

    #[test]
    fn a_missing_system_config_explains_the_remedy() {
        // The failure people will actually hit is "I enabled the service and nothing
        // happened", so the error has to name the command that fixes it.
        let e = load_config(Path::new("/nonexistent/mouse-rs/config.json")).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("greeter-install"), "got: {msg}");
    }
}
