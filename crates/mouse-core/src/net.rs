//! The async network link: TLS, HMAC handshake, heartbeat-based dead-link
//! detection, and cooperative cancellation.
//!
//! This is the module that fixes the Python app's two structural bugs:
//!
//! * **Stale "connected" state** — every read is bounded by a liveness window. A
//!   PING goes out on each heartbeat tick; if no frame of any kind arrives within
//!   `liveness_timeout`, the link is declared `TimedOut` and the UI is told. TCP
//!   keepalive is set as a backstop.
//!
//! * **Stop/Reload hangs** — the whole loop selects on a [`CancellationToken`].
//!   Cancelling it makes every branch return promptly; there are no blocking
//!   reads or un-interruptible threads to get stuck in.

use std::io;
use std::net::SocketAddr;
use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use futures::stream::FuturesUnordered;
use crate::{beacon, discover};
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, PairedDevice};
use std::sync::Arc;

use crate::bulk::{BulkServer, LinkHealth};
use crate::protocol::{CodecError, Lane, Message, MessageCodec, PROTOCOL_VERSION};
use crate::auth::Side;
use crate::motion_udp::{ClientMotion, ServerMotion};
use crate::{auth, tls};

/// Tunable timing for the link. Defaults are conservative for a LAN.
#[derive(Debug, Clone, Copy)]
pub struct LinkParams {
    /// How often to send a PING (and check liveness).
    pub heartbeat_interval: Duration,
    /// Declare the link dead if no frame arrives within this window.
    pub liveness_timeout: Duration,
    /// Hard cap on the handshake; a stalled peer can't hang us.
    pub handshake_timeout: Duration,
    /// Client reconnect: delay before the first retry after a drop/failed connect.
    pub reconnect_initial: Duration,
    /// Client reconnect: upper bound the backoff grows toward (doubling each try).
    pub reconnect_max: Duration,
}

impl Default for LinkParams {
    fn default() -> Self {
        LinkParams {
            heartbeat_interval: Duration::from_secs(2),
            liveness_timeout: Duration::from_secs(6),
            handshake_timeout: Duration::from_secs(10),
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(15),
        }
    }
}

/// Status updates emitted to the UI/log as the link's state changes.
#[derive(Debug, Clone)]
pub enum LinkEvent {
    Listening(SocketAddr),
    Connecting(String),
    /// `login_screen`: the peer is the login-screen instance, so its machine is at a
    /// password prompt and should get input only.
    Connected { peer: String, width: i32, height: i32, login_screen: bool },
    AuthFailed(String),
    Disconnected(DisconnectReason),
    Error(String),
}

/// Why a connection ended. Drives both logging and reconnect policy.
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    /// Local Stop/Reload cancelled the link.
    Cancelled,
    /// Peer closed the connection cleanly (EOF).
    PeerClosed,
    /// Heartbeat saw no traffic within the liveness window.
    TimedOut,
    /// The peer sent something malformed.
    ProtocolError(String),
    /// Underlying socket/TLS I/O error.
    Io(String),
    /// This machine's network changed (a cable, Wi-Fi, a new address). The session is
    /// dropped on purpose so the client reconnects over whichever path is now best.
    NetworkChanged,
    /// The server accepted a newer connection from the same peer and replaced this one.
    Superseded,
}

impl DisconnectReason {
    /// Whether an automatic reconnect makes sense (Phase 7 uses this).
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            DisconnectReason::PeerClosed
                | DisconnectReason::TimedOut
                | DisconnectReason::Io(_)
                | DisconnectReason::NetworkChanged
        )
    }
}

/// Classify a frame-read error from the session stream. A socket/TLS **`Io`** error —
/// e.g. the server process was killed/restarted → "connection reset by peer" — is a
/// **recoverable** drop, so the client reconnects. Only a genuinely malformed frame
/// (**`Json`**) is a fatal `ProtocolError`. (Getting this wrong made a server restart
/// permanently stop the client instead of reconnecting.)
fn classify_read_error(e: &CodecError) -> DisconnectReason {
    match e {
        CodecError::Io(io) => DisconnectReason::Io(io.to_string()),
        CodecError::Json(_) => DisconnectReason::ProtocolError(e.to_string()),
    }
}

/// Channels the engine uses to talk to a running link.
pub struct LinkChannels {
    /// Messages the app wants to send to the peer.
    pub to_peer: mpsc::UnboundedReceiver<Message>,
    /// Non-heartbeat messages received from the peer.
    pub from_peer: mpsc::UnboundedSender<Message>,
    /// Status events for the UI/log.
    pub events: mpsc::UnboundedSender<LinkEvent>,
}

// ----------------------------------------------------------------------------
// A stream that is either plaintext TCP or TLS, with one AsyncRead/AsyncWrite.
// All inner types are Unpin, so we can delegate via get_mut without projection.
// ----------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
pub enum LinkStream {
    Plain(TcpStream),
    ServerTls(Box<ServerTlsStream<TcpStream>>),
    ClientTls(Box<ClientTlsStream<TcpStream>>),
}

impl LinkStream {
    /// The key for this session's motion datagrams, from the TLS exporter. None on a
    /// plain link: without TLS there is nothing to derive it from, and motion stays on TCP.
    fn motion_key(&self) -> Option<crate::motion_udp::Key> {
        let out = [0u8; 32];
        let label = crate::motion_udp::KEY_LABEL;
        let exported = match self {
            LinkStream::Plain(_) => return None,
            LinkStream::ServerTls(s) => s.get_ref().1.export_keying_material(out, label, None),
            LinkStream::ClientTls(s) => s.get_ref().1.export_keying_material(out, label, None),
        };
        exported.ok().map(crate::motion_udp::Key)
    }

    fn tcp(&self) -> &TcpStream {
        match self {
            LinkStream::Plain(s) => s,
            LinkStream::ServerTls(s) => s.get_ref().0,
            LinkStream::ClientTls(s) => s.get_ref().0,
        }
    }

    /// Keying material exported from this TLS session, which ties the auth proofs to
    /// it (B34). A plain link has nothing to bind to and yields an empty binding.
    fn auth_binding(&self) -> Result<Vec<u8>, String> {
        let out = [0u8; 32];
        let exported = match self {
            LinkStream::Plain(_) => return Ok(Vec::new()),
            LinkStream::ServerTls(s) => {
                s.get_ref().1.export_keying_material(out, auth::TLS_EXPORTER_LABEL, None)
            }
            LinkStream::ClientTls(s) => {
                s.get_ref().1.export_keying_material(out, auth::TLS_EXPORTER_LABEL, None)
            }
        };
        exported.map(|b| b.to_vec()).map_err(|e| format!("tls exporter: {e}"))
    }
}

impl AsyncRead for LinkStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            LinkStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            LinkStream::ServerTls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            LinkStream::ClientTls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for LinkStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            LinkStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            LinkStream::ServerTls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            LinkStream::ClientTls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            LinkStream::Plain(s) => Pin::new(s).poll_flush(cx),
            LinkStream::ServerTls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            LinkStream::ClientTls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            LinkStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            LinkStream::ServerTls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            LinkStream::ClientTls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Set TCP keepalive as a transport-level backstop to the app heartbeat.
fn enable_keepalive(stream: &TcpStream) {
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(5))
        .with_interval(Duration::from_secs(2));
    let sock = socket2::SockRef::from(stream);
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        tracing::debug!("could not set TCP keepalive: {e}");
    }
    let _ = stream.set_nodelay(true); // input latency: don't Nagle-buffer tiny frames
}

// ----------------------------------------------------------------------------
// Server: accept one client at a time, keep listening across disconnects.
// ----------------------------------------------------------------------------

/// Run the server side until `cancel` fires. Re-accepts after each disconnect.
///
/// `bulk` receives any connection that declares [`Lane::Bulk`]. Pass
/// [`BulkServer::disabled`] to refuse them, which is what a build with the bulk lane
/// turned off does.
/// Most connections being authenticated at once on the computer with the mouse. Far
/// above what a few paired computers need; a bound on what a stranger can hold open.
const MAX_HANDSHAKES: usize = 16;

/// A connection that finished its handshake beside the hub: the stream, where it came
/// from and arrived, and how the handshake went.
type Shaken = (
    Framed<LinkStream, MessageCodec>,
    SocketAddr,
    Option<SocketAddr>,
    Result<Result<ClientHello, String>, tokio::time::error::Elapsed>,
);

