//! Orchestration: roster, sessions, message flow, event bus.
//!
//! The core owns every decision that isn't presentation. UIs (and the CLI)
//! talk to it through `Core` methods and consume `CoreEvent`s.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lantern_crypto::Identity;
use lantern_discovery::{now_ms, Discovered, Discovery, DiscoveryConfig, OFFLINE_AFTER};
use lantern_proto::{
    read_frame, write_frame, Beacon, BeaconType, ControlFrame, DeviceClass, PresenceState,
};
use lantern_store::{Store, StoredMessage};
use lantern_transport::{peer_identity, Transport};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

mod rate;
pub mod update;

use rate::RateMeter;

pub use lantern_crypto::{safety_words, short_hex};
/// Used throughout this crate, and re-exported because shells handle message
/// and transfer ids constantly — no reason for each to depend on `uuid` just
/// to name the type.
pub use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum CoreEvent {
    PeerSeen {
        id: [u8; 32],
        name: String,
        host: String,
        addr: SocketAddr,
        new: bool,
    },
    SessionEstablished {
        id: [u8; 32],
    },
    MessageReceived {
        peer: [u8; 32],
        peer_name: String,
        mid: Uuid,
        text: String,
        ts: u64,
        reply_to: Option<Uuid>,
    },
    MessageDelivered {
        mid: Uuid,
    },
    TrustWarning {
        id: [u8; 32],
        detail: String,
    },
    FileOffered {
        peer: [u8; 32],
        peer_name: String,
        xid: Uuid,
        name: String,
        size: u64,
    },
    FileReceived {
        peer: [u8; 32],
        xid: Uuid,
        name: String,
        path: PathBuf,
        size: u64,
    },
    /// The remote side confirmed (or refused) a file we sent.
    FileSent {
        xid: Uuid,
        ok: bool,
        err: Option<String>,
    },
    /// Sender-side stats after streaming: how many chunks the peer actually
    /// needed. `sent < total` is a resume in action.
    ChunksSent { xid: Uuid, sent: u32, total: u32 },
    /// Live progress in both directions. `bytes` of `total` of the file are in
    /// place — on a resume that starts partway up, because chunks already held
    /// count toward it.
    ///
    /// Clock-paced (about five a second), not once per chunk: a 150 MB/s LAN
    /// transfer moves 150 chunks a second, and no interface needs 150 frames
    /// to show one bar moving.
    ///
    /// The rate is measured here rather than left to each shell to divide for
    /// itself. Not because a shell couldn't, but because doing it well is the
    /// whole problem — a raw delta between two 1 MiB bursts swings by an order
    /// of magnitude, so `rate` smooths it and reports nothing at all until a
    /// measurement window has closed. Four shells dividing badly in four
    /// different ways is worse than one place doing it properly; a shell that
    /// wants its own figure still has `bytes` and its own clock.
    TransferProgress {
        xid: Uuid,
        /// True when we're the sender.
        outgoing: bool,
        bytes: u64,
        total: u64,
        /// Smoothed bytes per second — None until it's been measurable for
        /// long enough to state without lying (see `rate`).
        bps: Option<u64>,
        /// Seconds left at the current rate, when that can be said at all.
        eta_s: Option<u64>,
    },
}

pub const CHUNK_SIZE: u32 = 1024 * 1024;

/// A file offer's chunk manifest, held while its chunks arrive.
#[derive(Clone)]
struct Manifest {
    name: String,
    size: u64,
    chunk_size: u32,
    root: [u8; 32],
    chunk_hashes: Vec<[u8; 32]>,
}

impl Manifest {
    fn chunk_count(&self) -> u32 {
        self.chunk_hashes.len() as u32
    }
    fn chunk_len(&self, idx: u32) -> u32 {
        let start = idx as u64 * self.chunk_size as u64;
        (self.size - start).min(self.chunk_size as u64) as u32
    }
}

/// Durable receive state, keyed by content root hash so an interrupted
/// transfer resumes across process restarts — and across re-offers with a
/// fresh xid. Data lands in `<partial_dir>/<root>.data` at chunk offsets;
/// the sidecar records which chunks have been verified.
struct PartialState {
    data_path: PathBuf,
    sidecar_path: PathBuf,
    verified: std::collections::HashSet<u32>,
}

impl PartialState {
    fn open(partial_dir: &std::path::Path, root: &[u8; 32]) -> anyhow::Result<Self> {
        std::fs::create_dir_all(partial_dir)?;
        let stem = hex::encode(root);
        let data_path = partial_dir.join(format!("{stem}.data"));
        let sidecar_path = partial_dir.join(format!("{stem}.json"));
        let verified = match std::fs::read_to_string(&sidecar_path) {
            Ok(s) => s
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect(),
            Err(_) => Default::default(),
        };
        Ok(Self {
            data_path,
            sidecar_path,
            verified,
        })
    }

    fn need(&self, total: u32) -> Vec<u32> {
        (0..total).filter(|i| !self.verified.contains(i)).collect()
    }

    /// Write one verified chunk at its offset and durably record it.
    fn store_chunk(&mut self, idx: u32, chunk_size: u32, data: &[u8]) -> anyhow::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        // truncate(false) is load-bearing: this file accumulates chunks
        // across process lifetimes — truncating on open would destroy the
        // very state that makes resume work.
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.data_path)?;
        f.seek(SeekFrom::Start(idx as u64 * chunk_size as u64))?;
        f.write_all(data)?;
        f.sync_data()?;
        self.verified.insert(idx);
        // Append-only sidecar: one index per line, synced. Crash-safe in the
        // simplest possible way; a torn last line just re-fetches one chunk.
        let mut sc = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.sidecar_path)?;
        writeln!(sc, "{idx}")?;
        sc.sync_data()?;
        Ok(())
    }

    /// All chunks verified: move into downloads under a collision-safe name.
    fn finalize(self, downloads: &std::path::Path, name: &str) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(downloads)?;
        let safe = sanitize_filename(name)?;
        let mut target = downloads.join(&safe);
        let mut n = 1u32;
        while target.exists() {
            n += 1;
            let (stem, ext) = match safe.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                _ => (safe.clone(), String::new()),
            };
            target = downloads.join(format!("{stem} ({n}){ext}"));
        }
        // A rename cannot cross a filesystem. The download directory is no
        // longer guaranteed to sit beside the partial file — it follows the
        // user's XDG setting, which may be another mount entirely — so fall
        // back to copy-and-delete rather than failing a transfer that has
        // already downloaded and verified every chunk.
        if std::fs::rename(&self.data_path, &target).is_err() {
            std::fs::copy(&self.data_path, &target)?;
            std::fs::remove_file(&self.data_path)?;
        }
        std::fs::remove_file(&self.sidecar_path).ok();
        Ok(target)
    }
}

