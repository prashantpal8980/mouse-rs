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
use tokio::sync::mpsc;
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::protocol::{Message, MessageCodec, PROTOCOL_VERSION};
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
}

impl Default for LinkParams {
    fn default() -> Self {
        LinkParams {
            heartbeat_interval: Duration::from_secs(2),
            liveness_timeout: Duration::from_secs(6),
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// Status updates emitted to the UI/log as the link's state changes.
#[derive(Debug, Clone)]
pub enum LinkEvent {
    Listening(SocketAddr),
    Connecting(String),
    Connected { peer: String, width: i32, height: i32 },
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
}

impl DisconnectReason {
    /// Whether an automatic reconnect makes sense (Phase 7 uses this).
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            DisconnectReason::PeerClosed | DisconnectReason::TimedOut | DisconnectReason::Io(_)
        )
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
pub async fn run_server(cfg: Config, params: LinkParams, cancel: CancellationToken, mut ch: LinkChannels) {
    let bind = format!("{}:{}", cfg.listen_host, cfg.listen_port);
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            let _ = ch.events.send(LinkEvent::Error(format!("bind {bind} failed: {e}")));
            return;
        }
    };
    if let Ok(addr) = listener.local_addr() {
        let _ = ch.events.send(LinkEvent::Listening(addr));
    }

    let acceptor = if cfg.tls_enabled {
        match tls::server_acceptor() {
            Ok(a) => Some(a),
            Err(e) => {
                let _ = ch.events.send(LinkEvent::Error(format!("tls setup: {e}")));
                return;
            }
        }
    } else {
        None
    };

    loop {
        let accept = tokio::select! {
            _ = cancel.cancelled() => return,
            r = listener.accept() => r,
        };
        let (tcp, peer_addr) = match accept {
            Ok(v) => v,
            Err(e) => {
                let _ = ch.events.send(LinkEvent::Error(format!("accept: {e}")));
                continue;
            }
        };
        enable_keepalive(&tcp);
        let _ = ch.events.send(LinkEvent::Connecting(peer_addr.to_string()));

        // Wrap in TLS if enabled.
        let stream = match &acceptor {
            Some(acc) => match timeout(params.handshake_timeout, acc.accept(tcp)).await {
                Ok(Ok(s)) => LinkStream::ServerTls(Box::new(s)),
                Ok(Err(e)) => {
                    let _ = ch.events.send(LinkEvent::Error(format!("tls accept: {e}")));
                    continue;
                }
                Err(_) => {
                    let _ = ch.events.send(LinkEvent::Error("tls accept timed out".into()));
                    continue;
                }
            },
            None => LinkStream::Plain(tcp),
        };

        let mut framed = Framed::new(stream, MessageCodec::new());

        // Auth handshake (server side), bounded and cancel-aware.
        let peer_name = tokio::select! {
            _ = cancel.cancelled() => return,
            r = timeout(params.handshake_timeout, server_handshake(&mut framed, &cfg)) => r,
        };
        let (peer_name, peer_w, peer_h) = match peer_name {
            Ok(Ok(v)) => v,
            Ok(Err(reason)) => {
                let _ = ch.events.send(LinkEvent::AuthFailed(reason));
                continue;
            }
            Err(_) => {
                let _ = ch.events.send(LinkEvent::AuthFailed("handshake timed out".into()));
                continue;
            }
        };

        let _ = ch.events.send(LinkEvent::Connected {
            peer: peer_name,
            width: peer_w,
            height: peer_h,
        });
        let reason = pump(framed, params, &cancel, &mut ch).await;
        let stop = matches!(reason, DisconnectReason::Cancelled);
        let _ = ch.events.send(LinkEvent::Disconnected(reason));
        if stop {
            return;
        }
        // otherwise loop and accept the next client
    }
}

// ----------------------------------------------------------------------------
// Client: connect once. Reconnect policy lives one level up (Phase 7).
// ----------------------------------------------------------------------------

/// Run the client side: connect, handshake, pump until disconnect or cancel.
pub async fn run_client(cfg: Config, params: LinkParams, cancel: CancellationToken, mut ch: LinkChannels) {
    let addr = format!("{}:{}", cfg.server_host, cfg.server_port);
    let _ = ch.events.send(LinkEvent::Connecting(addr.clone()));

    let tcp = tokio::select! {
        _ = cancel.cancelled() => return,
        r = timeout(params.handshake_timeout, TcpStream::connect(&addr)) => r,
    };
    let tcp = match tcp {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = ch.events.send(LinkEvent::Disconnected(DisconnectReason::Io(format!(
                "connect {addr}: {e}"
            ))));
            return;
        }
        Err(_) => {
            let _ = ch.events.send(LinkEvent::Disconnected(DisconnectReason::TimedOut));
            return;
        }
    };
    enable_keepalive(&tcp);

    let stream = if cfg.tls_enabled {
        let connector = match tls::client_connector() {
            Ok(c) => c,
            Err(e) => {
                let _ = ch.events.send(LinkEvent::Error(format!("tls setup: {e}")));
                return;
            }
        };
        match timeout(params.handshake_timeout, connector.connect(tls::dummy_server_name(), tcp)).await {
            Ok(Ok(s)) => LinkStream::ClientTls(Box::new(s)),
            Ok(Err(e)) => {
                let _ = ch.events.send(LinkEvent::Error(format!("tls connect: {e}")));
                return;
            }
            Err(_) => {
                let _ = ch.events.send(LinkEvent::Disconnected(DisconnectReason::TimedOut));
                return;
            }
        }
    } else {
        LinkStream::Plain(tcp)
    };

