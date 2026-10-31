//! A node that restarts must find the peers that already know it at once —
//! not a heartbeat later.
//!
//! The roster is in memory on both sides, and a peer only answers a Hello
//! from someone new to it. So a restarted Lantern used to announce itself,
//! hear nothing back from everyone who already had it in their roster, and
//! show an empty sidebar until each of their 45 s heartbeats came round —
//! longer on Wi-Fi, where a broadcast is easily lost. That was "sometimes it
//! doesn't show anyone", and the fix is two things this test needs both of:
//! the first beacon is a Ping, which everyone answers, and the answer goes
//! straight back to the asker rather than only onto the broadcast.

use std::time::{Duration, Instant};

use lantern_core::{Core, CoreConfig, CoreEvent, LinkStatus};
use tokio::time::timeout;

fn config(name: &str, dir: &std::path::Path, my_port: u16, targets: Vec<u16>) -> CoreConfig {
    CoreConfig {
        data_dir: dir.to_path_buf(),
        display_name: name.into(),
        discovery_port: my_port,
        beacon_targets: targets,
        broadcast: false,
        quic_port: 0,
        in_memory_store: false,
        group: String::new(),
        auto_accept_limit: None,
        download_dir: None,
    }
}

async fn saw(rx: &mut tokio::sync::mpsc::Receiver<CoreEvent>, who: &str, within: Duration) {
    timeout(within, async {
        loop {
            match rx.recv().await.expect("event channel closed") {
                CoreEvent::PeerSeen { name, .. } if name == who => return,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {who} within {within:?}"));
}

#[tokio::test]
async fn a_restarted_node_is_answered_straight_away() {
    let tmp = std::env::temp_dir().join(format!("lantern-rediscovery-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    let (port_a, port_b, port_b2) = (24501u16, 24502u16, 24503u16);

    // Alice only ever beacons to her own port and Bob's first one.
    let (alice, mut ev_a) =
        Core::start(config("Alice", &tmp.join("alice"), port_a, vec![port_a, port_b]))
            .await
            .unwrap();
    let (_bob, mut ev_b) =
        Core::start(config("Bob", &tmp.join("bob"), port_b, vec![port_b, port_a]))
            .await
            .unwrap();
    saw(&mut ev_a, "Bob", Duration::from_secs(5)).await;
    saw(&mut ev_b, "Alice", Duration::from_secs(5)).await;
    assert_eq!(alice.link_status().0, LinkStatus::Ok);

    // Alice answers at most one Ping per two seconds; let Bob's first one age
    // out so the restart below is judged on its own.
    tokio::time::sleep(Duration::from_millis(2100)).await;

    // Bob "restarts": same identity — so to Alice this is nobody new — on a
    // port Alice never beacons to. Only a reply addressed to him can land.
    let started = Instant::now();
    let (_bob2, mut ev_b2) =
        Core::start(config("Bob", &tmp.join("bob"), port_b2, vec![port_b2, port_a]))
            .await
            .unwrap();
    saw(&mut ev_b2, "Alice", Duration::from_secs(5)).await;
    assert!(
        started.elapsed() < lantern_discovery_heartbeat() / 4,
        "rediscovery took {:?} — that is waiting for a heartbeat, not an answer",
        started.elapsed()
    );

    std::fs::remove_dir_all(&tmp).ok();
}

fn lantern_discovery_heartbeat() -> Duration {
    Duration::from_secs(45)
}