#[derive(Clone)]
pub struct CoreConfig {
    pub data_dir: PathBuf,
    pub display_name: String,
    pub discovery_port: u16,
    /// Ports beacons are sent to (loopback fan-out for same-host testing).
    pub beacon_targets: Vec<u16>,
    pub broadcast: bool,
    /// 0 = ephemeral.
    pub quic_port: u16,
    pub in_memory_store: bool,
    /// Where received files are placed. `None` keeps them inside the data
    /// directory (`<data_dir>/downloads`), which is what tests and embedders
    /// want — a test must never write into the real user's Downloads folder.
    /// Shells pass `user_download_dir()` so files land somewhere a person can
    /// actually find them.
    pub download_dir: Option<PathBuf>,
}

struct PeerInfo {
    name: String,
    host: String,
    /// ip from the beacon's source, port from the beacon's `port` field.
    quic_addr: SocketAddr,
    /// When the most recent signed beacon from this identity arrived.
    /// Drives presence: an entry is kept forever (so history and trust
    /// survive), but goes offline once the beacons stop.
    last_beacon: Instant,
}

/// A roster entry as the shells see it. Identity-keyed, so a peer that
/// changes IP is still the same peer (DESIGN §4.1).
pub struct PeerView {
    pub id: [u8; 32],
    pub name: String,
    pub host: String,
    pub quic_addr: SocketAddr,
    /// Age of the last beacon. `None` when the peer was learned some other
    /// way than a beacon (an inbound connection, say).
    pub since_beacon: Option<Duration>,
    /// False once three heartbeats have passed with no beacon (DESIGN §4.2).
    /// Sending to an offline peer will time out, so shells should say so
    /// up front rather than let the user watch a spinner.
    pub online: bool,
}

#[derive(Clone)]
struct SessionHandle {
    outbox: mpsc::Sender<ControlFrame>,
    conn: quinn::Connection,
}

struct Inner {
    roster: HashMap<[u8; 32], PeerInfo>,
    sessions: HashMap<[u8; 32], SessionHandle>,
    /// Offers announced on the control stream, awaiting their chunks.
    pending_offers: HashMap<Uuid, Manifest>,
    /// Senders parked on a oneshot until the peer's AcceptFile arrives.
    accept_waiters: HashMap<Uuid, tokio::sync::oneshot::Sender<Result<Vec<u32>, String>>>,
    /// Identities we've already raised a TOFU conflict for (dedup).
    warned: std::collections::HashSet<[u8; 32]>,
    /// When we last answered a Ping. Rate limit — see the discovery loop.
    last_ping_reply: Option<std::time::Instant>,
}

pub struct Core {
    identity: Arc<Identity>,
    store: Arc<std::sync::Mutex<Store>>,
    transport: Arc<Transport>,
    discovery: Arc<Discovery>,
    inner: Arc<Mutex<Inner>>,
    events: mpsc::Sender<CoreEvent>,
    seq: std::sync::atomic::AtomicU64,
    boot: [u8; 8],
    config: CoreConfig,
}

impl Core {
    pub async fn start(config: CoreConfig) -> anyhow::Result<(Arc<Self>, mpsc::Receiver<CoreEvent>)> {
        let identity = Arc::new(Identity::load_or_generate(
            &config.data_dir.join("identity.key"),
        )?);
        let store = Arc::new(std::sync::Mutex::new(if config.in_memory_store {
            Store::open_in_memory()?
        } else {
            Store::open(&config.data_dir.join("lantern.db"))?
        }));

        let transport = Arc::new(Transport::bind(&identity, config.quic_port)?);
        let discovery = Arc::new(
            Discovery::bind(DiscoveryConfig {
                bind_port: config.discovery_port,
                target_ports: config.beacon_targets.clone(),
                broadcast: config.broadcast,
            })
            .await?,
        );

        let (event_tx, event_rx) = mpsc::channel(256);
        let mut boot = [0u8; 8];
        // Random per-process nonce; equality-compared only (§2.2).
        getrandom(&mut boot);

        let core = Arc::new(Core {
            identity,
            store,
            transport,
            discovery,
            inner: Arc::new(Mutex::new(Inner {
                roster: HashMap::new(),
                sessions: HashMap::new(),
                pending_offers: HashMap::new(),
                accept_waiters: HashMap::new(),
                warned: std::collections::HashSet::new(),
                last_ping_reply: None,
            })),
            events: event_tx,
            seq: std::sync::atomic::AtomicU64::new(1),
            boot,
            config,
        });

        core.clone().spawn_discovery_loop();
        core.clone().spawn_accept_loop();
        core.clone().spawn_heartbeat();

        Ok((core, event_rx))
    }

    pub fn identity_id(&self) -> [u8; 32] {
        self.identity.public_bytes()
    }

