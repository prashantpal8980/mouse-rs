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
}

impl StatusHandle {
    pub fn new() -> StatusHandle {
        StatusHandle::default()
    }

    pub(crate) fn set_connected(&self, peer: &str, w: i32, h: i32) {
        if let Ok(mut p) = self.0.peer.lock() {
            *p = peer.to_owned();
        }
        self.0.peer_w.store(w, Ordering::SeqCst);
        self.0.peer_h.store(h, Ordering::SeqCst);
        self.0.engaged.store(false, Ordering::SeqCst);
        self.0.connected.store(true, Ordering::SeqCst);
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
        }
    }
}

/// Mark the peer connected, if a status handle is wired in.
pub(crate) fn set_status_connected(s: &Option<StatusHandle>, peer: &str, w: i32, h: i32) {
    if let Some(s) = s {
        s.set_connected(peer, w, h);
    }
}

/// Mark the peer disconnected, if a status handle is wired in.
pub(crate) fn set_status_disconnected(s: &Option<StatusHandle>) {
    if let Some(s) = s {
        s.set_disconnected();
    }
}

/// Update whether input is currently crossing, if a status handle is wired in.
pub(crate) fn set_status_engaged(s: &Option<StatusHandle>, engaged: bool) {
    if let Some(s) = s {
        s.set_engaged(engaged);
    }
}
