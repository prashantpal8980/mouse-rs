//! The edge-crossing engine: binds the async network link (`mouse-core::net`) to
//! OS input capture/injection (`mouse-input`).
//!
//! Threading model (so a stuck input path can never freeze networking, and Stop
//! never hangs):
//! * the **link** runs as async tasks on tokio;
//! * **capture** runs on its own native thread and pushes [`CaptureSignal`]s
//!   straight to the engine over a tokio channel (no blocking bridge);
//! * **injection** runs on its own native thread that applies events in order and
//!   releases everything held on any exit (see [`injector`]);
//! * the **engine** is one async task per role that owns the policy and selects
//!   over all of the above plus the [`CancellationToken`].
//!
//! All native-thread joins are performed by the caller (e.g. `main`) *off* the
//! async reactor after the engine task has returned, so a wedged X server can
//! tie up at most a bounded teardown, never a runtime worker.

pub mod client;
pub mod geometry;
pub mod hotkey;
pub mod injector;
pub mod server;
pub mod status;

pub use client::run_client_engine;
pub use hotkey::Hotkey;
pub use injector::{InjectCmd, Injector, InjectorTx};
pub use server::run_server_engine;
pub use status::{StatusHandle, StatusSnapshot};

use mouse_core::net::{LinkChannels, LinkEvent};
use mouse_core::protocol::Message;
use mouse_input::NamedKey;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

// Re-export the capture signal type so callers wire one channel type.
pub use mouse_input::CaptureSignal;

/// The engine's ends of the link channels (the mirror of [`LinkChannels`], which
/// the link task owns).
pub struct EngineLink {
    /// Messages the engine wants to send to the peer.
    pub to_peer: UnboundedSender<Message>,
    /// Non-heartbeat messages received from the peer.
    pub from_peer: UnboundedReceiver<Message>,
    /// Link status events.
    pub events: UnboundedReceiver<LinkEvent>,
    /// Optional live-status sink for a UI to observe (`None` for the headless CLI).
    /// The binary sets this after `new()`; the engine updates it on connect/
    /// disconnect and edge crossings.
    pub status: Option<status::StatusHandle>,
    /// Optional shared link health. The engine sets `crossed` on it as control moves
    /// between machines, which is what lets the bulk lane stand down at exactly the
    /// moment latency is felt. Independent of `status` on purpose: this must work in
    /// the headless build too, where there is no UI to observe.
    pub health: Option<std::sync::Arc<mouse_core::bulk::LinkHealth>>,
}

/// Wiring the binary hands the engine for clipboard sync. The engine routes
/// received clipboard messages to `tx` and flips `connected` with the peer
/// connection, so the clipboard thread only broadcasts local changes while a peer
/// is connected — no unbounded queue, and no stale flush to whoever connects next.
pub struct ClipboardRoute {
    /// Received `Clipboard`/`ClipboardImage` messages are forwarded here.
    pub tx: std::sync::mpsc::Sender<Message>,
    /// Set true while a peer is connected, false otherwise.
    pub connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Flip the clipboard route's connection flag, if clipboard sync is wired in.
pub(crate) fn set_clipboard_connected(route: &Option<ClipboardRoute>, connected: bool) {
    if let Some(c) = route {
        c.connected
            .store(connected, std::sync::atomic::Ordering::SeqCst);
    }
}

impl EngineLink {
    /// Create a matched pair: the [`LinkChannels`] to hand to
    /// [`run_server`](mouse_core::net::run_server) /
    /// [`run_client`](mouse_core::net::run_client), and the [`EngineLink`] the
    /// engine drives.
    pub fn new() -> (LinkChannels, EngineLink) {
        let (to_tx, to_rx) = unbounded_channel();
        let (from_tx, from_rx) = unbounded_channel();
        let (ev_tx, ev_rx) = unbounded_channel();
        (
            LinkChannels {
                to_peer: to_rx,
                from_peer: from_tx,
                events: ev_tx,
            },
            EngineLink {
                to_peer: to_tx,
                from_peer: from_rx,
                events: ev_rx,
                status: None,
                health: None,
            },
        )
    }
}

/// Parse a config hotkey token (e.g. `"scroll_lock"`) into a named key. Note this
/// uses [`NamedKey::from_name`], *not* `Key::from_str` — the config stores the
/// bare token without the `k:` wire prefix.
pub fn parse_hotkey(token: &str) -> Option<NamedKey> {
    NamedKey::from_name(token)
}

/// Whether a captured named key matches the configured hotkey, collapsing
/// left/right variants for modifier hotkeys.
pub fn matches_hotkey(named: NamedKey, hotkey: NamedKey) -> bool {
    if named == hotkey {
        return true;
    }
    match (named.modifier_token(), hotkey.modifier_token()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_parses_bare_config_token() {
        assert_eq!(parse_hotkey("scroll_lock"), Some(NamedKey::ScrollLock));
        // The bug the review flagged: the wire-prefixed form is NOT what config holds.
        assert_eq!(parse_hotkey("k:scroll_lock"), None);
    }

    #[test]
    fn hotkey_matching_collapses_modifier_sides() {
        assert!(matches_hotkey(NamedKey::ScrollLock, NamedKey::ScrollLock));
        assert!(matches_hotkey(NamedKey::CtrlR, NamedKey::CtrlL));
        assert!(!matches_hotkey(NamedKey::ScrollLock, NamedKey::CtrlL));
    }
}
