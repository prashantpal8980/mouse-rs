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
//! On Windows there is no equivalent that works without registering an application user
//! model ID at install time, which this does not have. Rather than ship a call that
//! silently fails, the Windows build says nothing here and the tray tooltip carries the
//! message on its own.

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

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn send(_summary: &str, body: &str) {
    // Windows: the tray tooltip is the notification. Logged so the event is still
    // traceable when someone asks why they were not told.
    tracing::info!("{body}");
}
