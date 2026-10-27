//! Pointer motion over UDP, so one lost Wi-Fi packet can't freeze the pointer.
//!
//! On 26 Oct 2026 the owner's 2.4 GHz Wi-Fi lost or reordered 9.4% of the input
//! connection's packets (Kali's `rcv_ooopack` 2323 of 24677) while a video loaded.
//! Over TCP every lost packet holds back everything after it until it is resent,
//! and Windows waits at least 300 ms to resend: the pointer vanished, then jumped.
//! Priority marking (see `qos`) can't help with loss.
//!
//! So motion takes UDP, and each datagram carries the **running total** of the
//! movement since the session began, not a delta. A lost datagram is simply covered
//! by the next one (about 8–16 ms later), nothing waits, and no movement is lost.
//! Keys, clicks, scroll and everything else stay on the reliable TCP connection.
//! Before any of those goes out, the server sends the current total over TCP too
//! (`MotionSync`), so a click always lands where the pointer was, even if the last
//! datagrams were lost.
//!
//! Setup: the server offers its UDP port and a session token over TCP
//! (`MotionOffer`); the client sends an authenticated hello from its own UDP socket
//! (which also opens its firewall for the replies) and repeats it with every
//! heartbeat. The server sends motion over UDP only while hellos keep arriving;
//! otherwise it falls back to TCP exactly as before. Every datagram is
//! authenticated with a key exported from the session's TLS connection, so nobody
//! else on the network can move the pointer. Without TLS there is no key, and
//! motion stays on TCP.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use tokio::net::UdpSocket;

use crate::protocol::Message;

type HmacSha256 = Hmac<Sha256>;

/// TLS exporter label for the datagram key. Distinct from the auth binding's label.
pub(crate) const KEY_LABEL: &[u8] = b"EXPORTER-mouse-rs-motion";

const HELLO: &[u8; 4] = b"MRH1";
const MOVE: &[u8; 4] = b"MRM2";
const TAG: usize = 16;
const HELLO_LEN: usize = 4 + 16 + TAG;
/// Magic, seq, total x, total y, send time (ms on the sender's clock), tag.
const MOVE_LEN: usize = 4 + 8 + 8 + 8 + 4 + TAG;
/// Room for any datagram we send, with margin.
pub(crate) const MAX_DATAGRAM: usize = 64;

/// After motion stops, the final total is sent again this long after the last move.
const REPEAT_AFTER: [Duration; 2] = [Duration::from_millis(25), Duration::from_millis(80)];

/// Names a file to write one line per received motion datagram to, for timing analysis.
const TRACE_ENV: &str = "MOUSE_RS_MOTION_TRACE";

/// Pointer playback mode on the receiving side: "auto" (default), "auto:<max ms>", or a fixed
/// delay in ms (see `playout::Mode::from_env`).
const SMOOTH_ENV: &str = "MOUSE_RS_SMOOTH";

/// How often each side logs its UDP motion counters.
const STATS_EVERY: Duration = Duration::from_secs(60);

struct ServerStats {
    since: Instant,
    udp: u64,
    tcp: u64,
    hellos: u64,
    readies: u64,
    junk: u64,
}

impl ServerStats {
    fn new() -> ServerStats {
        ServerStats { since: Instant::now(), udp: 0, tcp: 0, hellos: 0, readies: 0, junk: 0 }
    }
}

struct ClientStats {
    since: Instant,
    datagrams: u64,
    rejected: u64,
    guesses: u64,
}

impl ClientStats {
    fn new() -> ClientStats {
        ClientStats { since: Instant::now(), datagrams: 0, rejected: 0, guesses: 0 }
    }
}

/// Moves sent over UDP and not confirmed by the client for this long mean datagrams don't
/// arrive: back to TCP. More than two heartbeats, so a normal confirmation is never late.
const UNCONFIRMED: Duration = Duration::from_secs(5);
/// After that, how long before UDP is tried again.
const RETRY: Duration = Duration::from_secs(60);

