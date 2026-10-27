//! Live engine status for a UI to observe (Phase 6).
//!
//! The engine updates this at the same points it logs connect/disconnect and edge
//! crossings; a UI polls [`StatusHandle::snapshot`] each frame. It's cheap to clone
//! (an `Arc`) and lock-light (atomics plus a tiny mutex for the peer name), so the
//! hot input path is unaffected. The headless CLI simply passes `None` and never
//! touches it.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Inner {
    connected: AtomicBool,
    /// Input is currently crossing: on the server the cursor is on the peer; on the
    /// client this machine is being driven by the peer.
    engaged: AtomicBool,
    peer_w: AtomicI32,
    peer_h: AtomicI32,
    peer: Mutex<String>,
    /// The peer is the login-screen instance: its machine is at a password prompt.
    peer_at_login_screen: AtomicBool,
    /// Why the last connection attempt was refused, in words for the user.
    refused: Mutex<Option<String>>,
}

/// A shared, cheaply-cloned handle to the engine's live status.
#[derive(Clone, Default)]
pub struct StatusHandle(Arc<Inner>);

/// A point-in-time copy of the status for a UI to render.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub connected: bool,
    pub engaged: bool,
    pub peer: String,
    pub peer_w: i32,
    pub peer_h: i32,
    /// The peer is at its login screen. Input reaches it once the pointer crosses over.
    pub peer_at_login_screen: bool,
    /// Why the last connection attempt was refused (wrong secret, older version…).
    /// Cleared by the next successful connection.
    pub refused: Option<String>,
}

impl StatusHandle {
    pub fn new() -> StatusHandle {
        StatusHandle::default()
    }

    pub(crate) fn set_connected(&self, peer: &str, w: i32, h: i32, login_screen: bool) {
        if let Ok(mut p) = self.0.peer.lock() {
            *p = peer.to_owned();
        }
        self.0.peer_at_login_screen.store(login_screen, Ordering::SeqCst);
        self.0.peer_w.store(w, Ordering::SeqCst);
        self.0.peer_h.store(h, Ordering::SeqCst);
        self.0.engaged.store(false, Ordering::SeqCst);
        self.0.connected.store(true, Ordering::SeqCst);
        self.set_refused(None);
    }

    pub(crate) fn set_refused(&self, why: Option<String>) {
        if let Ok(mut r) = self.0.refused.lock() {
            *r = why;
        }
    }

    pub(crate) fn set_disconnected(&self) {
        self.0.connected.store(false, Ordering::SeqCst);
        self.0.engaged.store(false, Ordering::SeqCst);
    }

    pub(crate) fn set_engaged(&self, engaged: bool) {
        self.0.engaged.store(engaged, Ordering::SeqCst);
    }

    /// Read the current status. Fields are individually atomic; this is a best-effort
    /// snapshot, which is all a once-per-frame UI needs.
    pub fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            connected: self.0.connected.load(Ordering::SeqCst),
            engaged: self.0.engaged.load(Ordering::SeqCst),
            peer: self.0.peer.lock().map(|p| p.clone()).unwrap_or_default(),
            peer_w: self.0.peer_w.load(Ordering::SeqCst),
            peer_h: self.0.peer_h.load(Ordering::SeqCst),
            peer_at_login_screen: self.0.peer_at_login_screen.load(Ordering::SeqCst),
            refused: self.0.refused.lock().map(|r| r.clone()).unwrap_or_default(),
        }
    }
}

/// Mark the peer connected, if a status handle is wired in.
pub(crate) fn set_status_connected(
    s: &Option<StatusHandle>,
    peer: &str,
    w: i32,
    h: i32,
    login_screen: bool,
) {
    if let Some(s) = s {
        s.set_connected(peer, w, h, login_screen);
    }
}

/// Record a refused connection attempt. Only refusals the user can act on are shown;
/// a timeout or a dropped handshake is retried and would only be noise.
pub(crate) fn set_status_refused(s: &Option<StatusHandle>, reason: &str) {
    tracing::warn!("connection refused: {reason}");
    if let (Some(s), Some(why)) = (s, refusal_text(reason)) {
        s.set_refused(Some(why));
    }
}

