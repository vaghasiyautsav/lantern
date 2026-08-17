//! Phase 3 gate: an interrupted transfer resumes from its partial state —
//! the sender ships only the missing chunks, and the finished file is
//! byte-identical.

use std::time::Duration;

use lantern_core::{Core, CoreConfig, CoreEvent, CHUNK_SIZE};
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
async fn interrupted_transfer_resumes_from_partial() {
    let tmp = std::env::temp_dir().join(format!("lantern-resume-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    let (port_a, port_b) = (24401u16, 24402u16);

    // 20-chunk file (20 MiB), deterministic content.
    let total_chunks = 20u32;
    let payload: Vec<u8> = (0..total_chunks as usize * CHUNK_SIZE as usize)
        .map(|i| (i % 253) as u8)
        .collect();
    let src = tmp.join("big-dataset.bin");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(&src, &payload).unwrap();
    let root = *blake3::hash(&payload).as_bytes();

    // Simulate the aftermath of a kill -9 at 50%: the receiver's partial
    // dir already holds chunks 0..10 verified, exactly as store_chunk
    // would have left them — data at offsets, indices in the sidecar.
    let bob_dir = tmp.join("b");
    let partial_dir = bob_dir.join("partial");
    std::fs::create_dir_all(&partial_dir).unwrap();
    let stem = hex::encode(root);
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut data = std::fs::File::create(partial_dir.join(format!("{stem}.data"))).unwrap();
        let mut sidecar =
            std::fs::File::create(partial_dir.join(format!("{stem}.json"))).unwrap();
        for idx in 0..10u32 {
            let start = idx as usize * CHUNK_SIZE as usize;
            data.seek(SeekFrom::Start(start as u64)).unwrap();
            data.write_all(&payload[start..start + CHUNK_SIZE as usize])
                .unwrap();
            writeln!(sidecar, "{idx}").unwrap();
        }
    }

    let (alice, mut ev_a) = Core::start(config("Alice", &tmp.join("a"), port_a, port_b))
        .await
        .unwrap();
    let (bob, mut ev_b) = Core::start(config("Bob", &bob_dir, port_b, port_a))
        .await
        .unwrap();
    alice.announce().await;
    bob.announce().await;
    wait_for(&mut ev_a, 10, |e| {
        matches!(e, CoreEvent::PeerSeen { name, .. } if name == "Bob")
    })
    .await;

    // Alice sends. Bob's AcceptFile must ask for only the missing half.
    alice.send_file(bob.identity_id(), &src).await.unwrap();

    let stats = wait_for(&mut ev_a, 20, |e| {
        matches!(e, CoreEvent::ChunksSent { .. })
    })
    .await;
    let CoreEvent::ChunksSent { sent, total, .. } = stats else {
        unreachable!()
    };
    assert_eq!(total, total_chunks);
    assert_eq!(
        sent, 10,
        "resume must ship only the missing chunks, not the whole file"
    );

    // The file completes and is byte-identical.
    let received = wait_for(&mut ev_b, 30, |e| {
        matches!(e, CoreEvent::FileReceived { .. })
    })
    .await;
    let CoreEvent::FileReceived { path, .. } = received else {
        unreachable!()
    };
    let got = std::fs::read(&path).unwrap();
    assert_eq!(blake3::hash(&got), blake3::hash(&payload));

    // Partial state is gone once finalized.
    assert!(!partial_dir.join(format!("{stem}.json")).exists());
    assert!(!partial_dir.join(format!("{stem}.data")).exists());

    // Send the SAME content again. The partial state was consumed by
    // finalize, so this is a full re-transfer landing under a
    // collision-safe name. (A content-addressed chunk cache would make
    // this near-free — tracked as future Phase 3 work, deliberately not
    // faked here.)
    alice.send_file(bob.identity_id(), &src).await.unwrap();
    let stats2 = wait_for(&mut ev_a, 20, |e| {
        matches!(e, CoreEvent::ChunksSent { .. })
    })
    .await;
    let CoreEvent::ChunksSent { sent, .. } = stats2 else {
        unreachable!()
    };
    assert_eq!(sent, total_chunks, "post-finalize re-send is a full transfer");
    let received2 = wait_for(&mut ev_b, 30, |e| {
        matches!(e, CoreEvent::FileReceived { .. })
    })
    .await;
    let CoreEvent::FileReceived { path: p2, .. } = received2 else {
        unreachable!()
    };
    assert_ne!(p2, path, "second copy gets a collision-safe name");

    std::fs::remove_dir_all(&tmp).ok();
}