pub async fn run_server(
    cfg: Config,
    params: LinkParams,
    cancel: CancellationToken,
    ch: LinkChannels,
    bulk: BulkServer,
    health: Arc<LinkHealth>,
) {
    // The computers we are paired with, live from here on (R44).
    crate::trust::load(&cfg.device_id, &cfg.paired);
    crate::trust::set_input_peer(&cfg.device_id, None);
    // Kept for the accept loop's own diagnostics: `ch` moves into the session task for
    // the duration of a session, and an accept error still has to be reportable.
    let link_events = ch.events.clone();
    let bind = format!("{}:{}", cfg.listen_host, cfg.listen_port);
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            let _ = link_events.send(LinkEvent::Error(format!("bind {bind} failed: {e}")));
            return;
        }
    };
    if let Ok(addr) = listener.local_addr() {
        let _ = link_events.send(LinkEvent::Listening(addr));
    }
    // Also listen on IPv6 when listening everywhere, so a client can reach this machine on
    // its link-local address when the network gave it no IPv4 address (see `beacon`).
    let listener6 = if listener.local_addr().is_ok_and(|a| a.ip().is_unspecified()) {
        match listen_v6(cfg.listen_port) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::info!("no IPv6 listener on {} ({e}); IPv4 only", cfg.listen_port);
                None
            }
        }
    } else {
        None
    };
    // Pointer motion's UDP socket, on the same port number as the TCP listener. Without
    // it motion simply stays on TCP.
    let motion_sock = match tokio::net::UdpSocket::bind(&bind).await {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!("UDP {bind} unavailable ({e}); pointer motion stays on TCP");
            None
        }
    };
    // And beside the IPv6 listener, for a client that reached us over link-local.
    let motion_sock6 = match &listener6 {
        Some(_) => match udp_v6_only(cfg.listen_port) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::info!("UDP [::]:{} unavailable ({e}); motion over IPv6 stays on TCP", cfg.listen_port);
                None
            }
        },
        None => None,
    };
    // Tell any client on any network we are attached to where to find us. Stops with `cancel`.
    tokio::spawn(beacon::run_sender(
        cfg.device_id.clone(),
        cfg.name.clone(),
        cfg.listen_port,
        cfg.discovery_port,
        cancel.child_token(),
    ));

    let acceptor = if cfg.tls_enabled {
        match tls::server_acceptor() {
            Ok(a) => Some(a),
            Err(e) => {
                let _ = link_events.send(LinkEvent::Error(format!("tls setup: {e}")));
                return;
            }
        }
    } else {
        None
    };

    // R46: one input session per connected computer, all at once. The engine still sees
    // a single peer: the one in use. This hub forwards the engine's messages to that one,
    // passes on what that one sends, and tells the engine when the one in use changes.
    // Switching is instant because nothing reconnects; the computer left is sent a Leave.
    let LinkChannels { to_peer: mut engine_out, from_peer: engine_in, events: engine_events } = ch;
    let own = cfg.device_id.clone();
    let mut live: HashMap<String, LiveSession> = HashMap::new();
    let (ended_tx, mut ended_rx) = mpsc::unbounded_channel::<(String, u64, DisconnectReason)>();
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<(String, Message)>();
    let mut active: Option<String> = None;
    let mut epoch = 0u64;
    // Handshakes in progress, run beside the hub (see the accept arm).
    let handshake_slots = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
    let shake_cfg = Arc::new(cfg.clone());
    let (shaken_tx, mut shaken_rx) = mpsc::unbounded_channel::<Shaken>();
    // How often a switch asked for by a shortcut or a click is picked up.
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let events = link_events.clone();
    crate::trust::set_connected(&own, &[]);
    // The last large image or file list copied here, and the computers that were
    // connected then but not in use, so are still owed it. One that connects later is
    // not handed an old copy.
    // After a restart, the computer last in use (R45) gets a moment to come back: another
    // that happens to connect first is used meanwhile, then handed over when it arrives.
    // A switch the user asks for always wins.
    let started = tokio::time::Instant::now();
    let preferred = cfg.connect_to.clone();
    let mut stand_in = false;
    let mut last_offer: Option<Message> = None;
    let mut owed: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        if let (Some(a), Some(m)) = (active.as_ref(), last_offer.as_ref()) {
            if owed.remove(a) {
                if let Some(l) = live.get(a).filter(|l| !l.login_screen) {
                    tracing::info!(peer = %l.name, "offering the last copied files/image to {}", l.name);
                    let _ = l.to.send(m.clone());
                }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => break,

            // A session ended. A stale end (from a session already replaced by a newer
            // path from the same computer) is ignored by its epoch.
            Some((dev, ep, reason)) = ended_rx.recv() => {
                if live.get(&dev).is_none_or(|l| l.epoch != ep) {
                    continue;
                }
                let gone = live.remove(&dev).expect("checked above");
                owed.remove(&dev);
                crate::trust::set_connected(&own, &live.keys().cloned().collect::<Vec<_>>());
                tracing::info!(peer = %gone.name, "{} disconnected ({reason:?}); {} still connected", gone.name, live.len());
                if active.as_deref() == Some(dev.as_str()) {
                    active = None;
                    crate::trust::clear_input_peer(&own, &dev);
                    let _ = engine_events.send(LinkEvent::Disconnected(reason));
                    // Carry on with the computer that connected most recently, so the edge
                    // still leads somewhere.
                    if let Some(next) = live.iter().max_by_key(|(_, l)| l.epoch).map(|(d, _)| d.clone()) {
                        activate(&own, &live, &mut active, &next, &engine_events);
                    }
                }
            }

            // From the engine: to the computer in use. Copied text and small images go
            // to every connected computer, so all of them share one clipboard.
            out = engine_out.recv() => match out {
                None => break,
                Some(m) if is_shared_clip(&m) => {
                    let mut to = Vec::new();
                    for l in live.values().filter(|l| !l.login_screen) {
                        let _ = l.to.send(m.clone());
                        to.push(l.name.as_str());
                    }
                    tracing::info!("clipboard copied here, shared with: {}", to.join(", "));
                }
                Some(m) => {
                    // Large images and files are fetched over the bulk lane, which serves
                    // one computer at a time: offered to the one in use now, and to each
                    // other one when it comes into use.
                    if matches!(m, Message::ClipboardImageRef { .. } | Message::ClipboardFiles { .. }) {
                        last_offer = Some(m.clone());
                        owed = live.keys().filter(|d| active.as_ref() != Some(*d)).cloned().collect();
                    }
                    if let Some(l) = active.as_ref().and_then(|a| live.get(a)) {
                        let _ = l.to.send(m);
                    }
                }
            },

            // From a computer: only the one in use speaks to the engine, except for copied
            // text and small images, which any of them shares with this computer and the
            // others.
            Some((dev, m)) = incoming_rx.recv() => {
                if is_shared_clip(&m) {
                    if let Some(from) = live.get(&dev).filter(|l| !l.login_screen) {
                        let mut to = Vec::new();
                        for (_, l) in live.iter().filter(|(d, l)| **d != dev && !l.login_screen) {
                            let _ = l.to.send(m.clone());
                            to.push(l.name.as_str());
                        }
                        tracing::info!("clipboard copied on {}, shared with this computer and: {}", from.name, to.join(", "));
                        let _ = engine_in.send(m);
                    }
                } else if active.as_deref() == Some(dev.as_str()) {
                    let _ = engine_in.send(m);
                }
            }

            // A switch asked for (a shortcut or Connect): take it if that one is here.
            _ = tick.tick() => {
                // A computer whose pairing was just removed is cut off now, not at its next
                // reconnect (audit, deferred lead). Its session ends through the usual path.
                for (dev, l) in live.iter() {
                    if !l.cancel.is_cancelled() && crate::trust::get(&own, dev).is_none() {
                        tracing::info!(peer = %l.name, "the pairing with {} was removed; disconnecting it", l.name);
                        l.cancel.cancel();
                    }
                }
                if let Some(want) = crate::trust::reserved(&own) {
                    if active.as_deref() != Some(want.as_str()) && live.contains_key(&want) {
                        if let Some(old) = active.as_ref().and_then(|a| live.get(a)) {
                            let _ = old.to.send(Message::Leave);
                        }
                        stand_in = false;
                        activate(&own, &live, &mut active, &want, &engine_events);
                    } else if active.as_deref() == Some(want.as_str()) {
                        crate::trust::reserve(&own, None);
                    }
                }
            }

            accepted = accept_any(&listener, listener6.as_ref()) => {
                let (tcp, peer_addr) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = events.send(LinkEvent::Error(format!("accept: {e}")));
                        continue;
                    }
                };
                let local_addr = tcp.local_addr().ok();
                enable_keepalive(&tcp);

                // TLS and the handshake run beside this loop, not in it. Awaited here, one
                // connection that said nothing held up everything the hub does — input to
                // the computer in use included — for up to twice the handshake timeout,
                // and anyone on the network could repeat that for ever. The number at once
                // is capped, so a flood of silent connections cannot pile up either.
                let Ok(slot) = handshake_slots.clone().try_acquire_owned() else {
                    tracing::warn!("{peer_addr}: {MAX_HANDSHAKES} handshakes already in progress; dropped this connection");
                    continue;
                };
                let (acceptor, cfg, events, done, cancel, wait) =
                    (acceptor.clone(), shake_cfg.clone(), events.clone(), shaken_tx.clone(), cancel.clone(), params.handshake_timeout);
                tokio::spawn(async move {
                    let _slot = slot;
                    let stream = match &acceptor {
                        Some(acc) => match timeout(wait, acc.accept(tcp)).await {
                            Ok(Ok(s)) => LinkStream::ServerTls(Box::new(s)),
                            Ok(Err(e)) => {
                                let _ = events.send(LinkEvent::Error(format!("tls accept: {e}")));
                                return;
                            }
                            Err(_) => {
                                let _ = events.send(LinkEvent::Error("tls accept timed out".into()));
                                return;
                            }
                        },
                        None => LinkStream::Plain(tcp),
                    };
                    let mut framed = Framed::new(stream, MessageCodec::new());
                    // Auth handshake, bounded and cancel-aware. The lane is only known once
                    // the Hello has been read, so `Connecting` is not announced until we
                    // know this is the input connection — a bulk connection is plumbing and
                    // has no business changing what the panel says.
                    let shook = tokio::select! {
                        _ = cancel.cancelled() => return,
                        r = timeout(wait, server_handshake(&mut framed, &cfg)) => r,
                    };
                    let _ = done.send((framed, peer_addr, local_addr, shook));
                });
            }

            // A connection whose handshake has finished, one way or the other.
            Some((framed, peer_addr, local_addr, shook)) = shaken_rx.recv() => {
                let hello = match shook {
                    Ok(Ok(v)) => v,
                    Ok(Err(reason)) if reason.starts_with("pairing:") => {
                        tracing::info!("{reason}");
                        continue;
                    }
                    // Opened and closed without a word: a client that raced several of our
                    // addresses and kept a better one, or a Detect scan. Not a login attempt.
                    Ok(Err(reason)) if reason == "peer closed before hello" => {
                        tracing::debug!("{peer_addr} connected and left without a hello");
                        continue;
                    }
                    Ok(Err(reason)) => {
                        let _ = events.send(LinkEvent::AuthFailed(reason));
                        continue;
                    }
                    Err(_) => {
                        let _ = events.send(LinkEvent::AuthFailed("handshake timed out".into()));
                        continue;
                    }
                };

                let ClientHello { name: peer_name, device: peer_device, width: peer_w, height: peer_h, lane, login_screen: peer_login_screen } = hello;
                match lane {
                    Lane::Bulk => {
                        // Bulk connections are handed off and the loop continues. A new
                        // one replaces whatever came before. Only the computer in use gets
                        // one: another paired computer must not be served the clipboard of
                        // someone else's session (R44).
                        if crate::trust::input_peer(&own).as_deref() == Some(peer_device.as_str()) {
                            bulk.take_server_connection(framed, &cancel);
                        } else {
                            tracing::debug!(peer = %peer_name, "bulk lane from a computer not in use; refused for now");
                        }
                    }
                    Lane::Input => {
                        crate::qos::prioritise_input(framed.get_ref().tcp());
                        note_peer_ip(peer_addr.ip());
                        // The same computer on a newer path replaces its own old session.
                        let replacing = live.remove(&peer_device);
                        if let Some(old) = &replacing {
                            old.cancel.cancel();
                        }
                        tracing::info!(
                            peer = %peer_name,
                            device = short_id(&peer_device),
                            "input link from {peer_addr} arrived through {}",
                            local_path(local_addr)
                        );
                        let sock = if peer_addr.is_ipv6() { &motion_sock6 } else { &motion_sock };
                        let motion = match (sock, framed.get_ref().motion_key()) {
                            (Some(sock), Some(key)) => Motion::Server(ServerMotion::new(sock.clone(), key, peer_addr)),
                            _ => Motion::Off,
                        };
                        epoch += 1;
                        let c = cancel.child_token();
                        let (to_tx, to_rx) = mpsc::unbounded_channel();
                        let (from_tx, mut from_rx) = mpsc::unbounded_channel();
                        // Nothing in a session reports events; this end just has to exist.
                        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
                        let mut session_ch = LinkChannels { to_peer: to_rx, from_peer: from_tx, events: ev_tx };
                        let tagged = incoming_tx.clone();
                        let dev = peer_device.clone();
                        tokio::spawn(async move {
                            while let Some(m) = from_rx.recv().await {
                                if tagged.send((dev.clone(), m)).is_err() {
                                    break;
                                }
                            }
                        });
                        let (h, ended, dev, ep, sc) = (health.clone(), ended_tx.clone(), peer_device.clone(), epoch, c.clone());
                        tokio::spawn(async move {
                            let reason = pump(framed, params, &sc, &mut session_ch, &h, None, motion).await;
                            let _ = ended.send((dev, ep, reason));
                        });
                        live.insert(
                            peer_device.clone(),
                            LiveSession {
                                name: peer_name.clone(),
                                width: peer_w,
                                height: peer_h,
                                login_screen: peer_login_screen,
                                to: to_tx,
                                cancel: c,
                                epoch,
                            },
                        );
                        crate::trust::set_connected(&own, &live.keys().cloned().collect::<Vec<_>>());
                        let was_active = active.as_deref() == Some(peer_device.as_str());
                        let wanted = crate::trust::reserved(&own).as_deref() == Some(peer_device.as_str());
                        if was_active {
                            // The one in use came back on a newer path: same as before.
                            let _ = engine_events.send(LinkEvent::Disconnected(DisconnectReason::Superseded));
                            activate(&own, &live, &mut active, &peer_device, &engine_events);
                        } else if active.is_none()
                            || wanted
                            || (stand_in && peer_device == preferred && started.elapsed() < Duration::from_secs(20))
                        {
                            let first = active.is_none() && !wanted;
                            if stand_in && peer_device == preferred {
                                tracing::info!(peer = %peer_name, "{peer_name} was in use before the restart; using it again");
                            }
                            stand_in = first && !preferred.is_empty() && peer_device != preferred;
                            if let Some(old) = active.as_ref().and_then(|a| live.get(a)) {
                                let _ = old.to.send(Message::Leave);
                            }
                            activate(&own, &live, &mut active, &peer_device, &engine_events);
                        } else {
                            tracing::info!(peer = %peer_name, "{peer_name} is connected too; switch to it with its shortcut or Connect");
                        }
                    }
                }
            }
        }
    }

    // Cancelled: every session ends its own bounded teardown; report the one in use.
    for l in live.values() {
        l.cancel.cancel();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut active_reason = None;
    while !live.is_empty() {
        match tokio::time::timeout_at(deadline, ended_rx.recv()).await {
            Ok(Some((dev, ep, reason))) => {
                if live.get(&dev).is_some_and(|l| l.epoch == ep) {
                    live.remove(&dev);
                    if active.as_deref() == Some(dev.as_str()) {
                        active_reason = Some(reason);
                    }
                }
            }
            _ => break,
        }
    }
    crate::trust::set_connected(&own, &[]);
    if let Some(reason) = active_reason {
        let _ = engine_events.send(LinkEvent::Disconnected(reason));
    }
}

/// Copied text and small images: small enough to send to every connected computer at
/// once, so they all share one clipboard.
fn is_shared_clip(m: &Message) -> bool {
    matches!(m, Message::Clipboard { .. } | Message::ClipboardImage { .. })
}

/// One connected computer on the server (R46).
struct LiveSession {
    name: String,
    width: i32,
    height: i32,
    login_screen: bool,
    /// To this computer's session.
    to: mpsc::UnboundedSender<Message>,
    cancel: CancellationToken,
    /// Which session this is, so the end of a replaced one is not taken for the new one's.
    epoch: u64,
}

/// Make `dev` the computer in use and tell the engine, which treats it as a fresh
/// connection (it starts on this side of the edge).
fn activate(
    own: &str,
    live: &HashMap<String, LiveSession>,
    active: &mut Option<String>,
    dev: &str,
    engine_events: &mpsc::UnboundedSender<LinkEvent>,
) {
    let Some(l) = live.get(dev) else { return };
    *active = Some(dev.to_owned());
    crate::trust::set_input_peer(own, Some(dev));
    crate::trust::connected_with(own, dev);
    tracing::info!(peer = %l.name, "now using {}", l.name);
    let _ = engine_events.send(LinkEvent::Connected {
        peer: l.name.clone(),
        width: l.width,
        height: l.height,
        login_screen: l.login_screen,
    });
}

// ----------------------------------------------------------------------------
// Client: connect, then (when enabled) reconnect with backoff across drops.
// ----------------------------------------------------------------------------

/// What one connection attempt resulted in, and what the loop should do next.
enum AttemptOutcome {
    /// Local Stop/Reload fired — exit, no reconnect.
    Cancelled,
    /// A terminal failure whose own descriptive event (AuthFailed/Error) was
    /// already emitted — exit, no reconnect (retrying can't fix it).
    Fatal,
    /// The link dropped or a (re)connect attempt failed. `was_connected` is true
    /// if we'd fully connected this attempt (so the loop resets the backoff and
    /// always surfaces the drop); `reason` decides whether a retry makes sense.
    Lost {
        was_connected: bool,
        reason: DisconnectReason,
    },
}

/// Run the client side. Finds the server and connects, then (when enabled) reconnects after
/// any recoverable drop until `cancel` fires.
///
/// Where to connect is not fixed. Each attempt tries every address that might lead to the
/// server at once, and keeps the first that answers:
///   1. the address that worked last time (forgotten when the network changes),
///   2. the configured `server_host`, if set,
///   3. every address the server has beaconed on any network it is on, heard recently.
///
/// So the same setup works over Ethernet, Wi-Fi, a shared router or a phone hotspot, and a
/// change of network is followed without editing anything.
///
/// The client also watches its own network. When that changes it drops a live session at once
/// and reconnects straight away, rather than retrying a dead path until a timeout. With no
/// network at all it reports that, instead of reporting that the server was not found.
pub async fn run_client(
    cfg: Config,
    params: LinkParams,
    cancel: CancellationToken,
    mut ch: LinkChannels,
    health: Arc<LinkHealth>,
) {
    crate::trust::load(&cfg.device_id, &cfg.paired);
    // Back to the computer this one was last with (R45), if it is still paired.
    let chosen = Some(cfg.connect_to.as_str()).filter(|c| crate::trust::get(&cfg.device_id, c).is_some());
    crate::trust::set_choice(&cfg.device_id, chosen);
    let found = beacon::Found::new();
    let (epoch_tx, epoch_rx) = watch::channel(0u64);
    tokio::spawn(beacon::run_listener(
        cfg.device_id.clone(),
        cfg.discovery_port,
        found.clone(),
        cancel.child_token(),
    ));
    tokio::spawn(beacon::watch_network(epoch_tx, cancel.child_token()));

    let mut backoff = params.reconnect_initial;
    // Whether we've already told the UI we're down, so repeated failed retries
    // while the server stays offline don't spam Disconnected every backoff tick.
    let mut announced_down = false;
    let mut last_good: Option<SocketAddr> = None;
    // When the "still looking" line was last logged. While no server answers, one line a
    // minute says what this machine has, what it heard and what it tried.
    let mut looking_logged: Option<std::time::Instant> = None;

    loop {
        if cancel.is_cancelled() {
            return;
        }

        let no_network = discover::local_ipv4s().is_empty();
        let mut targets: Vec<SocketAddr> = last_good.into_iter().collect();
        if !cfg.server_host.trim().is_empty() {
            let host = format!("{}:{}", cfg.server_host.trim(), cfg.server_port);
            if let Ok(addrs) = tokio::net::lookup_host(host).await {
                for a in addrs {
                    if !targets.contains(&a) {
                        targets.push(a);
                    }
                }
            }
        }
        let only = crate::trust::choice(&cfg.device_id);
        for a in found.candidates_for(std::time::Instant::now(), only.as_deref()) {
            if !targets.contains(&a) {
                targets.push(a);
            }
        }

        let outcome = connect_once(
            &targets,
            &cfg,
            params,
            &cancel,
            &mut ch,
            &health,
            Some(epoch_rx.clone()),
            &mut last_good,
        )
        .await;

        match outcome {
            AttemptOutcome::Cancelled | AttemptOutcome::Fatal => return,
            AttemptOutcome::Lost { was_connected, reason } => {
                let now = std::time::Instant::now();
                if was_connected {
                    looking_logged = None;
                } else if looking_logged.map_or(true, |t| now.saturating_duration_since(t) >= LOOKING_LOG_EVERY) {
                    looking_logged = Some(now);
                    tracing::info!(
                        "still looking for the server: {reason:?}. This machine: {}. Heard: {}.",
                        beacon::describe_locals(&discover::local_ipv4s(), &discover::link_local_v6_ifaces()),
                        found.describe(now),
                    );
                }
                // With no network at all, a failed attempt is a fact about this machine, not
                // about the server, so say so.
                let reason = if no_network && !was_connected {
                    DisconnectReason::Io("no network connection".into())
                } else {
                    reason
                };
                let recoverable = reason.is_recoverable();
                if matches!(reason, DisconnectReason::NetworkChanged) {
                    // The path we had is gone or no longer best. Forget it, so the next race
                    // decides afresh, and reconnect at once.
                    last_good = None;
                    backoff = params.reconnect_initial;
                    let _ = ch.events.send(LinkEvent::Disconnected(reason.clone()));
                    announced_down = true;
                } else if was_connected {
                    // A live session ended: always surface it, and treat the next
                    // retry as a fresh start (fast backoff, re-announce if it fails).
                    let _ = ch.events.send(LinkEvent::Disconnected(reason));
                    backoff = params.reconnect_initial;
                    announced_down = true;
                } else if !announced_down {
                    // First failure of this outage: surface it once, then go quiet.
                    let _ = ch.events.send(LinkEvent::Disconnected(reason));
                    announced_down = true;
                }

                if !cfg.reconnect || !recoverable {
                    return;
                }
            }
        }

        // Wait before the next attempt. Wake on cancel, or at once if the network changes. With no
        // network, poll at a steady rate instead of backing off.
        // Refused over the PIN: someone is about to type it on the server, so look again
        // soon instead of backing off.
        let wait = if no_network {
            NO_NETWORK_POLL
        } else if crate::pin::awaiting() {
            PIN_POLL
        } else {
            backoff
        };
        let mut epoch = epoch_rx.clone();
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = epoch.changed() => { backoff = params.reconnect_initial; }
            _ = tokio::time::sleep(wait) => {}
        }
        if !no_network {
            backoff = (backoff * 2).min(params.reconnect_max);
        }
    }
}