/// Without a hello for this long, the server goes back to sending motion over TCP.
/// Hellos come with every heartbeat (2 s), so this tolerates two lost in a row.
const PEER_STALE: Duration = Duration::from_secs(6);

/// The per-session datagram key, exported from the TLS connection.
#[derive(Clone)]
pub(crate) struct Key(pub(crate) [u8; 32]);

fn tag(key: &Key, body: &[u8]) -> [u8; TAG] {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(body);
    let full = mac.finalize().into_bytes();
    let mut t = [0u8; TAG];
    t.copy_from_slice(&full[..TAG]);
    t
}

fn tag_ok(key: &Key, body: &[u8], t: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(body);
    // Constant-time comparison of the truncated tag.
    mac.verify_truncated_left(t).is_ok()
}

fn encode_hello(key: &Key, token: &[u8; 16]) -> [u8; HELLO_LEN] {
    let mut out = [0u8; HELLO_LEN];
    out[..4].copy_from_slice(HELLO);
    out[4..20].copy_from_slice(token);
    let t = tag(key, &out[..20]);
    out[20..].copy_from_slice(&t);
    out
}

fn decode_hello(key: &Key, buf: &[u8]) -> Option<[u8; 16]> {
    if buf.len() != HELLO_LEN || &buf[..4] != HELLO || !tag_ok(key, &buf[..20], &buf[20..]) {
        return None;
    }
    buf[4..20].try_into().ok()
}

fn encode_move(key: &Key, seq: u64, sx: i64, sy: i64, sent_ms: u32) -> [u8; MOVE_LEN] {
    let mut out = [0u8; MOVE_LEN];
    out[..4].copy_from_slice(MOVE);
    out[4..12].copy_from_slice(&seq.to_be_bytes());
    out[12..20].copy_from_slice(&sx.to_be_bytes());
    out[20..28].copy_from_slice(&sy.to_be_bytes());
    out[28..32].copy_from_slice(&sent_ms.to_be_bytes());
    let t = tag(key, &out[..32]);
    out[32..].copy_from_slice(&t);
    out
}

/// (seq, total x, total y, send time in ms on the sender's clock)
fn decode_move(key: &Key, buf: &[u8]) -> Option<(u64, i64, i64, u32)> {
    if buf.len() != MOVE_LEN || &buf[..4] != MOVE || !tag_ok(key, &buf[..32], &buf[32..]) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> [u8; 8] { buf[r].try_into().expect("8 bytes") };
    Some((
        u64::from_be_bytes(n(4..12)),
        i64::from_be_bytes(n(12..20)),
        i64::from_be_bytes(n(20..28)),
        u32::from_be_bytes(buf[28..32].try_into().expect("4 bytes")),
    ))
}

