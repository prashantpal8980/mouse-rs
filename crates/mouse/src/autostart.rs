//! "Start at login" — register the control panel to launch when the user logs in.
//!
//! Deliberately **per-user, not system-wide**: no root/admin, nothing to uninstall with
//! elevated rights, and it cannot affect other accounts on the machine.
//!
//! * Linux — an XDG autostart desktop entry at `~/.config/autostart/mouse-rs.desktop`.
//!   Honoured by Xfce, GNOME, KDE and anything else following the freedesktop spec.
//! * Windows — a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//!
//! Every function is best-effort and returns a `Result` the UI surfaces as feedback
//! rather than treating as fatal: failing to toggle autostart must never stop the app.

use anyhow::{Context, Result};

/// Filename/registry value we own. Distinct from the Python build's `mouse_share`
/// so both can coexist (and so we never delete an entry we didn't create).
const ENTRY: &str = "mouse-rs";

/// Absolute path of the running binary, for the autostart command line.
fn exe_path() -> Result<std::path::PathBuf> {
    std::env::current_exe().context("locating the running executable")
}

// ----------------------------------------------------------------------- Linux

#[cfg(unix)]
fn desktop_file() -> Result<std::path::PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            std::path::PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("autostart").join(format!("{ENTRY}.desktop")))
}

#[cfg(unix)]
pub fn is_enabled() -> bool {
    desktop_file().map(|p| p.is_file()).unwrap_or(false)
}

