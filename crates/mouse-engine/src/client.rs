//! The client-side engine: receives input from the peer and injects it locally,
//! and pushes control back when the user moves the cursor back through the entry
//! edge.
//!
//! All injection is gated on `active`, and all release safety lives in the
//! injector thread (see [`crate::injector`]), so a late/stray message or an
//! abnormal exit can't strand a key on this machine.

use mouse_core::config::Config;
use mouse_core::net::LinkEvent;
use mouse_core::protocol::Message;
use mouse_input::InputEvent;
use tokio_util::sync::CancellationToken;

use crate::geometry::in_edge_band;
use crate::injector::{InjectCmd, InjectorTx};
use crate::status::{set_status_connected, set_status_disconnected, set_status_engaged};
use crate::{set_clipboard_connected, ClipboardRoute, EngineLink};

/// Run the client engine until cancelled (or the link shuts down).
pub async fn run_client_engine(
    cfg: Config,
    local_dims: (i32, i32),
    link: EngineLink,
    inject: InjectorTx,
    cancel: CancellationToken,
    clipboard: Option<ClipboardRoute>,
) {
    let EngineLink {
        to_peer,
        mut from_peer,
        mut events,
        status,
    } = link;

    let entry_edge = cfg.entry_edge;
    let threshold = cfg.edge_threshold_px;
    let (w, h) = local_dims;
    let clamp = |x: i32, y: i32| (x.clamp(0, (w - 1).max(0)), y.clamp(0, (h - 1).max(0)));

    // Whether the peer currently owns our cursor.
    let mut active = false;
    // Edge-trigger: only allow a return once the cursor has moved out of the
    // entry band, so entering (which lands on that very edge) doesn't bounce back.
    let mut armed = false;
    let (mut vx, mut vy) = (0i32, 0i32);
    // Per-crossing accounting of what the peer actually sent us. The field report is
    // "the mouse crosses but the keyboard doesn't", and until now neither end could say
    // whether the keystrokes were never captured on the server or were captured, sent,
    // and then lost here. Motion and keys arrive on the same connection through the same
    // `active` gate, so `moves>0, keys=0` localises the fault to the server's capture,
    // and `keys>0` localises it to injection or window focus on this machine.
    let (mut moves, mut keys) = (0u64, 0u64);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                inject.send(InjectCmd::ReleaseAll);
                return;
            }

            msg = from_peer.recv() => {
                let Some(msg) = msg else {
                    inject.send(InjectCmd::ReleaseAll);
                    return;
                };
                match msg {
                    Message::Enter { x, y } => {
                        // Clean slate so no state carries across crossings.
                        inject.send(InjectCmd::ReleaseAll);
                        let (cx, cy) = clamp(x, y);
                        inject.send(InjectCmd::MoveAbs { x: cx, y: cy });
                        vx = cx;
                        vy = cy;
                        active = true;
                        armed = false;
                        moves = 0;
                        keys = 0;
                        set_status_engaged(&status, true);
                        tracing::info!("peer took control, entry ({cx},{cy})");
                    }
                    Message::Leave => {
                        inject.send(InjectCmd::ReleaseAll);
                        active = false;
                        armed = false;
                        set_status_engaged(&status, false);
                        report_crossing(moves, keys);
                    }
                    Message::MouseMove { dx, dy } if active => {
                        // saturating_add so a malformed/hostile peer delta can't
                        // overflow the virtual cursor (the clamp then pins it on-screen).
                        let (nx, ny) = clamp(vx.saturating_add(dx), vy.saturating_add(dy));
                        vx = nx;
                        vy = ny;
                        moves += 1;
                        inject.send(InjectCmd::Apply(InputEvent::MouseMove { dx, dy }));
                        if !in_edge_band(entry_edge, vx, vy, w, h, threshold) {
                            armed = true;
                        } else if armed {
                            let _ = to_peer.send(Message::LeaveRequest);
                            active = false;
                            armed = false;
                            set_status_engaged(&status, false);
                        }
                    }
                    // Clipboard sync is independent of who owns input: hand it to the
                    // clipboard thread (if enabled) whether active or not.
                    m @ (Message::Clipboard { .. } | Message::ClipboardImage { .. }) => {
                        if let Some(c) = &clipboard {
                            let _ = c.tx.send(m);
                        }
                    }
                    // Other input events (button/scroll/key) while active.
                    other if active => {
                        if let Some(ev) = InputEvent::from_message(&other) {
                            if let InputEvent::Key { key, pressed } = &ev {
                                keys += 1;
                                // First few only: enough to prove keys arrive and to see
                                // which ones, without the per-second spam that got the
                                // old kbd_diag counters deleted.
                                if keys <= 8 {
                                    tracing::info!(?key, pressed, n = keys, "key received from peer");
                                }
                            }
                            inject.send(InjectCmd::Apply(ev));
                        }
                    }
                    // Anything received while inactive is ignored (defense in depth).
                    _ => {}
                }
            }

            ev = events.recv() => {
                match ev {
                    None => {
                        inject.send(InjectCmd::ReleaseAll);
                        return;
                    }
                    Some(LinkEvent::Disconnected(reason)) => {
                        inject.send(InjectCmd::ReleaseAll);
                        active = false;
                        armed = false;
                        set_clipboard_connected(&clipboard, false);
                        set_status_disconnected(&status);
                        tracing::info!("disconnected from server: {reason:?}");
                    }
                    Some(LinkEvent::Connected { peer, width, height }) => {
                        // Clean slate on every (re)connect so no crossing state
                        // survives a reconnect — mirrors the server's reset and keeps
                        // this engine correct once Phase 7 reuses it across reconnects.
                        inject.send(InjectCmd::ReleaseAll);
                        active = false;
                        armed = false;
                        vx = 0;
                        vy = 0;
                        set_clipboard_connected(&clipboard, true);
                        set_status_connected(&status, &peer, width, height);
                        tracing::info!("connected to server '{peer}': {width}x{height}");
                    }
                    Some(_) => {}
                }
            }
        }
    }
}

