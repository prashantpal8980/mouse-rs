//! The server-side engine: owns the physical mouse/keyboard, decides when the
//! cursor crosses to the peer, and forwards input while it's over there.
//!
//! Lifecycle note (a bug the design review caught): the network link
//! [`run_server`](mouse_core::net::run_server) is a *persistent* loop that
//! re-accepts the next client after any disconnect. So this engine must **not**
//! return on a peer `Disconnected` — it resets to `Local`, drops suppression, and
//! keeps the capture thread + channels alive for the next client. It returns only
//! when the [`CancellationToken`] fires (real Stop) or the link's channels close.

use mouse_core::config::Config;
use mouse_core::net::LinkEvent;
use mouse_core::protocol::Message;
use mouse_input::{CaptureSignal, InputEvent, Key, NamedKey, Suppressor};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::geometry::{entry_point, in_edge_band, is_opposing};
use crate::{matches_hotkey, parse_hotkey, set_clipboard_connected, ClipboardRoute, EngineLink};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// The server owns input; watching for the cursor to reach the exit edge.
    Local,
    /// The peer owns input; captured input is forwarded.
    Remote,
}

/// Run the server engine until cancelled (or the link shuts down).
#[allow(clippy::too_many_arguments)]
pub async fn run_server_engine(
    cfg: Config,
    local_dims: (i32, i32),
    link: EngineLink,
    suppress: Suppressor,
    mut capture_rx: UnboundedReceiver<CaptureSignal>,
    cancel: CancellationToken,
    clipboard: Option<ClipboardRoute>,
) {
    let EngineLink {
        to_peer,
        mut from_peer,
        mut events,
    } = link;

    let exit_edge = cfg.exit_edge;
    let entry_edge = cfg.entry_edge;
    let threshold = cfg.edge_threshold_px;
    let (lw, lh) = local_dims;

    let hotkey = parse_hotkey(&cfg.lock_hotkey).unwrap_or_else(|| {
        tracing::warn!(
            "lock_hotkey '{}' not recognized; defaulting to scroll_lock",
            cfg.lock_hotkey
        );
        NamedKey::ScrollLock
    });
    if !is_opposing(exit_edge, entry_edge) {
        tracing::warn!(
            "exit_edge {exit_edge:?} and entry_edge {entry_edge:?} are not an opposing pair; \
             entry placement will be approximate"
        );
    }

    let mut state = State::Local;
    let mut connected = false;
    let mut peer_dims: Option<(i32, i32)> = None;
    // Edge-triggered crossing: the cursor must be seen *outside* the exit band
    // before a cross can fire, so parking at the edge (or connecting while there)
    // doesn't teleport, and returning doesn't immediately re-cross.
    let mut cross_armed = false;

    suppress.set(false);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                suppress.set(false);
                return;
            }

            sig = capture_rx.recv() => {
                let Some(sig) = sig else {
                    suppress.set(false);
                    return;
                };
                match (sig, state) {
                    (CaptureSignal::Cursor { x, y }, State::Local) => {
                        if !in_edge_band(exit_edge, x, y, lw, lh, threshold) {
                            cross_armed = true;
                        } else if cross_armed && connected {
                            if let Some(peer) = peer_dims {
                                let (ex, ey) =
                                    entry_point(exit_edge, entry_edge, (x, y), local_dims, peer);
                                let _ = to_peer.send(Message::Enter { x: ex, y: ey });
                                suppress.set(true);
                                state = State::Remote;
                                cross_armed = false;
                                tracing::info!("crossed to remote, entry ({ex},{ey})");
                            }
                        }
                    }
                    (CaptureSignal::Input(ev), State::Remote) => {
                        if is_hotkey_press(&ev, hotkey) {
                            return_to_local(&to_peer, &suppress, &mut state, &mut cross_armed);
                        } else {
                            let _ = to_peer.send(ev.to_message());
                        }
                    }
                    // Stale signals across the async suppression-toggle window:
                    // ignore Input while Local and Cursor while Remote. Never
                    // forward input we're not actively suppressing.
                    (CaptureSignal::Input(_), State::Local) => {}
                    (CaptureSignal::Cursor { .. }, State::Remote) => {}
                    // The capture thread couldn't take the grab after we optimistically
                    // crossed: undo the crossing. Sends Leave to the peer, drops the
                    // (never-taken) suppression, and returns to Local. Harmless if we're
                    // already Local (a redundant Leave the client ignores when inactive).
                    (CaptureSignal::SuppressRejected, _) => {
                        return_to_local(&to_peer, &suppress, &mut state, &mut cross_armed);
                    }
                }
            }

            ev = events.recv() => {
                match ev {
                    None => {
                        suppress.set(false);
                        return;
                    }
                    Some(LinkEvent::Connected { width, height, peer }) => {
                        connected = true;
                        peer_dims = Some((width, height));
                        cross_armed = false;
                        state = State::Local;
                        suppress.set(false);
                        set_clipboard_connected(&clipboard, true);
                        tracing::info!("peer '{peer}' connected: {width}x{height}");
                    }
                    Some(LinkEvent::Disconnected(reason)) => {
                        // Keep running: the link re-accepts the next client.
                        connected = false;
                        cross_armed = false;
                        state = State::Local;
                        suppress.set(false);
                        set_clipboard_connected(&clipboard, false);
                        tracing::info!("peer disconnected: {reason:?}");
                    }
                    Some(_) => {}
                }
            }

            msg = from_peer.recv() => {
                match msg {
                    None => {
                        suppress.set(false);
                        return;
                    }
                    // The peer pushed back through its entry edge.
                    Some(Message::LeaveRequest) => {
                        if state == State::Remote {
                            return_to_local(&to_peer, &suppress, &mut state, &mut cross_armed);
                        }
                    }
                    // Clipboard sync lives off the input path: hand it to the
                    // clipboard thread (if enabled) and otherwise ignore it.
                    Some(m @ (Message::Clipboard { .. } | Message::ClipboardImage { .. })) => {
                        if let Some(c) = &clipboard {
                            let _ = c.tx.send(m);
                        }
                    }
                    Some(_) => {}
                }
            }
        }
    }
}