/// Milliseconds since `epoch`, wrapping (a session would need 49 days to wrap).
fn ms_since(epoch: Instant) -> u32 {
    epoch.elapsed().as_millis() as u32
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The server's half: offers UDP, learns the client's address from its hellos, and
/// sends motion there while the hellos keep coming.
pub(crate) struct ServerMotion {
    sock: Arc<UdpSocket>,
    key: Key,
    token: [u8; 16],
    peer: Option<(SocketAddr, Instant)>,
    seq: u64,
    sum: (i64, i64),
    synced: (i64, i64),
    /// The address the TCP link comes from: where to send when the client names its port.
    tcp_peer: SocketAddr,
    /// The latest seq the client says it received, and since when moves have gone
    /// unconfirmed (None when it has everything).
    seen: u64,
    unconfirmed_since: Option<Instant>,
    /// After datagrams were found not to arrive: don't try UDP again before this.
    retry_after: Option<Instant>,
    stats: ServerStats,
    /// Whether the client has ever reported over TCP, and a hello's address held until it does.
    stats_readies_ever: u64,
    pending_hello: Option<SocketAddr>,
    /// When the last move went out, and how many of [`REPEAT_AFTER`] have been sent since.
    last_move: Option<(Instant, SocketAddr)>,
    repeats: usize,
    /// Zero of the send times in the datagrams.
    epoch: Instant,
}

impl ServerMotion {
    pub(crate) fn new(sock: Arc<UdpSocket>, key: Key, tcp_peer: SocketAddr) -> ServerMotion {
        let mut token = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut token);
        ServerMotion {
            sock,
            key,
            token,
            peer: None,
            seq: 0,
            sum: (0, 0),
            synced: (0, 0),
            tcp_peer,
            seen: 0,
            unconfirmed_since: None,
            retry_after: None,
            stats: ServerStats::new(),
            stats_readies_ever: 0,
            pending_hello: None,
            last_move: None,
            repeats: 0,
            epoch: Instant::now(),
        }
    }

    pub(crate) fn socket(&self) -> Arc<UdpSocket> {
        self.sock.clone()
    }

    /// The TCP message that tells the client where to send its hellos.
    pub(crate) fn offer(&self) -> Option<Message> {
        let port = self.sock.local_addr().ok()?.port();
        Some(Message::MotionOffer { port, token: hex(&self.token) })
    }

    pub(crate) fn on_datagram(&mut self, buf: &[u8], from: SocketAddr) {
        if decode_hello(&self.key, buf) != Some(self.token) {
            self.stats.junk += 1;
            return;
        }
        self.stats.hellos += 1;
        self.use_peer(from, "hello");
    }

    /// The client named its UDP port over TCP. Its hellos may never get here (a firewall
    /// blocking inbound UDP), but datagrams sent to it still arrive: its own hello opened
    /// its side.
    pub(crate) fn on_ready(&mut self, port: u16, seen: u64) {
        let now = Instant::now();
        self.stats.readies += 1;
        self.stats_readies_ever += 1;
        if now.duration_since(self.stats.since) >= STATS_EVERY {
            let peer = self.peer.map_or("none".to_string(), |(a, _)| a.to_string());
            tracing::info!(
                "UDP motion over the last minute: {} moves by UDP, {} by TCP; heard {} hellos, \
                 {} reports over TCP, {} junk datagrams; client has {} of {}; sending to {peer}",
                self.stats.udp,
                self.stats.tcp,
                self.stats.hellos,
                self.stats.readies,
                self.stats.junk,
                seen.max(self.seen),
                self.seq
            );
            self.stats = ServerStats::new();
        }
        if seen > self.seen {
            self.seen = seen;
            self.unconfirmed_since = (seen < self.seq).then_some(now);
        }
        let mut to = self.tcp_peer;
        to.set_port(port);
        // A hello that came before the first report: its source is the better address.
        if let Some(hello) = self.pending_hello.take() {
            self.use_peer(hello, "hello");
        }
        // A hello's source wins over the TCP address (it is right behind a NAT too).
        match self.peer {
            Some((_, heard)) if heard.elapsed() < PEER_STALE => self.peer = self.peer.map(|(a, _)| (a, now)),
            _ => self.use_peer(to, "TCP link"),
        }
    }

    fn use_peer(&mut self, to: SocketAddr, how: &str) {
        if self.retry_after.is_some_and(|t| Instant::now() < t) {
            return;
        }
        // Only a client that reports over TCP (MotionReady) can say whether datagrams arrive
        // and reads this datagram format. An older one says hello but would drop every move
        // (the login screen on 27 Oct: motion only came with clicks, seconds apart). It stays
        // on TCP, which every version reads.
        if self.stats_readies_ever == 0 {
            self.pending_hello = Some(to);
            return;
        }
        if self.peer.map(|(a, _)| a) != Some(to) {
            tracing::info!("pointer motion now goes over UDP to {to} (from the {how})");
            crate::qos::prioritise_datagrams(&self.sock, to);
        }
        self.peer = Some((to, Instant::now()));
    }

    /// Send one move over UDP. False when there is no live UDP path: the caller then
    /// sends it over TCP as before.
    pub(crate) fn send_move(&mut self, dx: i32, dy: i32) -> bool {
        let Some((to, heard)) = self.peer else {
            self.stats.tcp += 1;
            return false;
        };
        if heard.elapsed() > PEER_STALE {
            tracing::info!("nothing from {to} for {PEER_STALE:?}; pointer motion back on TCP");
            self.peer = None;
            self.stats.tcp += 1;
            return false;
        }
        if self.unconfirmed_since.is_some_and(|t| t.elapsed() > UNCONFIRMED) {
            tracing::warn!(
                "{to} reports no motion datagrams for {UNCONFIRMED:?} (latest it has: {} of {}); \
                 pointer motion back on TCP, UDP retried in {RETRY:?}",
                self.seen,
                self.seq
            );
            self.peer = None;
            self.unconfirmed_since = None;
            self.retry_after = Some(Instant::now() + RETRY);
            self.stats.tcp += 1;
            return false;
        }
        self.sum.0 += i64::from(dx);
        self.sum.1 += i64::from(dy);
        self.seq += 1;
        // Never waits. A datagram the socket can't take now is a lost datagram, and the
        // next one (or the next `MotionSync`) carries the total anyway.
        let datagram = encode_move(&self.key, self.seq, self.sum.0, self.sum.1, ms_since(self.epoch));
        let _ = self.sock.try_send_to(&datagram, to);
        let now = Instant::now();
        self.last_move = Some((now, to));
        self.repeats = 0;
        self.unconfirmed_since.get_or_insert(now);
        self.stats.udp += 1;
        true
    }

    /// When the final total of the latest motion is next due to be sent again, if it is.
    pub(crate) fn repeat_due(&self) -> Option<Instant> {
        let (at, _) = self.last_move?;
        REPEAT_AFTER.get(self.repeats).map(|d| at + *d)
    }

    /// Send the final total again once motion has stopped. The receiving side may have
    /// predicted the pointer on past where the hand stopped (see `playout`); this puts it back
    /// where the hand stopped, and covers a final datagram that was lost.
    pub(crate) fn send_repeat(&mut self) {
        let Some((_, to)) = self.last_move else { return };
        self.repeats += 1;
        let datagram = encode_move(&self.key, self.seq, self.sum.0, self.sum.1, ms_since(self.epoch));
        let _ = self.sock.try_send_to(&datagram, to);
    }

    /// The running total, if it moved since last said over TCP. Sent ahead of any other
    /// message, so a click or key lands with the pointer where the user put it.
    pub(crate) fn sync(&mut self) -> Option<Message> {
        if self.sum == self.synced {
            return None;
        }
        self.synced = self.sum;
        Some(Message::MotionSync { seq: self.seq, sx: self.sum.0, sy: self.sum.1 })
    }
}

