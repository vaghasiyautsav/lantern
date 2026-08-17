//! `lantern-doctor` — answers one question: is this machine sending beacons
//! anyone can hear, and is it hearing theirs?
//!
//! Run it on both machines at the same time. It prints the interface table, the
//! exact addresses it beacons to and whether each `sendto` succeeded, and then
//! every datagram that arrives on the discovery port — including ones that fail
//! to decode, which is how you tell "nothing arrives" apart from "something
//! arrives and Lantern rejects it".
//!
//! It sets `SO_REUSEPORT`, so it can run alongside a live Lantern; the kernel
//! delivers a copy of each broadcast to both. Quitting the app first still
//! gives the cleanest read.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use clap::Parser;
use lantern_crypto::Identity;
use lantern_discovery::net;
use lantern_proto::{Beacon, BeaconType, MAX_BEACON_BYTES};

#[derive(Parser, Debug)]
#[command(name = "lantern-doctor", about = "Diagnose Lantern LAN discovery")]
struct Args {
    /// Discovery port to bind and beacon to.
    #[arg(long, default_value_t = 3939)]
    port: u16,
    /// How long to listen, in seconds.
    #[arg(long, default_value_t = 30)]
    seconds: u64,
    /// Seconds between probe beacons.
    #[arg(long, default_value_t = 3)]
    every: u64,
    /// Name this probe advertises.
    #[arg(long, default_value = "lantern-doctor")]
    name: String,
    /// Listen only; send nothing.
    #[arg(long, default_value_t = false)]
    passive: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    println!("lantern-doctor · {} · port {}", host(), args.port);
    println!();

    // ---- 1. interfaces -----------------------------------------------------
    let ifaces = net::interfaces();
    println!("INTERFACES");
    if ifaces.is_empty() {
        println!("  (none — getifaddrs returned nothing; discovery cannot work)");
    }
    for i in &ifaces {
        println!(
            "  {:<12} {:<16} mask {:<16} bcast {:<16}{}",
            i.name,
            i.ip.to_string(),
            i.netmask.to_string(),
            i.broadcast
                .map(|b| b.to_string())
                .unwrap_or_else(|| "-".into()),
            if i.loopback { "  (loopback)" } else { "" }
        );
    }
    println!();

    let targets = net::broadcast_targets();
    println!("BEACON TARGETS  (one per subnet, plus the limited broadcast)");
    for t in &targets {
        println!(
            "  {}:{}{}",
            t,
            args.port,
            if *t == Ipv4Addr::BROADCAST {
                "   ← goes out ONE interface only; the others above are what actually reach the LAN"
            } else {
                ""
            }
        );
    }
    if targets.len() == 1 {
        println!();
        println!("  ! No non-loopback IPv4 interface has a usable broadcast address.");
        println!("    Discovery cannot reach another machine from here.");
    }
    println!();

    // ---- 2. bind -----------------------------------------------------------
    let discovery = match lantern_discovery::Discovery::bind(lantern_discovery::DiscoveryConfig {
        bind_port: args.port,
        target_ports: vec![args.port],
        broadcast: true,
    })
    .await
    {
        Ok(d) => d,
        Err(e) => {
            println!("BIND FAILED on 0.0.0.0:{} — {e}", args.port);
            println!();
            println!("  If this says 'address already in use', an older build without");
            println!("  SO_REUSEPORT is holding the port. Quit Lantern and retry.");
            return Ok(());
        }
    };
    println!("BOUND 0.0.0.0:{} (reuseaddr + reuseport)", args.port);
    println!();

    let identity = Identity::generate();
    let id = identity.public_bytes();

    // ---- 3. send -----------------------------------------------------------
    if !args.passive {
        let probe = probe_beacon(&args.name, id, args.port, 0);
        println!("PROBE SEND");
        for o in discovery
            .send_beacon_reporting(&probe, identity.signing_key())
            .await
        {
            match o.result {
                Ok(n) => println!("  ok    {:<22} {n} bytes", o.dst.to_string()),
                Err(e) => println!("  FAIL  {:<22} {e}", o.dst.to_string()),
            }
        }
        println!();
    }

