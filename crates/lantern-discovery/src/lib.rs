//! UDP beacon send/receive with the §2.2 replay rule.
//!
//! Prototype scope: IPv4 broadcast + loopback unicast fan-out. mDNS and the
//! RFC 3306 IPv6 group come later; the roster keys on identity, so adding
//! paths never duplicates peers.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use lantern_proto::{Beacon, BeaconType, MAX_BEACON_BYTES};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub const HEARTBEAT: Duration = Duration::from_secs(45);

/// A peer is offline once three heartbeats have passed with no beacon
/// (DESIGN §4.2). Three, not one, so a single dropped broadcast — routine
/// on congested Wi-Fi — doesn't flap someone offline mid-conversation.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(135);

#[derive(Debug, Clone)]
pub struct Discovered {
    pub beacon: Beacon,
    pub from: SocketAddr,
}

/// Per-identity replay state: `boot` compared for equality, `seq` ordered
/// within a boot. (Ordering a random boot nonce lexicographically blackholes
/// peers after ~half of restarts — DESIGN.md §11 defect 3.)
#[derive(Default)]
struct ReplayFilter {
    seen: HashMap<[u8; 32], ([u8; 8], u64)>,
}

impl ReplayFilter {
    /// Returns true if the beacon is fresh and should be processed.
    fn check_and_update(&mut self, b: &Beacon) -> bool {
        match self.seen.get(&b.id) {
            Some((boot, last_seq)) if *boot == b.boot => {
                if b.seq > *last_seq {
                    self.seen.insert(b.id, (b.boot, b.seq));
                    true
                } else {
                    false
                }
            }
            _ => {
                // New identity or new boot: accept, reset watermark.
                self.seen.insert(b.id, (b.boot, b.seq));
                true
            }
        }
    }
}

pub struct DiscoveryConfig {
    /// Port to bind for receiving beacons.
    pub bind_port: u16,
    /// Ports to send beacons to (loopback fan-out for multi-instance testing;
    /// on a real LAN this is just [DISCOVERY_PORT]).
    pub target_ports: Vec<u16>,
    /// Enable subnet broadcast (off in tests, on for real LAN use).
    pub broadcast: bool,
}

pub struct Discovery {
    socket: Arc<UdpSocket>,
    config: DiscoveryConfig,
}

impl Discovery {
    pub async fn bind(config: DiscoveryConfig) -> std::io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, config.bind_port)).await?;
        if config.broadcast {
            socket.set_broadcast(true)?;
        }
        Ok(Self {
            socket: Arc::new(socket),
            config,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Send one beacon to every configured target.
    pub async fn send_beacon(&self, beacon: &Beacon, signing_key: &SigningKey) {
        let bytes = match beacon.encode(signing_key) {
            Ok(b) => b,
            Err(e) => {
                warn!("beacon encode failed: {e}");
                return;
            }
        };
        for port in &self.config.target_ports {
            if self.config.broadcast {
                let _ = self
                    .socket
                    .send_to(&bytes, (Ipv4Addr::BROADCAST, *port))
                    .await;
            }
            // Loopback always — same-host instances and tests.
            let _ = self
                .socket
                .send_to(&bytes, (Ipv4Addr::LOCALHOST, *port))
                .await;
        }
    }

    /// Run the receive loop; verified, replay-filtered beacons flow out the
    /// channel. Beacons signed by `own_id` are dropped (we hear our own
    /// broadcasts).
    pub fn spawn_receiver(
        &self,
        own_id: [u8; 32],
    ) -> mpsc::Receiver<Discovered> {
        let (tx, rx) = mpsc::channel(64);
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let mut filter = ReplayFilter::default();
            let mut buf = vec![0u8; MAX_BEACON_BYTES + 1];
            loop {
                let (n, from) = match socket.recv_from(&mut buf).await {
                    Ok(x) => x,
                    Err(e) => {
                        warn!("beacon recv error: {e}");
                        continue;
                    }
                };
                let beacon = match Beacon::decode(&buf[..n]) {
                    Ok(b) => b,
                    Err(e) => {
                        debug!("dropping bad datagram from {from}: {e}");
                        continue;
                    }
                };
                if beacon.id == own_id {
                    continue;
                }
                if !filter.check_and_update(&beacon) {
                    debug!("replay/duplicate from {from}");
                    continue;
                }
                if tx.send(Discovered { beacon, from }).await.is_err() {
                    return; // receiver gone; stop quietly
                }
            }
        });
        rx
    }
}

/// Convenience: heartbeat loop sending a HELLO every `HEARTBEAT`.
pub fn spawn_heartbeat(
    discovery: Arc<Discovery>,
    signing_key: SigningKey,
    mut beacon_template: Beacon,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq: u64 = beacon_template.seq;
        loop {
            beacon_template.seq = seq;
            beacon_template.beacon_type = BeaconType::Hello;
            beacon_template.ts = now_ms();
            discovery.send_beacon(&beacon_template, &signing_key).await;
            seq += 1;
            tokio::time::sleep(HEARTBEAT).await;
        }
    })
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beacon_with(id: [u8; 32], boot: [u8; 8], seq: u64) -> Beacon {
        Beacon {
            beacon_type: BeaconType::Hello,
            flags: 0,
            id,
            name: "x".into(),
            host: "h".into(),
            group: String::new(),
            device: Default::default(),
            port: 1,
            state: Default::default(),
            status: String::new(),
            avatar: None,
            caps: 0,
            seq,
            boot,
            ts: 0,
        }
    }

    #[test]
    fn replay_rule() {
        let mut f = ReplayFilter::default();
        let id = [1u8; 32];
        let boot_a = [0xFFu8; 8];
        let boot_b = [0x01u8; 8]; // numerically smaller — must still be accepted

        assert!(f.check_and_update(&beacon_with(id, boot_a, 5)));
        assert!(!f.check_and_update(&beacon_with(id, boot_a, 5))); // duplicate
        assert!(!f.check_and_update(&beacon_with(id, boot_a, 4))); // replay
        assert!(f.check_and_update(&beacon_with(id, boot_a, 6))); // progress
        // Restart with a *smaller* random boot: accepted, watermark reset.
        assert!(f.check_and_update(&beacon_with(id, boot_b, 0)));
        assert!(f.check_and_update(&beacon_with(id, boot_b, 1)));
    }
}
