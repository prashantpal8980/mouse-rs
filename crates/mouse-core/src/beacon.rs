//! Finding the peer on whatever network both machines are on right now.
//!
//! A server does not have one fixed address. It is on Ethernet today, Wi-Fi tomorrow, and a
//! phone hotspot the day after, and each network gives it a different address. A client that
//! remembers one address therefore cannot follow it. So the server broadcasts a short beacon
//! on every network it is attached to, listing its current addresses. A client listening on
//! the same network hears it and learns where to connect, whichever network that is.
//!
//! * **Signed.** A beacon carries an HMAC over its body, keyed with the shared secret. A
//!   machine that does not hold the secret cannot make a client connect somewhere else.
//!   The beacon only offers addresses. Authentication still happens in the real handshake.
//! * **No clock.** A beacon is not timestamped, because the two machines' clocks may differ.
//!   A client expires a server by when it last heard one, on its own clock.
//! * **Bounded.** Beacons are small and sent every [`BEACON_EVERY`]. Nothing is sent when the
//!   machine has no network.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::discover::{local_ipv4s, LocalAddr};

/// The UDP port beacons are sent to and heard on, unless the config says otherwise.
pub const DEFAULT_DISCOVERY_PORT: u16 = 24801;

/// How often a server sends its beacon.
pub const BEACON_EVERY: Duration = Duration::from_secs(2);

/// How long a client keeps a server it has stopped hearing from. A few beacons' worth, so
/// one lost packet does not forget a server that is still there.
pub const BEACON_TTL: Duration = Duration::from_secs(8);

/// How often a client checks whether its set of network addresses has changed.
pub const NETWORK_WATCH_EVERY: Duration = Duration::from_secs(2);

/// What a server says about itself.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BeaconBody {
    pub name: String,
    /// The TCP port the server listens on.
    pub port: u16,
    /// Its IPv4 addresses, on every network it is attached to right now.
    pub addrs: Vec<String>,
}

/// The wire form: the body as exact text, and an HMAC over those exact bytes.
#[derive(Serialize, Deserialize)]
struct Envelope {
    body: String,
    mac: String,
}

fn hmac_for(secret: &str) -> Hmac<Sha256> {
    // HMAC takes a key of any length, so this cannot fail.
    Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length")
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The packet a server sends for `body`, sealed with `secret`.
pub fn seal(secret: &str, body: &BeaconBody) -> Vec<u8> {
    let text = serde_json::to_string(body).expect("a plain struct always serialises");
    let mut mac = hmac_for(secret);
    mac.update(text.as_bytes());
    let envelope = Envelope { body: text, mac: to_hex(&mac.finalize().into_bytes()) };
    serde_json::to_vec(&envelope).expect("a plain struct always serialises")
}

/// The body in `packet`, if it was sealed with `secret` and is intact. Anything else is `None`,
/// and a client simply ignores it.
pub fn open(secret: &str, packet: &[u8]) -> Option<BeaconBody> {
    let envelope: Envelope = serde_json::from_slice(packet).ok()?;
    let want = from_hex(&envelope.mac)?;
    let mut mac = hmac_for(secret);
    mac.update(envelope.body.as_bytes());
    mac.verify_slice(&want).ok()?;
    serde_json::from_str(&envelope.body).ok()
}

/// Where a beacon can be sent on one network: that network's broadcast address, and the
/// all-hosts broadcast. Duplicates removed.
pub fn broadcast_targets(locals: &[LocalAddr]) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for a in locals {
        let mask = if a.prefix == 0 { 0 } else { u32::MAX << (32 - a.prefix as u32) };
        let base = u32::from_be_bytes(a.ip.octets());
        let broadcast = Ipv4Addr::from((base & mask) | !mask);
        if !out.contains(&broadcast) {
            out.push(broadcast);
        }
    }
    let all = Ipv4Addr::BROADCAST;
    if !out.contains(&all) {
        out.push(all);
    }
    out
}

/// The servers a client has heard from recently, and where to reach each one.
///
/// Shared between the listener task (which writes) and the connect loop (which reads).
#[derive(Clone, Default)]
pub struct Found {
    inner: Arc<Mutex<Vec<Heard>>>,
}

