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
const UNIT_NAME: &str = "mouse-rs-greeter.service";
const UNIT_PATH: &str = "/etc/systemd/system/mouse-rs-greeter.service";

/// Where the service's copy of the binary lives.
///
/// The running executable is usually somewhere under `target/`, and a system service
/// must not depend on a build tree: the next `cargo clean` would leave a unit pointing
/// at nothing, and the failure would show up as a login screen that quietly stopped
/// working. So the binary is copied somewhere stable and the unit names that.
const INSTALLED_EXE: &str = "/usr/local/bin/mouse-rs";

/// Makes the kernel load `uinput` at boot, so the service never races the module.
const MODULES_LOAD_PATH: &str = "/etc/modules-load.d/mouse-rs-uinput.conf";

/// Are we root? Used to decide what a check can honestly claim to know.
fn is_root() -> bool {
    // SAFETY: geteuid takes no arguments and cannot fail.
    // Parenthesised: an `unsafe` block in statement position is parsed as a statement,
    // not an operand.
    (unsafe { libc::geteuid() }) == 0
}

fn require_root(what: &str) -> anyhow::Result<()> {
    if !is_root() {
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
         # uinput is a module on a stock kernel (CONFIG_INPUT_UINPUT=m) and is usually\n\
         # not loaded. Opening the static /dev/uinput node normally autoloads it, but\n\
         # relying on that from inside a sandboxed unit is how you get an ENODEV at boot\n\
         # and no keyboard at the login screen. Ask for it explicitly, outside our\n\
         # namespace, before we start.\n\
         Wants=modprobe@uinput.service\n\
         After=modprobe@uinput.service\n\
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

    // Belt and braces alongside the unit's `modprobe@uinput` dependency: this loads the
    // module at boot whether or not systemd's modprobe@ template exists, and costs
    // nothing on a kernel that has uinput built in.
    let _ = std::fs::create_dir_all("/etc/modules-load.d");
    if let Err(e) = std::fs::write(MODULES_LOAD_PATH, "# added by mouse-rs greeter-install\nuinput\n") {
        tracing::warn!("could not write {MODULES_LOAD_PATH}: {e}");
    }

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", UNIT_NAME])?;

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
    let _ = run_systemctl(&["disable", "--now", UNIT_NAME]);
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
    if std::fs::remove_file(MODULES_LOAD_PATH).is_ok() {
        removed.push(MODULES_LOAD_PATH.to_string());
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

// ------------------------------------------------------- status, for the panel

/// What the panel needs to know about the login-screen service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// The unit file and the root-owned config are both in place.
    pub installed: bool,
    /// systemd will start it at boot.
    pub enabled: bool,
    /// It is running right now. Normally **false** while you are logged in — the service
    /// stands down during a session on purpose — so this is not a health indicator.
    pub active: bool,
}

impl State {
    /// Is the login screen covered? Enablement is what survives a reboot, which is the
    /// question the toggle is actually asking; whether it happens to be running right
    /// now is not, because during a desktop session it is *meant* to be stopped.
    pub fn on(self) -> bool {
        self.installed && self.enabled
    }
}

/// Read the current state. Cheap enough to call on a panel redraw: one file check and
/// two `systemctl` queries that do not touch the network.
pub fn state() -> State {
    let installed = looks_installed(
        Path::new(UNIT_PATH).is_file(),
        config_path().parent().is_some_and(|d| d.is_dir()),
    );
    State {
        installed,
        enabled: installed && systemctl_says(&["is-enabled", UNIT_NAME], "enabled"),
        active: installed && systemctl_says(&["is-active", UNIT_NAME], "active"),
    }
}

/// Is the service installed, judged only from what an unprivileged process can see?
///
/// Split out and given booleans rather than paths so the rule is testable: the panel runs
/// as the user, and `/etc/mouse-rs` is `0700 root` because it holds the shared secret. So
/// the settings *directory* can be stat-ed and the config inside it never can.
///
/// Testing the file was a real bug: `Path::is_file` returns `false` on permission-denied
/// as readily as on absence, so a perfectly installed service reported itself missing,
/// which pinned `enabled` to false and left the toggle sitting at off while systemd was
/// quite happily set to start the thing at boot.
fn looks_installed(unit_present: bool, settings_dir_present: bool) -> bool {
    unit_present && settings_dir_present
}

/// `systemctl <args>` printed exactly `want`.
///
/// Matched on stdout rather than exit status because `is-enabled` exits non-zero for
/// several different "no" answers (disabled, static, masked) and we only care that it
/// is not the one "yes".
fn systemctl_says(args: &[&str], want: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == want)
        .unwrap_or(false)
}

/// Turn the login-screen service on or off from the panel, asking for the password
/// through polkit.
///
/// The panel runs as the user and this installs a root-owned service, so privilege has
/// to come from somewhere. `pkexec` is that somewhere: it prompts through the desktop's
/// own authentication agent, which means the password goes to polkit and never through
/// mouse-rs. We get an exit status and nothing else, which is the right amount.
///
/// Blocks until the user answers the prompt. The caller is the panel's update loop, so
/// the window is unresponsive while the dialog is up — acceptable for a deliberate
/// one-off action with a modal prompt of its own, and the alternative (a background task
/// posting back a result) buys nothing when there is nothing else to do meanwhile.
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    if which_pkexec().is_none() {
        anyhow::bail!(
            "pkexec is not installed, so the panel cannot ask for permission. Run this \
             in a terminal instead:\n  sudo mouse-rs greeter-{}",
            if on { "install" } else { "uninstall" }
        );
    }
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new("pkexec");
    cmd.arg(&exe);
    if on {
        // Pass our own config explicitly: under pkexec, SUDO_USER is not set and $HOME
        // is root's, so the install command could not find it to copy from.
        cmd.arg("greeter-install").arg(Config::default_path());
    } else {
        cmd.arg("greeter-uninstall");
    }
    let out = cmd.output()?;
    // Logged either way. The first version reported failures only into a transient panel
    // banner, so a silent failure left no trace anywhere and had to be reconstructed
    // from systemd's journal.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        tracing::info!(on, %stdout, "login-screen service {} via pkexec",
                       if on { "installed" } else { "removed" });
        return Ok(());
    }
    tracing::warn!(
        on,
        code = out.status.code().unwrap_or(-1),
        %stdout,
        %stderr,
        "pkexec could not change the login-screen service"
    );
    // 126/127 are pkexec's own: the user dismissed the prompt, or authorisation failed.
    // Those are answers rather than faults and should not read like a crash.
    let code = out.status.code().unwrap_or(-1);
    if code == 126 || code == 127 {
        anyhow::bail!("cancelled — nothing was changed");
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let detail = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("unknown error");
    anyhow::bail!("{detail}")
}

