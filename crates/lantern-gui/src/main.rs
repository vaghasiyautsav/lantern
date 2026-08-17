//! Lantern GUI: the full engine with a local graphical interface.
//!
//! Serves on 127.0.0.1 ONLY — the interface is reachable from this machine
//! and nowhere else. All peer traffic is the same QUIC/TLS protocol the CLI
//! speaks; the browser is just the window glass.
//!
//! This is the interim GUI. The native SwiftUI / GTK4 shells from the
//! design doc replace it; the HTTP API here deliberately mirrors the core
//! API those shells will consume via FFI.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{ws::WebSocketUpgrade, Path as AxPath, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use lantern_core::{Core, CoreConfig, CoreEvent};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

#[derive(Parser, Debug)]
#[command(name = "lantern-gui", about = "Lantern — serverless LAN messenger (GUI)")]
struct Args {
    #[arg(long, default_value = "")]
    name: String,
    #[arg(long, default_value = "~/.lantern")]
    data_dir: String,
    #[arg(long, default_value_t = 3939)]
    discovery_port: u16,
    /// Comma-separated beacon target ports (same-host testing).
    #[arg(long, default_value = "3939")]
    targets: String,
    /// Real-LAN mode: subnet broadcast on. Pass --broadcast=false for
    /// same-machine testing.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    broadcast: bool,
    /// Localhost port for the interface.
    #[arg(long, default_value_t = 3999)]
    gui_port: u16,
}

struct App {
    core: Arc<Core>,
    events: broadcast::Sender<String>,
    name: String,
    upload_dir: PathBuf,
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

    let name = if args.name.is_empty() {
        std::env::var("USER").unwrap_or_else(|_| "me".into())
    } else {
        args.name.clone()
    };
    let data_dir = expand_tilde(&args.data_dir);
    let targets: Vec<u16> = args
        .targets
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let (core, mut events_rx) = Core::start(CoreConfig {
        data_dir: data_dir.clone(),
        display_name: name.clone(),
        discovery_port: args.discovery_port,
        beacon_targets: targets,
        broadcast: args.broadcast,
        quic_port: 0,
        in_memory_store: false,
        download_dir: lantern_core::user_download_dir(),
    })
    .await?;
    core.announce().await;

    let (events_tx, _) = broadcast::channel::<String>(256);
    let app = Arc::new(App {
        core: core.clone(),
        events: events_tx.clone(),
        name: name.clone(),
        upload_dir: data_dir.join("outbox"),
    });

    // Pump core events → JSON → every connected browser.
    tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            let msg = event_json(&ev);
            let _ = events_tx.send(msg.to_string());
        }
    });

    let router = Router::new()
        .route("/", get(index))
        .route("/api/me", get(me))
        .route("/api/peers", get(peers))
        .route("/api/history/{id}", get(history))
        .route("/api/msg", post(send_msg))
        .route("/api/file", post(send_file))
        .route("/api/filepath", post(send_filepath))
        .route("/api/trust", post(trust))
        .route("/ws", get(ws_upgrade))
        .with_state(app);

    let addr: SocketAddr = ([127, 0, 0, 1], args.gui_port).into();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Lantern GUI → http://localhost:{}", args.gui_port);
    println!("identity: {}", Core::words_for(&core.identity_id()).join(" · "));
    axum::serve(listener, router).await?;
    Ok(())
}