    pub fn fingerprint_words(&self) -> Vec<&'static str> {
        lantern_crypto::safety_words(&self.identity.fingerprint())
    }

    pub fn quic_port(&self) -> u16 {
        self.transport.local_port()
    }

    /// Where a received file ends up. Shells display this, so it must be the
    /// same path `finalize` actually writes to — never a guess.
    pub fn download_dir(&self) -> PathBuf {
        self.config
            .download_dir
            .clone()
            .unwrap_or_else(|| self.config.data_dir.join("downloads"))
    }

    /// The roster, with presence. Supersedes the tuple this used to return:
    /// every shell needs to know whether a peer is reachable, and a tuple had
    /// nowhere to put that.
    pub async fn peers(&self) -> Vec<PeerView> {
        let inner = self.inner.lock().await;
        let now = Instant::now();
        inner
            .roster
            .iter()
            .map(|(id, p)| {
                let age = now.saturating_duration_since(p.last_beacon);
                PeerView {
                    id: *id,
                    name: p.name.clone(),
                    host: p.host.clone(),
                    quic_addr: p.quic_addr,
                    since_beacon: Some(age),
                    online: age < OFFLINE_AFTER,
                }
            })
            .collect()
    }

    pub fn history(&self, peer: &[u8; 32], limit: usize) -> Vec<StoredMessage> {
        self.store
            .lock()
            .unwrap()
            .history(peer, limit)
            .unwrap_or_default()
    }

    /// Delete one message from this machine's history. Returns whether a
    /// message with that id was there to delete.
    ///
    /// Local only: nothing goes on the wire, and the other machine keeps its
    /// copy. Shells must say that plainly rather than implying both sides.
    pub fn delete_message(&self, mid: &Uuid) -> bool {
        self.store
            .lock()
            .unwrap()
            .delete_message(mid)
            .unwrap_or(false)
    }

    /// Delete the whole conversation with `peer` from this machine's history,
    /// returning how many messages went. Purely local: nothing is sent, and
    /// the peer keeps their copy — any UI offering this must say so rather
    /// than implying a recall. Trust and the peer's name survive; see
    /// `Store::clear_history`.
    pub fn clear_history(&self, peer: &[u8; 32]) -> usize {
        self.store
            .lock()
            .unwrap()
            .clear_history(peer)
            .unwrap_or(0)
    }

    /// Where identity, history and update bookkeeping live.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.config.data_dir
    }

    /// Is there a newer Lantern on GitHub, and can this copy take it?
    pub async fn check_update(&self) -> update::UpdateCheck {
        update::check().await
    }

    /// Start the detached updater. An `Err` here is a reason to show someone,
    /// not an internal failure — see `update::start`.
    pub fn start_update(&self) -> Result<(), String> {
        update::start(&self.config.data_dir)
    }

    /// What the last update attempt did, if there was one. Read on launch:
    /// the app quits partway through its own update, so this is how it finds
    /// out how the update it started went.
    pub fn last_update_state(&self) -> Option<update::UpdateState> {
        update::last_state(&self.config.data_dir)
    }

    /// Safety words for any identity — what you read aloud to compare.
    pub fn words_for(id: &[u8; 32]) -> Vec<&'static str> {
        lantern_crypto::safety_words(&lantern_crypto::fingerprint_of(id))
    }

    /// Mark a peer verified after an out-of-band safety-word comparison.
    pub fn set_verified(&self, id: &[u8; 32], verified: bool) {
        let _ = self.store.lock().unwrap().set_verified(id, verified);
    }

    pub fn is_verified(&self, id: &[u8; 32]) -> bool {
        self.store.lock().unwrap().is_verified(id).unwrap_or(false)
    }

    fn make_beacon(&self, beacon_type: BeaconType) -> Beacon {
        Beacon {
            beacon_type,
            flags: 0,
            id: self.identity.public_bytes(),
            name: self.config.display_name.clone(),
            host: hostname(),
            group: String::new(),
            device: DeviceClass::Desktop,
            port: self.transport.local_port(),
            state: PresenceState::Active,
            status: String::new(),
            avatar: None,
            caps: 0b0000_0011, // text + file-v1 (files pending Phase 3)
            seq: self
                .seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            boot: self.boot,
            ts: now_ms(),
        }
    }

    pub async fn announce(&self) {
        let b = self.make_beacon(BeaconType::Hello);
        self.discovery.send_beacon(&b, self.identity.signing_key()).await;
    }

    /// Ask everyone on the link to announce themselves now, rather than
    /// waiting out the heartbeat.
    ///
    /// A plain `announce()` is not enough for this. Peers only answer a
    /// beacon from someone *new* to them, so re-announcing to a peer that
    /// already has us in its roster produces no reply and the roster stays
    /// stale for up to a heartbeat. `Ping` is the "everyone speak up" that
    /// `BeaconType` has always had a slot for (DESIGN §4.2) and nothing ever
    /// sent.
    pub async fn refresh(&self) {
        let b = self.make_beacon(BeaconType::Ping);
        self.discovery.send_beacon(&b, self.identity.signing_key()).await;
    }

    /// Send a text message; establishes the session on demand.
    pub async fn send_message(&self, peer: [u8; 32], text: &str) -> anyhow::Result<Uuid> {
        self.send_reply(peer, text, None).await
    }

    /// Send `text`, optionally as an answer to `reply_to`. The reference is
    /// carried on the wire (`ControlFrame::Msg.reply_to`) and stored on both
    /// ends, so a reply survives restarts and reads the same in history as
    /// it did live.
    pub async fn send_reply(
        &self,
        peer: [u8; 32],
        text: &str,
        reply_to: Option<Uuid>,
    ) -> anyhow::Result<Uuid> {
        let mid = Uuid::new_v4();
        let ts = now_ms();
        let frame = ControlFrame::Msg {
            mid,
            ts,
            text: text.to_string(),
            fmt: "plain".into(),
            reply_to,
            sealed: false,
            receipt: false,
        };

        self.store.lock().unwrap().insert_message(&StoredMessage {
            mid,
            peer_id: peer,
            outgoing: true,
            ts,
            text: text.to_string(),
            state: 0,
            reply_to,
        })?;

        let session = self.ensure_session(peer).await?;
        session
            .outbox
            .send(frame)
            .await
            .map_err(|_| anyhow::anyhow!("session closed"))?;
        Ok(mid)
    }

    /// Send a file, chunked and resumable (§2.5, single-file form).
    ///
    /// Flow: build the chunk manifest → OfferFile → wait for the peer's
    /// AcceptFile need-list → stream only those chunks on one uni stream.
    /// Re-sending the same file after an interruption ships only the gaps:
    /// the receiver's partial state is keyed by content root, not by xid.
    pub async fn send_file(&self, peer: [u8; 32], path: &std::path::Path) -> anyhow::Result<Uuid> {
        let meta = tokio::fs::metadata(path).await?;
        if !meta.is_file() {
            anyhow::bail!("not a file: {}", path.display());
        }
        let size = meta.len();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("unusable file name"))?
            .to_string();

        // One sequential pass: per-chunk hashes + whole-file root together.
        let (chunk_hashes, root) = {
            let path = path.to_path_buf();
            tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<[u8; 32]>, [u8; 32])> {
                use std::io::Read;
                let mut f = std::fs::File::open(&path)?;
                let mut whole = blake3::Hasher::new();
                let mut hashes = Vec::new();
                let mut buf = vec![0u8; CHUNK_SIZE as usize];
                loop {
                    let mut filled = 0;
                    while filled < buf.len() {
                        let n = f.read(&mut buf[filled..])?;
                        if n == 0 {
                            break;
                        }
                        filled += n;
                    }
                    if filled == 0 {
                        break;
                    }
                    whole.update(&buf[..filled]);
                    hashes.push(*blake3::hash(&buf[..filled]).as_bytes());
                    if filled < buf.len() {
                        break;
                    }
                }
                Ok((hashes, *whole.finalize().as_bytes()))
            })
            .await??
        };
        if chunk_hashes.is_empty() {
            anyhow::bail!("refusing to send an empty file");
        }
        let total = chunk_hashes.len() as u32;

        let xid = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner.lock().await.accept_waiters.insert(xid, tx);

        let session = self.ensure_session(peer).await?;
        session
            .outbox
            .send(ControlFrame::OfferFile {
                xid,
                name,
                size,
                chunk_size: CHUNK_SIZE,
                root: root.to_vec(),
                chunks: chunk_hashes.concat(),
            })
            .await
            .map_err(|_| anyhow::anyhow!("session closed"))?;

        // The offer is away, so the xid is real and the caller can have it
        // now. Everything after this — the peer's need-list, the streaming,
        // the progress — belongs to the event stream, because a shell that
        // only learns the xid once the bytes have all moved can't show a
        // transfer in flight at all. Failures past this point arrive as
        // `FileSent { ok: false }` rather than as a return value.
        let events = self.events.clone();
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        tokio::spawn(async move {
            let outcome =
                Self::stream_offered_file(&events, &inner, session, xid, &path, size, total, rx)
                    .await;
            if let Err(e) = outcome {
                let _ = events
                    .send(CoreEvent::FileSent {
                        xid,
                        ok: false,
                        err: Some(e.to_string()),
                    })
                    .await;
            }
        });
        Ok(xid)
    }

    /// The back half of `send_file`: wait for the need-list, then stream the
    /// chunks the peer actually asked for, reporting speed as they go.
    #[allow(clippy::too_many_arguments)]
    async fn stream_offered_file(
        events: &mpsc::Sender<CoreEvent>,
        inner: &Arc<Mutex<Inner>>,
        session: SessionHandle,
        xid: Uuid,
        path: &std::path::Path,
        size: u64,
        total: u32,
        rx: tokio::sync::oneshot::Receiver<Result<Vec<u32>, String>>,
    ) -> anyhow::Result<()> {
        // Wait for the need-list (or decline).
        let need = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(need))) => need,
            Ok(Ok(Err(reason))) => anyhow::bail!("peer declined: {reason}"),
            Ok(Err(_)) => anyhow::bail!("session dropped before accept"),
            Err(_) => {
                inner.lock().await.accept_waiters.remove(&xid);
                anyhow::bail!("peer did not answer the offer");
            }
        };

        // Said before the bytes move, not after: "only 3 of 180 chunks
        // needed" is context for the progress that follows it.
        let sent = need.len() as u32;
        let _ = events
            .send(CoreEvent::ChunksSent { xid, sent, total })
            .await;

        if !need.is_empty() {
            let mut uni = session.conn.open_uni().await?;
            let mut file = tokio::fs::File::open(path).await?;
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut buf = vec![0u8; CHUNK_SIZE as usize];
            // Chunks the peer already had count as done, so a resumed
            // transfer's progress starts where it left off instead of at zero.
            let already = (total as u64).saturating_sub(need.len() as u64)
                * CHUNK_SIZE as u64;
            let mut moved = 0u64;
            // Fed `moved`, not `already + moved`: the head start didn't happen
            // now, and counting it as this second's traffic would open with a
            // fantasy speed.
            let mut meter = RateMeter::new(Instant::now());
            for idx in &need {
                if *idx >= total {
                    continue; // hostile need-list; skip out-of-range
                }
                let start = *idx as u64 * CHUNK_SIZE as u64;
                let len = (size - start).min(CHUNK_SIZE as u64) as usize;
                file.seek(std::io::SeekFrom::Start(start)).await?;
                file.read_exact(&mut buf[..len]).await?;
                uni.write_all(xid.as_bytes()).await?;
                uni.write_all(&idx.to_be_bytes()).await?;
                uni.write_all(&(len as u32).to_be_bytes()).await?;
                uni.write_all(&buf[..len]).await?;
                moved += len as u64;
                // The meter decides when there's something worth saying, so
                // a 150 MB/s transfer emits ~5 events a second, not 150.
                if meter.sample(moved, Instant::now()) {
                    let bytes = (already + moved).min(size);
                    let _ = events
                        .send(CoreEvent::TransferProgress {
                            xid,
                            outgoing: true,
                            bytes,
                            total: size,
                            bps: meter.bps(),
                            eta_s: meter.eta_s(size.saturating_sub(bytes)),
                        })
                        .await;
                }
            }
            uni.finish()?;
            // Always land on 100%: without this a transfer that finishes
            // mid-window stops at "38 of 40 MB" and looks stuck.
            let _ = events
                .send(CoreEvent::TransferProgress {
                    xid,
                    outgoing: true,
                    bytes: (already + moved).min(size),
                    total: size,
                    bps: meter.bps(),
                    eta_s: Some(0),
                })
                .await;
        }
        Ok(())
    }

    async fn ensure_session(&self, peer: [u8; 32]) -> anyhow::Result<SessionHandle> {
        {
            let inner = self.inner.lock().await;
            if let Some(s) = inner.sessions.get(&peer) {
                return Ok(s.clone());
            }
        }
        let addr = {
            let inner = self.inner.lock().await;
            inner
                .roster
                .get(&peer)
                .map(|p| p.quic_addr)
                .ok_or_else(|| anyhow::anyhow!("peer not in roster"))?
        };

        let conn = self.transport.connect(addr, peer).await?;
        let (mut send, recv) = conn.open_bi().await?;

        write_frame(
            &mut send,
            &ControlFrame::Hello {
                proto: 1,
                id: self.identity.public_bytes().to_vec(),
                name: self.config.display_name.clone(),
                host: hostname(),
                caps: 0b11,
            },
        )
        .await?;

        let handle = self.register_session(peer, conn, send, recv).await;
        let _ = self.events.send(CoreEvent::SessionEstablished { id: peer }).await;
        Ok(handle)
    }

    /// Wire a session into the core: writer task, reader task, and a
    /// uni-stream acceptor for incoming files. Returns the handle.
    async fn register_session(
        self: &Core,
        peer: [u8; 32],
        conn: quinn::Connection,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
    ) -> SessionHandle {
        let (outbox_tx, mut outbox_rx) = mpsc::channel::<ControlFrame>(64);

        // Writer task.
        tokio::spawn(async move {
            while let Some(frame) = outbox_rx.recv().await {
                if let Err(e) = write_frame(&mut send, &frame).await {
                    warn!("write to peer failed: {e}");
                    break;
                }
            }
        });

        // Reader task.
        {
            let store = Arc::clone(&self.store);
            let events = self.events.clone();
            let inner = Arc::clone(&self.inner);
            let outbox_for_acks = outbox_tx.clone();
            let downloads_dir = self.download_dir();
            let partial_dir = self.config.data_dir.join("partial");
            tokio::spawn(async move {
                loop {
                    let frame = match read_frame(&mut recv).await {
                        Ok(f) => f,
                        Err(e) => {
                            debug!("session read ended: {e}");
                            break;
                        }
                    };
                    match frame {
                        ControlFrame::Msg { mid, ts, text, reply_to, .. } => {
                            let peer_name = {
                                let g = inner.lock().await;
                                g.roster
                                    .get(&peer)
                                    .map(|p| p.name.clone())
                                    .unwrap_or_else(|| hex::encode(&peer[..4]))
                            };
                            let _ = store.lock().unwrap().insert_message(&StoredMessage {
                                mid,
                                peer_id: peer,
                                outgoing: false,
                                ts,
                                text: text.clone(),
                                state: 1,
                                reply_to,
                            });
                            let _ = outbox_for_acks
                                .send(ControlFrame::Ack {
                                    mid,
                                    kind: "delivered".into(),
                                    ts: now_ms(),
                                })
                                .await;
                            let _ = events
                                .send(CoreEvent::MessageReceived {
                                    peer,
                                    peer_name,
                                    mid,
                                    text,
                                    ts,
                                    reply_to,
                                })
                                .await;
                        }
                        ControlFrame::Ack { mid, .. } => {
                            let _ = store.lock().unwrap().set_message_state(&mid, 1);
                            let _ = events.send(CoreEvent::MessageDelivered { mid }).await;
                        }
                        ControlFrame::OfferFile { xid, name, size, chunk_size, root, chunks } => {
                            // Validate the manifest's internal consistency
                            // before trusting any of it.
                            let Ok(root32) = <[u8; 32]>::try_from(root.as_slice()) else {
                                warn!("offer with malformed root; ignoring");
                                continue;
                            };
                            if chunk_size == 0
                                || chunks.is_empty()
                                || chunks.len() % 32 != 0
                                || size == 0
                                || size.div_ceil(chunk_size as u64)
                                    != (chunks.len() / 32) as u64
                            {
                                warn!("offer with inconsistent manifest; declining");
                                let _ = outbox_for_acks
                                    .send(ControlFrame::DeclineFile {
                                        xid,
                                        reason: Some("inconsistent manifest".into()),
                                    })
                                    .await;
                                continue;
                            }
                            let manifest = Manifest {
                                name: name.clone(),
                                size,
                                chunk_size,
                                root: root32,
                                chunk_hashes: chunks.as_chunks::<32>().0.to_vec(),
                            };
                            let total = manifest.chunk_count();

                            // Resume: what do we already hold for this content?
                            let need = match PartialState::open(&partial_dir, &root32) {
                                Ok(ps) => ps.need(total),
                                Err(e) => {
                                    warn!("partial state unavailable: {e}");
                                    (0..total).collect()
                                }
                            };

                            let peer_name = {
                                let mut g = inner.lock().await;
                                g.pending_offers.insert(xid, manifest.clone());
                                g.roster
                                    .get(&peer)
                                    .map(|p| p.name.clone())
                                    .unwrap_or_else(|| hex::encode(&peer[..4]))
                            };
                            let _ = events
                                .send(CoreEvent::FileOffered {
                                    peer,
                                    peer_name,
                                    xid,
                                    name,
                                    size,
                                })
                                .await;

                            if need.is_empty() {
                                // Everything already on disk: finalize now.
                                inner.lock().await.pending_offers.remove(&xid);
                                let done = PartialState::open(&partial_dir, &root32)
                                    .and_then(|ps| {
                                        ps.finalize(&downloads_dir, &manifest.name)
                                    });
                                match done {
                                    Ok(path) => {
                                        let _ = outbox_for_acks
                                            .send(ControlFrame::AcceptFile {
                                                xid,
                                                need: vec![],
                                            })
                                            .await;
                                        let _ = outbox_for_acks
                                            .send(ControlFrame::XferDone {
                                                xid,
                                                ok: true,
                                                err: None,
                                            })
                                            .await;
                                        let _ = events
                                            .send(CoreEvent::FileReceived {
                                                peer,
                                                xid,
                                                name: manifest.name.clone(),
                                                path,
                                                size,
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        // Partial claimed complete but can't
                                        // finalize — re-request everything.
                                        warn!("finalize from partial failed: {e}");
                                        let _ = outbox_for_acks
                                            .send(ControlFrame::AcceptFile {
                                                xid,
                                                need: (0..total).collect(),
                                            })
                                            .await;
                                    }
                                }
                            } else {
                                let _ = outbox_for_acks
                                    .send(ControlFrame::AcceptFile { xid, need })
                                    .await;
                            }
                        }
                        ControlFrame::AcceptFile { xid, need } => {
                            if let Some(tx) =
                                inner.lock().await.accept_waiters.remove(&xid)
                            {
                                let _ = tx.send(Ok(need));
                            }
                        }
                        ControlFrame::DeclineFile { xid, reason } => {
                            if let Some(tx) =
                                inner.lock().await.accept_waiters.remove(&xid)
                            {
                                let _ = tx.send(Err(
                                    reason.unwrap_or_else(|| "declined".into())
                                ));
                            }
                        }
                        ControlFrame::XferDone { xid, ok, err } => {
                            let _ = events.send(CoreEvent::FileSent { xid, ok, err }).await;
                        }
                        ControlFrame::Hello { .. } | ControlFrame::HelloAck { .. } => {
                            // Already handshaken on this stream; ignore.
                        }
                        ControlFrame::Error { code, msg } => {
                            warn!("peer error {code}: {msg}");
                        }
                    }
                }
                // Session ended: drop it from the map so the next send redials.
                inner.lock().await.sessions.remove(&peer);
            });
        }

        self.spawn_uni_acceptor(peer, conn.clone(), outbox_tx.clone());

        let handle = SessionHandle {
            outbox: outbox_tx,
            conn,
        };
        self.inner
            .lock()
            .await
            .sessions
            .insert(peer, handle.clone());
        handle
    }

    /// Accept incoming unidirectional streams: each carries a sequence of
    /// chunk records `xid(16) · idx(4 BE) · len(4 BE) · bytes` for offers
    /// this session accepted. Every chunk is verified against its manifest
    /// hash before it touches the partial file; a bad chunk aborts the
    /// stream but keeps every verified chunk on disk for resume.
    fn spawn_uni_acceptor(
        &self,
        peer: [u8; 32],
        conn: quinn::Connection,
        outbox: mpsc::Sender<ControlFrame>,
    ) {
        let inner = Arc::clone(&self.inner);
        let events = self.events.clone();
        let downloads = self.download_dir();
        let partial_dir = self.config.data_dir.join("partial");
        tokio::spawn(async move {
            loop {
                let mut stream = match conn.accept_uni().await {
                    Ok(s) => s,
                    Err(_) => break, // connection closed
                };
                let inner = Arc::clone(&inner);
                let events = events.clone();
                let outbox = outbox.clone();
                let downloads = downloads.clone();
                let partial_dir = partial_dir.clone();
                tokio::spawn(async move {
                    let mut header = [0u8; 24];
                    let mut buf = vec![0u8; CHUNK_SIZE as usize];
                    // One stream carries one transfer's chunks, so a single
                    // meter per stream measures exactly this transfer.
                    let mut meter = RateMeter::new(Instant::now());
                    let mut stream_bytes = 0u64;
                    loop {
                        // Read one record header (clean EOF between records
                        // ends the stream).
                        match stream.read_exact(&mut header).await {
                            Ok(()) => {}
                            Err(_) => break,
                        }
                        let xid = Uuid::from_bytes(header[..16].try_into().unwrap());
                        let idx =
                            u32::from_be_bytes(header[16..20].try_into().unwrap());
                        let len =
                            u32::from_be_bytes(header[20..24].try_into().unwrap());

                        // Look up the manifest (grace-wait: streams have no
                        // ordering vs the control frame that announced them).
                        let mut manifest = None;
                        for _ in 0..40 {
                            if let Some(m) =
                                inner.lock().await.pending_offers.get(&xid).cloned()
                            {
                                manifest = Some(m);
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        let Some(m) = manifest else {
                            warn!("chunk for unknown offer {xid}; aborting stream");
                            break;
                        };
                        if idx >= m.chunk_count() || len != m.chunk_len(idx) {
                            warn!("chunk record out of bounds; aborting stream");
                            break;
                        }
                        if stream.read_exact(&mut buf[..len as usize]).await.is_err() {
                            debug!("stream ended mid-chunk (interrupted transfer)");
                            break;
                        }

                        // Verify before write. A bad chunk is refused alone;
                        // everything verified so far stays for resume.
                        if *blake3::hash(&buf[..len as usize]).as_bytes()
                            != m.chunk_hashes[idx as usize]
                        {
                            warn!("chunk {idx} failed BLAKE3; aborting stream");
                            let _ = outbox
                                .send(ControlFrame::XferDone {
                                    xid,
                                    ok: false,
                                    err: Some(format!("chunk {idx} hash mismatch")),
                                })
                                .await;
                            break;
                        }

                        let store_result = {
                            let partial_dir = partial_dir.clone();
                            let data = buf[..len as usize].to_vec();
                            let root = m.root;
                            let chunk_size = m.chunk_size;
                            tokio::task::spawn_blocking(move || {
                                let mut ps = PartialState::open(&partial_dir, &root)?;
                                ps.store_chunk(idx, chunk_size, &data)?;
                                Ok::<usize, anyhow::Error>(ps.verified.len())
                            })
                            .await
                        };
                        let verified_count = match store_result {
                            Ok(Ok(n)) => n,
                            other => {
                                warn!("chunk store failed: {other:?}");
                                break;
                            }
                        };

                        // Verified bytes, not received bytes: a chunk that
                        // failed its hash never lands, so progress can only
                        // move on data that is actually good. Measured against
                        // the whole file, so a resumed transfer reads "60 of
                        // 100 MB" rather than restarting the bar at nothing.
                        stream_bytes += len as u64;
                        let have = (verified_count as u64 * m.chunk_size as u64).min(m.size);
                        let complete = verified_count as u32 == m.chunk_count();
                        // The rate comes from what this stream moved; `have`
                        // includes chunks from an earlier attempt.
                        if meter.sample(stream_bytes, Instant::now()) || complete {
                            let _ = events
                                .send(CoreEvent::TransferProgress {
                                    xid,
                                    outgoing: false,
                                    bytes: have,
                                    total: m.size,
                                    bps: meter.bps(),
                                    eta_s: if complete {
                                        Some(0)
                                    } else {
                                        meter.eta_s(m.size.saturating_sub(have))
                                    },
                                })
                                .await;
                        }

                        if complete {
                            // Complete: finalize off the async path.
                            inner.lock().await.pending_offers.remove(&xid);
                            let fin = {
                                let partial_dir = partial_dir.clone();
                                let downloads = downloads.clone();
                                let root = m.root;
                                let name = m.name.clone();
                                tokio::task::spawn_blocking(move || {
                                    PartialState::open(&partial_dir, &root)?
                                        .finalize(&downloads, &name)
                                })
                                .await
                            };
                            match fin {
                                Ok(Ok(path)) => {
                                    let _ = outbox
                                        .send(ControlFrame::XferDone {
                                            xid,
                                            ok: true,
                                            err: None,
                                        })
                                        .await;
                                    let _ = events
                                        .send(CoreEvent::FileReceived {
                                            peer,
                                            xid,
                                            name: m.name.clone(),
                                            path,
                                            size: m.size,
                                        })
                                        .await;
                                }
                                other => {
                                    warn!("finalize failed: {other:?}");
                                    let _ = outbox
                                        .send(ControlFrame::XferDone {
                                            xid,
                                            ok: false,
                                            err: Some("finalize failed".into()),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                });
            }
        });
    }

    fn spawn_discovery_loop(self: Arc<Self>) {
        let mut rx = self.discovery.spawn_receiver(self.identity.public_bytes());
        tokio::spawn(async move {
            while let Some(Discovered { beacon, from }) = rx.recv().await {
                let quic_addr = SocketAddr::new(from.ip(), beacon.port);
                let is_new = {
                    let mut inner = self.inner.lock().await;
                    let is_new = !inner.roster.contains_key(&beacon.id);
                    inner.roster.insert(
                        beacon.id,
                        PeerInfo {
                            name: beacon.name.clone(),
                            host: beacon.host.clone(),
                            quic_addr,
                            last_beacon: Instant::now(),
                        },
                    );
                    is_new
                };
                // TOFU: has this name+host been seen under a *different* key?
                // Check BEFORE recording the new sighting, warn once per id.
                let conflicts = self
                    .store
                    .lock()
                    .unwrap()
                    .conflicting_identities(&beacon.name, &beacon.host, &beacon.id)
                    .unwrap_or_default();
                if !conflicts.is_empty() {
                    let first_warning = self.inner.lock().await.warned.insert(beacon.id);
                    if first_warning {
                        let words =
                            lantern_crypto::safety_words(&lantern_crypto::fingerprint_of(&beacon.id));
                        let _ = self
                            .events
                            .send(CoreEvent::TrustWarning {
                                id: beacon.id,
                                detail: format!(
                                    "'{}' on {} is presenting a NEW key ({}). Previously known \
                                     under a different identity — verify in person before \
                                     trusting: {}",
                                    beacon.name,
                                    beacon.host,
                                    lantern_crypto::short_hex(&lantern_crypto::fingerprint_of(
                                        &beacon.id
                                    )),
                                    words.join(" · "),
                                ),
                            })
                            .await;
                    }
                }
                let _ = self
                    .store
                    .lock()
                    .unwrap()
                    .record_peer(&beacon.id, &beacon.name, &beacon.host, now_ms());
                // Answer a Ping so whoever asked learns us immediately, and
                // answer a first sighting so the new arrival does too.
                //
                // Ping replies are rate limited, because one broadcast Ping
                // draws a reply from every node on the link. Without a limit
                // an attacker sends Pings in a loop and each one costs them a
                // packet and everyone else N — a cheap amplifier, and signing
                // does not help since anyone can mint an identity. One reply
                // per two seconds keeps a refresh button instant while
                // flattening a flood into a trickle.
                let answer_ping = beacon.beacon_type == BeaconType::Ping && {
                    let mut inner = self.inner.lock().await;
                    let now = std::time::Instant::now();
                    let due = inner
                        .last_ping_reply
                        .is_none_or(|t| now.duration_since(t).as_secs_f32() >= 2.0);
                    if due {
                        inner.last_ping_reply = Some(now);
                    }
                    due
                };
                if is_new {
                    info!("discovered {} ({})", beacon.name, from);
                }
                if is_new || answer_ping {
                    // Answer so the other side learns us quickly too.
                    self.announce().await;
                }
                let _ = self
                    .events
                    .send(CoreEvent::PeerSeen {
                        id: beacon.id,
                        name: beacon.name,
                        host: beacon.host,
                        addr: quic_addr,
                        new: is_new,
                    })
                    .await;
            }
        });
    }

    fn spawn_accept_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                let Some(conn) = self.transport.accept().await else {
                    break;
                };
                let core = Arc::clone(&self);
                tokio::spawn(async move {
                    let cert_id = match peer_identity(&conn) {
                        Ok(id) => id,
                        Err(e) => {
                            warn!("inbound connection without usable identity: {e}");
                            return;
                        }
                    };
                    let (mut send, mut recv) = match conn.accept_bi().await {
                        Ok(x) => x,
                        Err(e) => {
                            debug!("no control stream: {e}");
                            return;
                        }
                    };
                    // First frame must be Hello, and its id must match the
                    // certificate the peer proved during the handshake.
                    match read_frame(&mut recv).await {
                        Ok(ControlFrame::Hello { id, name, host, .. }) => {
                            if id.as_slice() != cert_id.as_slice() {
                                let _ = core
                                    .events
                                    .send(CoreEvent::TrustWarning {
                                        id: cert_id,
                                        detail: "Hello identity does not match certificate"
                                            .into(),
                                    })
                                    .await;
                                let _ = write_frame(
                                    &mut send,
                                    &ControlFrame::HelloAck {
                                        accepted: false,
                                        reason: Some("identity mismatch".into()),
                                    },
                                )
                                .await;
                                return;
                            }
                            let _ = write_frame(
                                &mut send,
                                &ControlFrame::HelloAck {
                                    accepted: true,
                                    reason: None,
                                },
                            )
                            .await;
                            // Learn/refresh the roster entry from the session.
                            {
                                let mut inner = core.inner.lock().await;
                                // A live inbound session is better proof of
                                // presence than a beacon, so it refreshes the
                                // clock — but not `quic_addr`: the remote
                                // address here is their ephemeral source
                                // port, not the port they listen on.
                                inner
                                    .roster
                                    .entry(cert_id)
                                    .and_modify(|p| p.last_beacon = Instant::now())
                                    .or_insert(PeerInfo {
                                        name: name.clone(),
                                        host: host.clone(),
                                        quic_addr: conn.remote_address(),
                                        last_beacon: Instant::now(),
                                    });
                            }
                            let _ = core
                                .store
                                .lock()
                                .unwrap()
                                .record_peer(&cert_id, &name, &host, now_ms());
                            core.register_session(cert_id, conn.clone(), send, recv).await;
                            let _ = core
                                .events
                                .send(CoreEvent::SessionEstablished { id: cert_id })
                                .await;
                        }
                        Ok(_) | Err(_) => {
                            debug!("inbound stream did not start with Hello");
                        }
                    }
                });
            }
        });
    }

    fn spawn_heartbeat(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                self.announce().await;
                tokio::time::sleep(lantern_discovery::HEARTBEAT).await;
            }
        });
    }
}

/// Final-component-only, no separators, no traversal, nothing hidden,
/// no Windows-reserved names. DESIGN.md §2.5 "Safety".
fn sanitize_filename(name: &str) -> anyhow::Result<String> {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches(['.', ' '])
        .to_string();
    if base.is_empty() || base == "." || base == ".." || base.starts_with('.') {
        anyhow::bail!("unacceptable file name: {name:?}");
    }
    if base.contains(':') {
        anyhow::bail!("unacceptable file name (ADS/colon): {name:?}");
    }
    let upper = base
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&upper.as_str()) {
        anyhow::bail!("unacceptable file name (reserved): {name:?}");
    }
    Ok(base)
}

/// The user's real Downloads folder, if this machine has one.
///
/// Received files used to land in `<data_dir>/downloads`, i.e. inside a
/// dotfile directory. On Linux that is hidden: the file manager does not show
/// it, and the person who just accepted a file has no idea where it went.
///
/// Linux goes through the XDG user-dirs setting rather than hardcoding
/// `~/Downloads`, because the folder is localised — a French desktop calls it
/// `Téléchargements`, and writing to a literal `Downloads` there would quietly
/// create a second, wrong folder beside the real one. macOS has no such
/// indirection.
///
/// `None` means "no sensible answer" (no `HOME`), and the caller should fall
/// back to the data directory rather than guessing.
pub fn user_download_dir() -> Option<PathBuf> {
    // Windows has no HOME; the profile directory is USERPROFILE, and the
    // folder is not localised on disk the way XDG's is.
    #[cfg(windows)]
    {
        let profile = std::env::var_os("USERPROFILE")?;
        return Some(PathBuf::from(profile).join("Downloads"));
    }
    #[cfg(not(windows))]
    {
        let home = PathBuf::from(std::env::var_os("HOME")?);

        if cfg!(target_os = "macos") {
            return Some(home.join("Downloads"));
        }

        // An explicit environment override beats the config file.
        if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR").map(PathBuf::from) {
            if dir.is_absolute() {
                return Some(dir);
            }
        }

        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        if let Some(dir) = xdg_download_dir_from(&config_home.join("user-dirs.dirs"), &home) {
            return Some(dir);
        }

        Some(home.join("Downloads"))
    }
}