/// Summarise a finished crossing. The `moves>0, keys=0` case is called out explicitly
/// because that is the reported bug, and naming it in the log saves reading counters.
fn report_crossing(moves: u64, keys: u64) {
    if moves > 0 && keys == 0 {
        tracing::warn!(
            moves,
            keys = 0,
            "peer released control — MOTION ARRIVED BUT NO KEYS. The keystrokes were \
             never sent by the server, so they were not captured there (this machine \
             received nothing to inject)."
        );
    } else {
        tracing::info!(moves, keys, "peer released control");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injector::InjectCmd;
    use crate::EngineLink;
    use mouse_core::config::{Edge, Role};
    use mouse_input::{Button, Key};
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    fn client_cfg() -> Config {
        Config {
            role: Role::Client,
            entry_edge: Edge::Right,
            edge_threshold_px: 2,
            ..Default::default()
        }
    }

    /// Drain all InjectCmds available within a short window.
    fn drain(rx: &Receiver<InjectCmd>) -> Vec<InjectCmd> {
        let mut v = Vec::new();
        while let Ok(c) = rx.recv_timeout(Duration::from_millis(50)) {
            v.push(c);
        }
        v
    }

    #[tokio::test]
    async fn enter_injects_clean_slate_then_forwards_until_leave() {
        let (chans, engine_link) = EngineLink::new();
        let (inject, rx) = InjectorTx::for_test();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_client_engine(
            client_cfg(),
            (1920, 1080),
            engine_link,
            inject,
            cancel.clone(),
            None,
        ));

        // Enter at the right edge.
        chans.from_peer.send(Message::Enter { x: 1919, y: 500 }).unwrap();
        // A key press while active is injected.
        chans
            .from_peer
            .send(Message::Key { key: "c:a".into(), pressed: true })
            .unwrap();
        chans
            .from_peer
            .send(Message::MouseButton { button: Button::Left, pressed: true })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let cmds = drain(&rx);
        assert_eq!(cmds[0], InjectCmd::ReleaseAll, "Enter clears first");
        assert_eq!(cmds[1], InjectCmd::MoveAbs { x: 1919, y: 500 });
        assert!(cmds.contains(&InjectCmd::Apply(InputEvent::Key {
            key: Key::Char('a'),
            pressed: true
        })));
        assert!(cmds.contains(&InjectCmd::Apply(InputEvent::MouseButton {
            button: Button::Left,
            pressed: true
        })));

        // Leave releases everything.
        chans.from_peer.send(Message::Leave).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(drain(&rx).contains(&InjectCmd::ReleaseAll));

        // Input after Leave (inactive) is ignored.
        chans
            .from_peer
            .send(Message::MouseMove { dx: 9, dy: 9 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(drain(&rx).is_empty(), "no injection while inactive");

        cancel.cancel();
        let _ = h.await;
    }

    #[tokio::test]
    async fn bounce_back_only_after_moving_inward() {
        let (mut chans, engine_link) = EngineLink::new();
        let (inject, rx) = InjectorTx::for_test();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_client_engine(
            client_cfg(),
            (1920, 1080),
            engine_link,
            inject,
            cancel.clone(),
            None,
        ));

        chans.from_peer.send(Message::Enter { x: 1919, y: 500 }).unwrap();
        // A tiny jiggle at the entry edge must NOT bounce (not armed).
        chans.from_peer.send(Message::MouseMove { dx: 1, dy: 0 }).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(timeout_recv(&mut chans.to_peer).await.is_none(), "no premature LeaveRequest");

        // Move inward (arms), then back to the right edge (fires LeaveRequest).
        chans.from_peer.send(Message::MouseMove { dx: -1000, dy: 0 }).unwrap();
        chans.from_peer.send(Message::MouseMove { dx: 1000, dy: 0 }).unwrap();
        let m = timeout_recv(&mut chans.to_peer).await;
        assert_eq!(m, Some(Message::LeaveRequest));

        let _ = drain(&rx);
        cancel.cancel();
        let _ = h.await;
    }

    #[tokio::test]
    async fn connect_resets_stale_active_state() {
        // A (re)connect must clear any leftover active/armed state, so input from a
        // new session can't inject before a fresh Enter (matters once Phase 7 reuses
        // this engine across reconnects).
        let (chans, engine_link) = EngineLink::new();
        let (inject, rx) = InjectorTx::for_test();
        let cancel = CancellationToken::new();
        let h = tokio::spawn(run_client_engine(
            client_cfg(),
            (1920, 1080),
            engine_link,
            inject,
            cancel.clone(),
            None,
        ));

        // Become active via Enter.
        chans.from_peer.send(Message::Enter { x: 1919, y: 500 }).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = drain(&rx);

        // A connect event arrives — must reset to inactive and release.
        chans
            .events
            .send(LinkEvent::Connected { peer: "s".into(), width: 1920, height: 1080 })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(drain(&rx).contains(&InjectCmd::ReleaseAll), "connect releases held input");

        // Input now (inactive) must be ignored.
        chans.from_peer.send(Message::MouseMove { dx: 5, dy: 5 }).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(drain(&rx).is_empty(), "no injection while inactive after reconnect");

        cancel.cancel();
        let _ = h.await;
    }

    async fn timeout_recv(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Message>,
    ) -> Option<Message> {
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .ok()
            .flatten()
    }
}
