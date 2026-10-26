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

use crate::config::Config;
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
pub async fn run_server(
    cfg: Config,
    params: LinkParams,
    cancel: CancellationToken,
    ch: LinkChannels,
    bulk: BulkServer,
    health: Arc<LinkHealth>,
) {
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
        cfg.shared_secret.clone(),
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

    // The input session runs as a *task* rather than inline, so the accept loop keeps
    // running while a session is live. That is what lets the bulk lane connect on this
    // same port at any moment instead of having to arrive in a particular order.
    //
    // `pump` itself is untouched — only the orchestration around it changed. The
    // channels can only belong to one session at a time, so they are handed to the
    // session task and handed back when it ends; `ch_slot` being `None` is precisely
    // "an input session is live".
    let mut ch_slot: Option<LinkChannels> = Some(ch);
    let mut session: Option<tokio::task::JoinHandle<(LinkChannels, DisconnectReason)>> = None;
    // Cancels the live session on its own, so a newer connection can replace it.
    let mut session_cancel: Option<CancellationToken> = None;
    let events = link_events.clone();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            // An input session ended: take the channels back so the next client can be
            // served, and surface the reason exactly as the inline version did.
            joined = async { session.as_mut().expect("guarded by the `if`").await },
                if session.is_some() =>
            {
                session = None;
                match joined {
                    Ok((returned, reason)) => {
                        let stop = matches!(reason, DisconnectReason::Cancelled);
                        let _ = returned.events.send(LinkEvent::Disconnected(reason));
                        ch_slot = Some(returned);
                        if stop {
                            break;
                        }
                    }
                    Err(e) => {
                        // The session task panicked. Without the channels back there is
                        // no way to serve another client, so stop rather than spin.
                        let _ = events.send(LinkEvent::Error(format!("session task failed: {e}")));
                        break;
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

                // Wrap in TLS if enabled.
                let stream = match &acceptor {
                    Some(acc) => match timeout(params.handshake_timeout, acc.accept(tcp)).await {
                        Ok(Ok(s)) => LinkStream::ServerTls(Box::new(s)),
                        Ok(Err(e)) => {
                            let _ = events.send(LinkEvent::Error(format!("tls accept: {e}")));
                            continue;
                        }
                        Err(_) => {
                            let _ = events.send(LinkEvent::Error("tls accept timed out".into()));
                            continue;
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
                    _ = cancel.cancelled() => break,
                    r = timeout(params.handshake_timeout, server_handshake(&mut framed, &cfg)) => r,
                };
                let (peer_name, peer_w, peer_h, lane, peer_login_screen) = match shook {
                    Ok(Ok(v)) => v,
                    Ok(Err(reason)) => {
                        let _ = events.send(LinkEvent::AuthFailed(reason));
                        continue;
                    }
                    Err(_) => {
                        let _ = events.send(LinkEvent::AuthFailed("handshake timed out".into()));
                        continue;
                    }
                };

                match lane {
                    Lane::Bulk => {
                        // Bulk connections are handed off and the loop continues. A new
                        // one replaces whatever came before: the only reason a second
                        // arrives is that the peer reconnected it.
                        bulk.take_server_connection(framed, &cancel);
                    }
                    Lane::Input => {
                        crate::qos::prioritise_input(framed.get_ref().tcp());
                        // A new input connection that has passed the handshake is from the same
                        // trusted peer, now on a newer path. It replaces a session still open on
                        // the old one, rather than being refused until that session times out.
                        if session.is_some() {
                            if let Some(old) = session_cancel.take() {
                                old.cancel();
                            }
                            if let Some(h) = session.take() {
                                if let Ok((returned, _reason)) = h.await {
                                    let _ = returned.events.send(LinkEvent::Disconnected(DisconnectReason::Superseded));
                                    ch_slot = Some(returned);
                                }
                            }
                        }
                        let Some(mut owned) = ch_slot.take() else {
                            // One input client at a time. Refusing immediately is kinder
                            // than accepting into a queue the peer will time out on; it
                            // retries with backoff and connects once the stale session
                            // hits its liveness timeout.
                            tracing::warn!(
                                peer = %peer_name,
                                "refusing a second input connection; one is already live"
                            );
                            continue;
                        };
                        tracing::info!(
                            peer = %peer_name,
                            "input link from {peer_addr} arrived through {}",
                            local_path(local_addr)
                        );
                        let _ = owned.events.send(LinkEvent::Connecting(peer_addr.to_string()));
                        let _ = owned.events.send(LinkEvent::Connected {
                            peer: peer_name,
                            width: peer_w,
                            height: peer_h,
                            login_screen: peer_login_screen,
                        });
                        let sock = if peer_addr.is_ipv6() { &motion_sock6 } else { &motion_sock };
                        let motion = match (sock, framed.get_ref().motion_key()) {
                            (Some(sock), Some(key)) => Motion::Server(ServerMotion::new(sock.clone(), key)),
                            _ => Motion::Off,
                        };
                        let c = cancel.child_token();
                        session_cancel = Some(c.clone());
                        let h = health.clone();
                        session = Some(tokio::spawn(async move {
                            let reason = pump(framed, params, &c, &mut owned, &h, None, motion).await;
                            (owned, reason)
                        }));
                    }
                }
            }
        }
    }

    // Cancelled: let a live session finish its own bounded teardown, then report it.
    if let Some(handle) = session {
        if let Ok((returned, reason)) = handle.await {
            let _ = returned.events.send(LinkEvent::Disconnected(reason));
        }
    }
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
    let found = beacon::Found::new();
    let (epoch_tx, epoch_rx) = watch::channel(0u64);
    tokio::spawn(beacon::run_listener(
        cfg.shared_secret.clone(),
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
        for a in found.candidates(std::time::Instant::now()) {
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
        let wait = if no_network { NO_NETWORK_POLL } else { backoff };
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
            r = race_connect(&group, CONNECT_TRY) => r,
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
        "link to {addr} goes out through {} (candidates tried: {listing})",
        local_path(tcp.local_addr().ok())
    );
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
    let (server_name, server_w, server_h) = match server_name {
        Ok(Ok(v)) => v,
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
        Some(key) => Motion::Client { key, server: addr, live: None },
        None => Motion::Off,
    };
    let reason = pump(framed, params, cancel, ch, health, net, motion).await;
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
            // The same shared secret guards this port as the input one; a connection
            // that authenticates but asks for the input lane is refused here, because
            // input has exactly one home and it is not this listener.
            Ok(Ok((_, _, _, Lane::Bulk, _))) => bulk.take_server_connection(framed, &cancel),
            Ok(Ok((name, _, _, Lane::Input, _))) => {
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

async fn server_handshake(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
) -> Result<(String, i32, i32, Lane, bool), String> {
    let hello = framed.next().await;
    let (name, width, height, lane, login_screen, version, client_nonce) = match hello {
        Some(Ok(Message::Hello { name, width, height, lane, login_screen, version, nonce })) => {
            (name, width, height, lane, login_screen, version, nonce)
        }
        Some(Ok(other)) => return Err(format!("expected hello, got {other:?}")),
        Some(Err(e)) => return Err(format!("hello decode: {e}")),
        None => return Err("peer closed before hello".into()),
    };
    // A version 1 peer can't prove anything about this session: refuse it rather than
    // fall back, or anyone could downgrade a connection to the one-way check (B34).
    let client_nonce = match client_nonce {
        Some(n) if version >= PROTOCOL_VERSION => n,
        _ => {
            let _ = framed.send(refusal("old_version")).await;
            return Err(format!(
                "{name} runs an older mouse-rs (protocol {version}); update both machines"
            ));
        }
    };

    let binding = framed.get_ref().auth_binding()?;
    let nonce = auth::generate_nonce_hex();
    let proof = auth::proof_hex(&cfg.shared_secret, Side::Server, &binding, &client_nonce, &nonce)
        .map_err(|e| format!("bad nonce from {name}: {e}"))?;
    framed
        .send(Message::AuthChallenge { nonce: nonce.clone(), proof: Some(proof) })
        .await
        .map_err(|e| format!("send challenge: {e}"))?;

    let resp = framed.next().await;
    let hmac = match resp {
        Some(Ok(Message::AuthResponse { hmac })) => hmac,
        Some(Ok(other)) => return Err(format!("expected auth response, got {other:?}")),
        Some(Err(e)) => return Err(format!("auth decode: {e}")),
        None => return Err("peer closed before auth response".into()),
    };

    if !auth::verify_proof(&cfg.shared_secret, Side::Client, &binding, &client_nonce, &nonce, &hmac)
    {
        let _ = framed.send(refusal("bad_secret")).await;
        return Err(format!("bad secret from {name}"));
    }

    framed
        .send(Message::AuthOk {
            ok: true,
            server: Some(cfg.name.clone()),
            width: Some(cfg.screen_width),
            height: Some(cfg.screen_height),
            reason: None,
        })
        .await
        .map_err(|e| format!("send auth ok: {e}"))?;

    Ok((name, width, height, lane, login_screen))
}

fn refusal(reason: &str) -> Message {
    Message::AuthOk { ok: false, server: None, width: None, height: None, reason: Some(reason.into()) }
}

async fn client_handshake(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
    lane: Lane,
) -> Result<(String, i32, i32), String> {
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
        })
        .await
        .map_err(|e| format!("send hello: {e}"))?;

    let (nonce, proof) = match framed.next().await {
        Some(Ok(Message::AuthChallenge { nonce, proof })) => (nonce, proof),
        Some(Ok(Message::AuthOk { ok: false, reason, .. })) => {
            return Err(reason.unwrap_or_else(|| "rejected".into()))
        }
        Some(Ok(other)) => return Err(format!("expected challenge, got {other:?}")),
        Some(Err(e)) => return Err(format!("challenge decode: {e}")),
        None => return Err("peer closed before challenge".into()),
    };

    // The server proves itself first; nothing about our secret is sent to a server
    // that can't (B34).
    let Some(proof) = proof else {
        return Err("old_version".into());
    };
    if !auth::verify_proof(&cfg.shared_secret, Side::Server, &binding, &client_nonce, &nonce, &proof)
    {
        return Err("server_not_verified".into());
    }

    let hmac = auth::proof_hex(&cfg.shared_secret, Side::Client, &binding, &client_nonce, &nonce)
        .map_err(|e| format!("hmac: {e}"))?;
    framed
        .send(Message::AuthResponse { hmac })
        .await
        .map_err(|e| format!("send auth response: {e}"))?;

    match framed.next().await {
        Some(Ok(Message::AuthOk { ok: true, server, width, height, .. })) => Ok((
            server.unwrap_or_else(|| "server".into()),
            width.unwrap_or(0),
            height.unwrap_or(0),
        )),
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

/// How long one candidate address gets to accept the TCP connection.
const CONNECT_TRY: Duration = Duration::from_secs(3);

/// How often a client with no network re-checks for one.
const NO_NETWORK_POLL: Duration = Duration::from_secs(2);

/// Connect to every candidate at once and keep the first that accepts. Racing, rather than
/// trying in turn, means a dead address costs nothing. It also picks the faster path: when the
/// server is reachable over both Ethernet and Wi-Fi, the one that answers first has less delay.
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

async fn race_connect(targets: &[SocketAddr], per: Duration) -> Option<(SocketAddr, TcpStream)> {
    let mut attempts: FuturesUnordered<_> = targets
        .iter()
        .map(|a| async move { (*a, timeout(per, TcpStream::connect(a)).await) })
        .collect();
    while let Some((addr, result)) = attempts.next().await {
        if let Ok(Ok(stream)) = result {
            return Some((addr, stream));
        }
    }
    None
}

/// Pointer motion over UDP for this session (see `motion_udp`).
enum Motion {
    /// No UDP: plain link, or the socket couldn't be opened. Motion goes over TCP.
    Off,
    Server(ServerMotion),
    Client {
        key: crate::motion_udp::Key,
        server: SocketAddr,
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
        tokio::select! {
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
                if let Motion::Client { live: Some(m), .. } = &motion {
                    m.hello().await;
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
                            if let Motion::Client { key, server, live } = &mut motion {
                                match ClientMotion::start(*server, port, &token, key.clone()).await {
                                    Ok(m) => {
                                        tracing::info!("pointer motion will arrive over UDP from {server} (port {port})");
                                        *live = Some(m);
                                    }
                                    Err(e) => tracing::warn!("UDP to {server} (port {port}) unavailable ({e}); pointer motion stays on TCP"),
                                }
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

    fn test_cfg(role: Role, port: u16, tls: bool) -> Config {
        Config {
            name: match role {
                Role::Server => "test-server".into(),
                Role::Client => "test-client".into(),
            },
            role,
            shared_secret: "test-secret".into(),
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
    async fn silent_client_after_handshake(port: u16, secret: &str) {
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
            })
            .await
            .unwrap();
        let nonce = match framed.next().await {
            Some(Ok(Message::AuthChallenge { nonce, .. })) => nonce,
            other => panic!("expected challenge, got {other:?}"),
        };
        let hmac = auth::proof_hex(secret, Side::Client, &[], &client_nonce, &nonce).unwrap();
        framed.send(Message::AuthResponse { hmac }).await.unwrap();
        let _ = framed.next().await; // consume AuthOk
        // Go silent but keep the socket alive — do not answer pings.
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(framed);
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
    async fn wrong_secret_is_rejected() {
        let port = free_port().await;
        let cancel = CancellationToken::new();

        let (s_ch, _s_to, _s_from, mut s_events) = channels();
        let (c_ch, _c_to, _c_from, mut c_events) = channels();

        let mut bad_client = test_cfg(Role::Client, port, false);
        bad_client.shared_secret = "WRONG".into();

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
                other => panic!("expected a v2 hello, got {other:?}"),
            };
            let nonce = auth::generate_nonce_hex();
            let proof = auth::proof_hex("GUESS", Side::Server, &[], &client_nonce, &nonce).unwrap();
            framed.send(Message::AuthChallenge { nonce, proof: Some(proof) }).await.unwrap();
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

        let client = tokio::spawn(silent_client_after_handshake(port, "test-secret"));

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
        let got = race_connect(&targets, Duration::from_secs(2)).await;
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
        tokio::spawn(beacon::run_listener("s".into(), disc, found.clone(), cancel.child_token()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::spawn(beacon::run_sender("s".into(), "srv".into(), 4321, disc, cancel.child_token()));
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