/// Read `XDG_DOWNLOAD_DIR` out of a `user-dirs.dirs` file, whose lines look
/// like `XDG_DOWNLOAD_DIR="$HOME/Downloads"`.
///
/// Only the non-Windows path calls this — Windows names that folder itself,
/// so compiling it there is dead code and a warning. The parsing tests are
/// worth running wherever they compile, hence `test` rather than a blanket
/// `allow(dead_code)`.
#[cfg(any(not(windows), test))]
fn xdg_download_dir_from(path: &std::path::Path, home: &std::path::Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(value) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        // Paths under home are written "$HOME/Downloads"; anything else is
        // already absolute.
        let expanded = match value.strip_prefix("$HOME/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(value),
        };
        if expanded.is_absolute() {
            return Some(expanded);
        }
    }
    None
}

/// The machine's name, as every other peer will see it in their roster.
///
/// This used to try `$HOSTNAME` and then `/etc/hostname`. macOS has neither
/// — there is no `/etc/hostname`, and `HOSTNAME` is a *shell* variable that
/// a GUI app launched from Finder never inherits — so every Mac announced
/// itself as the literal string "unknown", to everybody, forever.
///
/// `gethostname(3)` is POSIX and answers on both platforms, so ask the
/// kernel instead of guessing at files and environment variables. The old
/// sources stay as fallbacks for anything exotic.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        // POSIX allows up to 255 bytes plus the terminator; 256 covers it,
        // and a truncated answer is still better than "unknown".
        let mut buf = vec![0u8; 256];
        // SAFETY: the pointer is valid for `buf.len()` bytes, which is what
        // we tell gethostname it may write. It may leave the buffer
        // unterminated on truncation, so the NUL search below is bounded by
        // the buffer rather than trusting one to be present.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end]) {
                let name = name.trim();
                // Macs on a Bonjour network report "Some-MacBook.local".
                // The short name is what a person recognises, and it matches
                // what Linux reports for the same machine.
                let short = name.split('.').next().unwrap_or(name);
                if !short.is_empty() {
                    return short.to_string();
                }
            }
        }
    }
    // Windows has no gethostname(3) outside winsock, but it does set
    // COMPUTERNAME in the environment for every process — unlike HOSTNAME,
    // which is a Unix *shell* variable and the reason this was broken on
    // macOS. Without this branch, Windows would ship the same "unknown".
    #[cfg(windows)]
    if let Some(name) = std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()) {
        return name;
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".into())
}

