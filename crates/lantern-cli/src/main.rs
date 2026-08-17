//! Headless harness: run a Lantern node in a terminal.
//!
//! Two of these in two terminals is the whole point of Phase 1.
//!
//! ```text
//! lantern run --name Mira --data-dir /tmp/mira \
//!     --discovery-port 4001 --targets 4001,4002
//! ```
//! Commands on stdin: /peers · /msg <name-prefix> <text> · /whoami · /quit

use std::io::Write as _;
use std::path::PathBuf;

use clap::Parser;
use lantern_core::{Core, CoreConfig, CoreEvent};

#[derive(Parser, Debug)]
#[command(name = "lantern", about = "Serverless LAN messenger (prototype CLI)")]
struct Args {
    /// Display name shown to peers.
    #[arg(long, default_value = "anon")]
    name: String,
    /// Where identity key + message db live.
    #[arg(long, default_value = "~/.lantern")]
    data_dir: String,
    /// UDP port to listen for beacons on.
    #[arg(long, default_value_t = 3939)]
    discovery_port: u16,
    /// Comma-separated beacon target ports (for same-host testing).
    #[arg(long, default_value = "3939")]
    targets: String,
    /// Subnet broadcast. On by default, matching the GUI shells — with it off
    /// this node is invisible to every other machine and says nothing about it.
    /// Pass `--broadcast=false` for isolated same-host tests.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    broadcast: bool,
    /// Keep the message log in memory only.
    #[arg(long, default_value_t = false)]
    ephemeral: bool,
    /// Print events as machine-readable lines (for tests).
    #[arg(long, default_value_t = false)]
    script: bool,
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();
    let targets: Vec<u16> = args
        .targets
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let (core, mut events) = Core::start(CoreConfig {
        data_dir: expand_tilde(&args.data_dir),
        display_name: args.name.clone(),
        discovery_port: args.discovery_port,
        beacon_targets: targets,
        broadcast: args.broadcast,
        quic_port: 0,
        in_memory_store: args.ephemeral,
        download_dir: lantern_core::user_download_dir(),
    })
    .await?;

    let id = core.identity_id();
    println!("lantern · {} · id {}…", args.name, hex::encode(&id[..6]));
    println!("safety words: {}", core.fingerprint_words().join(" · "));
    println!("quic port {} · discovery port {}", core.quic_port(), args.discovery_port);
    core.announce().await;