fn event_json(ev: &CoreEvent) -> serde_json::Value {
    match ev {
        CoreEvent::PeerSeen { id, name, host, addr, new } => json!({
            "type": "peer", "id": hex::encode(id), "name": name,
            "host": host, "addr": addr.to_string(), "new": new
        }),
        CoreEvent::SessionEstablished { id } => {
            json!({ "type": "session", "id": hex::encode(id) })
        }
        CoreEvent::MessageReceived { peer, peer_name, mid, text, ts } => json!({
            "type": "msg", "peer": hex::encode(peer), "peer_name": peer_name,
            "mid": mid.to_string(), "text": text, "ts": ts
        }),
        CoreEvent::MessageDelivered { mid } => {
            json!({ "type": "delivered", "mid": mid.to_string() })
        }
        CoreEvent::TrustWarning { id, detail } => json!({
            "type": "trust-warning", "id": hex::encode(id), "detail": detail
        }),
        CoreEvent::FileOffered { peer, peer_name, xid, name, size } => json!({
            "type": "file-offered", "peer": hex::encode(peer), "peer_name": peer_name,
            "xid": xid.to_string(), "name": name, "size": size
        }),
        CoreEvent::FileReceived { peer, xid, name, path, size } => json!({
            "type": "file-received", "peer": hex::encode(peer),
            "xid": xid.to_string(), "name": name,
            "path": path.display().to_string(), "size": size
        }),
        CoreEvent::FileSent { xid, ok, err } => json!({
            "type": "file-sent", "xid": xid.to_string(), "ok": ok, "err": err
        }),
        CoreEvent::ChunksSent { xid, sent, total } => json!({
            "type": "chunks-sent", "xid": xid.to_string(), "sent": sent, "total": total
        }),
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn me(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let id = app.core.identity_id();
    Json(json!({
        "name": app.name,
        "id": hex::encode(id),
        "short": hex::encode(&id[..6]),
        "words": Core::words_for(&id),
        "quic_port": app.core.quic_port(),
    }))
}

async fn peers(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let peers = app.core.peers().await;
    let list: Vec<_> = peers
        .into_iter()
        .map(|(id, name, host, addr)| {
            json!({
                "id": hex::encode(id),
                "name": name,
                "host": host,
                "addr": addr.to_string(),
                "verified": app.core.is_verified(&id),
                "words": Core::words_for(&id),
            })
        })
        .collect();
    Json(json!(list))
}

fn parse_id(hexid: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hexid).ok()?;
    bytes.try_into().ok()
}

async fn history(
    State(app): State<Arc<App>>,
    AxPath(id): AxPath<String>,
) -> impl IntoResponse {
    let Some(pid) = parse_id(&id) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad id"}))).into_response();
    };
    let msgs: Vec<_> = app
        .core
        .history(&pid, 500)
        .into_iter()
        .map(|m| {
            json!({
                "mid": m.mid.to_string(),
                "outgoing": m.outgoing,
                "ts": m.ts,
                "text": m.text,
                "state": m.state,
            })
        })
        .collect();
    Json(json!(msgs)).into_response()
}

#[derive(Deserialize)]
struct MsgReq {
    peer: String,
    text: String,
}

async fn send_msg(
    State(app): State<Arc<App>>,
    Json(req): Json<MsgReq>,
) -> impl IntoResponse {
    let Some(pid) = parse_id(&req.peer) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad id"}))).into_response();
    };
    match app.core.send_message(pid, &req.text).await {
        Ok(mid) => Json(json!({"mid": mid.to_string()})).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct FileParams {
    peer: String,
    name: String,
}

async fn send_file(
    State(app): State<Arc<App>>,
    Query(params): Query<FileParams>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(pid) = parse_id(&params.peer) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad id"}))).into_response();
    };
    // The browser hands us bytes; the core API wants a path. Stage into the
    // outbox dir under a sanitized name, then hand off.
    let safe: String = params
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("file")
        .chars()
        .filter(|c| !matches!(c, ':' | '\0'))
        .collect();
    let safe = if safe.is_empty() || safe.starts_with('.') {
        format!("upload-{}", uuid::Uuid::new_v4())
    } else {
        safe
    };
    if let Err(e) = tokio::fs::create_dir_all(&app.upload_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let path = app.upload_dir.join(&safe);
    if let Err(e) = tokio::fs::write(&path, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    match app.core.send_file(pid, &path).await {
        Ok(xid) => Json(json!({"xid": xid.to_string(), "name": safe})).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct FilePathReq {
    peer: String,
    path: String,
}

/// Send a file the native shell already has a real path for. The server
/// binds to 127.0.0.1 only, so callers are by definition local — same
/// user, same machine — and may reference any path they can read anyway.
async fn send_filepath(
    State(app): State<Arc<App>>,
    Json(req): Json<FilePathReq>,
) -> impl IntoResponse {
    let Some(pid) = parse_id(&req.peer) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad id"}))).into_response();
    };
    let path = std::path::PathBuf::from(&req.path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    match app.core.send_file(pid, &path).await {
        Ok(xid) => Json(json!({"xid": xid.to_string(), "name": name})).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct TrustReq {
    peer: String,
}

async fn trust(
    State(app): State<Arc<App>>,
    Json(req): Json<TrustReq>,
) -> impl IntoResponse {
    let Some(pid) = parse_id(&req.peer) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad id"}))).into_response();
    };
    app.core.set_verified(&pid, true);
    Json(json!({"ok": true})).into_response()
}

async fn ws_upgrade(
    State(app): State<Arc<App>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = app.events.subscribe();
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(msg) => {
                        if socket
                            .send(axum::extract::ws::Message::Text(msg.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(_)) => continue, // pings etc.
                    _ => break,
                },
            }
        }
    })
}