/// The client's half: says hello, and turns running totals back into moves.
pub(crate) struct ClientMotion {
    sock: Arc<UdpSocket>,
    key: Key,
    token: [u8; 16],
    last_seq: u64,
    applied: (i64, i64),
    epoch: Instant,
    delay: crate::timing::DelayLog,
    playout: crate::playout::Playout,
    /// Over the cable: every move goes straight to the pointer, nothing smoothed or guessed
    /// (owner, 27 Oct 2026: "at cable… I don't want it to guess at all"). Smoothing is for
    /// Wi-Fi, where moves arrive late and bunched.
    direct: bool,
    stats: ClientStats,
    /// Diagnostic only: one line per datagram, when `MOUSE_RS_MOTION_TRACE` names a file.
    trace: Option<std::io::BufWriter<std::fs::File>>,
}

impl ClientMotion {
    /// `server` is the address the TCP link reached, with its interface for an IPv6
    /// link-local one; the offered `port` replaces its port.
    pub(crate) async fn start(
        server: SocketAddr,
        port: u16,
        token: &str,
        key: Key,
        on_cable: bool,
    ) -> std::io::Result<ClientMotion> {
        let token = unhex16(token)
            .ok_or_else(|| std::io::Error::other("malformed motion token"))?;
        let any = match server {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        let mut to = server;
        to.set_port(port);
        let sock = UdpSocket::bind((any, 0)).await?;
        sock.connect(to).await?;
        crate::qos::prioritise_datagrams(&sock, to);
        let me = ClientMotion {
            sock: Arc::new(sock),
            key,
            token,
            last_seq: 0,
            applied: (0, 0),
            epoch: Instant::now(),
            delay: Default::default(),
            playout: crate::playout::Playout::new(crate::playout::Mode::from_env(
                std::env::var(SMOOTH_ENV).ok().as_deref(),
            )),
            // The environment setting is for testing, and overrides the cable rule.
            direct: on_cable && std::env::var_os(SMOOTH_ENV).is_none(),
            stats: ClientStats::new(),
            trace: std::env::var_os(TRACE_ENV).and_then(|p| {
                let f = std::fs::OpenOptions::new().create(true).append(true).open(&p).ok()?;
                tracing::info!("tracing every motion datagram to {}", p.to_string_lossy());
                Some(std::io::BufWriter::new(f))
            }),
        };
        if me.direct {
            tracing::info!("pointer playback: direct, every move as it arrives (the link is on the cable)");
        } else {
            tracing::info!("pointer playback: {:?} (set {SMOOTH_ENV} to change)", me.playout.mode());
        }
        me.hello().await;
        Ok(me)
    }

    pub(crate) fn socket(&self) -> Arc<UdpSocket> {
        self.sock.clone()
    }

    /// What to tell the server over TCP: this socket's port, and the latest move received.
    pub(crate) fn ready(&mut self) -> Option<Message> {
        if self.stats.since.elapsed() >= STATS_EVERY {
            tracing::info!(
                "UDP motion over the last minute: {} datagrams in, {} rejected; {} predicted \
                 frames; playback {}, delay now {:.0} ms; latest seq {}",
                self.stats.datagrams,
                self.stats.rejected,
                self.stats.guesses,
                if self.direct { "direct (cable)".to_string() } else { format!("{:?}", self.playout.mode()) },
                self.playout.delay_ms(),
                self.last_seq
            );
            self.stats = ClientStats::new();
        }
        let port = self.sock.local_addr().ok()?.port();
        Some(Message::MotionReady { port, seen: self.last_seq })
    }

    /// (Re)announce this socket to the server. Repeated with every heartbeat. Awaited,
    /// not `try_send`: on a fresh socket tokio doesn't yet know it is writable, and the
    /// first hello would be dropped.
    pub(crate) async fn hello(&self) {
        let _ = self.sock.send(&encode_hello(&self.key, &self.token)).await;
    }

    pub(crate) fn on_datagram(&mut self, buf: &[u8]) -> Option<Message> {
        self.stats.datagrams += 1;
        let Some((seq, sx, sy, sent_ms)) = decode_move(&self.key, buf) else {
            self.stats.rejected += 1;
            return None;
        };
        let now = Instant::now();
        let transit = i64::from(ms_since(self.epoch)) - i64::from(sent_ms);
        if let Some(t) = &mut self.trace {
            use std::io::Write;
            // arrival (µs on this clock), send (ms on the sender's), seq, running total x, y
            let _ = writeln!(t, "{},{},{},{},{}", now.duration_since(self.epoch).as_micros(), sent_ms, seq, sx, sy);
            if seq % 200 == 0 {
                let _ = t.flush();
            }
        }
        if let Some(line) = self.delay.push(transit, now) {
            tracing::info!("{line}");
        }
        if self.direct {
            // Newest wins; one overtaken by a later move is already included in its total.
            if seq <= self.last_seq {
                return None;
            }
            self.last_seq = seq;
            return self.move_to((sx, sy));
        }
        self.last_seq = self.last_seq.max(seq);
        self.playout.push(seq, f64::from(sent_ms), (sx, sy), self.ms(now));
        self.render(now)
    }

    /// Milliseconds since this socket opened, the playback clock.
    fn ms(&self, at: Instant) -> f64 {
        at.duration_since(self.epoch).as_secs_f64() * 1000.0
    }

    /// Move the pointer to where playback says it should be now.
    fn render(&mut self, now: Instant) -> Option<Message> {
        let before = self.playout.predicted;
        let (x, y) = self.playout.target(self.ms(now))?;
        self.stats.guesses += self.playout.predicted - before;
        self.move_to((x.round() as i64, y.round() as i64))
    }

    fn move_to(&mut self, to: (i64, i64)) -> Option<Message> {
        let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let (dx, dy) = (clamp(to.0 - self.applied.0), clamp(to.1 - self.applied.1));
        self.applied = to;
        (dx != 0 || dy != 0).then_some(Message::MouseMove { dx, dy })
    }

    pub(crate) fn on_sync(&mut self, seq: u64, sx: i64, sy: i64) -> Option<Message> {
        // A click or key follows: the pointer goes exactly to the total, now.
        self.last_seq = self.last_seq.max(seq);
        let now = Instant::now();
        if !self.direct {
            self.playout.sync(seq, (sx, sy), self.ms(now));
        }
        self.move_to((sx, sy))
    }

    /// Whether playback may move the pointer without new data (see `playout`).
    pub(crate) fn concealing(&self, now: Instant) -> bool {
        !self.direct && self.playout.active(self.ms(now))
    }

    /// Called every few ms while [`concealing`](Self::concealing).
    pub(crate) fn conceal(&mut self, now: Instant) -> Option<Message> {
        self.render(now)
    }
}

/// Receive one datagram, or never when there is no socket.
pub(crate) async fn recv(
    sock: Option<Arc<UdpSocket>>,
) -> std::io::Result<([u8; MAX_DATAGRAM], usize, SocketAddr)> {
    let Some(sock) = sock else {
        return std::future::pending().await;
    };
    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = sock.recv_from(&mut buf).await?;
    Ok((buf, n, from))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> Key {
        Key([b; 32])
    }

    #[test]
    fn datagrams_round_trip_and_reject_another_key() {
        let k = key(1);
        let m = encode_move(&k, 7, -3, 1200, 99);
        assert_eq!(decode_move(&k, &m), Some((7, -3, 1200, 99)));
        assert_eq!(decode_move(&key(2), &m), None, "another session's key");
        let mut bad = m;
        bad[13] ^= 1;
        assert_eq!(decode_move(&k, &bad), None, "tampered total");
        let h = encode_hello(&k, &[9; 16]);
        assert_eq!(decode_hello(&k, &h), Some([9; 16]));
        assert_eq!(decode_hello(&key(2), &h), None);
    }

    #[tokio::test]
    async fn motion_flows_over_udp_once_the_client_says_hello_and_syncs_before_a_click() {
        flow("127.0.0.1:0").await;
    }

    #[tokio::test]
    async fn motion_flows_over_ipv6_too() {
        // The hotspot fallback connects over IPv6 link-local; motion must not drop to TCP there.
        flow("[::1]:0").await;
    }

    #[tokio::test]
    async fn motion_flows_when_the_servers_firewall_drops_every_hello() {
        // The owner's laptop on Wi-Fi (26 Oct): its firewall let TCP in but not UDP. The
        // client names its port over TCP instead, and the datagrams still reach it.
        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let tcp_peer: SocketAddr = "127.0.0.1:40000".parse().unwrap(); // port is replaced
        let mut s = ServerMotion::new(server_sock.clone(), key(5), tcp_peer);
        let Some(Message::MotionOffer { port, token }) = s.offer() else { panic!("offer") };
        let mut c = ClientMotion::start(server_sock.local_addr().unwrap(), port, &token, key(5), false).await.unwrap();
        // The hello is never read: as if dropped. Only the TCP message arrives.
        let Some(Message::MotionReady { port: cport, seen: 0 }) = c.ready() else { panic!("ready") };
        s.on_ready(cport, 0);
        assert!(s.send_move(7, -2));
        // Skip the hello still waiting on the server's socket; read the move on the client's.
        let (buf, n, _) = recv(Some(c.socket())).await.unwrap();
        assert_eq!(c.on_datagram(&buf[..n]), Some(Message::MouseMove { dx: 7, dy: -2 }));
        let Some(Message::MotionReady { seen: 1, .. }) = c.ready() else { panic!("seen") };
    }

    #[tokio::test]
    async fn on_the_cable_every_move_lands_exactly_and_nothing_is_guessed() {
        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut s = ServerMotion::new(server_sock.clone(), key(7), server_sock.local_addr().unwrap());
        let Some(Message::MotionOffer { port, token }) = s.offer() else { panic!("offer") };
        let mut c = ClientMotion::start(server_sock.local_addr().unwrap(), port, &token, key(7), true).await.unwrap();
        let k = key(7);
        assert_eq!(c.on_datagram(&encode_move(&k, 1, 10, 0, 0)), Some(Message::MouseMove { dx: 10, dy: 0 }));
        assert_eq!(c.on_datagram(&encode_move(&k, 3, 25, 2, 8)), Some(Message::MouseMove { dx: 15, dy: 2 }));
        // Move 2 overtaken by move 3: its total is already in 3's, so it changes nothing.
        assert_eq!(c.on_datagram(&encode_move(&k, 2, 18, 1, 4)), None);
        // However long nothing arrives, the pointer stays exactly where the hand put it.
        assert!(!c.concealing(Instant::now() + Duration::from_millis(50)));
        assert_eq!(c.conceal(Instant::now() + Duration::from_millis(50)), None);
    }

    #[tokio::test]
    async fn moves_the_client_never_confirms_go_back_to_tcp() {
        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut s = ServerMotion::new(server_sock.clone(), key(6), "127.0.0.1:1".parse().unwrap());
        s.on_ready(9, 0); // a port where nobody listens
        assert!(s.send_move(1, 1));
        s.on_ready(9, 0); // the client says it has nothing
        s.unconfirmed_since = Some(Instant::now() - UNCONFIRMED - Duration::from_millis(1));
        assert!(!s.send_move(1, 1), "datagrams don't arrive: TCP");
        s.on_ready(9, 0);
        assert!(!s.send_move(1, 1), "and UDP is not retried at once");
    }

    async fn flow(bind: &str) {
        let server_sock = Arc::new(UdpSocket::bind(bind).await.unwrap());
        let port = server_sock.local_addr().unwrap().port();
        let mut s = ServerMotion::new(server_sock.clone(), key(4), server_sock.local_addr().unwrap());
        assert!(!s.send_move(1, 1), "no hello yet: TCP");

        let Some(Message::MotionOffer { port: p, token }) = s.offer() else { panic!("offer") };
        assert_eq!(p, port);
        let mut c = ClientMotion::start(server_sock.local_addr().unwrap(), p, &token, key(4), false).await.unwrap();
        let (buf, n, from) = recv(Some(server_sock.clone())).await.unwrap();
        s.on_datagram(&buf[..n], from);
        assert!(!s.send_move(0, 0), "a hello alone is not enough: an older client says hello too");
        let Some(Message::MotionReady { port: cport, seen }) = c.ready() else { panic!("ready") };
        s.on_ready(cport, seen);

        assert!(s.send_move(3, 4));
        assert!(s.send_move(-1, 0));
        let mut got = (0, 0);
        for _ in 0..2 {
            let (buf, n, _) = recv(Some(c.socket())).await.unwrap();
            if let Some(Message::MouseMove { dx, dy }) = c.on_datagram(&buf[..n]) {
                got = (got.0 + dx, got.1 + dy);
            }
        }
        assert_eq!(got, (2, 4));
        assert_eq!(s.sync(), Some(Message::MotionSync { seq: 2, sx: 2, sy: 4 }));
        assert_eq!(s.sync(), None, "nothing new to say");
    }
}
