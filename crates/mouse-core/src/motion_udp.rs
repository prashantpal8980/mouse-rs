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
const MOVE: &[u8; 4] = b"MRM1";
const TAG: usize = 16;
const HELLO_LEN: usize = 4 + 16 + TAG;
const MOVE_LEN: usize = 4 + 8 + 8 + 8 + TAG;
/// Room for any datagram we send, with margin.
pub(crate) const MAX_DATAGRAM: usize = 64;

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

fn encode_move(key: &Key, seq: u64, sx: i64, sy: i64) -> [u8; MOVE_LEN] {
    let mut out = [0u8; MOVE_LEN];
    out[..4].copy_from_slice(MOVE);
    out[4..12].copy_from_slice(&seq.to_be_bytes());
    out[12..20].copy_from_slice(&sx.to_be_bytes());
    out[20..28].copy_from_slice(&sy.to_be_bytes());
    let t = tag(key, &out[..28]);
    out[28..].copy_from_slice(&t);
    out
}

fn decode_move(key: &Key, buf: &[u8]) -> Option<(u64, i64, i64)> {
    if buf.len() != MOVE_LEN || &buf[..4] != MOVE || !tag_ok(key, &buf[..28], &buf[28..]) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> [u8; 8] { buf[r].try_into().expect("8 bytes") };
    Some((
        u64::from_be_bytes(n(4..12)),
        i64::from_be_bytes(n(12..20)),
        i64::from_be_bytes(n(20..28)),
    ))
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
}

impl ServerMotion {
    pub(crate) fn new(sock: Arc<UdpSocket>, key: Key) -> ServerMotion {
        let mut token = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut token);
        ServerMotion { sock, key, token, peer: None, seq: 0, sum: (0, 0), synced: (0, 0) }
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
            return;
        }
        if self.peer.map(|(a, _)| a) != Some(from) {
            tracing::info!("pointer motion now goes over UDP to {from}");
            crate::qos::prioritise_datagrams(&self.sock, from);
        }
        self.peer = Some((from, Instant::now()));
    }

    /// Send one move over UDP. False when there is no live UDP path: the caller then
    /// sends it over TCP as before.
    pub(crate) fn send_move(&mut self, dx: i32, dy: i32) -> bool {
        let Some((to, heard)) = self.peer else {
            return false;
        };
        if heard.elapsed() > PEER_STALE {
            tracing::info!("no UDP hello from {to} for {PEER_STALE:?}; pointer motion back on TCP");
            self.peer = None;
            return false;
        }
        self.sum.0 += i64::from(dx);
        self.sum.1 += i64::from(dy);
        self.seq += 1;
        // Never waits. A datagram the socket can't take now is a lost datagram, and the
        // next one (or the next `MotionSync`) carries the total anyway.
        let _ = self.sock.try_send_to(&encode_move(&self.key, self.seq, self.sum.0, self.sum.1), to);
        true
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
}

impl ClientMotion {
    pub(crate) async fn start(
        server: IpAddr,
        port: u16,
        token: &str,
        key: Key,
    ) -> std::io::Result<ClientMotion> {
        let token = unhex16(token)
            .ok_or_else(|| std::io::Error::other("malformed motion token"))?;
        let any = match server {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        let sock = UdpSocket::bind((any, 0)).await?;
        sock.connect((server, port)).await?;
        crate::qos::prioritise_datagrams(&sock, SocketAddr::new(server, port));
        let me = ClientMotion { sock: Arc::new(sock), key, token, last_seq: 0, applied: (0, 0) };
        me.hello().await;
        Ok(me)
    }

    pub(crate) fn socket(&self) -> Arc<UdpSocket> {
        self.sock.clone()
    }

    /// (Re)announce this socket to the server. Repeated with every heartbeat. Awaited,
    /// not `try_send`: on a fresh socket tokio doesn't yet know it is writable, and the
    /// first hello would be dropped.
    pub(crate) async fn hello(&self) {
        let _ = self.sock.send(&encode_hello(&self.key, &self.token)).await;
    }

    pub(crate) fn on_datagram(&mut self, buf: &[u8]) -> Option<Message> {
        let (seq, sx, sy) = decode_move(&self.key, buf)?;
        self.advance(seq, sx, sy)
    }

    pub(crate) fn on_sync(&mut self, seq: u64, sx: i64, sy: i64) -> Option<Message> {
        self.advance(seq, sx, sy)
    }

    /// The move that brings this machine from what it has applied to the server's
    /// total. Older or repeated totals are ignored.
    fn advance(&mut self, seq: u64, sx: i64, sy: i64) -> Option<Message> {
        if seq <= self.last_seq {
            return None;
        }
        self.last_seq = seq;
        let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let (dx, dy) = (clamp(sx - self.applied.0), clamp(sy - self.applied.1));
        self.applied = (sx, sy);
        (dx != 0 || dy != 0).then_some(Message::MouseMove { dx, dy })
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
        let m = encode_move(&k, 7, -3, 1200);
        assert_eq!(decode_move(&k, &m), Some((7, -3, 1200)));
        assert_eq!(decode_move(&key(2), &m), None, "another session's key");
        let mut bad = m;
        bad[13] ^= 1;
        assert_eq!(decode_move(&k, &bad), None, "tampered total");
        let h = encode_hello(&k, &[9; 16]);
        assert_eq!(decode_hello(&k, &h), Some([9; 16]));
        assert_eq!(decode_hello(&key(2), &h), None);
    }

    #[test]
    fn a_lost_datagram_is_covered_by_the_next_and_old_ones_are_ignored() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut c = ClientMotion::start(IpAddr::V4(Ipv4Addr::LOCALHOST), 9, &hex(&[0; 16]), key(1))
                .await
                .unwrap();
            // Totals 5, 9 (lost), 12: the client moves 5, then 7, and nothing is lost.
            assert_eq!(c.advance(1, 5, 0), Some(Message::MouseMove { dx: 5, dy: 0 }));
            assert_eq!(c.advance(3, 12, -2), Some(Message::MouseMove { dx: 7, dy: -2 }));
            // The late datagram 2 arrives after 3: ignored, not replayed.
            assert_eq!(c.advance(2, 9, 0), None);
            // A sync for what already arrived changes nothing.
            assert_eq!(c.on_sync(3, 12, -2), None);
        });
    }

    #[tokio::test]
    async fn motion_flows_over_udp_once_the_client_says_hello_and_syncs_before_a_click() {
        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let port = server_sock.local_addr().unwrap().port();
        let mut s = ServerMotion::new(server_sock.clone(), key(4));
        assert!(!s.send_move(1, 1), "no hello yet: TCP");

        let Some(Message::MotionOffer { port: p, token }) = s.offer() else { panic!("offer") };
        assert_eq!(p, port);
        let mut c = ClientMotion::start(IpAddr::V4(Ipv4Addr::LOCALHOST), p, &token, key(4)).await.unwrap();
        let (buf, n, from) = recv(Some(server_sock.clone())).await.unwrap();
        s.on_datagram(&buf[..n], from);

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