struct Heard {
    name: String,
    port: u16,
    addrs: Vec<Ipv4Addr>,
    at: Instant,
}

impl Found {
    pub fn new() -> Found {
        Found::default()
    }

    /// Record a beacon. `source` is the address the packet came from. It is a real address
    /// of the sender on the network the beacon arrived on, so it is kept even when the body
    /// omits it.
    pub fn record(&self, body: &BeaconBody, source: Option<Ipv4Addr>, now: Instant) {
        let mut addrs: Vec<Ipv4Addr> = body.addrs.iter().filter_map(|a| a.parse().ok()).collect();
        if let Some(src) = source {
            if !addrs.contains(&src) {
                addrs.push(src);
            }
        }
        let Ok(mut heard) = self.inner.lock() else { return };
        heard.retain(|h| h.name != body.name);
        heard.push(Heard { name: body.name.clone(), port: body.port, addrs, at: now });
    }

    /// Every address of every server heard within [`BEACON_TTL`], most recently heard first.
    pub fn candidates(&self, now: Instant) -> Vec<SocketAddr> {
        let Ok(heard) = self.inner.lock() else { return Vec::new() };
        let mut fresh: Vec<&Heard> = heard
            .iter()
            .filter(|h| now.saturating_duration_since(h.at) < BEACON_TTL)
            .collect();
        fresh.sort_by_key(|h| std::cmp::Reverse(h.at));
        let mut out: Vec<SocketAddr> = Vec::new();
        for h in fresh {
            for ip in &h.addrs {
                let sa = SocketAddr::new(IpAddr::V4(*ip), h.port);
                if !out.contains(&sa) {
                    out.push(sa);
                }
            }
        }
        out
    }
}

/// A UDP socket on `port` that several local processes may share, so the server and a client
/// on one machine (tests, or a machine that is both) can both hear the broadcasts.
fn bind_udp(port: u16, broadcast: bool) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    if broadcast {
        sock.set_broadcast(true)?;
    }
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
    UdpSocket::from_std(sock.into())
}

/// Server side: send a sealed beacon every [`BEACON_EVERY`] on every attached network, until
/// cancelled. Does nothing when there is no network or `discovery_port` is 0.
pub async fn run_sender(
    secret: String,
    name: String,
    port: u16,
    discovery_port: u16,
    cancel: CancellationToken,
) {
    if discovery_port == 0 {
        return;
    }
    let sock = match bind_udp(0, true) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("cannot send discovery beacons: {e}");
            return;
        }
    };
    let mut tick = tokio::time::interval(BEACON_EVERY);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let locals = local_ipv4s();
        if locals.is_empty() {
            continue;
        }
        let body = BeaconBody {
            name: name.clone(),
            port,
            // Link-local (169.254.x.x) addresses are not reachable from another machine's
            // route, so they are left out.
            addrs: locals
                .iter()
                .filter(|a| !a.ip.is_link_local())
                .map(|a| a.ip.to_string())
                .collect(),
        };
        let packet = seal(&secret, &body);
        for target in broadcast_targets(&locals) {
            // A send can fail on a network that just went away. The next tick retries.
            let _ = sock.send_to(&packet, (target, discovery_port)).await;
        }
    }
}

/// Client side: listen for beacons and record the servers that send valid ones, until
/// cancelled. Does nothing when `discovery_port` is 0.
pub async fn run_listener(
    secret: String,
    discovery_port: u16,
    found: Found,
    cancel: CancellationToken,
) {
    if discovery_port == 0 {
        return;
    }
    let sock = match bind_udp(discovery_port, false) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("cannot listen for discovery beacons on {discovery_port}: {e}");
            return;
        }
    };
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            received = sock.recv_from(&mut buf) => match received {
                Ok((n, from)) => {
                    if let Some(body) = open(&secret, &buf[..n]) {
                        let source = match from.ip() {
                            IpAddr::V4(v4) => Some(v4),
                            IpAddr::V6(_) => None,
                        };
                        found.record(&body, source, Instant::now());
                    }
                }
                // On Windows an unreachable beacon target can surface here as an error on
                // the next receive. That is noise, not a reason to stop listening.
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }
}