    // Event printer.
    let script = args.script;
    let core_events = core.clone();
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                CoreEvent::PeerSeen { id, name, host, addr, new } => {
                    if new {
                        if script {
                            println!("EVENT peer-new {} {} {}", hex::encode(&id[..6]), name, addr);
                        } else {
                            println!("● {name} ({host}) appeared at {addr}");
                        }
                    }
                }
                CoreEvent::SessionEstablished { id } => {
                    if script {
                        println!("EVENT session {}", hex::encode(&id[..6]));
                    }
                }
                CoreEvent::MessageReceived { peer_name, text, mid, .. } => {
                    if script {
                        println!("EVENT msg {mid} {peer_name} {text}");
                    } else {
                        println!("{peer_name}: {text}");
                    }
                }
                CoreEvent::MessageDelivered { mid } => {
                    if script {
                        println!("EVENT delivered {mid}");
                    } else {
                        println!("  ✓ delivered");
                    }
                }
                CoreEvent::TrustWarning { detail, .. } => {
                    println!("⚠ TRUST: {detail}");
                }
                CoreEvent::FileOffered { peer_name, name, size, .. } => {
                    if script {
                        println!("EVENT file-offer {peer_name} {name} {size}");
                    } else {
                        println!("⇣ {peer_name} is sending {name} ({})", human_size(size));
                    }
                }
                CoreEvent::FileReceived { name, path, size, .. } => {
                    if script {
                        println!("EVENT file-received {name} {size} {}", path.display());
                    } else {
                        println!("✓ received {name} ({}) → {}", human_size(size), path.display());
                    }
                }
                // Per-chunk and far too noisy for a terminal log; the CLI
                // reports the summary lines instead.
                CoreEvent::TransferProgress { .. } => {}
                CoreEvent::ChunksSent { xid, sent, total } => {
                    if script {
                        println!("EVENT chunks-sent {xid} {sent} {total}");
                    } else if sent < total {
                        println!("  ↻ resume: peer needed only {sent} of {total} chunks");
                    } else {
                        println!("  streaming {total} chunks…");
                    }
                }
                CoreEvent::FileSent { xid, ok, err } => {
                    if script {
                        println!("EVENT file-sent {xid} {ok}");
                    } else if ok {
                        println!("  ✓ file delivered and verified");
                    } else {
                        println!("  ✗ file refused: {}", err.unwrap_or_default());
                    }
                }
            }
            let _ = std::io::stdout().flush();
            let _ = &core_events; // keep core alive in this task
        }
    });

    // Stdin command loop.
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("/msg ") {
            let Some((target, text)) = rest.split_once(' ') else {
                println!("usage: /msg <name-prefix> <text>");
                continue;
            };
            let peers = core.peers().await;
            let matched: Vec<_> = peers
                .iter()
                .filter(|(_, name, _, _)| name.to_lowercase().starts_with(&target.to_lowercase()))
                .collect();
            match matched.as_slice() {
                [] => println!("no peer matching '{target}'"),
                [(pid, name, _, _)] => match core.send_message(*pid, text).await {
                    Ok(mid) => {
                        if args.script {
                            println!("SENT {mid}");
                        } else {
                            println!("→ {name}: {text}");
                        }
                    }
                    Err(e) => println!("send failed: {e}"),
                },
                many => println!(
                    "ambiguous: {}",
                    many.iter()
                        .map(|(_, n, _, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        } else if let Some(rest) = line.strip_prefix("/send ") {
            let Some((target, path)) = rest.split_once(' ') else {
                println!("usage: /send <name-prefix> <path>");
                continue;
            };
            match find_peer(&core, target).await {
                FindResult::One(pid, name) => {
                    match core.send_file(pid, std::path::Path::new(path.trim())).await {
                        Ok(xid) => {
                            if args.script {
                                println!("SENT-FILE {xid}");
                            } else {
                                println!("→ sending {} to {name}…", path.trim());
                            }
                        }
                        Err(e) => println!("send failed: {e}"),
                    }
                }
                other => other.report(target),
            }
        } else if let Some(target) = line.strip_prefix("/verify ") {
            match find_peer(&core, target.trim()).await {
                FindResult::One(pid, name) => {
                    println!("{name}'s safety words — compare with what THEY see on /whoami:");
                    println!("  {}", Core::words_for(&pid).join(" · "));
                    println!(
                        "  verified: {} — /trust {name} to confirm a match",
                        if core.is_verified(&pid) { "yes" } else { "not yet" }
                    );
                }
                other => other.report(target),
            }
        } else if let Some(target) = line.strip_prefix("/trust ") {
            match find_peer(&core, target.trim()).await {
                FindResult::One(pid, name) => {
                    core.set_verified(&pid, true);
                    println!("✓ {name} marked verified");
                }
                other => other.report(target),
            }
        } else if line == "/peers" {
            let peers = core.peers().await;
            if peers.is_empty() {
                println!("(nobody yet)");
            }
            for (pid, name, host, addr) in peers {
                println!("  {} {name} ({host}) {addr}", hex::encode(&pid[..6]));
            }
        } else if line == "/whoami" {
            println!("{} · {}", args.name, hex::encode(core.identity_id()));
            println!("safety words: {}", core.fingerprint_words().join(" · "));
        } else if line == "/quit" {
            break;
        } else {
            println!(
                "commands: /peers · /msg <name> <text> · /send <name> <path> · \
                 /verify <name> · /trust <name> · /whoami · /quit"
            );
        }
    }
    Ok(())
}

enum FindResult {
    One([u8; 32], String),
    None,
    Many(Vec<String>),
}

impl FindResult {
    fn report(&self, target: &str) {
        match self {
            FindResult::None => println!("no peer matching '{target}'"),
            FindResult::Many(names) => println!("ambiguous: {}", names.join(", ")),
            FindResult::One(..) => {}
        }
    }
}

async fn find_peer(core: &Core, prefix: &str) -> FindResult {
    let peers = core.peers().await;
    let matched: Vec<_> = peers
        .iter()
        .filter(|(_, name, _, _)| name.to_lowercase().starts_with(&prefix.to_lowercase()))
        .collect();
    match matched.as_slice() {
        [] => FindResult::None,
        [(pid, name, _, _)] => FindResult::One(*pid, name.clone()),
        many => FindResult::Many(many.iter().map(|(_, n, _, _)| n.clone()).collect()),
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
