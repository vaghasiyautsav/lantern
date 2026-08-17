//! Phase 1.5 + 2 gates: a file crosses between two nodes verified,
//! and an impostor key claiming a known name+host raises a TrustWarning.

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
    }
}

async fn wait_for<F>(rx: &mut mpsc::Receiver<CoreEvent>, secs: u64, mut pred: F) -> CoreEvent
where
    F: FnMut(&CoreEvent) -> bool,
{
    timeout(Duration::from_secs(secs), async {
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
async fn file_transfer_verified() {
    let tmp = std::env::temp_dir().join(format!("lantern-ft-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    let (port_a, port_b) = (24201u16, 24202u16);

    let (alice, mut ev_a) = Core::start(config("Alice", &tmp.join("a"), port_a, port_b))
        .await
        .unwrap();
    let (bob, mut ev_b) = Core::start(config("Bob", &tmp.join("b"), port_b, port_a))
        .await
        .unwrap();
    alice.announce().await;
    bob.announce().await;
    wait_for(&mut ev_a, 10, |e| {
        matches!(e, CoreEvent::PeerSeen { name, .. } if name == "Bob")
    })
    .await;

    // A 3 MiB file with non-trivial content.
    let payload: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let src = tmp.join("dataset.bin");
    std::fs::write(&src, &payload).unwrap();
    let expected_hash = blake3::hash(&payload);

    let xid = alice.send_file(bob.identity_id(), &src).await.unwrap();

    // Bob sees the offer, then the verified receive.
    wait_for(&mut ev_b, 10, |e| {
        matches!(e, CoreEvent::FileOffered { name, size, .. }
            if name == "dataset.bin" && *size == payload.len() as u64)
    })
    .await;
    let received = wait_for(&mut ev_b, 20, |e| {
        matches!(e, CoreEvent::FileReceived { .. })
    })
    .await;
    let CoreEvent::FileReceived { path, .. } = received else {
        unreachable!()
    };
    let got = std::fs::read(&path).unwrap();
    assert_eq!(got.len(), payload.len());
    assert_eq!(blake3::hash(&got), expected_hash, "bytes identical after transfer");

    // Alice hears the confirmation.
    let done = wait_for(&mut ev_a, 10, |e| {
        matches!(e, CoreEvent::FileSent { xid: x, .. } if *x == xid)
    })
    .await;
    assert!(matches!(done, CoreEvent::FileSent { ok: true, .. }));

    // Same name arriving twice gets a collision-safe suffix, not an overwrite.
    alice.send_file(bob.identity_id(), &src).await.unwrap();
    let second = wait_for(&mut ev_b, 20, |e| {
        matches!(e, CoreEvent::FileReceived { .. })
    })
    .await;
    let CoreEvent::FileReceived { path: p2, .. } = second else {
        unreachable!()
    };
    assert_ne!(p2, path, "second copy does not clobber the first");

    std::fs::remove_dir_all(&tmp).ok();
}

#[tokio::test]
async fn impostor_raises_trust_warning() {
    let tmp = std::env::temp_dir().join(format!("lantern-tofu-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    let (port_a, port_b) = (24301u16, 24302u16);

    let (alice, mut ev_a) = Core::start(config("Alice", &tmp.join("a"), port_a, port_b))
        .await
        .unwrap();
    let (bob, _ev_b) = Core::start(config("Bob", &tmp.join("bob-real"), port_b, port_a))
        .await
        .unwrap();
    alice.announce().await;
    bob.announce().await;
    wait_for(&mut ev_a, 10, |e| {
        matches!(e, CoreEvent::PeerSeen { name, .. } if name == "Bob")
    })
    .await;
    let real_bob = bob.identity_id();

    // "Bob" appears again — same display name, same host, fresh key.
    let (impostor, _ev_i) = Core::start(config("Bob", &tmp.join("bob-fake"), 24303, port_a))
        .await
        .unwrap();
    impostor.announce().await;
    assert_ne!(impostor.identity_id(), real_bob);

    let warning = wait_for(&mut ev_a, 10, |e| {
        matches!(e, CoreEvent::TrustWarning { .. })
    })
    .await;
    let CoreEvent::TrustWarning { id, detail } = warning else {
        unreachable!()
    };
    assert_eq!(id, impostor.identity_id(), "warning names the NEW key");
    assert!(detail.contains("NEW key"), "warning says what happened: {detail}");

    std::fs::remove_dir_all(&tmp).ok();
}
