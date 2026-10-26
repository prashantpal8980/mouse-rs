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
    /// Every clipboard-family message is forwarded here — see `is_clipboard_message`.
    pub tx: std::sync::mpsc::Sender<Message>,
    /// Set true while a peer is connected, false otherwise.
    pub connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Does this message belong to the clipboard thread rather than the input path?
///
/// **Written as an exhaustive match on purpose.** The previous version was two
/// variants listed inline at each call site, and when file transfer added
/// `ClipboardFiles`, `ClipboardImageRef`, `BlobRequest`, `BlobUnavailable` and
/// `BlobCancel`, none of them were added to either list. They fell through to the
/// catch-all arm and were dropped without a log line, so the entire file-transfer
/// feature was unreachable in a real session — every handler behind it correct, tested,
/// and never once called.
///
/// Listing every variant means the compiler stops the next person making that mistake:
/// adding a message to the protocol will not build until it is classified here.
pub(crate) fn is_clipboard_message(m: &Message) -> bool {
    match m {
        Message::Clipboard { .. }
        | Message::ClipboardImage { .. }
        | Message::ClipboardImageRef { .. }
        | Message::ClipboardFiles { .. }
        | Message::BlobRequest { .. }
        | Message::BlobUnavailable { .. }
        | Message::BlobCancel { .. } => true,

        // Input, geometry and liveness: everything here is the engine's own business.
        Message::Hello { .. }
        | Message::AuthChallenge { .. }
        | Message::AuthResponse { .. }
        | Message::AuthOk { .. }
        | Message::Enter { .. }
        | Message::Leave
        | Message::LeaveRequest
        | Message::MouseMove { .. }
        | Message::MouseButton { .. }
        | Message::MouseScroll { .. }
        | Message::Key { .. }
        | Message::Ping
        | Message::Pong
        // Handled inside the link itself; never reach the engine.
        | Message::MotionOffer { .. }
        | Message::MotionSync { .. }
        | Message::MotionReady { .. } => false,
    }
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

#[cfg(test)]
mod route_tests {
    use super::*;

    /// The bug this exists to prevent: file transfer was complete, unit-tested and
    /// entirely unreachable, because the engine's routing listed clipboard messages by
    /// name and nobody added the new ones. Every message the clipboard thread handles
    /// must reach it.
    #[test]
    fn every_message_the_clipboard_thread_handles_is_routed_to_it() {
        let must_route = [
            Message::Clipboard { text: "x".into() },
            Message::ClipboardImage { data: String::new(), w: 1, h: 1 },
            Message::ClipboardImageRef { hash: "a".repeat(64), w: 1, h: 1, size: 1 },
            Message::ClipboardFiles { files: vec![], origin: "p".into(), dirs: vec![] },
            Message::BlobRequest { hash: "a".repeat(64), offset: 0 },
            Message::BlobUnavailable { hash: "a".repeat(64), reason: "gone".into() },
            Message::BlobCancel { hash: "a".repeat(64) },
        ];
        for m in must_route {
            assert!(
                is_clipboard_message(&m),
                "{m:?} is handled by the clipboard thread but the engine would drop it"
            );
        }
    }

    /// The other half: input must never be diverted off the input path, which would
    /// silently break crossing rather than silently break the clipboard.
    #[test]
    fn input_and_liveness_stay_on_the_input_path() {
        let must_not_route = [
            Message::MouseMove { dx: 1, dy: 1 },
            Message::MouseButton { button: mouse_core::protocol::Button::Left, pressed: true },
            Message::MouseScroll { dx: 0, dy: 1 },
            Message::Key { key: "a".into(), pressed: true },
            Message::Leave,
            Message::LeaveRequest,
            Message::Ping,
            Message::Pong,
        ];
        for m in must_not_route {
            assert!(!is_clipboard_message(&m), "{m:?} must stay on the input path");
        }
    }
}
