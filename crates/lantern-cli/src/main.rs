//! Headless harness: run a Lantern node in a terminal.
//!
//! Two of these in two terminals is the whole point of Phase 1.
//!
//! ```text
//! lantern run --name Mira --data-dir /tmp/mira \
//!     --discovery-port 4001 --targets 4001,4002
//! ```
//! Commands on stdin: /peers · /msg <name-prefix> <text> ·
//! /clear <name-prefix> (delete this machine's history with them) ·
//! /whoami · /quit

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
    /// Enable subnet broadcast (real LAN mode).
    #[arg(long, default_value_t = false)]
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
                CoreEvent::ChunksSent { xid, sent, total } => {
                    if script {
                        println!("EVENT chunks-sent {xid} {sent} {total}");
                    } else if sent < total {
                        println!("  ↻ resume: peer needed only {sent} of {total} chunks");
                    } else {
                        println!("  streaming {total} chunks…");
                    }
                }
                CoreEvent::TransferProgress { xid, outgoing, done, total, bps, eta_s } => {
                    if script {
                        println!(
                            "EVENT progress {xid} {} {done} {total} {}",
                            if outgoing { "out" } else { "in" },
                            bps.unwrap_or(0)
                        );
                    } else {
                        // One line that rewrites itself: a terminal watching a
                        // 2 GB transfer shouldn't scroll a thousand lines.
                        print!(
                            "\r  {} {} / {}{}{}   ",
                            if outgoing { "↑" } else { "↓" },
                            human_size(done),
                            human_size(total),
                            bps.map(|b| format!(" · {}/s", human_size(b)))
                                .unwrap_or_default(),
                            eta_s
                                .map(|s| if s == 0 {
                                    String::new()
                                } else {
                                    format!(" · {s}s left")
                                })
                                .unwrap_or_default(),
                        );
                        if done >= total {
                            println!();
                        }
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
                .filter(|p| p.name.to_lowercase().starts_with(&target.to_lowercase()))
                .collect();
            match matched.as_slice() {
                [] => println!("no peer matching '{target}'"),
                [p] => match core.send_message(p.id, text).await {
                    Ok(mid) => {
                        if args.script {
                            println!("SENT {mid}");
                        } else {
                            println!("→ {}: {text}", p.name);
                        }
                    }
                    Err(e) => println!("send failed: {e}"),
                },
                many => println!(
                    "ambiguous: {}",
                    many.iter()
                        .map(|p| p.name.as_str())
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
        } else if let Some(target) = line.strip_prefix("/clear ") {
            match find_peer(&core, target.trim()).await {
                FindResult::One(pid, name) => {
                    // Local only — the peer keeps their copy, and there is no
                    // undo, so a headless shell states both before it goes.
                    let n = core.clear_history(&pid);
                    if args.script {
                        println!("CLEARED {n}");
                    } else if n == 0 {
                        println!("nothing stored for {name} here");
                    } else {
                        println!(
                            "✓ deleted {n} message{} with {name} from this machine \
                             ({name} keeps their own copy)",
                            if n == 1 { "" } else { "s" }
                        );
                    }
                }
                other => other.report(target.trim()),
            }
        } else if line == "/update" || line == "/update now" {
            let check = core.check_update().await;
            println!("{}", check.summary());
            for c in &check.commits {
                println!("  · {c}");
            }
            if line == "/update now" {
                if !check.can_update() {
                    println!("nothing to install");
                } else {
                    match core.start_update() {
                        // The updater stops the engine and reopens the app, so
                        // this CLI's own engine is about to go away with it.
                        Ok(()) => println!(
                            "updating in the background — watch \
                             ~/.lantern/update.log; this node will be stopped"
                        ),
                        Err(e) => println!("can't update: {e}"),
                    }
                }
            } else if check.can_update() {
                println!("run /update now to install");
            }
        } else if line == "/version" {
            let b = lantern_core::update::BuildInfo::current();
            println!("build {} ({})", b.commit, b.date);
            match b.repo {
                Some(p) => println!("built from {}", p.display()),
                None => println!("built without a source checkout"),
            }
            if let Some(s) = core.last_update_state() {
                println!("last update: {} — {}", s.state, s.message);
            }
        } else if line == "/peers" {
            let peers = core.peers().await;
            if peers.is_empty() {
                println!("(nobody yet)");
            }
            for p in peers {
                let presence = if p.online {
                    "online".to_string()
                } else {
                    match p.since_beacon {
                        Some(d) => format!("offline, last seen {}s ago", d.as_secs()),
                        None => "offline".to_string(),
                    }
                };
                println!(
                    "  {} {} ({}) {} — {presence}",
                    hex::encode(&p.id[..6]),
                    p.name,
                    p.host,
                    p.quic_addr
                );
            }
        } else if line == "/whoami" {
            println!("{} · {}", args.name, hex::encode(core.identity_id()));
            println!("safety words: {}", core.fingerprint_words().join(" · "));
        } else if line == "/quit" {
            break;
        } else {
            println!(
                "commands: /peers · /msg <name> <text> · /send <name> <path> · \
                 /verify <name> · /trust <name> · /clear <name> · /whoami · \
                 /version · /update [now] · /quit"
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
        .filter(|p| p.name.to_lowercase().starts_with(&prefix.to_lowercase()))
        .collect();
    match matched.as_slice() {
        [] => FindResult::None,
        [p] => FindResult::One(p.id, p.name.clone()),
        many => FindResult::Many(many.iter().map(|p| p.name.clone()).collect()),
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