    // ---- 4. listen ---------------------------------------------------------
    println!(
        "LISTENING {}s — run this on the other machine too",
        args.seconds
    );
    println!();

    let mut heard: HashMap<[u8; 32], (String, SocketAddr, u32)> = HashMap::new();
    let mut undecodable = 0usize;
    let mut own = 0usize;
    let mut datagrams = 0usize;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.seconds);
    let mut probe_tick = tokio::time::interval(Duration::from_secs(args.every.max(1)));
    probe_tick.tick().await; // fires immediately; we already sent one
    let mut seq = 1u64;
    let mut buf = vec![0u8; MAX_BEACON_BYTES * 2];

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,

            _ = probe_tick.tick(), if !args.passive => {
                let probe = probe_beacon(&args.name, id, args.port, seq);
                seq += 1;
                let outs = discovery
                    .send_beacon_reporting(&probe, identity.signing_key())
                    .await;
                let failed = outs.iter().filter(|o| o.result.is_err()).count();
                if failed > 0 {
                    println!("  … probe #{seq}: {failed}/{} sends failed", outs.len());
                }
            }

            r = discovery.recv_raw(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                datagrams += 1;
                match Beacon::decode(&buf[..n]) {
                    Ok(b) if b.id == id => {
                        own += 1;
                    }
                    Ok(b) => {
                        let e = heard.entry(b.id).or_insert_with(|| {
                            println!(
                                "  HEARD {:<14} from {:<21} quic:{} id {}",
                                b.name,
                                from.to_string(),
                                b.port,
                                hex::encode(&b.id[..4])
                            );
                            (b.name.clone(), from, 0)
                        });
                        e.2 += 1;
                    }
                    Err(err) => {
                        undecodable += 1;
                        if undecodable <= 5 {
                            println!("  BAD   {n:>4} bytes from {from} — {err}");
                        }
                    }
                }
            }
        }
    }

    // ---- 5. verdict --------------------------------------------------------
    println!();
    println!("SUMMARY");
    println!("  datagrams received       {datagrams}");
    println!("  own beacons looped back  {own}");
    println!("  undecodable              {undecodable}");
    println!("  distinct peers heard     {}", heard.len());
    for (pid, (name, from, count)) in &heard {
        println!(
            "    {:<14} {:<21} ×{count}  id {}",
            name,
            from.to_string(),
            hex::encode(&pid[..4])
        );
    }
    println!();

    if !heard.is_empty() {
        println!("  ✓ This machine hears other Lantern nodes. Discovery works here.");
        println!("    If the app still shows an empty roster, the problem is above the");
        println!("    discovery layer — check the QUIC port in the beacon is reachable.");
    } else if own == 0 && !args.passive {
        println!("  ✗ Not even our own broadcast came back.");
        println!("    A local firewall is dropping inbound UDP {}. On macOS check", args.port);
        println!("    System Settings → Network → Firewall; on Linux check ufw/firewalld.");
    } else {
        println!("  ✗ Nothing heard from any other machine.");
        println!("    Our own beacon looped back, so the socket is fine — the packets");
        println!("    are being lost between the two hosts. Most likely, in order:");
        println!("      1. The other machine is not running Lantern, or is on a");
        println!("         build older than this fix (pre-fix CLI defaulted");
        println!("         --broadcast to false and only ever sent to 255.255.255.255).");
        println!("      2. The two are not on the same subnet. Compare the IPs above.");
        println!("      3. Wireless client isolation on the access point — very common");
        println!("         on guest networks; it drops broadcast between clients.");
        println!("      4. Inbound UDP {} blocked by the other machine's firewall.", args.port);
    }

    Ok(())
}

fn probe_beacon(name: &str, id: [u8; 32], port: u16, seq: u64) -> Beacon {
    Beacon {
        beacon_type: BeaconType::Hello,
        flags: 0,
        id,
        name: name.to_string(),
        host: host(),
        group: String::new(),
        device: Default::default(),
        port,
        state: Default::default(),
        status: "diagnostic probe".into(),
        avatar: None,
        caps: 0,
        seq,
        boot: [0xD0, 0xC7, 0x08, 0, 0, 0, 0, 1],
        ts: lantern_discovery::now_ms(),
    }
}

fn host() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}
