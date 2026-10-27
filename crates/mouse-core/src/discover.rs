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

/// What kind of network an interface is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetKind {
    Cable,
    Wifi,
    /// A tunnel, bridge or anything not recognised (Tailscale, VPNs, virtual adapters).
    Other,
}

/// The kind of the interface called `iface`. On Linux the kernel says: a wireless device
/// has `/sys/class/net/<name>/wireless`, a physical one has `device`. On Windows the
/// interface's name is all there is, and the system names them "Ethernet…" and "Wi-Fi…".
pub fn iface_kind(iface: &str) -> NetKind {
    #[cfg(target_os = "linux")]
    {
        let base = std::path::Path::new("/sys/class/net").join(iface);
        if base.exists() {
            if base.join("wireless").exists() || base.join("phy80211").exists() {
                return NetKind::Wifi;
            }
            if base.join("device").exists() {
                return NetKind::Cable;
            }
            return NetKind::Other;
        }
    }
    kind_from_name(iface)
}

fn kind_from_name(iface: &str) -> NetKind {
    let n = iface.to_ascii_lowercase();
    if ["wi-fi", "wifi", "wlan", "wireless", "wlp", "wlo"].iter().any(|k| n.contains(k)) {
        NetKind::Wifi
    } else if ["tailscale", "vpn", "tun", "tap", "wg", "zt", "vethernet", "virtual", "loopback"].iter().any(|k| n.contains(k)) {
        NetKind::Other
    } else if ["ethernet", "eth", "enp", "eno", "ens", "enx", "lan"].iter().any(|k| n.contains(k)) {
        NetKind::Cable
    } else {
        NetKind::Other
    }
}

/// The name of the interface with index `index` (the `%n` of an IPv6 link-local address).
pub fn iface_name(index: u32) -> Option<String> {
    if_addrs::get_if_addrs().ok()?.into_iter().find(|i| i.index == Some(index)).map(|i| i.name)
}

/// The kind of network `target` is reached through directly: the interface whose subnet
/// contains it. `None` when it is on none of them (reached through a router or a tunnel).
pub fn on_link_kind(target: Ipv4Addr) -> Option<NetKind> {
    local_ipv4s().into_iter().find(|a| same_subnet(a.ip, target, a.prefix)).map(|a| iface_kind(&a.iface))
}

fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    if prefix == 0 || prefix > 32 {
        return false;
    }
    let mask = u32::MAX << (32 - prefix as u32);
    (u32::from(a) & mask) == (u32::from(b) & mask)
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

/// The interface index of every network this machine is on, for reaching peers over IPv6
/// link-local (fe80::). Every interface gives itself such an address, whether or not the
/// network handed out one, so these still work when DHCP failed and IPv4 fell back to
/// 169.254.x.x. `if_addrs` leaves fe80:: addresses out of its list, so this takes every
/// non-loopback interface it does list; a send on one without IPv6 simply fails.
pub fn link_local_v6_ifaces() -> Vec<u32> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<u32> = ifaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| i.index)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// fe80::/10. (`Ipv6Addr::is_unicast_link_local` is not stable on our minimum Rust.)
pub fn is_link_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
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
    fn interface_names_say_cable_wifi_or_other() {
        assert_eq!(kind_from_name("Ethernet 2"), NetKind::Cable);
        assert_eq!(kind_from_name("eth0"), NetKind::Cable);
        assert_eq!(kind_from_name("enp3s0"), NetKind::Cable);
        assert_eq!(kind_from_name("Wi-Fi 2"), NetKind::Wifi);
        assert_eq!(kind_from_name("wlan0"), NetKind::Wifi);
        assert_eq!(kind_from_name("Tailscale"), NetKind::Other);
        assert_eq!(kind_from_name("vEthernet (WSL)"), NetKind::Other);
    }

    #[test]
    fn subnets_match_by_prefix() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert!(same_subnet(ip("192.168.137.2"), ip("192.168.137.1"), 24));
        assert!(!same_subnet(ip("192.168.137.2"), ip("192.168.1.2"), 24));
        assert!(!same_subnet(ip("192.168.137.2"), ip("192.168.137.1"), 0));
    }

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