fn which_pkexec() -> Option<PathBuf> {
    let p = PathBuf::from("/usr/bin/pkexec");
    p.is_file().then_some(p)
}

/// Check every link in the chain and say whether the login screen will actually work.
///
/// Needs no privilege. This exists because the feature is otherwise only testable by
/// logging out — and if it does not work, the way you find out is standing at a password
/// prompt with no keyboard. Everything here is checked rather than assumed.
pub fn status() -> anyhow::Result<()> {
    let st = state();
    let mut problems: Vec<String> = Vec::new();

    println!("Login-screen sharing\n");

    // --- is it installed and will it start ---
    //
    // Cascading on purpose. When nothing is installed, every one of these fails and
    // listing four remedies for one cause buries the only instruction that matters —
    // and "installed but not enabled" is simply untrue at that point.
    let unit = Path::new(UNIT_PATH).is_file();
    check("service installed", unit, &mut problems,
          "not installed — turn on 'Share input at the login screen' in the panel's \
           Startup tab, or run: sudo mouse-rs greeter-install");
    if unit {
        check("starts at boot", st.enabled, &mut problems,
              "installed but not enabled — run: sudo systemctl enable mouse-rs-greeter");
        check("binary in place", Path::new(INSTALLED_EXE).is_file(), &mut problems,
              &format!("{INSTALLED_EXE} is missing — re-run the install"));
        // Only root can see inside the settings directory, so an unprivileged run can
        // confirm the directory and no more. Saying FAIL here would be reporting our own
        // lack of permission as the service's fault.
        let dir_ok = config_path().parent().is_some_and(|d| d.is_dir());
        if is_root() {
            check("settings in place", config_path().is_file(), &mut problems,
                  &format!("{} is missing — re-run the install", config_path().display()));
        } else {
            check("settings directory", dir_ok, &mut problems,
                  &format!("{} is missing — re-run the install",
                           config_path().parent().unwrap_or(Path::new("/etc/mouse-rs")).display()));
            println!("  -     settings file   (root-only; re-run with sudo to check)");
        }
    } else {
        for label in ["starts at boot", "binary in place", "settings in place"] {
            println!("  -     {label}");
        }
    }

    // --- can it inject at all ---
    let node = Path::new("/dev/uinput").exists();
    check("/dev/uinput present", node, &mut problems,
          "the kernel has no uinput device — load it with: sudo modprobe uinput");
    let loaded = module_loaded("uinput");
    // Not a failure on its own: the node autoloads the module on first open, and some
    // kernels build it in. Only worth saying when nothing has arranged for it.
    let arranged = loaded || Path::new(MODULES_LOAD_PATH).is_file();
    check("uinput module arranged", arranged || !unit, &mut problems,
          "uinput is a module, is not loaded, and nothing loads it at boot — re-run the \
           install, which writes /etc/modules-load.d/mouse-rs-uinput.conf");

    // --- can it reach the peer ---
    match peer_target() {
        Some((host, port)) => {
            let ok = tcp_reachable(&host, port);
            check(&format!("peer reachable ({host}:{port})"), ok, &mut problems,
                  "cannot reach the other machine — it will retry, but the login screen \
                   has no keyboard until it connects");
        }
        None => println!("  ?  peer address    could not read any config to find it"),
    }

    println!();
    if st.on() && problems.is_empty() {
        println!("The login screen will accept the other machine's keyboard.");
        if !st.active {
            println!();
            println!("Not running right now is normal while you are logged in: the service stands");
            println!("down during a session so input is not injected twice, and returns at logout.");
        }
    } else {
        println!("The login screen will NOT accept the other machine's keyboard yet.");
        for p in &problems {
            println!("  - {p}");
        }
    }
    Ok(())
}