/// One client connection attempt: connect → TLS → handshake → pump. Emits the
/// progress events (Connecting/Connected, plus AuthFailed/Error for terminal
/// failures); the caller decides whether to retry and emits the final Disconnected.
#[allow(clippy::too_many_arguments)] // the attempt needs all of these; a struct would only rename them
async fn connect_once(
    targets: &[SocketAddr],
    cfg: &Config,
    params: LinkParams,
    cancel: &CancellationToken,
    ch: &mut LinkChannels,
    health: &Arc<LinkHealth>,
    net: Option<watch::Receiver<u64>>,
    last_good: &mut Option<SocketAddr>,
) -> AttemptOutcome {
    if targets.is_empty() {
        return AttemptOutcome::Lost {
            was_connected: false,
            reason: DisconnectReason::Io("no server address known yet".into()),
        };
    }
    let listing = targets.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
    let _ = ch.events.send(LinkEvent::Connecting(listing.clone()));

    // IPv4 first. IPv6 link-local only when no IPv4 address answers: it is the fallback for
    // a network that gave no address, not a rival.
    let (v4, v6): (Vec<SocketAddr>, Vec<SocketAddr>) = targets.iter().partition(|a| a.is_ipv4());
    let mut raced = None;
    for group in [v4, v6] {
        if group.is_empty() {
            continue;
        }
        raced = tokio::select! {
            _ = cancel.cancelled() => return AttemptOutcome::Cancelled,
            r = race_connect(&group, CONNECT_TRY, cfg.network_preference) => r,
        };
        if raced.is_some() {
            break;
        }
    }
    // Connection refused or unreachable on every candidate: recoverable, the server may not
    // be up yet, or may be on a network this machine is not on at the moment.
    let Some((addr, tcp)) = raced else {
        return AttemptOutcome::Lost {
            was_connected: false,
            reason: DisconnectReason::Io(format!("no server answered at {listing}")),
        };
    };
    *last_good = Some(addr);
    health.set_peer(addr);
    tracing::info!(
        "link to {addr} goes out through {} (candidates tried: {listing}; preference: {})",
        local_path(tcp.local_addr().ok()),
        cfg.network_preference
    );
    let on_cable = local_kind(tcp.local_addr().ok(), addr) == Some(discover::NetKind::Cable);
    enable_keepalive(&tcp);
    crate::qos::prioritise_input(&tcp);

    let stream = if cfg.tls_enabled {
        let connector = match tls::client_connector() {
            Ok(c) => c,
            // Local TLS provider setup failing won't fix itself: terminal.
            Err(e) => {
                let _ = ch.events.send(LinkEvent::Error(format!("tls setup: {e}")));
                return AttemptOutcome::Fatal;
            }
        };
        match timeout(params.handshake_timeout, connector.connect(tls::dummy_server_name(), tcp)).await {
            Ok(Ok(s)) => LinkStream::ClientTls(Box::new(s)),
            // A TLS error mid-restart (TCP accepted before TLS is ready) is transient;
            // treat as recoverable so reconnect keeps trying.
            Ok(Err(e)) => {
                return AttemptOutcome::Lost {
                    was_connected: false,
                    reason: DisconnectReason::Io(format!("tls connect: {e}")),
                }
            }
            Err(_) => {
                return AttemptOutcome::Lost {
                    was_connected: false,
                    reason: DisconnectReason::TimedOut,
                }
            }
        }
    } else {
        LinkStream::Plain(tcp)
    };

    let mut framed = Framed::new(stream, MessageCodec::new());

    let server_name = tokio::select! {
        _ = cancel.cancelled() => return AttemptOutcome::Cancelled,
        r = timeout(params.handshake_timeout, client_handshake(&mut framed, cfg, Lane::Input)) => r,
    };
    let (server_name, server_w, server_h, server_device) = match server_name {
        Ok(Ok(v)) => {
            // Only once the server has proved the pairing key (B71): the log shipper
            // falls back to this address, and a host that merely answered TCP must not
            // receive the log.
            note_peer_ip(addr.ip());
            v
        }
        // Refused (a different secret, an older version, a server that can't prove the
        // secret). Keep retrying with the usual backoff: the fix happens on the other
        // machine, and the link should come up by itself once it does. Giving up would
        // also let a fake server switch the client off for good.
        Ok(Err(reason)) => {
            let _ = ch.events.send(LinkEvent::AuthFailed(reason.clone()));
            return AttemptOutcome::Lost {
                was_connected: false,
                reason: DisconnectReason::Io(format!("refused: {reason}")),
            };
        }
        // A handshake timeout can be a transient/overloaded server: recoverable.
        Err(_) => {
            return AttemptOutcome::Lost {
                was_connected: false,
                reason: DisconnectReason::TimedOut,
            }
        }
    };

    let _ = ch.events.send(LinkEvent::Connected {
        peer: server_name,
        width: server_w,
        height: server_h,
        login_screen: false,
    });
    let motion = match framed.get_ref().motion_key() {
        Some(key) => Motion::Client { key, server: addr, on_cable, live: None },
        None => Motion::Off,
    };
    // Removing the pairing here ends the session at once, not at the next reconnect: a
    // computer the user has just removed must stop driving this one (audit, deferred lead).
    let session = cancel.child_token();
    let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (session, revoked, own) = (session.clone(), revoked.clone(), cfg.device_id.clone());
        tokio::spawn(async move {
            let mut every = interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = session.cancelled() => break,
                    _ = every.tick() => {
                        if crate::trust::get(&own, &server_device).is_none() {
                            tracing::info!("the pairing with this server was removed; disconnecting");
                            revoked.store(true, std::sync::atomic::Ordering::Relaxed);
                            session.cancel();
                            break;
                        }
                    }
                }
            }
        });
    }
    let reason = pump(framed, params, &session, ch, health, net, motion).await;
    session.cancel(); // stops the watcher
    if revoked.load(std::sync::atomic::Ordering::Relaxed) && !cancel.is_cancelled() {
        return AttemptOutcome::Lost {
            was_connected: true,
            reason: DisconnectReason::Io("the pairing was removed".into()),
        };
    }
    match reason {
        DisconnectReason::Cancelled => AttemptOutcome::Cancelled,
        reason => AttemptOutcome::Lost {
            was_connected: true,
            reason,
        },
    }
}

/// A second listener that only serves the bulk lane.
///
/// Only used when the owner has set a bulk port different from the input one. The
/// default (`bulk_port = 0`) shares the input listener and never calls this, so the
/// common deployment still needs exactly one port reachable.
pub async fn run_bulk_listener(
    cfg: Config,
    port: u16,
    params: LinkParams,
    cancel: CancellationToken,
    bulk: BulkServer,
) {
    let bind = format!("{}:{}", cfg.listen_host, port);
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            // Not fatal: the input link is untouched and the app simply has no bulk
            // lane. Saying so is the whole value here — a silent failure would look
            // exactly like a lane that works but never gets used.
            tracing::warn!("bulk listener could not bind {bind}: {e}; the bulk lane is off");
            return;
        }
    };
    tracing::info!("bulk lane listening on {bind}");

    let acceptor = if cfg.tls_enabled {
        match tls::server_acceptor() {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!("bulk listener tls setup failed: {e}; the bulk lane is off");
                return;
            }
        }
    } else {
        None
    };

    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => return,
            r = listener.accept() => r,
        };
        let Ok((tcp, _addr)) = accepted else { continue };
        enable_keepalive(&tcp);

        let stream = match &acceptor {
            Some(acc) => match timeout(params.handshake_timeout, acc.accept(tcp)).await {
                Ok(Ok(s)) => LinkStream::ServerTls(Box::new(s)),
                _ => continue,
            },
            None => LinkStream::Plain(tcp),
        };

        let mut framed = Framed::new(stream, MessageCodec::new());
        let shook = tokio::select! {
            _ = cancel.cancelled() => return,
            r = timeout(params.handshake_timeout, server_handshake(&mut framed, &cfg)) => r,
        };
        match shook {
            // The same pairing keys guard this port as the input one; a connection
            // that authenticates but asks for the input lane is refused here, because
            // input has exactly one home and it is not this listener.
            Ok(Ok(ClientHello { lane: Lane::Bulk, device, name, .. })) => {
                // Only the computer that holds the input link (R44).
                if crate::trust::input_peer(&cfg.device_id).as_deref() == Some(device.as_str()) {
                    bulk.take_server_connection(framed, &cancel)
                } else {
                    tracing::warn!(peer = %name, "bulk lane from a computer that does not hold the input link; refused")
                }
            }
            Ok(Ok(ClientHello { lane: Lane::Input, name, .. })) => {
                tracing::warn!(peer = %name, "input connection offered to the bulk port; refused")
            }
            Ok(Err(reason)) => tracing::warn!("bulk handshake rejected: {reason}"),
            Err(_) => tracing::warn!("bulk handshake timed out"),
        }
    }
}

/// Connect, TLS, and handshake for a *secondary* lane, returning the live connection.
///
/// Separate from [`connect_once`] on purpose: that function owns the input link's
/// reconnect policy and its UI events, and neither belongs to a lane whose failure the
/// user should never have to think about. Errors come back as a string for the caller
/// to log and retry at its own pace.
/// Connects to `to`, or to the configured `server_host` on `port` when `to` is None.
pub async fn connect_and_handshake(
    cfg: &Config,
    to: Option<SocketAddr>,
    port: u16,
    lane: Lane,
    handshake_timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Framed<LinkStream, MessageCodec>, String> {
    let addr = match to {
        Some(a) => a.to_string(),
        None => format!("{}:{}", cfg.server_host, port),
    };
    let tcp = tokio::select! {
        _ = cancel.cancelled() => return Err("cancelled".into()),
        r = timeout(handshake_timeout, async {
            match to {
                Some(a) => TcpStream::connect(a).await,
                None => TcpStream::connect(&addr).await,
            }
        }) => r,
    };
    let tcp = match tcp {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("connect {addr}: {e}")),
        Err(_) => return Err(format!("connect {addr}: timed out")),
    };
    enable_keepalive(&tcp);

    let stream = if cfg.tls_enabled {
        let connector = tls::client_connector().map_err(|e| format!("tls setup: {e}"))?;
        match timeout(handshake_timeout, connector.connect(tls::dummy_server_name(), tcp)).await {
            Ok(Ok(s)) => LinkStream::ClientTls(Box::new(s)),
            Ok(Err(e)) => return Err(format!("tls connect: {e}")),
            Err(_) => return Err("tls connect timed out".into()),
        }
    } else {
        LinkStream::Plain(tcp)
    };

    let mut framed = Framed::new(stream, MessageCodec::new());
    let shook = tokio::select! {
        _ = cancel.cancelled() => return Err("cancelled".into()),
        r = timeout(handshake_timeout, client_handshake(&mut framed, cfg, lane)) => r,
    };
    match shook {
        Ok(Ok(_)) => Ok(framed),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("handshake timed out".into()),
    }
}

// ----------------------------------------------------------------------------
// Handshake (async) — mirrors auth.py's HMAC challenge/response.
// ----------------------------------------------------------------------------

/// Who a client said it is, once the handshake has proved it.
#[derive(Debug, Clone)]
struct ClientHello {
    name: String,
    /// Its device ID: the pairing it proved it holds (R44).
    device: String,
    width: i32,
    height: i32,
    lane: Lane,
    login_screen: bool,
}

async fn server_handshake(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
) -> Result<ClientHello, String> {
    let hello = framed.next().await;
    let (name, width, height, lane, login_screen, version, client_nonce, device) = match hello {
        Some(Ok(Message::Hello { name, width, height, lane, login_screen, version, nonce, device })) => {
            (name, width, height, lane, login_screen, version, nonce, device)
        }
        Some(Ok(Message::PairStart { name, msg, device })) => {
            return Err(server_pair(framed, cfg, name, device, msg).await);
        }
        Some(Ok(other)) => return Err(format!("expected hello, got {other:?}")),
        Some(Err(e)) => return Err(format!("hello decode: {e}")),
        None => return Err("peer closed before hello".into()),
    };
    // An older peer can't prove anything about this pairing: refuse it rather than fall
    // back, or anyone could downgrade a connection to a weaker check (B34, R44).
    let (client_nonce, device) = match (client_nonce, device) {
        (Some(n), Some(d)) if version >= PROTOCOL_VERSION && !d.is_empty() => (n, d),
        _ => {
            let _ = framed.send(refusal("old_version")).await;
            return Err(format!(
                "{name} runs an older mouse-rs (protocol {version}); update both machines"
            ));
        }
    };
    // Only a computer this one paired with gets any further, and only with the key of
    // that pairing (R44). Nothing is proved to a stranger.
    let Some(pairing) = crate::trust::get(&cfg.device_id, &device) else {
        let _ = framed.send(refusal("not_paired")).await;
        return Err(format!("{name} is not paired with this computer; refused"));
    };
    let ends = auth::Ends { client: &device, server: &cfg.device_id };

    let binding = framed.get_ref().auth_binding()?;
    let nonce = auth::generate_nonce_hex();
    let proof = auth::proof_hex(&pairing.key, Side::Server, ends, &binding, &client_nonce, &nonce)
        .map_err(|e| format!("bad nonce from {name}: {e}"))?;
    // The client's PIN, if someone has typed it here this run. The client checks it.
    let pin_proof = crate::pin::typed_for(&device)
        .map(|pin| crate::pin::proof_hex(&pairing.key, &pin, &binding, &client_nonce, &nonce));
    framed
        .send(Message::AuthChallenge {
            nonce: nonce.clone(),
            device: Some(cfg.device_id.clone()),
            proof: Some(proof),
            pin_proof,
        })
        .await
        .map_err(|e| format!("send challenge: {e}"))?;

    let resp = framed.next().await;
    let hmac = match resp {
        Some(Ok(Message::AuthResponse { hmac })) => hmac,
        // The client refused us, most often over its PIN: ask our user for it. Only a
        // refusal signed with the pairing key may change what the panel asks (B65): this
        // is before the client has proved anything, so an unsigned one could come from any
        // computer that knows a paired device ID.
        Some(Ok(Message::AuthOk { ok: false, reason, proof, .. })) => {
            let reason = reason.unwrap_or_else(|| "refused".into());
            let signed = proof.is_some_and(|p| {
                auth::verify_refusal(&pairing.key, ends, &binding, &client_nonce, &nonce, &reason, &p)
            });
            if !signed {
                return Err(format!("{name} refused: {reason} (unsigned, so the PIN prompt is left alone)"));
            }
            return Err(if crate::pin::note_refusal(&device, &name, &reason) {
                format!("{reason} from {name}")
            } else {
                format!("{name} refused: {reason}")
            });
        }
        Some(Ok(other)) => return Err(format!("expected auth response, got {other:?}")),
        Some(Err(e)) => return Err(format!("auth decode: {e}")),
        None => return Err("peer closed before auth response".into()),
    };

    if !auth::verify_proof(&pairing.key, Side::Client, ends, &binding, &client_nonce, &nonce, &hmac) {
        let _ = framed.send(refusal("bad_secret")).await;
        return Err(format!("bad proof from {name}"));
    }
    crate::pin::accepted(&device);
    // Proved, so its name is its own: a computer renamed since pairing shows as it is now (B61).
    crate::trust::note_name(&cfg.device_id, &device, &name);

    framed
        .send(Message::AuthOk {
            ok: true,
            server: Some(cfg.name.clone()),
            width: Some(cfg.screen_width),
            height: Some(cfg.screen_height),
            reason: None,
            proof: None,
        })
        .await
        .map_err(|e| format!("send auth ok: {e}"))?;

    Ok(ClientHello { name, device, width, height, lane, login_screen })
}

