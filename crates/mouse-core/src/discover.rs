//! Finding real addresses instead of asking the user to invent them.
//!
//! The panel used to seed `server_host` with a hardcoded `192.168.1.50` — an address that
//! exists on nobody's network but looks exactly like a configured one. A user restoring
//! defaults got a plausible-looking wrong answer rather than an obviously empty field.
//!
//! Two questions are answerable locally and are worth answering:
//!
//! * **Which addresses can this machine listen on?** Straightforward enumeration.
//! * **Which machines nearby are running mouse-rs?** A bounded TCP connect sweep of our
//!   own subnets on the configured port. It only probes networks this machine is already
//!   attached to, and only the one port the app uses.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// An IPv4 address on one of this machine's interfaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAddr {
    pub iface: String,
    pub ip: Ipv4Addr,
    pub prefix: u8,
}

/// Every non-loopback IPv4 address on this machine, with its subnet size.
pub fn local_ipv4s() -> Vec<LocalAddr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<LocalAddr> = ifaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(LocalAddr {
                iface: i.name,
                ip: v4.ip,
                prefix: mask_to_prefix(v4.netmask),
            }),
            _ => None,
        })
        .collect();
    out.sort_by_key(|a| a.ip.octets());
    out.dedup();
    out
}

/// Addresses a server can bind to: the wildcard first (the usual right answer), then each
/// specific interface for the case where binding everywhere is not wanted.
pub fn listen_candidates() -> Vec<String> {
    let mut v = vec!["0.0.0.0".to_string()];
    v.extend(local_ipv4s().into_iter().map(|a| a.ip.to_string()));
    v
}

fn mask_to_prefix(mask: Ipv4Addr) -> u8 {
    u32::from_be_bytes(mask.octets()).count_ones() as u8
}

/// Hosts to probe on a subnet, excluding our own address and the network/broadcast ends.
fn subnet_hosts(addr: &LocalAddr) -> Vec<Ipv4Addr> {
    // Only sweep networks small enough to finish quickly. A /24 is 254 hosts; anything
    // wider is both slow and unlikely to be a desk setup, so it is skipped rather than
    // silently turned into a long scan.
    if addr.prefix < 24 || addr.prefix > 30 {
        return Vec::new();
    }
    let base = u32::from_be_bytes(addr.ip.octets());
    let mask = u32::MAX << (32 - addr.prefix as u32);
    let network = base & mask;
    let broadcast = network | !mask;
    ((network + 1)..broadcast)
        .map(Ipv4Addr::from)
        .filter(|ip| *ip != addr.ip)
        .collect()
}

/// Sweep this machine's own subnets for hosts accepting connections on `port`.
///
/// Blocking and bounded: it probes at most a few hundred addresses with a short per-host
/// timeout, spread over a fixed number of threads, so the worst case is a couple of
/// seconds. Intended to be called off the UI thread.
pub fn scan_for_servers(port: u16, timeout: Duration) -> Vec<Ipv4Addr> {
    let targets: Vec<Ipv4Addr> = local_ipv4s().iter().flat_map(subnet_hosts).collect();
    if targets.is_empty() {
        return Vec::new();
    }

    const WORKERS: usize = 64;
    let targets = Arc::new(targets);
    let next = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    for _ in 0..WORKERS.min(targets.len()) {
        let targets = targets.clone();
        let next = next.clone();
        handles.push(std::thread::spawn(move || {
            let mut found = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&ip) = targets.get(i) else { break };
                // A successful TCP connect is all we claim: "something is listening
                // here". The handshake still has to succeed, and will fail on a wrong
                // secret — so this narrows the list, it does not promise a valid peer.
                if TcpStream::connect_timeout(&SocketAddr::from((ip, port)), timeout).is_ok() {
                    found.push(ip);
                }
            }
            found
        }));
    }

    let mut found: Vec<Ipv4Addr> = handles
        .into_iter()
        .filter_map(|h| h.join().ok())
        .flatten()
        .collect();
    found.sort_by_key(|ip| ip.octets());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netmask_becomes_a_prefix_length() {
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 255, 0)), 24);
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 0, 0)), 16);
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 255, 252)), 30);
    }

    #[test]
    fn a_slash_24_sweep_covers_hosts_but_not_network_broadcast_or_self() {
        let me = LocalAddr {
            iface: "eth0".into(),
            ip: Ipv4Addr::new(192, 168, 137, 2),
            prefix: 24,
        };
        let hosts = subnet_hosts(&me);
        assert_eq!(hosts.len(), 253, "254 usable hosts minus ourselves");
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 137, 1)));
        assert!(!hosts.contains(&me.ip), "never probe ourselves");
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 137, 0)), "network address");
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 137, 255)), "broadcast");
    }

    /// A wide subnet must be skipped rather than turned into a scan of thousands of
    /// hosts that appears to hang the panel.
    #[test]
    fn subnets_too_wide_to_sweep_quickly_are_skipped() {
        let wide = LocalAddr {
            iface: "eth0".into(),
            ip: Ipv4Addr::new(10, 0, 0, 5),
            prefix: 8,
        };
        assert!(subnet_hosts(&wide).is_empty());
    }

    #[test]
    fn listen_candidates_always_offer_the_wildcard_first() {
        assert_eq!(listen_candidates().first().map(String::as_str), Some("0.0.0.0"));
    }
}
