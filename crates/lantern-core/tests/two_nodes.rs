//! Phase 1 gate: two nodes discover each other and exchange messages,
//! and the message log survives a restart.

use std::time::Duration;

use lantern_core::{Core, CoreConfig, CoreEvent};
use tokio::sync::mpsc;
use tokio::time::timeout;

fn config(name: &str, dir: &std::path::Path, my_port: u16, other_port: u16) -> CoreConfig {
    CoreConfig {
        data_dir: dir.to_path_buf(),
        display_name: name.into(),
        discovery_port: my_port,
        beacon_targets: vec![my_port, other_port],
        broadcast: false,
        quic_port: 0,
        in_memory_store: false,
        group: String::new(),
        auto_accept_limit: None,
        // Stay inside the test's tempdir — never the real Downloads folder.
        download_dir: None,
    }
}

async fn wait_for<F>(rx: &mut mpsc::Receiver<CoreEvent>, mut pred: F) -> CoreEvent
where
    F: FnMut(&CoreEvent) -> bool,
{
    timeout(Duration::from_secs(10), async {
        loop {
            let ev = rx.recv().await.expect("event channel closed");
            if pred(&ev) {
                return ev;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test]
async fn discover_chat_persist() {
    let tmp = std::env::temp_dir().join(format!("lantern-it-{}", std::process::id()));
    let dir_a = tmp.join("alice");
    let dir_b = tmp.join("bob");
    std::fs::remove_dir_all(&tmp).ok();

    // Distinct loopback discovery ports, cross-targeted.
    let (port_a, port_b) = (24101u16, 24102u16);

    let (alice, mut ev_a) = Core::start(config("Alice", &dir_a, port_a, port_b))
        .await
        .unwrap();
    let (bob, mut ev_b) = Core::start(config("Bob", &dir_b, port_b, port_a))
        .await
        .unwrap();

    alice.announce().await;
    bob.announce().await;

    // 1. Mutual discovery.
    wait_for(&mut ev_a, |e| {
        matches!(e, CoreEvent::PeerSeen { name, .. } if name == "Bob")
    })
    .await;
    wait_for(&mut ev_b, |e| {
        matches!(e, CoreEvent::PeerSeen { name, .. } if name == "Alice")
    })
    .await;

    let bob_id = bob.identity_id();
    let alice_id = alice.identity_id();

    // 2. Alice → Bob.
    let mid1 = alice.send_message(bob_id, "hello bob").await.unwrap();
    let ev = wait_for(&mut ev_b, |e| matches!(e, CoreEvent::MessageReceived { .. })).await;
    match ev {
        CoreEvent::MessageReceived { peer, text, .. } => {
            assert_eq!(peer, alice_id, "message attributed to the right identity");
            assert_eq!(text, "hello bob");
        }
        _ => unreachable!(),
    }
    // Delivery ack comes back to Alice.
    wait_for(&mut ev_a, |e| {
        matches!(e, CoreEvent::MessageDelivered { mid } if *mid == mid1)
    })
    .await;

    // 3. Bob → Alice (reuses or establishes the reverse path).
    bob.send_message(alice_id, "hi alice").await.unwrap();
    wait_for(&mut ev_a, |e| {
        matches!(e, CoreEvent::MessageReceived { text, .. } if text == "hi alice")
    })
    .await;

    // 4. History is on disk for both.
    let hist_a = alice.history(&bob_id, 10);
    assert!(hist_a.iter().any(|m| m.text == "hello bob" && m.outgoing));
    assert!(hist_a.iter().any(|m| m.text == "hi alice" && !m.outgoing));

    // 5. Restart Alice: same identity, history intact.
    drop(alice);
    drop(ev_a);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (alice2, _ev_a2) = Core::start(config("Alice", &dir_a, 24103, port_b))
        .await
        .unwrap();
    assert_eq!(
        alice2.identity_id(),
        alice_id,
        "identity survives restart"
    );
    let hist = alice2.history(&bob_id, 10);
    assert!(
        hist.iter().any(|m| m.text == "hello bob"),
        "messages survive restart"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

/// Sealed: the receive event withholds the text, opening reveals it and the
/// sender hears MessageOpened with their copy marked state 2.
#[tokio::test]
async fn sealed_message_round_trip() {
    let tmp = std::env::temp_dir().join(format!("lantern-seal-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    let (port_a, port_b) = (24221u16, 24222u16);
    let (alice, mut ev_a) = Core::start(config("Alice", &tmp.join("a"), port_a, port_b))
        .await
        .unwrap();
    let (bob, mut ev_b) = Core::start(config("Bob", &tmp.join("b"), port_b, port_a))
        .await
        .unwrap();
    alice.announce().await;
    bob.announce().await;
    wait_for(&mut ev_a, |e| {
        matches!(e, CoreEvent::PeerSeen { name, .. } if name == "Bob")
    })
    .await;

    let mid = alice.send_sealed(bob.identity_id(), "the cake is ready").await.unwrap();

    let got = wait_for(&mut ev_b, |e| matches!(e, CoreEvent::MessageReceived { .. })).await;
    let CoreEvent::MessageReceived { text, sealed, mid: got_mid, .. } = got else {
        unreachable!()
    };
    assert!(sealed, "arrives sealed");
    assert!(text.is_empty(), "the event must not leak sealed text");
    assert_eq!(got_mid, mid);
    // …but the text is on Bob's disk, flagged sealed, for the shell to reveal.
    let row = bob
        .history(&alice.identity_id(), 10)
        .into_iter()
        .find(|m| m.mid == mid)
        .unwrap();
    assert!(row.sealed);
    assert_eq!(row.text, "the cake is ready");

    bob.open_message(mid).await;
    wait_for(&mut ev_a, |e| {
        matches!(e, CoreEvent::MessageOpened { mid: m } if *m == mid)
    })
    .await;
    let mine = alice
        .history(&bob.identity_id(), 10)
        .into_iter()
        .find(|m| m.mid == mid)
        .unwrap();
    assert_eq!(mine.state, 2, "opened is a distinct delivery state");
    // Opening is idempotent and un-sealed on Bob's side now.
    assert!(!bob.history(&alice.identity_id(), 10)[0].sealed);

    std::fs::remove_dir_all(&tmp).ok();
}

/// The queue: a message to someone unreachable stays at state 0 and is
/// delivered when their beacon shows up.
#[tokio::test]
async fn queued_message_delivers_when_the_peer_appears() {
    let tmp = std::env::temp_dir().join(format!("lantern-queue-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    let (port_a, port_b) = (24231u16, 24232u16);
    let (alice, mut ev_a) = Core::start(config("Alice", &tmp.join("a"), port_a, port_b))
        .await
        .unwrap();
    // Bob exists only as an identity Alice knows — no reachable session.
    let (bob, mut ev_b) = Core::start(config("Bob", &tmp.join("b"), port_b, port_a))
        .await
        .unwrap();
    let bob_id = bob.identity_id();

    // Alice has never heard a beacon from Bob: unreachable, so this queues.
    let mid = alice.send_message(bob_id, "waited for you").await.unwrap();
    let pending = alice
        .history(&bob_id, 10)
        .into_iter()
        .find(|m| m.mid == mid)
        .unwrap();
    assert_eq!(pending.state, 0, "still queued");

    // Bob comes onto the network; his beacon triggers the flush.
    bob.announce().await;
    alice.announce().await;
    wait_for(&mut ev_b, |e| {
        matches!(e, CoreEvent::MessageReceived { text, .. } if text == "waited for you")
    })
    .await;
    wait_for(&mut ev_a, |e| {
        matches!(e, CoreEvent::MessageDelivered { mid: m } if *m == mid)
    })
    .await;
    assert_eq!(
        alice.history(&bob_id, 10).into_iter().find(|m| m.mid == mid).unwrap().state,
        1,
        "delivered after the peer appeared"
    );

    std::fs::remove_dir_all(&tmp).ok();
}
