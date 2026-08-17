//! Where a beacon actually has to go on a real network.
//!
//! The prototype sent to `255.255.255.255` and nothing else. That address is
//! the *limited* broadcast, and the kernel emits it on exactly **one**
//! interface — whichever the routing table picks, normally the default route.
//! On a developer machine with `docker0`, `virbr0`, a VPN, or both Wi-Fi and
//! Ethernet up, that is frequently the wrong interface, and the beacon never
//! reaches the LAN. Nothing reports an error: `sendto` succeeds, the packet
//! just goes somewhere useless.
//!
//! The fix is the one the design doc already specified (§4.1): send to **each
//! interface's directed broadcast** — `192.168.1.255` for a `192.168.1.0/24` —
//! because a directed broadcast has a specific route, so the kernel puts it on
//! the interface that owns that subnet. `255.255.255.255` is kept as a belt-
//! and-braces extra.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// How long a computed interface list is trusted before re-enumerating.
/// Interfaces come and go (VPN up, cable in, Wi-Fi roam) and re-enumerating is
/// cheap next to a 45-second heartbeat.
pub const REFRESH: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceReport {
    pub name: String,
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    /// Directed broadcast address, from the OS if it supplies one, otherwise
    /// computed as `ip | !netmask`.
    pub broadcast: Option<Ipv4Addr>,
    pub loopback: bool,
}

/// `ip | !netmask` — the directed broadcast for the subnet this address is on.
pub fn directed_broadcast(ip: Ipv4Addr, netmask: Ipv4Addr) -> Option<Ipv4Addr> {
    let ip = u32::from(ip);
    let mask = u32::from(netmask);
    // A /32 has no broadcast address, and a zero mask is meaningless here.
    if mask == u32::MAX || mask == 0 {
        return None;
    }
    Some(Ipv4Addr::from(ip | !mask))
}

/// Every IPv4 interface the OS will admit to, loopback included, for display.
pub fn interfaces() -> Vec<IfaceReport> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .filter_map(|i| {
            let loopback = i.is_loopback();
            let name = i.name.clone();
            match i.addr {
                if_addrs::IfAddr::V4(v4) => {
                    let broadcast = v4
                        .broadcast
                        .or_else(|| directed_broadcast(v4.ip, v4.netmask))
                        .filter(|b| !b.is_unspecified());
                    Some(IfaceReport {
                        name,
                        ip: v4.ip,
                        netmask: v4.netmask,
                        broadcast,
                        loopback,
                    })
                }
                // IPv6 discovery is the RFC 3306 group, still to come.
                if_addrs::IfAddr::V6(_) => None,
            }
        })
        .collect()
}

/// The addresses a beacon should be sent to for the LAN to see it.
///
/// One directed broadcast per non-loopback IPv4 interface, plus
/// `255.255.255.255` last. Deduplicated, order stable.
pub fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for i in interfaces() {
        if i.loopback {
            continue;
        }
        if let Some(b) = i.broadcast {
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    if !out.contains(&Ipv4Addr::BROADCAST) {
        out.push(Ipv4Addr::BROADCAST);
    }
    out
}

/// Interface list cached for [`REFRESH`], so the heartbeat does not call
/// `getifaddrs` on every beacon.
pub struct TargetCache {
    addrs: Vec<Ipv4Addr>,
    at: Instant,
}

impl Default for TargetCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TargetCache {
    pub fn new() -> Self {
        Self {
            addrs: broadcast_targets(),
            at: Instant::now(),
        }
    }

    pub fn get(&mut self) -> &[Ipv4Addr] {
        if self.at.elapsed() >= REFRESH {
            self.addrs = broadcast_targets();
            self.at = Instant::now();
        }
        &self.addrs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_broadcast_for_a_slash_24() {
        assert_eq!(
            directed_broadcast(
                Ipv4Addr::new(192, 168, 1, 42),
                Ipv4Addr::new(255, 255, 255, 0)
            ),
            Some(Ipv4Addr::new(192, 168, 1, 255))
        );
    }

    #[test]
    fn directed_broadcast_for_a_slash_16_and_slash_22() {
        assert_eq!(
            directed_broadcast(
                Ipv4Addr::new(172, 16, 3, 9),
                Ipv4Addr::new(255, 255, 0, 0)
            ),
            Some(Ipv4Addr::new(172, 16, 255, 255))
        );
        assert_eq!(
            directed_broadcast(
                Ipv4Addr::new(10, 0, 5, 7),
                Ipv4Addr::new(255, 255, 252, 0)
            ),
            Some(Ipv4Addr::new(10, 0, 7, 255))
        );
    }

    #[test]
    fn a_slash_32_has_no_broadcast() {
        assert_eq!(
            directed_broadcast(
                Ipv4Addr::new(100, 64, 0, 1),
                Ipv4Addr::new(255, 255, 255, 255)
            ),
            None
        );
    }

    #[test]
    fn targets_always_include_the_limited_broadcast_and_never_duplicate() {
        let t = broadcast_targets();
        assert!(t.contains(&Ipv4Addr::BROADCAST));
        let mut sorted = t.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), t.len(), "duplicate broadcast targets");
        assert_eq!(
            *t.last().unwrap(),
            Ipv4Addr::BROADCAST,
            "255.255.255.255 is the fallback, so it goes last"
        );
    }

    #[test]
    fn loopback_is_never_a_broadcast_target() {
        // Loopback is reached by explicit unicast fan-out, not by broadcast;
        // including 127.255.255.255 here would double-deliver on some stacks.
        for t in broadcast_targets() {
            assert!(!t.is_loopback());
        }
    }
}