fn getrandom(buf: &mut [u8]) {
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lantern-xdg-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("user-dirs.dirs");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn hostname_is_a_real_name() {
        let h = hostname();
        // The bug this guards: macOS fell through every source and shipped
        // the literal "unknown" to every peer on the network.
        assert_ne!(h, "unknown", "hostname() fell through to its last resort");
        assert!(!h.is_empty());
        assert!(!h.contains('.'), "should be the short name, got {h:?}");
        assert!(!h.contains('\0'), "NUL leaked out of the C buffer: {h:?}");
    }

    #[test]
    fn expands_home_relative_entry() {
        let dir = scratch("home");
        let file = write(&dir, "XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n");
        assert_eq!(
            xdg_download_dir_from(&file, std::path::Path::new("/home/u")),
            Some(PathBuf::from("/home/u/Downloads"))
        );
    }

    #[test]
    fn keeps_a_localised_absolute_path() {
        // The folder is localised, which is the whole reason for reading this
        // file instead of hardcoding "Downloads".
        let dir = scratch("l10n");
        let file = write(&dir, "XDG_DOWNLOAD_DIR=\"/mnt/big/Téléchargements\"\n");
        assert_eq!(
            xdg_download_dir_from(&file, std::path::Path::new("/home/u")),
            Some(PathBuf::from("/mnt/big/Téléchargements"))
        );
    }

    #[test]
    fn ignores_comments_and_other_keys() {
        let dir = scratch("skip");
        let file = write(
            &dir,
            "# XDG_DOWNLOAD_DIR=\"$HOME/Wrong\"\n\
             XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n\
             XDG_DOWNLOAD_DIR=\"$HOME/Right\"\n",
        );
        assert_eq!(
            xdg_download_dir_from(&file, std::path::Path::new("/home/u")),
            Some(PathBuf::from("/home/u/Right"))
        );
    }

    #[test]
    fn no_entry_means_no_answer() {
        let dir = scratch("none");
        let file = write(&dir, "XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n");
        assert_eq!(
            xdg_download_dir_from(&file, std::path::Path::new("/home/u")),
            None
        );
    }

    #[test]
    fn missing_file_means_no_answer() {
        assert_eq!(
            xdg_download_dir_from(
                std::path::Path::new("/nonexistent/user-dirs.dirs"),
                std::path::Path::new("/home/u")
            ),
            None
        );
    }
}
