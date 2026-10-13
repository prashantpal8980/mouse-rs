//! Desktop notifications for things waiting on an answer.
//!
//! ## Why this is not a dialog
//!
//! The obvious implementation — pop a window asking "this file is big, send it?" — is
//! the one thing this must never do. Windows stops delivering `WH_KEYBOARD_LL`
//! callbacks while our own process owns the foreground window, which is exactly the
//! keyboard leak fixed on 31 July. A window that jumps to the front every time someone
//! copies a large file would bring it back on a schedule.
//!
//! So the user is told in three places that all leave the foreground alone: the panel
//! banner, the tray tooltip, and this.
//!
//! ## Why `notify-send` rather than a crate
//!
//! On Linux the desktop notification service is reached over D-Bus, and `notify-send`
//! (from libnotify, present on essentially every desktop install) already speaks it.
//! Shelling out costs one short-lived process per notification — far less than pulling
//! a D-Bus client into the dependency tree for one line of text.
//!
//! On Windows a toast normally needs an application user model ID registered by an
//! installer. There is a way round it: PowerShell's own AUMID is present on every
//! Windows 10 and 11 machine, so raising the toast through PowerShell works with
//! nothing installed. It costs a detached process launch, which is fine for something
//! that happens once per waiting file.

/// Tell the user that transfers are waiting for an answer.
///
/// Best-effort by design: a machine with no notification daemon is not a broken one,
/// and the panel banner and tray tooltip say the same thing regardless.
pub fn file_waiting(count: usize) {
    let body = match count {
        1 => "A file is waiting — open mouse-rs to bring it over.".to_owned(),
        n => format!("{n} files are waiting — open mouse-rs to bring them over."),
    };
    send("mouse-rs", &body);
}

/// Tell the user the other machine is waiting at its login screen, and how to reach it.
///
/// Once per arrival. Without this, the only sign was a peer name in a closed panel, and
/// the password got typed on a second keyboard instead.
pub fn peer_at_login_screen(peer: &str) {
    send(
        "mouse-rs",
        &format!("{peer} is at its login screen. Move the pointer across to it to type the password."),
    );
}

#[cfg(all(unix, not(target_os = "macos")))]
fn send(summary: &str, body: &str) {
    use std::process::{Command, Stdio};
    // Spawned and deliberately not waited on: this runs on the UI thread, and a
    // notification daemon that hangs must not take the panel with it. The child is
    // short-lived and reaped by init if we exit first.
    let spawned = Command::new("notify-send")
        .arg("--app-name=mouse-rs")
        .arg("--expire-time=8000")
        .arg(summary)
        .arg(body)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        // Debug, not warn: no notification daemon is a normal state on a bare server,
        // and this would otherwise log on every single waiting file.
        tracing::debug!("no desktop notification sent: {e}");
    }
}

#[cfg(windows)]
fn send(summary: &str, body: &str) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    // Windows toasts normally require an application user model ID registered by an
    // installer, which this has no way to create. The way round it is to borrow one
    // that is always present: PowerShell's own AUMID is registered on every Windows 10
    // and 11 machine, so a toast raised through it appears without installing anything.
    // The cost is spawning PowerShell — about a second, on a detached process, off the
    // path of anything the user is waiting for.
    const PS_AUMID: &str =
        r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // Single-quoted PowerShell strings take '' as a literal quote; nothing else needs
    // escaping, and the text is ours rather than the peer's in any case.
    let esc = |s: &str| s.replace('\'', "''");
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] > $null; \
         $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $t = $x.GetElementsByTagName('text'); \
         $t.Item(0).AppendChild($x.CreateTextNode('{}')) > $null; \
         $t.Item(1).AppendChild($x.CreateTextNode('{}')) > $null; \
         $n = [Windows.UI.Notifications.ToastNotification]::new($x); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{}').Show($n)",
        esc(summary),
        esc(body),
        PS_AUMID,
    );

    let spawned = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        // Not waited on: this runs from the UI thread, and PowerShell taking a second
        // must not be a second the panel is frozen for.
        Ok(_) => tracing::info!("notified: {body}"),
        Err(e) => tracing::debug!("no toast sent ({e}); the tray tooltip still says it"),
    }
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
fn send(_summary: &str, body: &str) {
    // Nothing platform-specific to use here; the tray tooltip carries it. Logged so the
    // event is still traceable when someone asks why they were not told.
    tracing::info!("{body}");
}