#[cfg(unix)]
pub fn enable() -> Result<()> {
    let path = desktop_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let exe = exe_path()?;
    // No arguments: a bare launch opens the control panel. Whether it then connects
    // by itself is `autoconnect`, so the two settings stay independent.
    let body = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=mouse-rs\n\
         Comment=Share one keyboard and mouse between machines\n\
         Exec={exe}\n\
         Icon=input-mouse\n\
         Terminal=false\n\
         Categories=Utility;Network;\n\
         X-GNOME-Autostart-enabled=true\n",
        exe = exe.display()
    );
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
pub fn disable() -> Result<()> {
    let path = desktop_file()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        // Already gone is success — the requested end state holds either way.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Elevated autostart is a Windows-only concept: X11 imposes no equivalent restriction
/// on reading input, so there is nothing to escalate for.
#[cfg(unix)]
pub fn elevated_is_enabled() -> bool {
    false
}

#[cfg(unix)]
pub fn enable_elevated() -> Result<()> {
    anyhow::bail!("elevated autostart is a Windows-only setting")
}

#[cfg(unix)]
pub fn disable_elevated() -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn relaunch_elevated() -> Result<bool> {
    anyhow::bail!("running as administrator is a Windows-only concept")
}

#[cfg(unix)]
pub fn set_elevated_via_prompt(_on: bool) -> Result<bool> {
    anyhow::bail!("elevated autostart is a Windows-only setting")
}

/// Windows Firewall only: a stock Linux desktop accepts the connection, so there is no
/// rule to add.
#[cfg(unix)]
pub fn firewall_rule_exists() -> bool {
    true
}

#[cfg(unix)]
pub fn firewall_allow_via_prompt() -> Result<bool> {
    Ok(true)
}

// --------------------------------------------------------------------- Windows

#[cfg(windows)]
mod win {
    use super::{Result, ENTRY};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Open the per-user Run key with the given access. Caller must `RegCloseKey`.
    fn open_run(access: u32) -> Result<HKEY> {
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `RUN_KEY` is a valid NUL-terminated wide string and `key` is a valid
        // out-param; on any non-success return we do not use `key`.
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                0,
                access,
                &mut key,
            )
        };
        if rc != ERROR_SUCCESS {
            anyhow::bail!("opening HKCU\\{RUN_KEY} failed (code {rc})");
        }
        Ok(key)
    }

    pub fn is_enabled() -> bool {
        let Ok(key) = open_run(KEY_READ) else {
            return false;
        };
        let mut len: u32 = 0;
        // SAFETY: querying size only — null data pointer with a valid length out-param.
        let rc = unsafe {
            RegQueryValueExW(
                key,
                wide(ENTRY).as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        // SAFETY: `key` came from a successful open and is not used again.
        unsafe { RegCloseKey(key) };
        rc == ERROR_SUCCESS
    }

    pub fn enable() -> Result<()> {
        let exe = super::exe_path()?;
        // Quote the path: Program Files and friends contain spaces.
        let cmd = format!("\"{}\"", exe.display());
        let data = wide(&cmd);
        let key = open_run(KEY_WRITE)?;
        // SAFETY: `data` is a NUL-terminated wide buffer; the byte length we pass includes
        // that terminator, as REG_SZ requires.
        let rc = unsafe {
            RegSetValueExW(
                key,
                wide(ENTRY).as_ptr(),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * std::mem::size_of::<u16>()) as u32,
            )
        };
        // SAFETY: `key` came from a successful open and is not used again.
        unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS {
            anyhow::bail!("writing the Run entry failed (code {rc})");
        }
        Ok(())
    }

    /// Relaunch this program with administrator rights and ask the current instance to
    /// quit.
    ///
    /// `runas` is the shell verb behind "Run as administrator", so Windows shows its own
    /// consent prompt — which is the point. Elevation must be the user's decision, made
    /// through the OS, not something a program can grant itself.
    ///
    /// Returns `Ok(false)` when the prompt was declined: a refusal is an answer, not a
    /// failure, and must not be reported as an error.
    pub fn relaunch_elevated() -> Result<bool> {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let exe = super::exe_path()?;
        let verb = wide("runas");
        let file = wide(&exe.to_string_lossy());
        // SAFETY: both strings are NUL-terminated wide buffers that outlive the call.
        let rc = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecuteW returns a fake HINSTANCE; anything over 32 means it started.
        // SE_ERR_ACCESSDENIED (5) is what a declined UAC prompt produces.
        let code = rc as isize;
        if code > 32 {
            Ok(true)
        } else if code == 5 {
            Ok(false)
        } else {
            anyhow::bail!("could not restart as administrator (code {code})")
        }
    }

    /// Turn the elevated login task on or off from a normal-privilege window: Windows shows
    /// its own "Do you want to allow this app to make changes?" prompt, and on Yes a short
    /// elevated copy of mouse-rs (`autostart-elevated-on` / `-off`) makes the change and
    /// exits. This window keeps running as it was.
    ///
    /// Returns `Ok(false)` when the prompt was declined.
    pub fn set_elevated_via_prompt(on: bool) -> Result<bool> {
        run_elevated(if on { "autostart-elevated-on" } else { "autostart-elevated-off" })
    }

    /// The firewall rule's name, shared by the check and the helper that adds it.
    const FIREWALL_RULE: &str = "mouse-rs";

    /// Is there an inbound Windows Firewall rule for this program?
    pub fn firewall_rule_exists() -> bool {
        std::process::Command::new("netsh.exe")
            .args(["advfirewall", "firewall", "show", "rule", &format!("name={FIREWALL_RULE}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Ask, through Windows' own prompt, to let mouse-rs through the firewall.
    pub fn firewall_allow_via_prompt() -> Result<bool> {
        run_elevated("firewall-allow")
    }

    /// The elevated half: one inbound rule for this program, on every network profile.
    /// A rule for the program rather than a port, so the main port, the lane port and
    /// any port chosen later are all covered by one approval.
    pub fn firewall_allow() -> Result<()> {
        let exe = super::exe_path()?;
        let name = format!("name={FIREWALL_RULE}");
        // Replaced rather than added beside: an old rule may name an older exe path.
        let _ = std::process::Command::new("netsh.exe")
            .args(["advfirewall", "firewall", "delete", "rule", &name])
            .output();
        let out = std::process::Command::new("netsh.exe")
            .args([
                "advfirewall", "firewall", "add", "rule", &name, "dir=in", "action=allow",
                &format!("program={}", exe.display()), "enable=yes", "profile=any",
            ])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("netsh could not add the rule: {}", String::from_utf8_lossy(&out.stdout).trim());
        }
        Ok(())
    }

    /// Run this program elevated with `params`, through Windows' consent prompt, and wait
    /// for it to finish. `Ok(false)` when the prompt was declined.
    fn run_elevated(params: &str) -> Result<bool> {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        use windows_sys::Win32::UI::Shell::{
            ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let exe = super::exe_path()?;
        let verb = wide("runas");
        let file = wide(&exe.to_string_lossy());
        let params = wide(params);
        // SAFETY: a zeroed SHELLEXECUTEINFOW is valid (all pointers null); the strings are
        // NUL-terminated wide buffers that outlive the call, and the process handle is
        // closed below.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        let ok = unsafe { ShellExecuteExW(&mut info) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_CANCELLED {
                return Ok(false);
            }
            anyhow::bail!("could not ask Windows for administrator rights (code {err})");
        }
        if info.hProcess.is_null() {
            return Ok(true);
        }
        // The helper only runs schtasks; a minute is far more than it needs.
        let mut code: u32 = 1;
        unsafe {
            WaitForSingleObject(info.hProcess, 60_000);
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
        }
        if code != 0 {
            anyhow::bail!("the administrator step did not finish (exit code {code})");
        }
        Ok(true)
    }

    /// Name of the scheduled task used for the elevated variant.
    const TASK: &str = "mouse-rs (elevated)";

    /// Is there a login task that starts us with administrator rights?
    ///
    /// A plain `Run` registry entry always starts at normal privilege, and Windows
    /// gives no way to raise that without a UAC prompt at every login. A scheduled task
    /// marked "run with highest privileges" is the one mechanism that starts elevated at
    /// login *silently*, which is why autostart has two shapes rather than one.
    pub fn elevated_is_enabled() -> bool {
        std::process::Command::new("schtasks.exe")
            .args(["/Query", "/TN", TASK])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Create that task. **Requires the current process to be elevated** — Windows will
    /// not let a normal-privilege process register a task that runs with highest
    /// privileges, and refusing clearly is better than appearing to succeed.
    pub fn enable_elevated() -> Result<()> {
        let exe = super::exe_path()?;
        let out = std::process::Command::new("schtasks.exe")
            .args([
                "/Create",
                "/TN",
                TASK,
                "/TR",
                &format!("\"{}\"", exe.display()),
                "/SC",
                "ONLOGON",
                "/RL",
                "HIGHEST",
                "/F",
            ])
            .output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "could not create the elevated login task — mouse-rs must itself be \
                 running as administrator to set this up. {}",
                err.trim()
            );
        }
        Ok(())
    }

    pub fn disable_elevated() -> Result<()> {
        let out = std::process::Command::new("schtasks.exe")
            .args(["/Delete", "/TN", TASK, "/F"])
            .output()?;
        // A task that is already absent is the requested end state.
        if !out.status.success() && elevated_is_enabled() {
            anyhow::bail!(
                "could not remove the elevated login task: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    pub fn disable() -> Result<()> {
        let key = open_run(KEY_WRITE)?;
        // SAFETY: valid key and NUL-terminated value name.
        let rc = unsafe { RegDeleteValueW(key, wide(ENTRY).as_ptr()) };
        // SAFETY: `key` came from a successful open and is not used again.
        unsafe { RegCloseKey(key) };
        // Treat "no such value" as success: the requested end state holds.
        const ERROR_FILE_NOT_FOUND: u32 = 2;
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            anyhow::bail!("removing the Run entry failed (code {rc})");
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{
    disable, disable_elevated, elevated_is_enabled, enable, enable_elevated, is_enabled,
    firewall_allow, firewall_allow_via_prompt, firewall_rule_exists, relaunch_elevated,
    set_elevated_via_prompt,
};

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    /// Drives enable/is_enabled/disable against a scratch XDG_CONFIG_HOME so the test
    /// never touches the developer's real autostart directory.
    #[test]
    fn enable_then_disable_round_trips() {
        let dir = std::env::temp_dir().join(format!("mouse-rs-autostart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: single-threaded test; no other thread reads the environment here.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };

        assert!(!is_enabled(), "starts disabled");

        enable().unwrap();
        assert!(is_enabled(), "enable() must make is_enabled() true");
        let body = std::fs::read_to_string(dir.join("autostart/mouse-rs.desktop")).unwrap();
        assert!(body.starts_with("[Desktop Entry]"));
        assert!(body.contains("Type=Application"));
        // Must point at a real absolute path, else the entry silently does nothing.
        let exec = body
            .lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .expect("Exec= line");
        assert!(
            std::path::Path::new(exec).is_absolute(),
            "Exec must be absolute, got {exec}"
        );

        // Enabling twice is not an error and leaves one entry.
        enable().unwrap();
        assert!(is_enabled());

        disable().unwrap();
        assert!(!is_enabled(), "disable() must remove the entry");
        // Disabling when already absent is success, not an error.
        disable().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