/// Serve one pairing attempt (see `pairing`). Always ends the connection: the client
/// reconnects with the secret it was given. Returns the line for the log, starting
/// with "pairing:" so the accept loop knows this was not a refused connection.
async fn server_pair(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
    client: String,
    client_device: String,
    client_msg: String,
) -> String {
    let done = |reason: &str| Message::PairDone { ok: false, server: None, reason: Some(reason.into()) };
    if client_device.is_empty() {
        let _ = framed.send(done("old_version")).await;
        return format!("pairing: {client} runs an older mouse-rs; update it");
    }
    let Some(code) = crate::pairing::code_for_attempt() else {
        let _ = framed.send(done("no_code")).await;
        return format!("pairing: {client} tried to pair, but no code is showing");
    };
    let binding = match framed.get_ref().auth_binding() {
        Ok(b) => b,
        Err(e) => return format!("pairing: {e}"),
    };
    let start = crate::pairing::start_server(&code);
    let server_msg = start.msg.clone();
    let us = crate::pairing::Party { id: &cfg.device_id, name: &cfg.name };
    let them = crate::pairing::Party { id: &client_device, name: &client };
    let session = match start.finish(&client_msg, &binding, &client_msg, &server_msg, them, us) {
        Ok(s) => s,
        Err(e) => {
            crate::pairing::note_failure();
            return format!("pairing: {client}: {e}");
        }
    };
    let reply = Message::PairReply {
        msg: server_msg,
        confirm: session.server_confirm(),
        device: cfg.device_id.clone(),
        name: cfg.name.clone(),
    };
    if framed.send(reply).await.is_err() {
        return format!("pairing: {client} went away");
    }
    let confirmed = match framed.next().await {
        Some(Ok(Message::PairConfirm { confirm })) => session.check(&session.client_confirm(), &confirm),
        // A client with the wrong code sees our confirmation fail and hangs up.
        _ => false,
    };
    if !confirmed {
        crate::pairing::note_failure();
        let _ = framed.send(done("wrong_code")).await;
        return format!("pairing: {client} typed a wrong code");
    }
    // Both sides now hold the same new key, and know exactly whom they share it with.
    crate::trust::add(
        &cfg.device_id,
        PairedDevice { id: client_device.clone(), name: client.clone(), key: session.pair_key(), paired_at: now_secs() },
    );
    let _ = framed.send(Message::PairDone { ok: true, server: Some(cfg.name.clone()), reason: None }).await;
    crate::pairing::note_success(&client);
    format!("pairing: paired with {client} (device {})", short_id(&client_device))
}

/// Seconds since 1970, for when a pairing was made.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The start of a device ID, enough to tell two apart in a log.
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Pair with the server at `to` using the code it shows. On success the new pairing is
/// live in `trust` (and waiting there for the panel to save), and is returned too.
pub async fn pair_with_server(cfg: &Config, to: &str, code: &str, wait: Duration) -> Result<PairedDevice, String> {
    // "1.2.3.4", "1.2.3.4:24800", "fe80::1%3", "[fe80::1%3]:24800" or "name[:port]".
    let to = if to.parse::<SocketAddr>().is_ok() {
        to.to_owned()
    } else if let Ok(ip) = to.parse::<std::net::IpAddr>() {
        SocketAddr::new(ip, cfg.server_port).to_string()
    } else if to.contains(':') {
        to.to_owned()
    } else {
        format!("{to}:{}", cfg.server_port)
    };
    let tcp = match timeout(wait, TcpStream::connect(&to)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("could not reach {to}: {e}")),
        Err(_) => return Err(format!("could not reach {to}: timed out")),
    };
    let stream = if cfg.tls_enabled {
        let connector = tls::client_connector().map_err(|e| format!("tls setup: {e}"))?;
        match timeout(wait, connector.connect(tls::dummy_server_name(), tcp)).await {
            Ok(Ok(s)) => LinkStream::ClientTls(Box::new(s)),
            Ok(Err(e)) => return Err(format!("tls connect: {e}")),
            Err(_) => return Err("tls connect timed out".into()),
        }
    } else {
        LinkStream::Plain(tcp)
    };
    let mut framed = Framed::new(stream, MessageCodec::new());
    match timeout(wait, client_pair(&mut framed, cfg, code)).await {
        Ok(r) => r,
        Err(_) => Err("the server did not answer in time".into()),
    }
}

async fn client_pair(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
    code: &str,
) -> Result<PairedDevice, String> {
    let binding = framed.get_ref().auth_binding()?;
    let start = crate::pairing::start_client(code);
    let client_msg = start.msg.clone();
    framed
        .send(Message::PairStart { name: cfg.name.clone(), msg: client_msg.clone(), device: cfg.device_id.clone() })
        .await
        .map_err(|e| format!("send: {e}"))?;
    let (server_msg, confirm, server_device, server_name) = match framed.next().await {
        Some(Ok(Message::PairReply { msg, confirm, device, name })) if !device.is_empty() => (msg, confirm, device, name),
        Some(Ok(Message::PairReply { .. })) => return Err("old_version".into()),
        Some(Ok(Message::PairDone { reason, .. })) => return Err(reason.unwrap_or_else(|| "refused".into())),
        Some(Ok(Message::AuthOk { ok: false, reason, .. })) => return Err(reason.unwrap_or_else(|| "refused".into())),
        Some(Ok(_)) | Some(Err(_)) => return Err("old_version".into()),
        None => return Err("old_version".into()),
    };
    if server_device == cfg.device_id {
        return Err("that is this computer".into());
    }
    let us = crate::pairing::Party { id: &cfg.device_id, name: &cfg.name };
    let them = crate::pairing::Party { id: &server_device, name: &server_name };
    // The confirmation covers both IDs and names: if it checks out, the computer showing
    // this code is `server_device`, and nobody changed that on the way.
    let session = start.finish(&server_msg, &binding, &client_msg, &server_msg, us, them)?;
    if !session.check(&session.server_confirm(), &confirm) {
        // Either the code is wrong or someone is in the middle; nothing is sent either way.
        return Err("wrong_code".into());
    }
    framed
        .send(Message::PairConfirm { confirm: session.client_confirm() })
        .await
        .map_err(|e| format!("send: {e}"))?;
    match framed.next().await {
        Some(Ok(Message::PairDone { ok: true, .. })) => {
            let dev = PairedDevice { id: server_device, name: server_name, key: session.pair_key(), paired_at: now_secs() };
            crate::trust::add(&cfg.device_id, dev.clone());
            Ok(dev)
        }
        Some(Ok(Message::PairDone { reason, .. })) => Err(reason.unwrap_or_else(|| "refused".into())),
        _ => Err("the server closed the connection".into()),
    }
}

fn refusal(reason: &str) -> Message {
    Message::AuthOk { ok: false, server: None, width: None, height: None, reason: Some(reason.into()), proof: None }
}

async fn client_handshake(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
    lane: Lane,
) -> Result<(String, i32, i32, String), String> {
    let binding = framed.get_ref().auth_binding()?;
    let client_nonce = auth::generate_nonce_hex();
    framed
        .send(Message::Hello {
            name: cfg.name.clone(),
            width: cfg.screen_width,
            height: cfg.screen_height,
            version: PROTOCOL_VERSION,
            lane,
            login_screen: cfg.at_login_screen,
            nonce: Some(client_nonce.clone()),
            device: Some(cfg.device_id.clone()),
        })
        .await
        .map_err(|e| format!("send hello: {e}"))?;

    let (nonce, proof, pin_proof, server_device) = match framed.next().await {
        Some(Ok(Message::AuthChallenge { nonce, proof, pin_proof, device })) => (nonce, proof, pin_proof, device),
        Some(Ok(Message::AuthOk { ok: false, reason, .. })) => {
            return Err(reason.unwrap_or_else(|| "rejected".into()))
        }
        Some(Ok(other)) => return Err(format!("expected challenge, got {other:?}")),
        Some(Err(e)) => return Err(format!("challenge decode: {e}")),
        None => return Err("peer closed before challenge".into()),
    };

    // The server proves itself first; nothing about our key is sent to a server that
    // can't (B34). And it must be a computer we paired with (R44): a server we are not
    // paired with gets nothing at all, even if it answered first.
    let (Some(proof), Some(server_device)) = (proof, server_device) else {
        return Err("old_version".into());
    };
    let Some(pairing) = crate::trust::get(&cfg.device_id, &server_device) else {
        return Err("not_paired_server".into());
    };
    // Paired, but not the computer this one is to be with right now (R45): go no further,
    // so the one we were last with, or the one the user chose, is the one we reach.
    if crate::trust::choice(&cfg.device_id).is_some_and(|c| c != server_device) {
        return Err("not_chosen_server".into());
    }
    let ends = auth::Ends { client: &cfg.device_id, server: &server_device };
    if !auth::verify_proof(&pairing.key, Side::Server, ends, &binding, &client_nonce, &nonce, &proof) {
        return Err("server_not_verified".into());
    }

    let hmac = auth::proof_hex(&pairing.key, Side::Client, ends, &binding, &client_nonce, &nonce)
        .map_err(|e| format!("hmac: {e}"))?;
    // The connection PIN (R35): this machine is the one being driven, so it decides who
    // may drive it. The server must show the PIN, typed there by its user; the check and
    // the try limits are here. This applies at the login screen too: the PIN is typed on
    // the server, which has a keyboard.
    let pin_on = if cfg.at_login_screen { cfg.login_pin_enabled } else { cfg.pin_enabled };
    if pin_on && !cfg.connection_pin.is_empty() {
        use crate::pin::{check_attempt, locked_for, refusal_reason, verify, Verdict, REASON_REQUIRED};
        let verdict = match (locked_for(), pin_proof) {
            (Some(left), _) => Some(Verdict::Locked(left)),
            // No PIN typed there yet: not a wrong guess, nothing to count.
            (None, None) => None,
            (None, Some(p)) => Some(check_attempt(verify(&pairing.key, &cfg.connection_pin, &binding, &client_nonce, &nonce, &p))),
        };
        let reason = match verdict {
            Some(Verdict::Accept) => None,
            Some(v) => Some(refusal_reason(&v)),
            None => Some(REASON_REQUIRED.to_owned()),
        };
        if let Some(reason) = reason {
            crate::pin::set_awaiting(true);
            // Signed, so the server can trust it enough to ask its user for the PIN (B65).
            let mut msg = refusal(&reason);
            if let Message::AuthOk { proof, .. } = &mut msg {
                *proof = auth::refusal_hex(&pairing.key, ends, &binding, &client_nonce, &nonce, &reason).ok();
            }
            let _ = framed.send(msg).await;
            return Err(reason);
        }
    }
    crate::pin::set_awaiting(false);
    framed
        .send(Message::AuthResponse { hmac })
        .await
        .map_err(|e| format!("send auth response: {e}"))?;

    match framed.next().await {
        Some(Ok(Message::AuthOk { ok: true, server, width, height, .. })) => {
            if lane == Lane::Input {
                crate::trust::connected_with(&cfg.device_id, &server_device);
                if let Some(n) = server.as_deref() {
                    crate::trust::note_name(&cfg.device_id, &server_device, n);
                }
            }
            Ok((server.unwrap_or_else(|| "server".into()), width.unwrap_or(0), height.unwrap_or(0), server_device))
        }
        Some(Ok(Message::AuthOk { ok: false, reason, .. })) => {
            Err(reason.unwrap_or_else(|| "rejected".into()))
        }
        Some(Ok(other)) => Err(format!("expected auth ok, got {other:?}")),
        Some(Err(e)) => Err(format!("auth ok decode: {e}")),
        None => Err("peer closed before auth ok".into()),
    }
}

// ----------------------------------------------------------------------------
// The pump: the steady-state loop. Heartbeat + liveness + cancellation.
// ----------------------------------------------------------------------------

/// Resolves when the network watch reports a change. Pends forever when there is no watch.
/// Which of this machine's interfaces a connection uses, e.g. `eth0 (192.168.137.2)`, so a
/// log shows whether the link went over the cable or Wi-Fi after a network change.
fn local_path(local: Option<SocketAddr>) -> String {
    let Some(local) = local else {
        return "an unknown interface".into();
    };
    let iface = discover::local_ipv4s()
        .into_iter()
        .find(|a| std::net::IpAddr::V4(a.ip) == local.ip())
        .map(|a| a.iface);
    match iface {
        Some(name) => format!("{name} ({})", local.ip()),
        None => local.ip().to_string(),
    }
}

/// The kind of network a connection to `remote` from `local` is on. An IPv6 link-local
/// peer names its interface itself (the `%n`); IPv4 is matched by the local address.
fn local_kind(local: Option<SocketAddr>, remote: SocketAddr) -> Option<discover::NetKind> {
    if let SocketAddr::V6(v6) = remote {
        if v6.scope_id() != 0 {
            return discover::iface_name(v6.scope_id()).map(|n| discover::iface_kind(&n));
        }
    }
    let local = local?;
    discover::local_ipv4s()
        .into_iter()
        .find(|a| std::net::IpAddr::V4(a.ip) == local.ip())
        .map(|a| discover::iface_kind(&a.iface))
}