/// The words shown for an authentication refusal (B34), or None for one not worth showing.
pub(crate) fn refusal_text(reason: &str) -> Option<String> {
    let text = match reason {
        "bad_secret" => "the other machine has a different shared secret".to_owned(),
        "old_version" => "the other machine runs an older mouse-rs; update it".to_owned(),
        "server_not_verified" => {
            "the server could not prove it has the shared secret; check it on both machines"
                .to_owned()
        }
        r if r.starts_with("bad secret from ") => {
            format!("{} has a different shared secret", &r["bad secret from ".len()..])
        }
        // Connection PIN (R35). The client sees the bare reason; the server sees it
        // followed by " from <name>".
        r if r.starts_with(mouse_core::pin::REASON_REQUIRED) => match r.split_once(" from ") {
            Some((_, who)) => format!("{who} is waiting for the connection PIN to be typed there"),
            None => "type the connection PIN shown on the server".to_owned(),
        },
        r if r.starts_with(mouse_core::pin::REASON_WRONG) => {
            let left = r.split(':').nth(1).and_then(|n| n.split(' ').next()).unwrap_or("?");
            match r.split_once(" from ") {
                Some((_, who)) => format!("{who} typed a wrong connection PIN ({left} tries left)"),
                None => format!("wrong connection PIN; {left} tries left"),
            }
        }
        r if r.starts_with(mouse_core::pin::REASON_LOCKED) => {
            let secs: u64 = r.split(':').nth(1).and_then(|n| n.split(' ').next()?.parse().ok()).unwrap_or(0);
            let mins = secs.div_ceil(60).max(1);
            match r.split_once(" from ") {
                Some((_, who)) => format!("too many wrong PINs from {who}; PIN entry locked for {mins} min"),
                None => format!("too many wrong PINs; try again in {mins} min"),
            }
        }
        r if r.contains("runs an older mouse-rs") => {
            r.split(" (protocol").next().unwrap_or(r).to_owned() + "; update it"
        }
        _ => return None,
    };
    Some(text)
}

/// Mark the peer disconnected, if a status handle is wired in.
pub(crate) fn set_status_disconnected(s: &Option<StatusHandle>) {
    if let Some(s) = s {
        s.set_disconnected();
    }
}

/// Update whether input is currently crossing.
///
/// Two sinks, deliberately independent: the UI's status handle (absent headless) and
/// the shared link health the bulk lane paces against (present in both builds, because
/// a headless session still must not stutter under a transfer).
pub(crate) fn set_status_engaged(s: &Option<StatusHandle>, engaged: bool) {
    if let Some(s) = s {
        s.set_engaged(engaged);
    }
}

/// Tell the bulk lane whether the user is currently driving the other machine.
pub(crate) fn set_crossed(
    h: &Option<std::sync::Arc<mouse_core::bulk::LinkHealth>>,
    crossed: bool,
) {
    if let Some(h) = h {
        h.set_crossed(crossed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_the_user_can_act_on_are_worded_and_cleared_by_a_connection() {
        assert_eq!(
            refusal_text("bad secret from MSI").as_deref(),
            Some("MSI has a different shared secret")
        );
        assert_eq!(
            refusal_text("MSI runs an older mouse-rs (protocol 1); update both machines").as_deref(),
            Some("MSI runs an older mouse-rs; update it")
        );
        assert!(refusal_text("server_not_verified").is_some());
        assert_eq!(
            refusal_text("wrong_pin:3 from kali-desktop").as_deref(),
            Some("kali-desktop typed a wrong connection PIN (3 tries left)")
        );
        assert_eq!(refusal_text("wrong_pin:2").as_deref(), Some("wrong connection PIN; 2 tries left"));
        assert_eq!(refusal_text("pin_locked:300").as_deref(), Some("too many wrong PINs; try again in 5 min"));
        assert!(refusal_text("pin_required from kali").unwrap().contains("kali is waiting"));
        assert_eq!(refusal_text("handshake timed out"), None);

        let s = StatusHandle::new();
        s.set_refused(refusal_text("bad_secret"));
        assert!(s.snapshot().refused.is_some());
        s.set_connected("MSI", 1920, 1080, false);
        assert_eq!(s.snapshot().refused, None);
    }
}