/// Hand control back to the local machine: release suppression and tell the peer.
fn return_to_local(
    to_peer: &UnboundedSender<Message>,
    suppress: &Suppressor,
    state: &mut State,
    cross_armed: &mut bool,
) {
    suppress.set(false);
    let _ = to_peer.send(Message::Leave);
    *state = State::Local;
    *cross_armed = false;
    tracing::info!("returned to local");
}

fn is_hotkey_press(ev: &InputEvent, hotkey: NamedKey) -> bool {
    matches!(
        ev,
        InputEvent::Key { key: Key::Named(n), pressed: true } if matches_hotkey(*n, hotkey)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineLink;
    use mouse_core::config::{Edge, Role};
    use mouse_core::protocol::Message;
    use std::time::Duration;
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::time::timeout;

    fn server_cfg() -> Config {
        Config {
            role: Role::Server,
            exit_edge: Edge::Left,
            entry_edge: Edge::Right,
            edge_threshold_px: 2,
            lock_hotkey: "scroll_lock".into(),
            ..Default::default()
        }
    }

    async fn recv_msg(rx: &mut UnboundedReceiver<Message>) -> Option<Message> {
        timeout(Duration::from_millis(500), rx.recv()).await.ok().flatten()
    }

    #[tokio::test]
    async fn arms_then_crosses_forwards_and_returns_on_hotkey() {
        let (mut chans, engine_link) = EngineLink::new();
        let supp = Suppressor::new();
        let (cap_tx, cap_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_server_engine(
            server_cfg(),
            (1920, 1080),
            engine_link,
            supp.clone(),
            cap_rx,
            cancel.clone(),
            None,
        ));

        // Peer connects (1920x1080). Let the engine process it before any cursor
        // (the two are on different channels; select! order isn't guaranteed).
        chans
            .events
            .send(LinkEvent::Connected { peer: "c".into(), width: 1920, height: 1080 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;

        // Park at the edge first: must NOT cross (not armed yet).
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();
        // Move inward to arm.
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 540 }).unwrap();
        // Now reach the exit (Left) edge -> cross.
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();

        let m = recv_msg(&mut chans.to_peer).await;
        assert!(matches!(m, Some(Message::Enter { x: 1919, .. })), "got {m:?}");
        assert!(supp.get(), "suppression must be on after crossing");

        // While remote, input is forwarded.
        cap_tx
            .send(CaptureSignal::Input(InputEvent::MouseMove { dx: 5, dy: -3 }))
            .unwrap();
        assert_eq!(recv_msg(&mut chans.to_peer).await, Some(Message::MouseMove { dx: 5, dy: -3 }));

        // scroll_lock returns control.
        cap_tx
            .send(CaptureSignal::Input(InputEvent::Key {
                key: Key::Named(NamedKey::ScrollLock),
                pressed: true,
            }))
            .unwrap();
        assert_eq!(recv_msg(&mut chans.to_peer).await, Some(Message::Leave));
        assert!(!supp.get(), "suppression must be off after returning");

        cancel.cancel();
        let _ = timeout(Duration::from_millis(500), h).await;
    }

    #[tokio::test]
    async fn does_not_cross_before_connected() {
        let (mut chans, engine_link) = EngineLink::new();
        let supp = Suppressor::new();
        let (cap_tx, cap_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_server_engine(
            server_cfg(),
            (1920, 1080),
            engine_link,
            supp.clone(),
            cap_rx,
            cancel.clone(),
            None,
        ));

        // No Connected yet. Arm and reach the edge.
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 540 }).unwrap();
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();

        assert!(recv_msg(&mut chans.to_peer).await.is_none(), "must not cross while disconnected");
        assert!(!supp.get());

        cancel.cancel();
        let _ = timeout(Duration::from_millis(500), h).await;
    }

    #[tokio::test]
    async fn suppress_rejected_undoes_the_crossing() {
        // If the X grab is refused after we optimistically crossed (sent Enter +
        // turned suppression on), the capture thread reports SuppressRejected and
        // the engine must reverse course: send Leave, drop suppression, go Local.
        let (mut chans, engine_link) = EngineLink::new();
        let supp = Suppressor::new();
        let (cap_tx, cap_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_server_engine(
            server_cfg(),
            (1920, 1080),
            engine_link,
            supp.clone(),
            cap_rx,
            cancel.clone(),
            None,
        ));

        chans
            .events
            .send(LinkEvent::Connected { peer: "c".into(), width: 1920, height: 1080 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;

        // Arm and cross.
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 540 }).unwrap();
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();
        assert!(matches!(recv_msg(&mut chans.to_peer).await, Some(Message::Enter { .. })));
        assert!(supp.get(), "suppression on right after crossing");

        // The grab was refused: revert.
        cap_tx.send(CaptureSignal::SuppressRejected).unwrap();
        assert_eq!(recv_msg(&mut chans.to_peer).await, Some(Message::Leave));
        assert!(!supp.get(), "suppression dropped after rejection");

        // Back to Local: a fresh arm + cross must work again.
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 540 }).unwrap();
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();
        assert!(
            matches!(recv_msg(&mut chans.to_peer).await, Some(Message::Enter { .. })),
            "can re-cross after a rejection"
        );

        cancel.cancel();
        let _ = timeout(Duration::from_millis(500), h).await;
    }

    #[tokio::test]
    async fn survives_disconnect_and_re_arms_on_reconnect() {
        // The critical lifecycle fix: a disconnect must NOT kill the engine.
        let (mut chans, engine_link) = EngineLink::new();
        let supp = Suppressor::new();
        let (cap_tx, cap_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_server_engine(
            server_cfg(),
            (1920, 1080),
            engine_link,
            supp.clone(),
            cap_rx,
            cancel.clone(),
            None,
        ));

        // First client connects, crosses, then drops.
        chans
            .events
            .send(LinkEvent::Connected { peer: "a".into(), width: 1920, height: 1080 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 540 }).unwrap();
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();
        assert!(matches!(recv_msg(&mut chans.to_peer).await, Some(Message::Enter { .. })));
        assert!(supp.get());

        chans
            .events
            .send(LinkEvent::Disconnected(mouse_core::net::DisconnectReason::PeerClosed))
            .unwrap();
        // Suppression must drop on disconnect.
        // (give the engine a moment to process)
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!supp.get(), "suppression released on disconnect");

        // Second client connects — engine is still alive and must work.
        chans
            .events
            .send(LinkEvent::Connected { peer: "b".into(), width: 1280, height: 720 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 360 }).unwrap();
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 360 }).unwrap();
        let m = recv_msg(&mut chans.to_peer).await;
        assert!(matches!(m, Some(Message::Enter { x: 1279, .. })), "re-cross to new client, got {m:?}");
        assert!(supp.get());

        cancel.cancel();
        let _ = timeout(Duration::from_millis(500), h).await;
    }
}
