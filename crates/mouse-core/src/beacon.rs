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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::discover::{is_link_local_v6, link_local_v6_ifaces, local_ipv4s, LocalAddr};

/// The UDP port beacons are sent to and heard on, unless the config says otherwise.
pub const DEFAULT_DISCOVERY_PORT: u16 = 24801;

/// Beacons also go to every IPv6 host on each network (all-nodes multicast), from the
/// sender's link-local address. A client that hears one connects back to that address, so
/// two machines on the same Wi-Fi still find each other when the network handed one of them
/// no IPv4 address (Windows then shows 169.254.x.x): seen on a phone hotspot, 26 Oct 2026.
const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

/// Beacons heard this close together came from the same tick of the sender.
const SAME_TICK: Duration = Duration::from_millis(500);

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
    addrs: Vec<SocketAddr>,
    at: Instant,
}

impl Found {
    pub fn new() -> Found {
        Found::default()
    }

    /// Record a beacon. `source` is the address the packet came from. It is a real address
    /// of the sender on the network the beacon arrived on, so it is kept even when the body
    /// omits it. An IPv6 source is kept only if link-local, with its interface (scope).
    pub fn record(&self, body: &BeaconBody, source: Option<SocketAddr>, now: Instant) {
        let mut addrs: Vec<SocketAddr> = body
            .addrs
            .iter()
            .filter_map(|a| a.parse::<Ipv4Addr>().ok())
            .map(|ip| SocketAddr::new(IpAddr::V4(ip), body.port))
            .collect();
        let source = match source {
            Some(SocketAddr::V4(v4)) => Some(SocketAddr::new(IpAddr::V4(*v4.ip()), body.port)),
            Some(SocketAddr::V6(v6)) if is_link_local_v6(v6.ip()) => {
                Some(SocketAddr::V6(SocketAddrV6::new(*v6.ip(), body.port, 0, v6.scope_id())))
            }
            _ => None,
        };
        if let Some(src) = source {
            if !addrs.contains(&src) {
                addrs.push(src);
            }
        }
        let Ok(mut heard) = self.inner.lock() else { return };
        // One tick's IPv4 and IPv6 beacons arrive as separate packets a moment apart: merge
        // those. A later tick's beacon replaces, so addresses the server lost are forgotten.
        if let Some(h) = heard.iter_mut().find(|h| h.name == body.name) {
            if now.saturating_duration_since(h.at) < SAME_TICK {
                for a in addrs {
                    if !h.addrs.contains(&a) {
                        h.addrs.push(a);
                    }
                }
                h.at = now;
                return;
            }
        }
        heard.retain(|h| h.name != body.name);
        heard.push(Heard { name: body.name.clone(), addrs, at: now });
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
            for sa in &h.addrs {
                if !out.contains(sa) {
                    out.push(*sa);
                }
            }
        }
        out
    }

    /// What has been heard, for the "still looking" log line.
    pub fn describe(&self, now: Instant) -> String {
        let Ok(heard) = self.inner.lock() else { return String::new() };
        if heard.is_empty() {
            return "no beacon heard".into();
        }
        heard
            .iter()
            .map(|h| {
                let addrs = h.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
                format!(
                    "'{}' at [{addrs}], {:.0}s ago",
                    h.name,
                    now.saturating_duration_since(h.at).as_secs_f32()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
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

/// The IPv6 counterpart of [`bind_udp`]: IPv6 only, joined to all-nodes on every network
/// so beacons sent there are heard (Linux delivers them anyway; Windows wants the join).
fn bind_udp6(port: u16) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_only_v6(true)?;
    sock.set_reuse_address(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
    if port != 0 {
        for idx in link_local_v6_ifaces() {
            let _ = sock.join_multicast_v6(&ALL_NODES, idx);
        }
    }
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
    // Without IPv6 the IPv4 beacons still go out.
    let sock6 = bind_udp6(0).ok();
    let mut tick = tokio::time::interval(BEACON_EVERY);
    let mut last_said = String::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let locals = local_ipv4s();
        let v6_ifaces = link_local_v6_ifaces();
        let said = describe_locals(&locals, &v6_ifaces);
        if said != last_said {
            tracing::info!("announcing this server on: {said}");
            last_said = said;
        }
        if locals.is_empty() && v6_ifaces.is_empty() {
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
        if !locals.is_empty() {
            for target in broadcast_targets(&locals) {
                // A send can fail on a network that just went away. The next tick retries.
                let _ = sock.send_to(&packet, (target, discovery_port)).await;
            }
        }
        if let Some(sock6) = &sock6 {
            for idx in &v6_ifaces {
                let to = SocketAddrV6::new(ALL_NODES, discovery_port, 0, *idx);
                let _ = sock6.send_to(&packet, to).await;
            }
        }
    }
}

/// This machine's networks, for the log: each IPv4 address, flagging 169.254.x.x (the network
/// gave no address), and how many networks can be reached over IPv6 link-local.
pub fn describe_locals(locals: &[LocalAddr], v6_ifaces: &[u32]) -> String {
    let mut parts: Vec<String> = locals
        .iter()
        .map(|a| {
            if a.ip.is_link_local() {
                format!("{} {} (no address from the network)", a.iface, a.ip)
            } else {
                format!("{} {}/{}", a.iface, a.ip, a.prefix)
            }
        })
        .collect();
    parts.push(format!("IPv6 link-local on {} network(s)", v6_ifaces.len()));
    parts.join(", ")
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
    let sock6 = match bind_udp6(discovery_port) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::info!("no IPv6 discovery on {discovery_port} ({e}); IPv4 only");
            None
        }
    };
    let mut buf = vec![0u8; 2048];
    let mut buf6 = vec![0u8; 2048];
    loop {
        let received = tokio::select! {
            _ = cancel.cancelled() => return,
            r = sock.recv_from(&mut buf) => r.map(|(n, from)| (&buf[..n], from)),
            r = recv6(sock6.as_ref(), &mut buf6) => r,
        };
        match received {
            Ok((packet, from)) => {
                if let Some(body) = open(&secret, packet) {
                    found.record(&body, Some(from), Instant::now());
                }
            }
            // On Windows an unreachable beacon target can surface here as an error on
            // the next receive. That is noise, not a reason to stop listening.
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

async fn recv6<'a>(
    sock: Option<&UdpSocket>,
    buf: &'a mut [u8],
) -> std::io::Result<(&'a [u8], SocketAddr)> {
    let Some(sock) = sock else {
        return std::future::pending().await;
    };
    let (n, from) = sock.recv_from(buf).await?;
    Ok((&buf[..n], from))
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
        found.record(&body(), Some(SocketAddr::from(([192, 168, 1, 77], 5555))), t0);
        let now = t0 + Duration::from_secs(1);
        let c = found.candidates(now);
        assert!(c.contains(&"192.168.137.2:24800".parse().unwrap()));
        assert!(c.contains(&"192.168.1.9:24800".parse().unwrap()));
        assert!(c.contains(&"192.168.1.77:24800".parse().unwrap()), "source address kept");

        let later = t0 + BEACON_TTL + Duration::from_secs(1);
        assert!(found.candidates(later).is_empty(), "a server not heard for a while expires");
    }

    #[test]
    fn a_link_local_ipv6_beacon_is_reachable_with_its_interface() {
        // The hotspot case: the server has only 169.254.x.x, which it does not announce, so
        // its IPv6 link-local source is the only way back to it.
        let found = Found::new();
        let t0 = Instant::now();
        let none = BeaconBody { addrs: vec![], ..body() };
        found.record(&none, Some("[fe80::1234%7]:5555".parse().unwrap()), t0);
        found.record(&none, Some("[2409::1]:5555".parse().unwrap()), t0);
        let c = found.candidates(t0);
        assert_eq!(c, vec![SocketAddr::V6(SocketAddrV6::new("fe80::1234".parse().unwrap(), 24800, 0, 7))]);
    }

    #[test]
    fn one_ticks_ipv4_and_ipv6_beacons_merge() {
        let found = Found::new();
        let t0 = Instant::now();
        found.record(&body(), None, t0);
        found.record(&body(), Some("[fe80::1%3]:5555".parse().unwrap()), t0 + Duration::from_millis(5));
        let c = found.candidates(t0 + Duration::from_millis(5));
        assert!(c.contains(&"192.168.1.9:24800".parse().unwrap()));
        assert!(c.contains(&"[fe80::1%3]:24800".parse().unwrap()));
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