/// Print one check line and record the remedy if it failed.
fn check(label: &str, ok: bool, problems: &mut Vec<String>, remedy: &str) {
    println!("  {}  {label}", if ok { "ok  " } else { "FAIL" });
    if !ok {
        problems.push(remedy.to_string());
    }
}

fn module_loaded(name: &str) -> bool {
    std::fs::read_to_string("/proc/modules")
        .map(|m| m.lines().any(|l| l.split_whitespace().next() == Some(name)))
        .unwrap_or(false)
}

/// The peer to test against: the service's own config if we can read it, otherwise the
/// invoking user's, which names the same machine.
fn peer_target() -> Option<(String, u16)> {
    for p in [config_path(), Config::default_path()] {
        if let Ok(c) = Config::load(&p) {
            if !c.server_host.is_empty() {
                return Some((c.server_host, c.listen_port));
            }
        }
    }
    None
}

fn tcp_reachable(host: &str, port: u16) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs.any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(600)).is_ok())
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
    fn installed_is_judged_without_reading_the_root_only_config() {
        // The bug this pins cost a working install. /etc/mouse-rs is 0700 root because it
        // holds the shared secret, so the panel -- running as the user -- gets
        // permission-denied on the config inside it. `Path::is_file` returns false for
        // that just as it does for a missing file, so a fully installed, enabled service
        // reported itself absent and the toggle stayed off however many times it was
        // clicked. The directory is stat-able; the file is not, so only the directory may
        // be part of the test.
        assert!(looks_installed(true, true), "unit + settings dir is installed");
        assert!(!looks_installed(false, true), "no unit means not installed");
        assert!(!looks_installed(true, false), "no settings dir means not installed");
    }

    #[test]
    fn the_live_state_agrees_with_systemd() {
        // Against real systemd. If the unit is on disk and systemd says enabled, the
        // panel must agree -- disagreeing is what made a working service look broken.
        let st = state();
        if Path::new(UNIT_PATH).is_file() {
            let systemd_says = systemctl_says(&["is-enabled", UNIT_NAME], "enabled");
            assert_eq!(
                st.enabled, systemd_says,
                "the panel and systemd must not disagree about enablement"
            );
        }
    }

    #[test]
    fn on_means_it_will_come_back_after_a_reboot() {
        // "On" is about surviving a reboot, not about this instant. Reading `active`
        // as the answer would show the toggle off for the entire time someone is
        // logged in, because that is exactly when the service is meant to be stopped.
        let installed_enabled = State { installed: true, enabled: true, active: false };
        assert!(installed_enabled.on(), "stopped during a session is still ON");

        let running = State { installed: true, enabled: true, active: true };
        assert!(running.on());
    }

    #[test]
    fn a_half_installed_service_is_not_on() {
        // Each of these is a state `systemctl disable` or a partial removal can leave
        // behind, and claiming any of them is ON would promise a login screen that
        // accepts the peer's keyboard when it does not — discovered by being locked out.
        assert!(!State { installed: true, enabled: false, active: false }.on());
        assert!(!State { installed: false, enabled: true, active: false }.on());
        assert!(!State { installed: false, enabled: false, active: true }.on());
    }

    #[test]
    fn the_live_system_reports_a_coherent_state() {
        // Against real systemd on this machine. Not asserting on/off — that depends on
        // whether it happens to be installed — but the invariant must hold either way.
        let st = state();
        if !st.installed {
            assert!(!st.enabled && !st.active, "nothing can be enabled or active unless installed");
            assert!(!st.on());
        }
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