async fn network_moved(net: &mut Option<watch::Receiver<u64>>) {
    match net {
        Some(r) => {
            if r.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// How often the client logs what it is trying while no server answers.
const LOOKING_LOG_EVERY: Duration = Duration::from_secs(60);

/// While motion is under way, how often to check for a gap to hide.
const CONCEAL_TICK: Duration = Duration::from_millis(2);

/// How long one candidate address gets to accept the TCP connection.
const CONNECT_TRY: Duration = Duration::from_secs(3);

/// How often a client with no network re-checks for one.
const NO_NETWORK_POLL: Duration = Duration::from_secs(2);

/// How often a client that refused a server over the PIN tries again, so it connects
/// within a couple of seconds of the PIN being typed on the server.
const PIN_POLL: Duration = Duration::from_secs(2);

/// Connect to every candidate at once and keep the first that accepts. Racing, rather than
/// trying in turn, means a dead address costs nothing. It also picks the faster path: when the
/// server is reachable over both Ethernet and Wi-Fi, the one that answers first has less delay.
/// The address of the peer this machine is linked to now (or was last), for the development
/// log shipper: it falls back to the peer when the cable's collector address is unreachable.
static PEER_IP: std::sync::Mutex<Option<std::net::IpAddr>> = std::sync::Mutex::new(None);

pub fn last_peer_ip() -> Option<std::net::IpAddr> {
    PEER_IP.lock().ok().and_then(|p| *p)
}

fn note_peer_ip(ip: std::net::IpAddr) {
    if let Ok(mut p) = PEER_IP.lock() {
        *p = Some(ip);
    }
}

/// An IPv6-only listener on `port`, beside the IPv4 one.
fn listen_v6(port: u16) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_only_v6(true)?;
    #[cfg(unix)]
    sock.set_reuse_address(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
    sock.listen(128)?;
    TcpListener::from_std(sock.into())
}

/// An IPv6-only UDP socket on `port`, beside the IPv4 one on the same port.
fn udp_v6_only(port: u16) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_only_v6(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
    tokio::net::UdpSocket::from_std(sock.into())
}

async fn accept_any(
    v4: &TcpListener,
    v6: Option<&TcpListener>,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    match v6 {
        None => v4.accept().await,
        Some(v6) => tokio::select! {
            r = v4.accept() => r,
            r = v6.accept() => r,
        },
    }
}

/// Once a less preferred network has answered, how long a preferred one still pending gets
/// to answer too. Short: on a working cable the answer takes about a millisecond.
const PREFER_WAIT: Duration = Duration::from_millis(400);

/// How much `pref` wants `addr`, lower first: the network it is reached on directly (its
/// subnet is one of this machine's), then any network, then through a router or tunnel.
fn path_rank(addr: &SocketAddr, pref: crate::config::NetworkPreference) -> u8 {
    use crate::config::NetworkPreference as P;
    use discover::NetKind as K;
    let kind = match addr {
        SocketAddr::V4(a) => discover::on_link_kind(*a.ip()),
        SocketAddr::V6(_) => None,
    };
    match (pref, kind) {
        (P::Fastest, _) => 0,
        (P::Cable, Some(K::Cable)) | (P::Wifi, Some(K::Wifi)) => 0,
        (_, Some(K::Cable | K::Wifi)) => 1,
        (_, Some(K::Other)) => 2,
        (_, None) => 3,
    }
}

/// Connect to every candidate at once and keep the best that accepts: the preferred
/// network's if it answers within [`PREFER_WAIT`] of the first answer, else the first.
/// Racing means a dead address costs nothing; ranking means a Wi-Fi address that happens
/// to answer first no longer wins over the cable (27 Oct 2026: after a network change the
/// laptop's Wi-Fi address won the race and the link was counted as Wi-Fi for a minute).
async fn race_connect(
    targets: &[SocketAddr],
    per: Duration,
    pref: crate::config::NetworkPreference,
) -> Option<(SocketAddr, TcpStream)> {
    let ranks: Vec<u8> = targets.iter().map(|a| path_rank(a, pref)).collect();
    let mut pending = vec![true; targets.len()];
    let mut attempts: FuturesUnordered<_> = targets
        .iter()
        .enumerate()
        .map(|(i, a)| async move { (i, timeout(per, TcpStream::connect(a)).await) })
        .collect();
    let mut best: Option<(u8, SocketAddr, TcpStream)> = None;
    let mut deadline: Option<Instant> = None;
    loop {
        if let Some((have, ..)) = &best {
            let better_pending = (0..targets.len()).any(|i| pending[i] && ranks[i] < *have);
            if !better_pending {
                break;
            }
        }
        let next = match deadline {
            Some(d) => tokio::select! {
                r = attempts.next() => r,
                _ = tokio::time::sleep_until(d) => break,
            },
            None => attempts.next().await,
        };
        let Some((i, result)) = next else { break };
        pending[i] = false;
        if let Ok(Ok(stream)) = result {
            if best.as_ref().map_or(true, |(have, ..)| ranks[i] < *have) {
                best = Some((ranks[i], targets[i], stream));
                deadline.get_or_insert_with(|| Instant::now() + PREFER_WAIT);
            }
        }
    }
    best.map(|(_, addr, stream)| (addr, stream))
}

/// Pointer motion over UDP for this session (see `motion_udp`).
enum Motion {
    /// No UDP: plain link, or the socket couldn't be opened. Motion goes over TCP.
    Off,
    Server(ServerMotion),
    Client {
        key: crate::motion_udp::Key,
        server: SocketAddr,
        /// The link goes over the cable: show each move as it arrives, no smoothing.
        on_cable: bool,
        /// Set once the server's offer arrives.
        live: Option<ClientMotion>,
    },
}

impl Motion {
    fn socket(&self) -> Option<Arc<tokio::net::UdpSocket>> {
        match self {
            Motion::Server(m) => Some(m.socket()),
            Motion::Client { live: Some(m), .. } => Some(m.socket()),
            _ => None,
        }
    }
}

async fn pump(
    framed: Framed<LinkStream, MessageCodec>,
    params: LinkParams,
    cancel: &CancellationToken,
    ch: &mut LinkChannels,
    health: &Arc<LinkHealth>,
    mut net: Option<watch::Receiver<u64>>,
    mut motion: Motion,
) -> DisconnectReason {
    let (mut sink, mut stream) = framed.split();
    if let Motion::Server(m) = &motion {
        if let Some(offer) = m.offer() {
            if let Err(reason) = send_cancelable(&mut sink, cancel, offer, "send motion offer").await {
                return reason;
            }
        }
    }
    // Only changes from here on count. The network was already settled when we connected.
    if let Some(r) = net.as_mut() {
        r.borrow_and_update();
    }

    let mut hb = interval(params.heartbeat_interval);
    hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_recv = Instant::now();
    // When the outstanding PING went out, so the matching PONG measures a round trip.
    // The heartbeat already exists for liveness; reading its timing costs nothing and
    // gives the bulk lane a true measurement of the latency it must not damage.
    let mut ping_sent: Option<Instant> = None;
    let mut rtt_log = crate::timing::RttLog::default();

    loop {
        let udp = motion.socket();
        let concealing = matches!(&motion, Motion::Client { live: Some(m), .. } if m.concealing(std::time::Instant::now()));
        let repeat_at = match &motion {
            Motion::Server(m) => m.repeat_due(),
            _ => None,
        };
        tokio::select! {
            _ = tokio::time::sleep_until(repeat_at.unwrap_or_else(std::time::Instant::now).into()), if repeat_at.is_some() => {
                if let Motion::Server(m) = &mut motion {
                    m.send_repeat();
                }
            }

            // Hide a short gap in pointer motion (see `motion_udp::Conceal`).
            _ = tokio::time::sleep(CONCEAL_TICK), if concealing => {
                if let Motion::Client { live: Some(m), .. } = &mut motion {
                    if let Some(mv) = m.conceal(std::time::Instant::now()) {
                        let _ = ch.from_peer.send(mv);
                    }
                }
            }

            // Local Stop/Reload: return immediately, no blocking.
            _ = cancel.cancelled() => return DisconnectReason::Cancelled,

            // This machine's network moved under the session. Drop it now, so the connect loop
            // can take the best path, rather than waiting out a dead one for the liveness timeout.
            _ = network_moved(&mut net) => return DisconnectReason::NetworkChanged,

            // Heartbeat tick: check liveness, then send a PING.
            _ = hb.tick() => {
                if last_recv.elapsed() > params.liveness_timeout {
                    return DisconnectReason::TimedOut;
                }
                if let Err(reason) = send_cancelable(&mut sink, cancel, Message::Ping, "send ping").await {
                    return reason;
                }
                // Let the reference latency drift up slowly, so moving to a genuinely
                // slower network is eventually accepted as normal rather than read as
                // permanent congestion and throttled forever.
                health.decay_baseline();
                ping_sent = Some(Instant::now());
                if let Motion::Client { live: Some(m), .. } = &mut motion {
                    m.hello().await;
                    if let Some(ready) = m.ready() {
                        if let Err(reason) = send_cancelable(&mut sink, cancel, ready, "send motion ready").await {
                            return reason;
                        }
                    }
                }
            }

            // Incoming frame from the peer.
            item = stream.next() => match item {
                None => return DisconnectReason::PeerClosed,
                Some(Err(e)) => return classify_read_error(&e),
                Some(Ok(msg)) => {
                    last_recv = Instant::now();
                    match msg {
                        Message::Ping => {
                            if let Err(reason) = send_cancelable(&mut sink, cancel, Message::Pong, "send pong").await {
                                return reason;
                            }
                        }
                        Message::Pong => {
                            // Liveness is already refreshed above; the timing is what
                            // the bulk lane paces against.
                            if let Some(sent) = ping_sent.take() {
                                health.record_rtt(sent.elapsed());
                                if let Some(line) = rtt_log.push(sent.elapsed(), std::time::Instant::now()) {
                                    tracing::info!("{line}");
                                }
                            }
                        }
                        Message::MotionOffer { port, token } => {
                            if let Motion::Client { key, server, on_cable, live } = &mut motion {
                                match ClientMotion::start(*server, port, &token, key.clone(), *on_cable).await {
                                    Ok(m) => {
                                        tracing::info!("pointer motion will arrive over UDP from {server} (port {port})");
                                        let mut m = m;
                                        let ready = m.ready();
                                        *live = Some(m);
                                        if let Some(ready) = ready {
                                            if let Err(reason) = send_cancelable(&mut sink, cancel, ready, "send motion ready").await {
                                                return reason;
                                            }
                                        }
                                    }
                                    Err(e) => tracing::warn!("UDP to {server} (port {port}) unavailable ({e}); pointer motion stays on TCP"),
                                }
                            }
                        }
                        Message::MotionReady { port, seen } => {
                            if let Motion::Server(m) = &mut motion {
                                m.on_ready(port, seen);
                            }
                        }
                        Message::MotionSync { seq, sx, sy } => {
                            if let Motion::Client { live: Some(m), .. } = &mut motion {
                                if let Some(mv) = m.on_sync(seq, sx, sy) {
                                    let _ = ch.from_peer.send(mv);
                                }
                            }
                        }
                        other => {
                            // Never blocks the read loop (unbounded). A dropped
                            // send only happens if the engine is gone → we'll be
                            // cancelled shortly anyway.
                            let _ = ch.from_peer.send(other);
                        }
                    }
                }
            },

            // Outgoing message from the app.
            out = ch.to_peer.recv() => match out {
                None => return DisconnectReason::Cancelled, // engine dropped the sender
                Some(msg) => {
                    if let Motion::Server(m) = &mut motion {
                        if let Message::MouseMove { dx, dy } = msg {
                            if m.send_move(dx, dy) {
                                continue;
                            }
                        } else if let Some(sync) = m.sync() {
                            if let Err(reason) = send_cancelable(&mut sink, cancel, sync, "send motion sync").await {
                                return reason;
                            }
                        }
                    }
                    if let Err(reason) = send_cancelable(&mut sink, cancel, msg, "send").await {
                        return reason;
                    }
                }
            },

            // Pointer motion datagrams: hellos on the server, moves on the client.
            got = crate::motion_udp::recv(udp) => match (got, &mut motion) {
                (Ok((buf, n, from)), Motion::Server(m)) => m.on_datagram(&buf[..n], from),
                (Ok((buf, n, _)), Motion::Client { live: Some(m), .. }) => {
                    if let Some(mv) = m.on_datagram(&buf[..n]) {
                        let _ = ch.from_peer.send(mv);
                    }
                }
                // A failed receive (e.g. ICMP port unreachable surfacing on Windows) is not a
                // reason to drop the session; TCP carries on.
                _ => {}
            },
        }
    }
}

/// Send one frame, but abort the instant `cancel` fires. A peer whose TCP receive
/// window is full makes `sink.send` block until it drains; without this, a local
/// Stop would wait on that back-pressure. Returning here is safe: the connection
/// is being torn down, so the half-sent frame doesn't matter.
async fn send_cancelable(
    sink: &mut SplitSink<Framed<LinkStream, MessageCodec>, Message>,
    cancel: &CancellationToken,
    msg: Message,
    ctx: &'static str,
) -> Result<(), DisconnectReason> {
    tokio::select! {
        _ = cancel.cancelled() => Err(DisconnectReason::Cancelled),
        r = sink.send(msg) => r.map_err(|e| DisconnectReason::Io(format!("{ctx}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Edge, Role};

    /// A mid-session socket drop (server killed → "connection reset by peer") MUST be
    /// classified recoverable so the client reconnects; only a malformed frame is fatal.
    /// Regression: this was collapsed to a fatal `ProtocolError`, so any server restart
    /// permanently stopped the client.
    #[test]
    fn read_io_drop_is_recoverable_malformed_is_fatal() {
        let reset = CodecError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        ));
        let by_reset = classify_read_error(&reset);
        assert!(
            matches!(by_reset, DisconnectReason::Io(_)) && by_reset.is_recoverable(),
            "a connection reset must be a recoverable Io drop, got {by_reset:?}"
        );

        let malformed = CodecError::Json(serde_json::from_slice::<Message>(b"not json").unwrap_err());
        let by_malformed = classify_read_error(&malformed);
        assert!(
            matches!(by_malformed, DisconnectReason::ProtocolError(_)) && !by_malformed.is_recoverable(),
            "a malformed frame must be a fatal ProtocolError, got {by_malformed:?}"
        );
    }

    /// The pairing key the test server and test client share.
    const TEST_KEY: &str = "test-key";

    /// The test server's and client's device IDs. They include the port, which each test
    /// picks fresh, so tests running at once never see each other's pairings in `trust`.
    fn server_id(port: u16) -> String {
        format!("srv-{port}")
    }
    fn client_id(port: u16) -> String {
        format!("cli-{port}")
    }

    fn pairing(id: String, name: &str, key: &str) -> PairedDevice {
        PairedDevice { id, name: name.into(), key: key.into(), paired_at: 0 }
    }

    /// A server or client for `port`, paired with the other one under [`TEST_KEY`].
    fn test_cfg(role: Role, port: u16, tls: bool) -> Config {
        let (device_id, peer) = match role {
            Role::Server => (server_id(port), pairing(client_id(port), "test-client", TEST_KEY)),
            Role::Client => (client_id(port), pairing(server_id(port), "test-server", TEST_KEY)),
        };
        Config {
            name: match role {
                Role::Server => "test-server".into(),
                Role::Client => "test-client".into(),
            },
            role,
            device_id,
            paired: vec![peer],
            tls_enabled: tls,
            listen_host: "127.0.0.1".into(),
            listen_port: port,
            server_host: "127.0.0.1".into(),
            server_port: port,
            exit_edge: Edge::Left,
            entry_edge: Edge::Right,
            screen_width: 1920,
            screen_height: 1080,
            ..Default::default()
        }
    }

    fn channels() -> (
        LinkChannels,
        mpsc::UnboundedSender<Message>,
        mpsc::UnboundedReceiver<Message>,
        mpsc::UnboundedReceiver<LinkEvent>,
    ) {
        let (to_peer_tx, to_peer_rx) = mpsc::unbounded_channel();
        let (from_peer_tx, from_peer_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        (
            LinkChannels { to_peer: to_peer_rx, from_peer: from_peer_tx, events: events_tx },
            to_peer_tx,
            from_peer_rx,
            events_rx,
        )
    }

    async fn wait_connected(events: &mut mpsc::UnboundedReceiver<LinkEvent>) -> bool {
        loop {
            match timeout(Duration::from_secs(3), events.recv()).await {
                Ok(Some(LinkEvent::Connected { .. })) => return true,
                Ok(Some(LinkEvent::AuthFailed(_))) | Ok(Some(LinkEvent::Disconnected(_))) => {
                    return false
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return false,
            }
        }
    }

    // A short heartbeat so the timeout test runs fast.
    fn fast_params() -> LinkParams {
        LinkParams {
            heartbeat_interval: Duration::from_millis(100),
            liveness_timeout: Duration::from_millis(400),
            handshake_timeout: Duration::from_secs(3),
            reconnect_initial: Duration::from_millis(50),
            reconnect_max: Duration::from_millis(200),
        }
    }

    /// Wait (up to `dur`) for the next `Connected` event, ignoring everything else
    /// (Connecting/Disconnected churn during a reconnect). Returns false on timeout.
    async fn wait_for_connect(
        events: &mut mpsc::UnboundedReceiver<LinkEvent>,
        dur: Duration,
    ) -> bool {
        let deadline = Instant::now() + dur;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match timeout(remaining, events.recv()).await {
                Ok(Some(LinkEvent::Connected { .. })) => return true,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return false,
            }
        }
    }

    async fn free_port() -> u16 {
        // Bind ephemeral port via OS, then reuse it for both sides.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
    }

    /// A client that completes the handshake then holds the socket open while
    /// going completely silent — never answering pings. This is a genuinely dead
    /// peer (no FIN), unlike a dropped socket which closes cleanly.
    async fn silent_client_after_handshake(port: u16, key: &str) {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut framed = Framed::new(LinkStream::Plain(tcp), MessageCodec::new());
        let client_nonce = auth::generate_nonce_hex();
        framed
            .send(Message::Hello {
                name: "silent".into(),
                width: 1920,
                height: 1080,
                version: PROTOCOL_VERSION,
                lane: Lane::Input,
                login_screen: false,
                nonce: Some(client_nonce.clone()),
                device: Some(client_id(port)),
            })
            .await
            .unwrap();
        let nonce = match framed.next().await {
            Some(Ok(Message::AuthChallenge { nonce, .. })) => nonce,
            other => panic!("expected challenge, got {other:?}"),
        };
        let (cid, sid) = (client_id(port), server_id(port));
        let ends = auth::Ends { client: &cid, server: &sid };
        let hmac = auth::proof_hex(key, Side::Client, ends, &[], &client_nonce, &nonce).unwrap();
        framed.send(Message::AuthResponse { hmac }).await.unwrap();
        let _ = framed.next().await; // consume AuthOk
        // Go silent but keep the socket alive — do not answer pings.
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(framed);
    }

    /// The next AuthFailed reason from `events`, within a few seconds.
    async fn next_refusal(events: &mut mpsc::UnboundedReceiver<LinkEvent>) -> Option<String> {
        loop {
            match timeout(Duration::from_secs(3), events.recv()).await {
                Ok(Some(LinkEvent::AuthFailed(r))) => return Some(r),
                Ok(Some(LinkEvent::Connected { .. })) => return None,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return None,
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_client_with_a_pin_refuses_a_server_until_its_user_types_it() {
        let _g = crate::pin::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let server = tokio::spawn(run_server(test_cfg(Role::Server, port, true), fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut client_cfg = test_cfg(Role::Client, port, true);
        client_cfg.connection_pin = "Ab#4321".into();
        client_cfg.pin_enabled = true;
        let client = tokio::spawn(run_client(client_cfg, fast_params(), cancel.clone(), c_ch, LinkHealth::new()));

        // Nothing typed on the server yet: the client refuses and the server asks its user.
        assert_eq!(next_refusal(&mut c_events).await.as_deref(), Some("pin_required"));
        assert_eq!(next_refusal(&mut s_events).await.as_deref(), Some("pin_required from test-client"));
        assert_eq!(crate::pin::prompt(), Some(("test-client".into(), crate::pin::Prompt::Needed)));

        assert_eq!(crate::pin::enter("1111".into()).as_deref(), Some("test-client"));
        let mut wrong = None;
        for _ in 0..5 {
            match next_refusal(&mut c_events).await {
                Some(r) if r.starts_with("wrong_pin") => {
                    wrong = Some(r);
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert_eq!(wrong.as_deref(), Some("wrong_pin:2"), "counted on the client");
        while let Some(r) = next_refusal(&mut s_events).await {
            if r.starts_with("wrong_pin") {
                break;
            }
        }
        assert_eq!(crate::pin::typed_for(&client_id(port)), None, "the server forgets a wrong PIN");

        crate::pin::enter("Ab#4321".into());
        assert!(wait_for_connect(&mut c_events, Duration::from_secs(5)).await, "the right PIN connects");
        assert_eq!(crate::pin::prompt(), None);

        cancel.cancel();
        let _ = tokio::join!(server, client);
        let _ = crate::pin::check_attempt(true); // leave the limiter clean
        crate::pin::set_awaiting(false);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn pairing_with_the_code_makes_a_key_for_these_two_and_they_connect_with_it() {
        let _g = crate::pin::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        // A server paired with nobody yet, and a fresh client.
        let mut server_cfg = test_cfg(Role::Server, port, true);
        server_cfg.paired.clear();
        let server = tokio::spawn(run_server(server_cfg, fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut fresh = test_cfg(Role::Client, port, true);
        fresh.paired.clear();
        let to = format!("127.0.0.1:{port}");
        let wait = Duration::from_secs(3);

        crate::pairing::cancel_code();
        assert_eq!(pair_with_server(&fresh, &to, "000000", wait).await, Err("no_code".into()));

        let code = crate::pairing::start_code();
        let wrong = if code == "111111" { "222222" } else { "111111" };
        assert_eq!(pair_with_server(&fresh, &to, wrong, wait).await, Err("wrong_code".into()));
        assert!(crate::trust::get(&server_id(port), &client_id(port)).is_none(), "a wrong code pairs nothing");

        let got = pair_with_server(&fresh, "127.0.0.1", &code, wait).await.expect("pairs");
        assert_eq!(crate::pairing::current_code(), None, "a code is used once");
        assert_eq!(crate::pairing::take_last_paired().as_deref(), Some("test-client"));
        // The client knows exactly which computer showed the code, and the two hold the
        // same new key, which is not the test key nor any old shared secret.
        assert_eq!(got.id, server_id(port));
        assert_eq!(got.name, "test-server");
        let server_side = crate::trust::get(&server_id(port), &client_id(port)).expect("the server kept the pairing");
        assert_eq!(server_side.key, got.key);
        assert_eq!(server_side.name, "test-client");
        assert_ne!(got.key, TEST_KEY);
        // Both are waiting for their panels to save.
        assert_eq!(crate::trust::take_unsaved(&server_id(port)).len(), 1);
        assert_eq!(crate::trust::take_unsaved(&client_id(port)).len(), 1);

        // And with that key, and nothing else, the client connects.
        fresh.paired = vec![got];
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(fresh, fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
        assert!(wait_connected(&mut c_events).await, "the new pairing connects");
        assert!(wait_connected(&mut s_events).await);

        cancel.cancel();
        let _ = tokio::join!(server, client);
    }

    /// A computer for these tests: `id`, called `name`, serving or connecting on `port`,
    /// paired with exactly the computers in `paired`.
    fn computer(role: Role, port: u16, id: &str, name: &str, paired: Vec<PairedDevice>) -> Config {
        Config { device_id: id.into(), name: name.into(), paired, ..test_cfg(role, port, false) }
    }

    /// The first AuthFailed or Connected on `events`: Ok(()) for connected, Err(reason).
    async fn outcome(events: &mut mpsc::UnboundedReceiver<LinkEvent>) -> Result<(), String> {
        // An overall deadline too: a client retrying a dead address sends other events
        // for ever, and each one would otherwise restart the wait.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(LinkEvent::Connected { .. })) => return Ok(()),
                Ok(Some(LinkEvent::AuthFailed(r))) => return Err(r),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return Err("nothing happened".into()),
            }
        }
    }

    /// B50, exactly as it happened on 30 Oct: the laptop and Kali are paired with each
    /// other; the VM pairs with the laptop only. The VM must be refused by Kali, and get
    /// in to the laptop.
    #[tokio::test]
    async fn a_computer_paired_with_one_computer_is_refused_by_another() {
        let (laptop_port, kali_port) = (free_port().await, free_port().await);
        let tag = format!("{laptop_port}-{kali_port}");
        let (laptop, kali, vm) = (format!("laptop-{tag}"), format!("kali-{tag}"), format!("vm-{tag}"));
        let cancel = CancellationToken::new();
        let start = |cfg: Config| {
            let (ch, _to, _from, events) = channels();
            let h = tokio::spawn(run_server(cfg, fast_params(), cancel.clone(), ch, BulkServer::disabled(), LinkHealth::new()));
            (h, events, _to, _from)
        };
        let (s1, mut laptop_events, _a, _b) = start(computer(
            Role::Server,
            laptop_port,
            &laptop,
            "laptop",
            vec![pairing(kali.clone(), "kali", "laptop-kali"), pairing(vm.clone(), "vm", "laptop-vm")],
        ));
        let (s2, mut kali_events, _c, _d) =
            start(computer(Role::Server, kali_port, &kali, "kali", vec![pairing(laptop.clone(), "laptop", "laptop-kali")]));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The VM tries Kali.
        let vm_cfg = computer(Role::Client, kali_port, &vm, "vm", vec![pairing(laptop.clone(), "laptop", "laptop-vm")]);
        let (c_ch, _e, _f, mut vm_events) = channels();
        let c1 = tokio::spawn(run_client(vm_cfg.clone(), fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
        assert_eq!(outcome(&mut vm_events).await, Err("not_paired".into()), "the VM must not get into Kali");
        let kali_said = outcome(&mut kali_events).await.unwrap_err();
        assert!(kali_said.contains("not paired"), "Kali says why, by name: {kali_said}");

        // The same VM, pointed at the laptop it paired with, gets in.
        let (c_ch, _g, _h, mut vm_events2) = channels();
        let c2 = tokio::spawn(run_client(
            Config { server_port: laptop_port, ..vm_cfg },
            fast_params(),
            cancel.clone(),
            c_ch,
            LinkHealth::new(),
        ));
        assert_eq!(outcome(&mut vm_events2).await, Ok(()));
        assert_eq!(outcome(&mut laptop_events).await, Ok(()));

        cancel.cancel();
        let _ = tokio::join!(s1, s2, c1, c2);
    }

    /// Swapping who has the mouse needs no new pairing: the one key works both ways.
    #[tokio::test]
    async fn swapping_roles_uses_the_same_pairing() {
        for swapped in [false, true] {
            let port = free_port().await;
            let (a, b) = (format!("a-{port}"), format!("b-{port}"));
            let a_pairs = vec![pairing(b.clone(), "b", "ab-key")];
            let b_pairs = vec![pairing(a.clone(), "a", "ab-key")];
            let (server_cfg, client_cfg) = if swapped {
                (computer(Role::Server, port, &b, "b", b_pairs), computer(Role::Client, port, &a, "a", a_pairs))
            } else {
                (computer(Role::Server, port, &a, "a", a_pairs), computer(Role::Client, port, &b, "b", b_pairs))
            };
            let cancel = CancellationToken::new();
            let (s_ch, _s_to, _s_from, mut s_events) = channels();
            let (c_ch, _c_to, _c_from, mut c_events) = channels();
            let server = tokio::spawn(run_server(server_cfg, fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
            tokio::time::sleep(Duration::from_millis(100)).await;
            let client = tokio::spawn(run_client(client_cfg, fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
            assert_eq!(outcome(&mut c_events).await, Ok(()), "swapped={swapped}");
            assert_eq!(outcome(&mut s_events).await, Ok(()), "swapped={swapped}");
            cancel.cancel();
            let _ = tokio::join!(server, client);
        }
    }

    /// A server the client is not paired with gets nothing from it, not even a proof made
    /// with some other key, whatever name it gives.
    #[tokio::test]
    async fn a_client_gives_nothing_to_a_server_it_is_not_paired_with() {
        let port = free_port().await;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let fake = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(LinkStream::Plain(tcp), MessageCodec::new());
            let client_nonce = match framed.next().await {
                Some(Ok(Message::Hello { nonce: Some(n), .. })) => n,
                other => panic!("expected a hello, got {other:?}"),
            };
            let nonce = auth::generate_nonce_hex();
            let ends = auth::Ends { client: "whoever", server: "stranger" };
            let proof = auth::proof_hex("k", Side::Server, ends, &[], &client_nonce, &nonce).unwrap();
            framed
                .send(Message::AuthChallenge { nonce, device: Some("stranger".into()), proof: Some(proof), pin_proof: None })
                .await
                .unwrap();
            timeout(Duration::from_secs(2), framed.next()).await.ok().flatten()
        });
        let cancel = CancellationToken::new();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(test_cfg(Role::Client, port, false), fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
        assert_eq!(outcome(&mut c_events).await, Err("not_paired_server".into()));
        let after = fake.await.unwrap();
        assert!(
            !matches!(after, Some(Ok(Message::AuthResponse { .. }))),
            "the client sent a proof to a server it is not paired with: {after:?}"
        );
        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), client).await;
    }

    /// R46: the computer with the mouse stays connected to every paired computer at once,
    /// and a switch (a shortcut or a click) moves input to another instantly: the engine
    /// is told, what it sends goes to the new one, and the one left is sent a Leave.
    #[tokio::test]
    async fn the_computer_with_the_mouse_is_connected_to_all_and_switches_instantly() {
        let port = free_port().await;
        let (srv, a, b) = (format!("srv46-{port}"), format!("a46-{port}"), format!("b46-{port}"));
        let cancel = CancellationToken::new();
        let server_cfg = computer(
            Role::Server,
            port,
            &srv,
            "laptop",
            vec![pairing(a.clone(), "kali", "k-a"), pairing(b.clone(), "win-vm", "k-b")],
        );
        let (s_ch, s_to, _s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(server_cfg, fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = |id: &str, name: &str, key: &str| computer(Role::Client, port, id, name, vec![pairing(srv.clone(), "laptop", key)]);

        let (ch, _a1, mut a_from, mut a_events) = channels();
        let ca = tokio::spawn(run_client(client(&a, "kali", "k-a"), fast_params(), cancel.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut a_events).await, Ok(()));
        let (ch, _b1, mut b_from, mut b_events) = channels();
        let cb = tokio::spawn(run_client(client(&b, "win-vm", "k-b"), fast_params(), cancel.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut b_events).await, Ok(()), "the second computer is let in too");

        // The engine was told about Kali (first in), and nothing else yet.
        let first = loop {
            match timeout(Duration::from_secs(2), s_events.recv()).await {
                Ok(Some(LinkEvent::Connected { peer, .. })) => break peer,
                Ok(Some(_)) => continue,
                other => panic!("no Connected: {other:?}"),
            }
        };
        assert_eq!(first, "kali");
        let mut both = crate::trust::connected(&srv);
        both.sort();
        let mut want = vec![a.clone(), b.clone()];
        want.sort();
        assert_eq!(both, want, "both are connected at once");

        // What the engine sends goes to Kali.
        s_to.send(Message::MouseMove { dx: 1, dy: 1 }).unwrap();
        let got = timeout(Duration::from_secs(2), a_from.recv()).await.unwrap();
        assert!(matches!(got, Some(Message::MouseMove { .. })), "{got:?}");

        // Switch to the VM: the engine hears it, Kali is left, the VM gets the input.
        crate::trust::reserve(&srv, Some(&b));
        let switched = loop {
            match timeout(Duration::from_secs(2), s_events.recv()).await {
                Ok(Some(LinkEvent::Connected { peer, .. })) => break peer,
                Ok(Some(_)) => continue,
                other => panic!("no switch: {other:?}"),
            }
        };
        assert_eq!(switched, "win-vm");
        let left = timeout(Duration::from_secs(2), a_from.recv()).await.unwrap();
        assert!(matches!(left, Some(Message::Leave)), "Kali is told it was left: {left:?}");
        s_to.send(Message::MouseMove { dx: 2, dy: 2 }).unwrap();
        let got = timeout(Duration::from_secs(2), b_from.recv()).await.unwrap();
        assert!(matches!(got, Some(Message::MouseMove { .. })), "{got:?}");
        assert_eq!(crate::trust::reserved(&srv), None, "the switch was taken");
        assert_eq!(crate::trust::input_peer(&srv).as_deref(), Some(b.as_str()));

        // The VM goes away: Kali, still connected, is used again.
        let (ch2, ..) = channels();
        drop(ch2);
        cb.abort();
        let back = loop {
            match timeout(Duration::from_secs(5), s_events.recv()).await {
                Ok(Some(LinkEvent::Connected { peer, .. })) => break peer,
                Ok(Some(_)) => continue,
                other => panic!("did not fall back: {other:?}"),
            }
        };
        assert_eq!(back, "kali");

        cancel.cancel();
        let _ = tokio::join!(server, ca);
    }

    /// After a restart the computer last in use gets it back, even when another computer
    /// happens to reconnect first (seen on hardware: the VM came back before Kali, and the
    /// edge led to the VM).
    #[tokio::test]
    async fn after_a_restart_the_computer_last_in_use_is_used_again() {
        let port = free_port().await;
        let (srv, a, b) = (format!("srvlast-{port}"), format!("alast-{port}"), format!("blast-{port}"));
        let cancel = CancellationToken::new();
        let mut server_cfg = computer(
            Role::Server,
            port,
            &srv,
            "laptop",
            vec![pairing(a.clone(), "kali", "k-a"), pairing(b.clone(), "win-vm", "k-b")],
        );
        server_cfg.connect_to = a.clone();
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(server_cfg, fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = |id: &str, name: &str, key: &str| computer(Role::Client, port, id, name, vec![pairing(srv.clone(), "laptop", key)]);
        async fn next_use(ev: &mut mpsc::UnboundedReceiver<LinkEvent>) -> String {
            loop {
                match timeout(Duration::from_secs(3), ev.recv()).await {
                    Ok(Some(LinkEvent::Connected { peer, .. })) => return peer,
                    Ok(Some(_)) => continue,
                    other => panic!("no Connected: {other:?}"),
                }
            }
        }

        // The VM is back first: used meanwhile.
        let (ch, _b1, mut b_from, mut b_events) = channels();
        let cb = tokio::spawn(run_client(client(&b, "win-vm", "k-b"), fast_params(), cancel.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut b_events).await, Ok(()));
        assert_eq!(next_use(&mut s_events).await, "win-vm");

        // Kali, last in use before the restart, arrives: it is used again, the VM is left.
        let (ch, _a1, _a_from, mut a_events) = channels();
        let ca = tokio::spawn(run_client(client(&a, "kali", "k-a"), fast_params(), cancel.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut a_events).await, Ok(()));
        assert_eq!(next_use(&mut s_events).await, "kali");
        let left = loop {
            match timeout(Duration::from_secs(2), b_from.recv()).await.unwrap() {
                Some(Message::Leave) => break true,
                Some(_) => continue,
                None => break false,
            }
        };
        assert!(left, "the VM is told it was left");
        assert_eq!(crate::trust::input_peer(&srv).as_deref(), Some(a.as_str()));

        cancel.cancel();
        let _ = tokio::join!(server, ca, cb);
    }

    /// One clipboard for all: text copied on any computer reaches every connected one,
    /// and a large image copied here is offered to each other computer when it comes
    /// into use.
    #[tokio::test]
    async fn every_connected_computer_shares_one_clipboard() {
        let port = free_port().await;
        let (srv, a, b) = (format!("srvclip-{port}"), format!("aclip-{port}"), format!("bclip-{port}"));
        let cancel = CancellationToken::new();
        let server_cfg = computer(
            Role::Server,
            port,
            &srv,
            "laptop",
            vec![pairing(a.clone(), "kali", "k-a"), pairing(b.clone(), "win-vm", "k-b")],
        );
        let (s_ch, s_to, mut s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(server_cfg, fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = |id: &str, name: &str, key: &str| computer(Role::Client, port, id, name, vec![pairing(srv.clone(), "laptop", key)]);
        let (ch, a_to, mut a_from, mut a_events) = channels();
        let ca = tokio::spawn(run_client(client(&a, "kali", "k-a"), fast_params(), cancel.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut a_events).await, Ok(()));
        let (ch, _b_to, mut b_from, mut b_events) = channels();
        let cb = tokio::spawn(run_client(client(&b, "win-vm", "k-b"), fast_params(), cancel.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut b_events).await, Ok(()));
        loop {
            match timeout(Duration::from_secs(2), s_events.recv()).await {
                Ok(Some(LinkEvent::Connected { .. })) => break,
                Ok(Some(_)) => continue,
                other => panic!("no Connected: {other:?}"),
            }
        }
        async fn next_clip(rx: &mut mpsc::UnboundedReceiver<Message>) -> Message {
            loop {
                match timeout(Duration::from_secs(2), rx.recv()).await {
                    Ok(Some(m @ (Message::Clipboard { .. } | Message::ClipboardImageRef { .. }))) => return m,
                    Ok(Some(_)) => continue,
                    other => panic!("no clipboard message: {other:?}"),
                }
            }
        }

        // Copied here: both get it, though only Kali is in use.
        s_to.send(Message::Clipboard { text: "from laptop".into() }).unwrap();
        assert_eq!(next_clip(&mut a_from).await, Message::Clipboard { text: "from laptop".into() });
        assert_eq!(next_clip(&mut b_from).await, Message::Clipboard { text: "from laptop".into() });

        // Copied on Kali: this computer and the VM get it.
        a_to.send(Message::Clipboard { text: "from kali".into() }).unwrap();
        assert_eq!(next_clip(&mut s_from).await, Message::Clipboard { text: "from kali".into() });
        assert_eq!(next_clip(&mut b_from).await, Message::Clipboard { text: "from kali".into() });

        // A large image copied here goes to Kali now, and to the VM once it is in use.
        let big = Message::ClipboardImageRef { hash: "h".into(), w: 1, h: 1, size: 1 };
        s_to.send(big.clone()).unwrap();
        assert_eq!(next_clip(&mut a_from).await, big);
        crate::trust::reserve(&srv, Some(&b));
        assert_eq!(next_clip(&mut b_from).await, big, "offered to the VM when it came into use");

        cancel.cancel();
        let _ = tokio::join!(server, ca, cb);
    }

    /// R45: a controlled computer goes back to the computer it was last with, not to
    /// another paired one that happens to answer.
    #[tokio::test]
    async fn a_controlled_computer_goes_only_to_the_one_it_was_last_with() {
        let (p1, p2) = (free_port().await, free_port().await);
        let (laptop, kali, vm) = (format!("lap45-{p1}"), format!("kal45-{p2}"), format!("vm45-{p1}"));
        // The servers outlive the first client.
        let servers = CancellationToken::new();
        let cancel = CancellationToken::new();
        let (ch, _x, _y, _e1) = channels();
        let s1 = tokio::spawn(run_server(
            computer(Role::Server, p1, &laptop, "laptop", vec![pairing(vm.clone(), "vm", "k1")]),
            fast_params(), servers.clone(), ch, BulkServer::disabled(), LinkHealth::new(),
        ));
        let (ch, _x2, _y2, _e2) = channels();
        let s2 = tokio::spawn(run_server(
            computer(Role::Server, p2, &kali, "kali", vec![pairing(vm.clone(), "vm", "k2")]),
            fast_params(), servers.clone(), ch, BulkServer::disabled(), LinkHealth::new(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let vm_pairs = vec![pairing(laptop.clone(), "laptop", "k1"), pairing(kali.clone(), "kali", "k2")];

        // Last with the laptop, and Kali is the one answering: nothing is given to Kali.
        let mut vm_cfg = computer(Role::Client, p2, &vm, "vm", vm_pairs.clone());
        vm_cfg.connect_to = laptop.clone();
        let (ch, _a, _b, mut ev) = channels();
        let c1 = tokio::spawn(run_client(vm_cfg, fast_params(), cancel.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut ev).await, Err("not_chosen_server".into()));
        cancel.cancel();
        let _ = c1.await;

        // The user picks Kali: now it connects there.
        let cancel2 = CancellationToken::new();
        let mut vm_cfg = computer(Role::Client, p2, &vm, "vm", vm_pairs);
        vm_cfg.connect_to = kali.clone();
        let (ch, _a, _b, mut ev) = channels();
        let c2 = tokio::spawn(run_client(vm_cfg, fast_params(), cancel2.clone(), ch, LinkHealth::new()));
        assert_eq!(outcome(&mut ev).await, Ok(()));
        cancel2.cancel();
        servers.cancel();
        let _ = tokio::join!(s1, s2, c2);
    }

    /// B65: a computer that knows a paired device ID but not the key answers the
    /// challenge with a refusal. It must neither crash the server (a huge `pin_locked`
    /// overflowed Instant and aborted the process) nor touch the PIN prompt.
    #[tokio::test]
    async fn an_unsigned_refusal_neither_crashes_the_server_nor_changes_the_pin_prompt() {
        let _g = crate::pin::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let (s_ch, _s_to, _s_from, _s_events) = channels();
        let server = tokio::spawn(run_server(test_cfg(Role::Server, port, false), fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = crate::pin::prompt();
        for reason in ["pin_locked:18446744073709551615", "pin_required", "wrong_pin:1"] {
            let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let mut framed = Framed::new(LinkStream::Plain(tcp), MessageCodec::new());
            framed
                .send(Message::Hello {
                    name: "impostor".into(), width: 1, height: 1, version: PROTOCOL_VERSION, lane: Lane::Input,
                    login_screen: false, nonce: Some(auth::generate_nonce_hex()), device: Some(client_id(port)),
                })
                .await
                .unwrap();
            assert!(matches!(framed.next().await, Some(Ok(Message::AuthChallenge { .. }))));
            framed.send(refusal(reason)).await.unwrap();
            let _ = timeout(Duration::from_millis(300), framed.next()).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!server.is_finished(), "the server must survive an unsigned refusal");
        assert_eq!(crate::pin::prompt(), before, "an unsigned refusal must not change the PIN prompt");
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_ok(), "still accepting");
        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
    }

    /// Audit, deferred lead: a connection that says nothing used to hold the hub for the
    /// whole handshake timeout, so a paired computer could not get in meanwhile (and the
    /// computer in use got no input). The handshake now runs beside the hub.
    #[tokio::test]
    async fn a_silent_connection_does_not_hold_up_a_paired_computer() {
        for tls in [false, true] {
            let port = free_port().await;
            let cancel = CancellationToken::new();
            let (s_ch, _s_to, _s_from, _s_events) = channels();
            let server = tokio::spawn(run_server(test_cfg(Role::Server, port, tls), fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Silent: connects, sends nothing, stays open past the test.
            let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (c_ch, _c_to, _c_from, mut c_events) = channels();
            let client = tokio::spawn(run_client(test_cfg(Role::Client, port, tls), fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
            // fast_params' handshake timeout is 3 s: connecting well inside it proves the
            // silent one is not being waited on first.
            assert!(wait_for_connect(&mut c_events, Duration::from_secs(1)).await, "tls={tls}: the paired computer got in");
            cancel.cancel();
            let _ = tokio::join!(server, client);
        }
    }

    /// Audit, deferred lead: removing a pairing ends a session already running, on
    /// whichever side it is removed.
    #[tokio::test]
    async fn removing_a_pairing_ends_the_live_session() {
        async fn disconnected(events: &mut mpsc::UnboundedReceiver<LinkEvent>, within: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + within;
            loop {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Some(LinkEvent::Disconnected(_))) => return true,
                    Ok(Some(_)) => continue,
                    _ => return false,
                }
            }
        }
        for removed_on_server in [true, false] {
            let port = free_port().await;
            let cancel = CancellationToken::new();
            let (s_ch, _s_to, _s_from, _s_events) = channels();
            let server = tokio::spawn(run_server(test_cfg(Role::Server, port, true), fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (c_ch, _c_to, _c_from, mut c_events) = channels();
            let client = tokio::spawn(run_client(test_cfg(Role::Client, port, true), fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
            assert!(wait_for_connect(&mut c_events, Duration::from_secs(3)).await);
            if removed_on_server {
                crate::trust::remove(&server_id(port), &client_id(port));
            } else {
                crate::trust::remove(&client_id(port), &server_id(port));
            }
            assert!(
                disconnected(&mut c_events, Duration::from_secs(1)).await,
                "removed on the {}: the session must end at once",
                if removed_on_server { "server" } else { "client" }
            );
            cancel.cancel();
            let _ = tokio::join!(server, client);
        }
    }

    /// B61: a computer renamed after pairing shows its new name once it has proved
    /// itself, on the computer it connects to, and the change is kept for the panel.
    #[tokio::test]
    async fn a_renamed_computer_shows_its_new_name_after_proving_itself() {
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let (s_ch, _s_to, _s_from, _s_events) = channels();
        let server = tokio::spawn(run_server(test_cfg(Role::Server, port, true), fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut ccfg = test_cfg(Role::Client, port, true);
        ccfg.name = "laptop-renamed".into();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(ccfg, fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
        assert!(wait_for_connect(&mut c_events, Duration::from_secs(3)).await);
        assert_eq!(crate::trust::get(&server_id(port), &client_id(port)).unwrap().name, "laptop-renamed");
        assert_eq!(
            crate::trust::take_renamed(&server_id(port)),
            vec![(client_id(port), "laptop-renamed".to_string())]
        );
        cancel.cancel();
        let _ = tokio::join!(server, client);
    }

    /// Removing a computer refuses it from that moment, without restarting sharing.
    #[tokio::test]
    async fn a_removed_computer_is_refused_at_once() {
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let (s_ch, _s_to, _s_from, _s_events) = channels();
        let server = tokio::spawn(run_server(test_cfg(Role::Server, port, false), fast_params(), cancel.clone(), s_ch, BulkServer::disabled(), LinkHealth::new()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        crate::trust::remove(&server_id(port), &client_id(port));
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(test_cfg(Role::Client, port, false), fast_params(), cancel.clone(), c_ch, LinkHealth::new()));
        assert_eq!(outcome(&mut c_events).await, Err("not_paired".into()));
        cancel.cancel();
        let _ = tokio::join!(server, client);
    }

    #[tokio::test]
    async fn handshake_succeeds_and_heartbeat_keeps_alive() {
        for tls in [false, true] {
            let port = free_port().await;
            let cancel = CancellationToken::new();

            let (s_ch, _s_to, _s_from, mut s_events) = channels();
            let (c_ch, _c_to, _c_from, mut c_events) = channels();

            let server = tokio::spawn(run_server(
                test_cfg(Role::Server, port, tls),
                fast_params(),
                cancel.clone(),
                s_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));
            // Give the listener a moment to bind.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let client = tokio::spawn(run_client(
                test_cfg(Role::Client, port, tls),
                fast_params(),
                cancel.clone(),
                c_ch,
                LinkHealth::new(),
            ));

            assert!(wait_connected(&mut s_events).await, "server connect (tls={tls})");
            assert!(wait_connected(&mut c_events).await, "client connect (tls={tls})");

            // Stay up well past several heartbeat intervals — must NOT time out.
            tokio::time::sleep(Duration::from_millis(700)).await;

            // Clean stop: cancel must tear both sides down promptly.
            cancel.cancel();
            let _ = timeout(Duration::from_secs(2), server).await.expect("server stops");
            let _ = timeout(Duration::from_secs(2), client).await.expect("client stops");
        }
    }

    #[tokio::test]
    async fn wrong_key_is_rejected() {
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();

        let mut bad_client = test_cfg(Role::Client, port, false);
        bad_client.paired[0].key = "WRONG".into();

        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            cancel.clone(),
            s_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(bad_client, fast_params(), cancel.clone(), c_ch,
                LinkHealth::new(),
            ));

        assert!(!wait_connected(&mut c_events).await, "client must be rejected");
        // server reports an AuthFailed event
        let mut saw_auth_failed = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(2), s_events.recv()).await {
            if let LinkEvent::AuthFailed(_) = ev {
                saw_auth_failed = true;
                break;
            }
        }
        assert!(saw_auth_failed, "server must report AuthFailed");

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
        let _ = timeout(Duration::from_secs(2), client).await;
    }

    /// A fake server that doesn't know the secret: the client must refuse it before it
    /// sends a proof of its own (B34).
    #[tokio::test]
    async fn a_server_that_cannot_prove_the_secret_is_refused() {
        let port = free_port().await;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let fake = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(LinkStream::Plain(tcp), MessageCodec::new());
            let client_nonce = match framed.next().await {
                Some(Ok(Message::Hello { nonce: Some(n), .. })) => n,
                other => panic!("expected a v3 hello, got {other:?}"),
            };
            let nonce = auth::generate_nonce_hex();
            // It even claims the right device ID; it just does not hold the key.
            let (cid, sid) = (client_id(port), server_id(port));
            let ends = auth::Ends { client: &cid, server: &sid };
            let proof = auth::proof_hex("GUESS", Side::Server, ends, &[], &client_nonce, &nonce).unwrap();
            framed
                .send(Message::AuthChallenge { nonce, device: Some(sid.clone()), proof: Some(proof), pin_proof: None })
                .await
                .unwrap();
            // Whatever comes next must not be the client's proof.
            timeout(Duration::from_secs(2), framed.next()).await.ok().flatten()
        });

        let cancel = CancellationToken::new();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            cancel.clone(),
            c_ch,
            LinkHealth::new(),
        ));

        let mut refused = None;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(3), c_events.recv()).await {
            match ev {
                LinkEvent::AuthFailed(reason) => {
                    refused = Some(reason);
                    break;
                }
                LinkEvent::Connected { .. } => panic!("connected to a server without the secret"),
                _ => {}
            }
        }
        assert_eq!(refused.as_deref(), Some("server_not_verified"));
        let after = fake.await.unwrap();
        assert!(
            !matches!(after, Some(Ok(Message::AuthResponse { .. }))),
            "the client sent its proof to an unverified server: {after:?}"
        );

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), client).await;
    }

    /// A version 1 client (no nonce) is refused, so a connection can't be downgraded to
    /// the old one-way check.
    #[tokio::test]
    async fn an_old_client_is_refused() {
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            cancel.clone(),
            s_ch,
            BulkServer::disabled(),
            LinkHealth::new(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut framed = Framed::new(LinkStream::Plain(tcp), MessageCodec::new());
        framed
            .send(Message::Hello {
                name: "old".into(),
                width: 1920,
                height: 1080,
                version: 1,
                lane: Lane::Input,
                login_screen: false,
                nonce: None,
                device: None,
            })
            .await
            .unwrap();
        match framed.next().await {
            Some(Ok(Message::AuthOk { ok: false, reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("old_version"))
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let mut saw_auth_failed = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(2), s_events.recv()).await {
            if let LinkEvent::AuthFailed(_) = ev {
                saw_auth_failed = true;
                break;
            }
        }
        assert!(saw_auth_failed, "server must report AuthFailed");

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn dead_peer_is_detected_via_liveness_timeout() {
        // A peer that handshakes then goes silent (socket stays open, no pongs)
        // must be detected by the heartbeat liveness window — not hang forever.
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let (s_ch, _s_to, _s_from, mut s_events) = channels();

        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            cancel.clone(),
            s_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = tokio::spawn(silent_client_after_handshake(port, TEST_KEY));

        assert!(wait_connected(&mut s_events).await, "server connect");

        // The server must emit Disconnected(TimedOut) within a few liveness windows.
        let mut timed_out = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(3), s_events.recv()).await {
            if let LinkEvent::Disconnected(DisconnectReason::TimedOut) = ev {
                timed_out = true;
                break;
            }
        }
        assert!(timed_out, "server must detect the dead peer via liveness timeout");

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
        let _ = timeout(Duration::from_secs(6), client).await;
    }

    #[tokio::test]
    async fn cancel_stops_promptly_without_hanging() {
        // The core fix for the Stop/Reload hang: cancel returns fast.
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();

        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            LinkParams::default(), // real 2s heartbeat — cancel must not wait for it
            cancel.clone(),
            s_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            LinkParams::default(),
            cancel.clone(),
            c_ch,
                LinkHealth::new(),
            ));

        assert!(wait_connected(&mut s_events).await);
        assert!(wait_connected(&mut c_events).await);

        let t0 = Instant::now();
        cancel.cancel();
        timeout(Duration::from_millis(500), server)
            .await
            .expect("server stops within 500ms")
            .unwrap();
        timeout(Duration::from_millis(500), client)
            .await
            .expect("client stops within 500ms")
            .unwrap();
        assert!(t0.elapsed() < Duration::from_millis(900), "stop was prompt");
    }

    #[tokio::test]
    async fn a_hello_without_a_lane_is_still_the_input_link() {
        // Backwards compatibility, and the reason `Lane` defaults to `Input`: a peer
        // built before the bulk lane existed sends no `lane` field at all. If that
        // decoded as anything else, upgrading one machine would break the pair.
        let raw = r#"{"t":"Hello","name":"old","width":1920,"height":1080,"version":1}"#;
        let msg: Message = serde_json::from_str(raw).expect("an older Hello must still parse");
        match msg {
            Message::Hello { lane, name, .. } => {
                assert_eq!(lane, Lane::Input);
                assert_eq!(name, "old");
            }
            other => panic!("expected a Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_bulk_lane_shares_the_port_without_disturbing_input() {
        // The whole point of the restructure: a bulk connection arrives on the *same*
        // listener while an input session is already live, is routed by its declared
        // lane, and neither link notices the other. Before this, the accept loop was
        // blocked inside the input pump and the bulk connection would never be taken.
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let (s_ch, s_to, _s_from, mut s_events) = channels();
        let (c_ch, _c_to, mut c_from, mut c_events) = channels();

        // Server side of the bulk lane, so we can observe what arrives on it.
        let (bulk_in_tx, bulk_in_rx) = std::sync::mpsc::channel();
        let (_bulk_out_tx, bulk_out_rx) = mpsc::unbounded_channel();
        let health = LinkHealth::new();
        let bulk = BulkServer::enabled(
            crate::bulk::BulkChannels {
                outgoing: bulk_out_rx,
                incoming: bulk_in_tx,
                queue: std::sync::Arc::new(crate::bulk::BulkQueue::default()),
            },
            health.clone(),
            crate::bulk::BulkSettings::default(),
        );

        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            cancel.clone(),
            s_ch,
            bulk,
            health,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            cancel.clone(),
            c_ch,
            LinkHealth::new(),
        ));

        assert!(wait_connected(&mut s_events).await);
        assert!(wait_connected(&mut c_events).await);

        // Now — with the input session already running — bring up the bulk lane.
        let cfg = test_cfg(Role::Client, port, false);
        let framed = timeout(
            Duration::from_secs(3),
            connect_and_handshake(&cfg, None, port, Lane::Bulk, Duration::from_secs(2), &cancel),
        )
        .await
        .expect("bulk handshake must not hang")
        .expect("the server must accept a bulk connection while input is live");

        // Send one bulk frame and confirm it lands on the bulk side, not the input side.
        let mut bulk_framed = {
            let parts = framed.into_parts();
            let mut new = tokio_util::codec::FramedParts::new::<crate::bulk::BulkFrame>(
                parts.io,
                crate::bulk::BulkCodec::new(),
            );
            new.read_buf = parts.read_buf;
            Framed::from_parts(new)
        };
        bulk_framed
            .send(crate::bulk::BulkFrame::Chunk { transfer: 1, seq: 0, data: vec![9; 32] })
            .await
            .expect("send a bulk chunk");

        let got = tokio::task::spawn_blocking(move || {
            bulk_in_rx.recv_timeout(Duration::from_secs(3))
        })
        .await
        .expect("join")
        .expect("a bulk frame must arrive");
        assert_eq!(got, crate::bulk::BulkFrame::Chunk { transfer: 1, seq: 0, data: vec![9; 32] });

        // And input still works, unaffected by the bulk traffic.
        s_to.send(Message::MouseMove { dx: 4, dy: 4 }).unwrap();
        let moved = timeout(Duration::from_secs(2), c_from.recv()).await.unwrap();
        assert_eq!(moved, Some(Message::MouseMove { dx: 4, dy: 4 }));

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
        let _ = timeout(Duration::from_secs(2), client).await;
    }

    #[tokio::test]
    async fn the_heartbeat_measures_a_round_trip_for_the_pacer() {
        // The bulk lane paces against this number, so if the heartbeat ever stopped
        // recording it the pacer would silently run wide open forever.
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client_health = LinkHealth::new();

        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            cancel.clone(),
            s_ch,
            BulkServer::disabled(),
            LinkHealth::new(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            cancel.clone(),
            c_ch,
            client_health.clone(),
        ));

        assert!(wait_connected(&mut s_events).await);
        assert!(wait_connected(&mut c_events).await);
        // Give the heartbeat a few ticks to complete a ping/pong round trip.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(client_health.latest_us() > 0, "a round trip should have been recorded");
        assert!(client_health.baseline_us() > 0, "and it should have set a baseline");

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
        let _ = timeout(Duration::from_secs(2), client).await;
    }

    #[tokio::test]
    async fn app_messages_flow_across_the_link() {
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let (s_ch, s_to, _s_from, mut s_events) = channels();
        let (c_ch, _c_to, mut c_from, mut c_events) = channels();

        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            cancel.clone(),
            s_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            cancel.clone(),
            c_ch,
                LinkHealth::new(),
            ));

        assert!(wait_connected(&mut s_events).await);
        assert!(wait_connected(&mut c_events).await);

        // Server sends a mouse move; client must receive it (heartbeats filtered).
        s_to.send(Message::MouseMove { dx: 5, dy: -3 }).unwrap();
        let got = timeout(Duration::from_secs(2), c_from.recv()).await.unwrap();
        assert_eq!(got, Some(Message::MouseMove { dx: 5, dy: -3 }));

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
        let _ = timeout(Duration::from_secs(2), client).await;
    }

    #[tokio::test]
    async fn client_reconnects_after_server_restart() {
        // The Phase 7 headline: a client survives the server going away and comes
        // back on its own when the server returns — no manual restart.
        let port = free_port().await;
        let client_cancel = CancellationToken::new();

        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            client_cancel.clone(),
            c_ch,
                LinkHealth::new(),
            ));

        // First server: connect, confirm, then drop it.
        let s1_cancel = CancellationToken::new();
        let (s1_ch, _s1_to, _s1_from, mut s1_events) = channels();
        let server1 = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            s1_cancel.clone(),
            s1_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));
        assert!(wait_connected(&mut s1_events).await, "server1 sees the client");
        assert!(wait_for_connect(&mut c_events, Duration::from_secs(3)).await, "initial connect");

        // Kill the server. The client must NOT exit — it should start retrying.
        s1_cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server1).await;

        // Bring a fresh server up on the same port after a beat of downtime
        // (the client is backing off and polling the whole time).
        tokio::time::sleep(Duration::from_millis(150)).await;
        let s2_cancel = CancellationToken::new();
        let (s2_ch, _s2_to, _s2_from, mut s2_events) = channels();
        let server2 = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            s2_cancel.clone(),
            s2_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));

        // The client reconnects on its own.
        assert!(wait_connected(&mut s2_events).await, "server2 sees the reconnect");
        assert!(
            wait_for_connect(&mut c_events, Duration::from_secs(3)).await,
            "client reconnected without intervention"
        );

        client_cancel.cancel();
        s2_cancel.cancel();
        let _ = timeout(Duration::from_secs(2), client).await;
        let _ = timeout(Duration::from_secs(2), server2).await;
    }

    #[tokio::test]
    async fn client_stops_when_reconnect_disabled() {
        // With reconnect off, a failed connect (no server) ends the client promptly
        // instead of looping — preserving the old one-shot behaviour.
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let mut cfg = test_cfg(Role::Client, port, false);
        cfg.reconnect = false;

        let (c_ch, _c_to, _c_from, _c_events) = channels();
        let client = tokio::spawn(run_client(cfg, fast_params(), cancel.clone(), c_ch,
                LinkHealth::new(),
            ));

        // No server is listening, so the connect fails and the client returns.
        timeout(Duration::from_secs(2), client)
            .await
            .expect("client exits without reconnect")
            .unwrap();
    }

    #[tokio::test]
    async fn client_keeps_retrying_until_server_appears() {
        // Start the client first against a dead port; it must keep polling and
        // connect once a server finally shows up.
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            cancel.clone(),
            c_ch,
                LinkHealth::new(),
            ));

        // Let it fail a few times first.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(
            test_cfg(Role::Server, port, false),
            fast_params(),
            cancel.clone(),
            s_ch,
                BulkServer::disabled(),
                LinkHealth::new(),
            ));

        assert!(wait_connected(&mut s_events).await, "server sees the waiting client");
        assert!(
            wait_for_connect(&mut c_events, Duration::from_secs(3)).await,
            "client connected once the server appeared"
        );

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), client).await;
        let _ = timeout(Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn the_race_takes_the_address_that_answers_and_ignores_a_dead_one() {
        let dead = free_port().await; // nothing listens here, so the connect is refused
        let live = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = live.local_addr().unwrap();
        let targets = [
            SocketAddr::from(([127, 0, 0, 1], dead)),
            live_addr,
        ];
        let got = race_connect(&targets, Duration::from_secs(2), crate::config::NetworkPreference::Fastest).await;
        assert_eq!(got.map(|(a, _)| a), Some(live_addr));
    }

    #[tokio::test]
    async fn a_server_listening_everywhere_also_answers_on_ipv6() {
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let mut server_cfg = test_cfg(Role::Server, port, false);
        server_cfg.listen_host = "0.0.0.0".into();
        let (s_ch, _s_to, _s_from, _s_events) = channels();
        let server = tokio::spawn(run_server(
            server_cfg,
            fast_params(),
            cancel.clone(),
            s_ch,
            BulkServer::disabled(),
            LinkHealth::new(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cfg = test_cfg(Role::Client, port, false);
        // Called directly, not through run_client, so load the client's pairings here.
        crate::trust::load(&cfg.device_id, &cfg.paired);
        let to: SocketAddr = format!("[::1]:{port}").parse().unwrap();
        let got = connect_and_handshake(&cfg, Some(to), port, Lane::Input, Duration::from_secs(2), &cancel).await;
        assert!(got.is_ok(), "IPv6 connect and handshake: {:?}", got.err());
        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn a_beacon_is_heard_over_ipv6_link_local_with_its_interface() {
        // The hotspot case (26 Oct 2026): a network that gave no IPv4 address. Needs an
        // interface with a link-local address, so it is skipped on a machine without one.
        if crate::discover::link_local_v6_ifaces().is_empty() {
            return;
        }
        let disc = free_port().await;
        let cancel = CancellationToken::new();
        let found = beacon::Found::new();
        // A server and a client paired with each other.
        let (srv, cli) = (format!("bsrv-{disc}"), format!("bcli-{disc}"));
        crate::trust::load(&srv, &[pairing(cli.clone(), "cli", "k")]);
        crate::trust::load(&cli, &[pairing(srv.clone(), "srv", "k")]);
        tokio::spawn(beacon::run_listener(cli, disc, found.clone(), cancel.child_token()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::spawn(beacon::run_sender(srv, "srv".into(), 4321, disc, cancel.child_token()));
        let mut heard = None;
        for _ in 0..40 {
            heard = found
                .candidates(std::time::Instant::now())
                .into_iter()
                .find(|a| matches!(a, SocketAddr::V6(v6) if v6.scope_id() != 0));
            if heard.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        cancel.cancel();
        let heard = heard.expect("a link-local candidate with its interface");
        assert_eq!(heard.port(), 4321);
    }

    #[tokio::test]
    async fn a_client_with_no_address_configured_finds_the_server_by_beacon() {
        // The server is reached only through what it beacons. No address is configured on
        // the client. Needs a real network interface to broadcast on, so it is skipped
        // on a machine without one.
        if crate::discover::local_ipv4s().is_empty() {
            return;
        }
        let port = free_port().await;
        let disc = free_port().await;
        let cancel = CancellationToken::new();

        let mut server_cfg = test_cfg(Role::Server, port, false);
        server_cfg.listen_host = "0.0.0.0".into();
        server_cfg.discovery_port = disc;
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(
            server_cfg,
            fast_params(),
            cancel.clone(),
            s_ch,
            BulkServer::disabled(),
            LinkHealth::new(),
        ));

        let mut client_cfg = test_cfg(Role::Client, port, false);
        client_cfg.server_host = String::new();
        client_cfg.discovery_port = disc;
        let (c_ch, _c_to, _c_from, mut c_events) = channels();
        let client = tokio::spawn(run_client(
            client_cfg,
            fast_params(),
            cancel.clone(),
            c_ch,
            LinkHealth::new(),
        ));

        assert!(
            wait_for_connect(&mut c_events, Duration::from_secs(10)).await,
            "the client found the server without being given an address"
        );
        assert!(wait_connected(&mut s_events).await, "the server sees the client");

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), client).await;
        let _ = timeout(Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn a_newer_connection_from_the_same_peer_replaces_a_stale_session() {
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let mut server_cfg = test_cfg(Role::Server, port, false);
        server_cfg.discovery_port = 0;
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(
            server_cfg,
            fast_params(),
            cancel.clone(),
            s_ch,
            BulkServer::disabled(),
            LinkHealth::new(),
        ));

        // The first client never reconnects, so it cannot supersede the second in turn.
        let old_cancel = CancellationToken::new();
        let mut old_cfg = test_cfg(Role::Client, port, false);
        old_cfg.reconnect = false;
        let (o_ch, _o_to, _o_from, mut o_events) = channels();
        let old_client = tokio::spawn(run_client(
            old_cfg,
            fast_params(),
            old_cancel.clone(),
            o_ch,
            LinkHealth::new(),
        ));
        assert!(wait_for_connect(&mut o_events, Duration::from_secs(3)).await);
        assert!(wait_connected(&mut s_events).await, "the server holds the first session");

        // A second client arrives while that session is still live.
        let (n_ch, _n_to, _n_from, mut n_events) = channels();
        let new_client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            cancel.clone(),
            n_ch,
            LinkHealth::new(),
        ));
        assert!(
            wait_for_connect(&mut n_events, Duration::from_secs(3)).await,
            "the new connection is accepted instead of refused"
        );

        // The server reports that the old session was replaced.
        let mut superseded = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match timeout(Duration::from_millis(200), s_events.recv()).await {
                Ok(Some(LinkEvent::Disconnected(DisconnectReason::Superseded))) => {
                    superseded = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => {}
            }
        }
        assert!(superseded, "the stale session was reported as superseded");

        old_cancel.cancel();
        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), old_client).await;
        let _ = timeout(Duration::from_secs(2), new_client).await;
        let _ = timeout(Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn a_network_change_wakes_a_waiting_session() {
        let (tx, rx) = watch::channel(0u64);
        let mut net = Some(rx);
        let waiter = tokio::spawn(async move { network_moved(&mut net).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "nothing changed, so nothing should wake");
        tx.send_modify(|e| *e += 1);
        assert!(
            timeout(Duration::from_secs(1), waiter).await.is_ok(),
            "a change on the network watch wakes the session"
        );
    }

    #[tokio::test]
    async fn the_server_is_told_when_the_client_is_at_its_login_screen() {
        let port = free_port().await;
        let cancel = CancellationToken::new();
        let mut server_cfg = test_cfg(Role::Server, port, false);
        server_cfg.discovery_port = 0;
        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let server = tokio::spawn(run_server(
            server_cfg,
            fast_params(),
            cancel.clone(),
            s_ch,
            BulkServer::disabled(),
            LinkHealth::new(),
        ));

        let mut client_cfg = test_cfg(Role::Client, port, false);
        client_cfg.at_login_screen = true;
        let (c_ch, _c_to, _c_from, _c_events) = channels();
        let client = tokio::spawn(run_client(
            client_cfg,
            fast_params(),
            cancel.clone(),
            c_ch,
            LinkHealth::new(),
        ));

        let mut flagged = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && flagged.is_none() {
            if let Ok(Some(LinkEvent::Connected { login_screen, .. })) =
                timeout(Duration::from_millis(200), s_events.recv()).await
            {
                flagged = Some(login_screen);
            }
        }
        assert_eq!(flagged, Some(true), "the server learns the peer is at its login screen");

        cancel.cancel();
        let _ = timeout(Duration::from_secs(2), client).await;
        let _ = timeout(Duration::from_secs(2), server).await;
    }
}
