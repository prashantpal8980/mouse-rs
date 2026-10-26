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
use mouse_input::{CaptureSignal, InputEvent, Suppressor};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::geometry::{entry_point, in_edge_band, is_opposing};
use crate::hotkey::{self, Hotkey};
use crate::status::{
    set_crossed, set_status_connected, set_status_disconnected, set_status_engaged,
    set_status_refused,
};
use crate::{set_clipboard_connected, ClipboardRoute, EngineLink};

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
        status,
        health,
    } = link;

    let exit_edge = cfg.exit_edge;
    let entry_edge = cfg.entry_edge;
    let threshold = cfg.edge_threshold_px;
    let (lw, lh) = local_dims;

    // The single-key lock/return hotkey (Scroll Lock by default; also the in-proc
    // capture failsafe) and an optional multi-key return combo (e.g. "ctrl+win+alt+esc").
    // Both are matched here from the captured key stream, so they work whichever side is
    // the server.
    let lock_hk = Hotkey::parse(&cfg.lock_hotkey).unwrap_or_else(|| {
        tracing::warn!(
            "lock_hotkey '{}' not recognized; defaulting to scroll_lock",
            cfg.lock_hotkey
        );
        Hotkey::parse("scroll_lock").expect("scroll_lock parses")
    });
    let return_hk = Hotkey::parse(&cfg.return_hotkey);
    if return_hk.is_some() {
        tracing::info!("return hotkey: '{}'", cfg.return_hotkey);
    }
    if !is_opposing(exit_edge, entry_edge) {
        tracing::warn!(
            "exit_edge {exit_edge:?} and entry_edge {entry_edge:?} are not an opposing pair; \
             entry placement will be approximate"
        );
    }

    let mut state = State::Local;
    let mut connected = false;
    let mut peer_dims: Option<(i32, i32)> = None;
    // Running modifier bitset for combo-hotkey matching while Remote.
    let mut held_mods: u8 = 0;
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
                                held_mods = 0; // fresh modifier state per crossing
                                set_status_engaged(&status, true);
                        set_crossed(&health, true);
                                tracing::info!("crossed to remote, entry ({ex},{ey})");
                            }
                        }
                    }
                    (CaptureSignal::Input(ev), State::Remote) => {
                        if check_return(&ev, &mut held_mods, &lock_hk, &return_hk) {
                            return_to_local(&to_peer, &suppress, &mut state, &mut cross_armed, &status, &health, &mut held_mods);
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
                        return_to_local(&to_peer, &suppress, &mut state, &mut cross_armed, &status, &health, &mut held_mods);
                    }
                }
            }

            ev = events.recv() => {
                match ev {
                    None => {
                        suppress.set(false);
                        return;
                    }
                    Some(LinkEvent::Connected { width, height, peer, login_screen }) => {
                        connected = true;
                        peer_dims = Some((width, height));
                        cross_armed = false;
                        state = State::Local;
                        suppress.set(false);
                        // Nothing from this machine's clipboard goes to a login screen:
                        // nobody is logged in there to have asked for it.
                        set_clipboard_connected(&clipboard, !login_screen);
                        if login_screen {
                            let label = format!("{peer} (login screen)");
                            set_status_connected(&status, &label, width, height, true);
                            tracing::info!(
                                "peer '{peer}' is at its LOGIN SCREEN ({width}x{height}): move the pointer \
                                 off the {exit_edge:?} edge of this screen to type the password there. \
                                 Move it a little inward first if it is resting on that edge. \
                                 The clipboard is not shared until someone logs in"
                            );
                        } else {
                            set_status_connected(&status, &peer, width, height, false);
                            tracing::info!("peer '{peer}' connected: {width}x{height}");
                        }
                    }
                    Some(LinkEvent::Disconnected(reason)) => {
                        // Keep running: the link re-accepts the next client.
                        connected = false;
                        cross_armed = false;
                        state = State::Local;
                        held_mods = 0;
                        suppress.set(false);
                        set_crossed(&health, false);
                        set_clipboard_connected(&clipboard, false);
                        set_status_disconnected(&status);
                        tracing::info!("peer disconnected: {reason:?}");
                    }
                    Some(LinkEvent::AuthFailed(reason)) => set_status_refused(&status, &reason),
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
                            return_to_local(&to_peer, &suppress, &mut state, &mut cross_armed, &status, &health, &mut held_mods);
                        }
                    }
                    // Clipboard sync lives off the input path: hand it to the
                    // clipboard thread (if enabled) and otherwise ignore it.
                    //
                    // This must match *every* clipboard-family message. It once listed
                    // only text and images by name, so `ClipboardFiles`,
                    // `ClipboardImageRef` and the blob messages fell through to the
                    // catch-all below and were dropped without a trace — which meant
                    // file transfer could not work at all, however correct the code
                    // behind it was. See `is_clipboard_message`.
                    Some(m) if crate::is_clipboard_message(&m) => {
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
#[allow(clippy::too_many_arguments)]
fn return_to_local(
    to_peer: &UnboundedSender<Message>,
    suppress: &Suppressor,
    state: &mut State,
    cross_armed: &mut bool,
    status: &Option<crate::status::StatusHandle>,
    health: &Option<std::sync::Arc<mouse_core::bulk::LinkHealth>>,
    held_mods: &mut u8,
) {
    suppress.set(false);
    let _ = to_peer.send(Message::Leave);
    *state = State::Local;
    *cross_armed = false;
    *held_mods = 0; // every path out of Remote clears modifier tracking
    set_status_engaged(status, false);
    set_crossed(health, false);
    tracing::info!("returned to local");
}

/// Maintain the held-modifier bitset from the key stream and report whether `ev` is a
/// press of the lock hotkey or the return combo (so control should snap back to local).
fn check_return(ev: &InputEvent, held: &mut u8, lock: &Hotkey, ret: &Option<Hotkey>) -> bool {
    let InputEvent::Key { key, pressed } = ev else {
        return false;
    };
    if let Some(bit) = hotkey::modifier_bit(key) {
        if *pressed {
            *held |= bit;
        } else {
            *held &= !bit;
        }
    }
    *pressed && (lock.matches(key, *held) || ret.as_ref().is_some_and(|h| h.matches(key, *held)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineLink;
    use mouse_core::config::{Edge, Role};
    use mouse_core::protocol::Message;
    use mouse_input::{Key, NamedKey};
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
            .send(LinkEvent::Connected { peer: "c".into(), width: 1920, height: 1080, login_screen: false })
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
    async fn combo_return_hotkey_hands_control_back() {
        // The user's Ctrl+Win+Alt+Esc escape: with all modifiers held, pressing the
        // trigger returns control (sends Leave, drops suppression) and the trigger is
        // not forwarded. Runs the same engine code that executes on a Windows server.
        let mut cfg = server_cfg();
        cfg.return_hotkey = "ctrl+win+alt+esc".into();
        let (mut chans, engine_link) = EngineLink::new();
        let supp = Suppressor::new();
        let (cap_tx, cap_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_server_engine(
            cfg,
            (1920, 1080),
            engine_link,
            supp.clone(),
            cap_rx,
            cancel.clone(),
            None,
        ));

        chans
            .events
            .send(LinkEvent::Connected { peer: "c".into(), width: 1920, height: 1080, login_screen: false })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 540 }).unwrap();
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();
        assert!(matches!(recv_msg(&mut chans.to_peer).await, Some(Message::Enter { .. })));
        assert!(supp.get());

        // Hold each modifier (forwarded to the peer), then the trigger.
        for k in [NamedKey::CtrlL, NamedKey::SuperL, NamedKey::AltL] {
            cap_tx
                .send(CaptureSignal::Input(InputEvent::Key { key: Key::Named(k), pressed: true }))
                .unwrap();
            assert!(
                matches!(recv_msg(&mut chans.to_peer).await, Some(Message::Key { .. })),
                "held modifier {k:?} is forwarded"
            );
        }
        cap_tx
            .send(CaptureSignal::Input(InputEvent::Key {
                key: Key::Named(NamedKey::Escape),
                pressed: true,
            }))
            .unwrap();
        // The combo fires: Leave (not the Esc key) is what reaches the peer.
        assert_eq!(recv_msg(&mut chans.to_peer).await, Some(Message::Leave));
        assert!(!supp.get(), "suppression dropped by the return combo");

        // A lone Esc (no modifiers) must NOT return — it's just forwarded.
        cap_tx.send(CaptureSignal::Cursor { x: 800, y: 540 }).unwrap();
        cap_tx.send(CaptureSignal::Cursor { x: 0, y: 540 }).unwrap();
        assert!(matches!(recv_msg(&mut chans.to_peer).await, Some(Message::Enter { .. })));
        cap_tx
            .send(CaptureSignal::Input(InputEvent::Key {
                key: Key::Named(NamedKey::Escape),
                pressed: true,
            }))
            .unwrap();
        assert!(
            matches!(recv_msg(&mut chans.to_peer).await, Some(Message::Key { .. })),
            "bare Esc is forwarded, not treated as the combo"
        );

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
            .send(LinkEvent::Connected { peer: "c".into(), width: 1920, height: 1080, login_screen: false })
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
            .send(LinkEvent::Connected { peer: "a".into(), width: 1920, height: 1080, login_screen: false })
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
            .send(LinkEvent::Connected { peer: "b".into(), width: 1280, height: 720, login_screen: false })
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