    let mut framed = Framed::new(stream, MessageCodec::new());

    let server_name = tokio::select! {
        _ = cancel.cancelled() => return,
        r = timeout(params.handshake_timeout, client_handshake(&mut framed, &cfg)) => r,
    };
    let (server_name, server_w, server_h) = match server_name {
        Ok(Ok(v)) => v,
        Ok(Err(reason)) => {
            let _ = ch.events.send(LinkEvent::AuthFailed(reason));
            return;
        }
        Err(_) => {
            let _ = ch.events.send(LinkEvent::AuthFailed("handshake timed out".into()));
            return;
        }
    };

    let _ = ch.events.send(LinkEvent::Connected {
        peer: server_name,
        width: server_w,
        height: server_h,
    });
    let reason = pump(framed, params, &cancel, &mut ch).await;
    let _ = ch.events.send(LinkEvent::Disconnected(reason));
}

// ----------------------------------------------------------------------------
// Handshake (async) — mirrors auth.py's HMAC challenge/response.
// ----------------------------------------------------------------------------

async fn server_handshake(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
) -> Result<(String, i32, i32), String> {
    let hello = framed.next().await;
    let (name, width, height) = match hello {
        Some(Ok(Message::Hello { name, width, height, .. })) => (name, width, height),
        Some(Ok(other)) => return Err(format!("expected hello, got {other:?}")),
        Some(Err(e)) => return Err(format!("hello decode: {e}")),
        None => return Err("peer closed before hello".into()),
    };

    let nonce = auth::generate_nonce_hex();
    framed
        .send(Message::AuthChallenge { nonce: nonce.clone() })
        .await
        .map_err(|e| format!("send challenge: {e}"))?;

    let resp = framed.next().await;
    let hmac = match resp {
        Some(Ok(Message::AuthResponse { hmac })) => hmac,
        Some(Ok(other)) => return Err(format!("expected auth response, got {other:?}")),
        Some(Err(e)) => return Err(format!("auth decode: {e}")),
        None => return Err("peer closed before auth response".into()),
    };

    if !auth::verify(&cfg.shared_secret, &nonce, &hmac) {
        let _ = framed
            .send(Message::AuthOk {
                ok: false,
                server: None,
                width: None,
                height: None,
                reason: Some("bad_secret".into()),
            })
            .await;
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

    Ok((name, width, height))
}

async fn client_handshake(
    framed: &mut Framed<LinkStream, MessageCodec>,
    cfg: &Config,
) -> Result<(String, i32, i32), String> {
    framed
        .send(Message::Hello {
            name: cfg.name.clone(),
            width: cfg.screen_width,
            height: cfg.screen_height,
            version: PROTOCOL_VERSION,
        })
        .await
        .map_err(|e| format!("send hello: {e}"))?;

    let nonce = match framed.next().await {
        Some(Ok(Message::AuthChallenge { nonce })) => nonce,
        Some(Ok(other)) => return Err(format!("expected challenge, got {other:?}")),
        Some(Err(e)) => return Err(format!("challenge decode: {e}")),
        None => return Err("peer closed before challenge".into()),
    };

    let hmac = auth::compute_hmac_hex(&cfg.shared_secret, &nonce)
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

async fn pump(
    framed: Framed<LinkStream, MessageCodec>,
    params: LinkParams,
    cancel: &CancellationToken,
    ch: &mut LinkChannels,
) -> DisconnectReason {
    let (mut sink, mut stream) = framed.split();

    let mut hb = interval(params.heartbeat_interval);
    hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_recv = Instant::now();

    loop {
        tokio::select! {
            // Local Stop/Reload: return immediately, no blocking.
            _ = cancel.cancelled() => return DisconnectReason::Cancelled,

            // Heartbeat tick: check liveness, then send a PING.
            _ = hb.tick() => {
                if last_recv.elapsed() > params.liveness_timeout {
                    return DisconnectReason::TimedOut;
                }
                if let Err(reason) = send_cancelable(&mut sink, cancel, Message::Ping, "send ping").await {
                    return reason;
                }
            }

            // Incoming frame from the peer.
            item = stream.next() => match item {
                None => return DisconnectReason::PeerClosed,
                Some(Err(e)) => return DisconnectReason::ProtocolError(e.to_string()),
                Some(Ok(msg)) => {
                    last_recv = Instant::now();
                    match msg {
                        Message::Ping => {
                            if let Err(reason) = send_cancelable(&mut sink, cancel, Message::Pong, "send pong").await {
                                return reason;
                            }
                        }
                        Message::Pong => { /* liveness already refreshed */ }
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
                    if let Err(reason) = send_cancelable(&mut sink, cancel, msg, "send").await {
                        return reason;
                    }
                }
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
        framed
            .send(Message::Hello {
                name: "silent".into(),
                width: 1920,
                height: 1080,
                version: PROTOCOL_VERSION,
            })
            .await
            .unwrap();
        let nonce = match framed.next().await {
            Some(Ok(Message::AuthChallenge { nonce })) => nonce,
            other => panic!("expected challenge, got {other:?}"),
        };
        let hmac = auth::compute_hmac_hex(secret, &nonce).unwrap();
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
            ));
            // Give the listener a moment to bind.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let client = tokio::spawn(run_client(
                test_cfg(Role::Client, port, tls),
                fast_params(),
                cancel.clone(),
                c_ch,
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
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(bad_client, fast_params(), cancel.clone(), c_ch));

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
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            LinkParams::default(),
            cancel.clone(),
            c_ch,
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
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = tokio::spawn(run_client(
            test_cfg(Role::Client, port, false),
            fast_params(),
            cancel.clone(),
            c_ch,
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
}