/// The part of this machine's network that a reconnect depends on: which addresses it has.
fn signature(locals: &[LocalAddr]) -> Vec<(String, Ipv4Addr, u8)> {
    locals.iter().map(|a| (a.iface.clone(), a.ip, a.prefix)).collect()
}

/// Watch this machine's network addresses, and bump `epoch` each time they change: a cable
/// plugged or unplugged, Wi-Fi joined or left, a new address handed out. A client uses this
/// to drop a session on a dead path at once, rather than waiting for it to time out.
pub async fn watch_network(epoch: watch::Sender<u64>, cancel: CancellationToken) {
    let mut last = signature(&local_ipv4s());
    let mut tick = tokio::time::interval(NETWORK_WATCH_EVERY);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let now = signature(&local_ipv4s());
        if now != last {
            last = now;
            epoch.send_modify(|e| *e = e.wrapping_add(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> BeaconBody {
        BeaconBody {
            name: "laptop".into(),
            port: 24800,
            addrs: vec!["192.168.137.2".into(), "192.168.1.9".into()],
        }
    }

    #[test]
    fn a_sealed_beacon_opens_with_the_same_secret() {
        let packet = seal("s3cret", &body());
        assert_eq!(open("s3cret", &packet), Some(body()));
    }

    #[test]
    fn a_beacon_from_a_machine_without_the_secret_is_ignored() {
        let packet = seal("not-the-secret", &body());
        assert_eq!(open("s3cret", &packet), None);
    }

    #[test]
    fn a_tampered_beacon_is_ignored() {
        // Change the advertised address, keep the old MAC. A forged redirect must fail.
        let packet = seal("s3cret", &body());
        let text = String::from_utf8(packet).unwrap().replace("192.168.137.2", "10.0.0.66");
        assert_eq!(open("s3cret", text.as_bytes()), None);
    }

    #[test]
    fn junk_on_the_discovery_port_is_ignored() {
        assert_eq!(open("s3cret", b"not json at all"), None);
        assert_eq!(open("s3cret", b"{\"body\":\"{}\",\"mac\":\"zz\"}"), None);
    }

    #[test]
    fn broadcast_targets_cover_each_network_and_the_all_hosts_address() {
        let locals = vec![
            LocalAddr { iface: "eth0".into(), ip: Ipv4Addr::new(192, 168, 137, 2), prefix: 24 },
            LocalAddr { iface: "wlan0".into(), ip: Ipv4Addr::new(192, 168, 1, 2), prefix: 24 },
            LocalAddr { iface: "eth1".into(), ip: Ipv4Addr::new(192, 168, 137, 9), prefix: 24 },
        ];
        assert_eq!(
            broadcast_targets(&locals),
            vec![
                Ipv4Addr::new(192, 168, 137, 255),
                Ipv4Addr::new(192, 168, 1, 255),
                Ipv4Addr::BROADCAST,
            ]
        );
    }

    #[test]
    fn a_server_heard_recently_is_a_candidate_and_a_stale_one_is_not() {
        let found = Found::new();
        let t0 = Instant::now();
        found.record(&body(), Some(Ipv4Addr::new(192, 168, 1, 77)), t0);
        let now = t0 + Duration::from_secs(1);
        let c = found.candidates(now);
        assert!(c.contains(&"192.168.137.2:24800".parse().unwrap()));
        assert!(c.contains(&"192.168.1.9:24800".parse().unwrap()));
        assert!(c.contains(&"192.168.1.77:24800".parse().unwrap()), "source address kept");

        let later = t0 + BEACON_TTL + Duration::from_secs(1);
        assert!(found.candidates(later).is_empty(), "a server not heard for a while expires");
    }

    #[test]
    fn a_fresh_beacon_from_the_same_server_replaces_its_old_addresses() {
        let found = Found::new();
        let t0 = Instant::now();
        found.record(&body(), None, t0);
        let moved = BeaconBody { addrs: vec!["10.1.1.5".into()], ..body() };
        found.record(&moved, None, t0 + Duration::from_secs(3));
        let c = found.candidates(t0 + Duration::from_secs(3));
        assert_eq!(c, vec!["10.1.1.5:24800".parse::<SocketAddr>().unwrap()]);
    }
}
