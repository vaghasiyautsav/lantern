# Lantern — Design Document

**A serverless LAN messenger for macOS, Windows, and Linux.**
Working codename: **Lantern** (app) / **Wisp** (protocol).

| | |
|---|---|
| **Status** | Draft v0.2 — for review. v0.2 applies an adversarial technical review; see §11. |
| **Date** | 17 August 2026 |
| **Author** | Utsav Vaghasiya, with Claude |
| **Scope** | Educational / personal LAN use. Not for publication or distribution. |
| **Prior art** | [IP Messenger](https://ipmsg.org) by H. Shirouzu (1996–present) |

---

## 0. Position

IP Messenger got one thing profoundly right in 1996 and has been right about it ever since: **on a local network, you do not need a server.** No account, no cloud, no company in the middle. You open the app and everyone on the LAN is simply *there*.

That idea is more valuable in 2026 than it was in 1996, and almost nobody ships it. Slack, Teams, Discord — all require a round trip to someone else's datacenter to move a file between two machines sitting on the same switch.

Where IP Messenger stopped innovating is everything *around* that idea:

| Area | Where ipmsg stopped | Where Lantern goes |
|---|---|---|
| **Wire format** | Colon-delimited ASCII, 1996. Unframed, ambiguous escaping, no versioning beyond a leading `1:` | CBOR over QUIC. Versioned, self-describing, forward-compatible |
| **Transport** | UDP for messages, a fresh TCP connection per file | One QUIC connection per peer. Multiplexed, encrypted, migrating |
| **Identity** | Any host can claim any name. No identity at all | Ed25519 device identity. Signed presence beacons. TOFU pinning with safety words |
| **Crypto** | Bolted on later; RSA-ECB + RC2/Blowfish historically, RSA2048+AES256 today, still optional | TLS 1.3 via QUIC, mandatory, with identity-pinned certificates. No downgrade path |
| **File transfer** | Whole-file, non-resumable, one TCP stream, no integrity check | BLAKE3-verified chunked streaming. Resumable, parallel, deduplicating, folder-native |
| **Discovery** | UDP broadcast only; "Member Master" bolt-on for cross-router | mDNS-SD + broadcast + IPv6 multicast + signed anchor nodes, unified |
| **Presence** | Online / absent | State, status text, device class, avatar, idle detection, groups |
| **UI** | Win32, functional, unchanged in shape for two decades | Three genuinely native front ends sharing one design language |

Lantern is not a port or a clone of IP Messenger. It is an answer to the question *"what would this look like if you designed it now?"* — with a compatibility bridge so it can still see the old clients on the same wire.

### Non-goals

- Internet-scale messaging, NAT traversal, relays over WAN, federation.
- Accounts, directories, or any server component.
- Telemetry, crash reporting, auto-update phone-home. The app makes **zero** connections off the local link.
- Mobile clients (the protocol permits them; we aren't building them).
- Publication or distribution. This is a learning project.

---

## 1. Architecture at a glance

```
┌──────────────┬──────────────┬──────────────┐
│   macOS      │   Windows    │    Linux     │
│ SwiftUI +    │  WinUI 3 /   │  GTK4 +      │
│ AppKit       │  C# (WinAppSDK) │ libadwaita  │
└──────┬───────┴──────┬───────┴──────┬───────┘
       │ UniFFI       │ csbindgen    │ gtk4-rs
       │ (Swift)      │ (C ABI/C#)   │ (direct link)
┌──────┴──────────────┴──────────────┴───────┐
│      lantern-ffi        │  lantern-ipmsg   │
│  command/event facade   │  legacy bridge   │
│   (no business logic)   │ (public API only)│
├─────────────────────────┴──────────────────┤
│              lantern-core                  │
│   roster · sessions · queues · state m/c   │
├──────────┬──────────┬──────────┬───────────┤
│ discovery│transport │  xfer    │  store    │
│ mdns/bcast│  quinn   │ blake3   │ sqlite    │
├──────────┴──────────┴──────────┴───────────┤
│   lantern-crypto   │   lantern-proto       │
│   ed25519 · TOFU   │   cbor · framing      │
└────────────────────┴───────────────────────┘
```

The bridge sits **above** the core, not beside the transports: it is a consumer of the same public API the UI uses, with no privileged access, and it compiles out entirely behind a Cargo feature.

**One Rust core, three native shells.** The core owns every decision that isn't "how does a button look on this OS." The shells own presentation, OS integration, and nothing else — no protocol logic, no file I/O policy, no trust decisions. This is what makes three native UIs affordable rather than a 3× tax.

### Crate layout

```
lantern/
├── crates/
│   ├── lantern-proto/       # wire types, CBOR codec, framing, test vectors
│   ├── lantern-crypto/      # identity keys, TOFU store, fingerprints, keychain
│   ├── lantern-discovery/   # mDNS-SD, UDP broadcast, IPv6 multicast, anchors
│   ├── lantern-transport/   # QUIC sessions (quinn), stream muxing, reconnect
│   ├── lantern-xfer/        # chunking, BLAKE3 verified streaming, resume
│   ├── lantern-store/       # SQLite + FTS5, message log, blob cache
│   ├── lantern-ipmsg/       # legacy protocol bridge (UDP+TCP 2425)
│   ├── lantern-core/        # orchestration, event bus, public API
│   ├── lantern-ffi/         # UniFFI + C ABI facade
│   └── lantern-cli/         # headless harness for protocol dev & testing
├── apps/
│   ├── macos/               # Xcode project, SwiftUI
│   ├── windows/             # .NET 9 / WinUI 3
│   └── linux/               # Rust binary, gtk4-rs
├── design/
│   ├── tokens.json          # single source of truth for the design system
│   └── generate-tokens.py   # → Swift / XAML / GTK CSS
└── tests/
    ├── vectors/             # protocol conformance fixtures
    └── harness/             # multi-instance integration tests
```

### Why this stack

**Rust core.** The hard parts of this app are all systems work: socket handling across three OSes, packet parsing that must never panic on hostile input, cryptography, concurrent chunked I/O. Rust is the right tool and it links into all three UI stacks.

**Truly native UI per platform.** You asked for the best possible UI. There is no cross-platform toolkit that produces a macOS app that feels like a macOS app *and* a Windows app that feels like a Windows app. The cost is real — three UI implementations — and it's mitigated by (a) the core owning all logic, (b) a shared token system so the three look like siblings, and (c) building them sequentially, not in parallel.

**Async runtime.** Tokio, single multi-threaded runtime owned by the core, started on `Core::new()`. The FFI boundary is synchronous-looking (commands return immediately, results arrive as events) so no UI framework has to understand Rust futures.

### FFI strategy per platform

| Platform | Mechanism | Notes |
|---|---|---|
| **Linux** | `gtk4-rs`, direct link | No `lantern-ffi` layer — the GTK app *is* a Rust binary. (It is of course still FFI to a large C library, with GTK's thread-affinity rules.) Fastest dev loop; build here first. |
| **macOS** | [UniFFI](https://github.com/mozilla/uniffi-rs) → Swift | Mozilla-maintained with first-class Swift support. Production-usable but **pre-1.0** (0.3x), with breaking changes across minor versions — pin the version and budget for upgrades. Core ships as an XCFramework. |
| **Windows** | C ABI → C# P/Invoke, `csbindgen`-assisted | See the caveat below. `SafeHandle` wrappers hand-written on the C# side. |

**The Windows FFI is the hard one, and the tooling only covers half of it.** `csbindgen` (Cysharp) generates the Rust→C# call direction from `extern "C"` signatures — but its last release is v1.9.8 (May 2024), it does not generate `SafeHandle` wrappers, and crucially it does not generate the **C#→Rust callback** plumbing that the event model below depends on. That part is hand-written `[UnmanagedCallersOnly]` trampolines which must never unwind, may be invoked from a Tokio worker thread with no .NET synchronization context, and must marshal onto `DispatcherQueue`. Before this decision calcifies, re-evaluate [`uniffi-bindgen-cs`](https://github.com/NordSecurity/uniffi-bindgen-cs) (NordSecurity) — the "third-party and unmaintained" argument cuts both ways now.

**Event delivery.** Each shell registers one callback. The core pushes `CoreEvent` values (peer appeared, message received, transfer progressed, trust warning raised). Every shell marshals that onto its UI thread — `DispatchQueue.main`, `DispatcherQueue`, `glib::MainContext`. On Linux specifically, reconciling Tokio's multithreaded runtime with `glib::MainContext`'s thread affinity is real work, not a formality: the core's event sink is an `async_channel` drained by a `glib` source on the main context. Progress events are coalesced in the core to ≤20 Hz per transfer so a fast LAN transfer doesn't drown the UI thread.

---

## 2. Protocol: Wisp/1

Two planes, deliberately separated — this is the biggest structural departure from ipmsg, which conflated them on a single port.

- **Discovery plane** — connectionless, UDP, small signed beacons. Its only job is "who is out there and where."
- **Session plane** — QUIC, one connection per peer pair, carries everything else.

### 2.1 Ports

| Purpose | Port | Protocol |
|---|---|---|
| Wisp discovery | **3939** | UDP (broadcast / multicast / unicast) |
| Wisp sessions | **3939** | QUIC (UDP) — same socket, demuxed by QUIC header |
| mDNS-SD service | — | `_lantern._udp.local`, TXT advertises identity + port |
| ipmsg bridge | **2425** | UDP + TCP, **only when compat mode is on** |

Native Lantern deliberately does **not** squat on 2425, so Lantern and the compat bridge (and a real IP Messenger installation) can coexist on one machine.

**Sharing one socket with QUIC — done correctly.** Wisp beacons and QUIC live on the same UDP port 3939, demuxed on the first byte. The naive choice of an ASCII magic like `"WISP"` is *unsafe*: `'W'` is `0x57`, and RFC 9000 §17.3 assigns `0x40–0x7F` to QUIC short headers whose low 5 bits are header-protected and therefore pseudorandom on the wire. `0x57` occurs in roughly 1 of every 32 short-header packets.

RFC 9000 §17.2 reserves the pattern with the **Fixed Bit clear** — first byte in `0x00–0x3F` — precisely so QUIC can share a port with other protocols. Wisp uses it:

```
magic = 0x2A 0x57 0x53 0x50      ("*WSP", first byte 0x2A → Fixed Bit clear)
```

Demux rule: if `byte[0] & 0xC0 == 0x00`, it is not QUIC — parse as Wisp. Note that `0x80–0xBF` would **not** be safe, because QUIC Version Negotiation packets (§17.2.1) fix only the high bit.

### 2.2 Discovery beacon

Sent on: startup, presence change, every 45 s heartbeat, and on `HELLO` from an unknown peer. Sent to IPv4 subnet broadcast, `255.255.255.255`, a dedicated IPv6 link-local group, plus any configured anchor addresses. Advertised in parallel over mDNS-SD.

**Multicast group.** Not `ff02::1` — that is IPv6 all-nodes, which every host, printer, and embedded stack on the link must process up its IP stack, and which MLD-snooping switches cannot prune. Wisp uses a unicast-prefix-based group per RFC 3306 (`ff32::`/`ff35::` derived from the host's own prefix), which avoids IANA allocation and collisions. Link scope for normal discovery, site scope for routed IPv6.

```
Offset  Size  Field
0       4     magic     0x2A 'W' 'S' 'P'
4       1     version   0x01
5       1     type      0x01 HELLO · 0x02 HELLO_ACK · 0x03 BYE
                        0x04 PING  · 0x05 PONG
6       2     flags     bit0 request_ack, bit1 is_anchor,
                        bit2 low_power, bits3-15 reserved
8       2     length    payload length, big-endian, ≤ 1200 total datagram
10      N     payload   CBOR map (see below)
```

Roster exchange is **not** a beacon type. Relaying hundreds of peers cannot fit a 1200-byte datagram and there is no reassembly scheme here by design — it belongs on the QUIC control stream (§2.4 `RosterReq`/`RosterRsp`), which already has framing, ordering, and a 1 MiB frame budget.

Total datagram capped at **1200 bytes** so it never fragments on a 1280-MTU path. Avatars and any other bulk data are *referenced by hash*, never inlined.

Payload CBOR map (integer keys for compactness):

| Key | Name | Type | Meaning |
|---|---|---|---|
| 1 | `id` | bytes[32] | Ed25519 public key — the device identity |
| 2 | `name` | text | Display name, ≤64 chars |
| 3 | `host` | text | Hostname, ≤64 chars |
| 4 | `group` | text | Self-declared group tag, ≤32 chars |
| 5 | `device` | uint | 0 desktop · 1 laptop · 2 server · 3 handheld |
| 6 | `port` | uint | QUIC port (usually 3939) |
| 7 | `state` | uint | 0 active · 1 idle · 2 away · 3 dnd · 4 invisible |
| 8 | `status` | text | Free status message, ≤128 chars |
| 9 | `avatar` | bytes[32] | BLAKE3 of the avatar image, or absent |
| 10 | `caps` | uint | Capability bitfield (see §2.6) |
| 11 | `seq` | uint | Counter, strictly increasing **within one boot** |
| 12 | `boot` | bytes[8] | Random per-process nonce, **compared for equality only** |
| 13 | `ts` | uint | Unix milliseconds, for staleness only |
| 14 | `addrs` | array | The originator's own reachable addresses — required for anchor relay |
| 99 | `sig` | bytes[64] | Ed25519 signature — see below |

**Every beacon is signed.** This is the single most important protocol-level improvement over ipmsg. In IP Messenger, any host on the LAN can broadcast an entry claiming to be anyone — the roster is pure assertion. In Wisp, the name in your roster is cryptographically bound to a key, and if a peer's key changes, the UI says so loudly.

**The signature covers the header too.** Signing only the CBOR payload would leave `type` and `flags` malleable: an on-path attacker — explicitly in scope per §3 — could capture a valid `HELLO`, flip `type` to `0x03 BYE`, and evict the victim from every roster on the LAN with the signature still verifying. So:

```
sig = Ed25519-Sign(identity_sk,
        "wisp-beacon-v1\x00" || version || type || flags || canonical_cbor(keys 1..14))
```

The `"wisp-beacon-v1\x00"` prefix is **domain separation**. The same identity key also signs TLS 1.3 `CertificateVerify` during the QUIC handshake, and TLS domain-separates its own input; without a matching prefix here, the two signing contexts overlap. That is a gratuitous risk to carry, and the prefix costs nothing.

**Replay and ordering.** `boot` is random, so it cannot be *ordered* — only compared. The rule is:

- `boot` differs from the last seen value for this `id` → the peer restarted. Accept, reset the `seq` watermark.
- `boot` matches → require `seq` strictly greater than the watermark, else drop as duplicate or replay.

(A naive "`(boot, seq)` must increase lexicographically" rule fails catastrophically: a peer that restarts and draws a numerically smaller `boot` is blackholed forever. Roughly half of all restarts.)

Beacons that fail the signature check are dropped. `ts` skew beyond 5 minutes is logged, not fatal — clock skew on a LAN is ordinary.

**Anchors and cross-subnet discovery.** Any instance can be marked an **anchor**. Anchors keep a roster of every peer they've seen and answer `RosterReq` over QUIC from peers on other subnets, returning signed beacon payloads verbatim.

Be precise about what this buys. Because the payloads are signed — including the `addrs` field — a malicious anchor cannot fabricate a peer or redirect you to an attacker-controlled host. It *can* still withhold peers, replay stale entries, and learn who is asking. **Anchors are untrusted for identity and semi-trusted for availability.** That is a strictly better position than IP Messenger's Member Master, which is trusted for both, but it is not "no trust at all." (Had `addrs` been omitted, the anchor would have had to supply addressing out of band and would control the identity→address mapping outright; TLS pinning would still catch the impersonation at connect time, but the result would be a confusing "new device" prompt instead of a clean failure.)

### 2.3 Session plane

One QUIC connection per peer pair. ALPN: `wisp/1`.

- **Endpoint certificate**: self-signed X.509, valid 10 years, whose SubjectPublicKeyInfo is the device's Ed25519 identity key. There is no CA. Verification is: *does this certificate's key equal the identity we pinned for this peer?* If we have no pin, TOFU-pin it and surface a "new device" event.
- **Who dials**: *anyone may dial at any time.* If both peers dial simultaneously, the tie-break is: keep the connection initiated by the **lower** `id` (lexicographic on the 32 bytes), close the other with error code `0x01 DUPLICATE`. This is a collision resolver, **not** a prohibition — an earlier draft made the lower id the only permitted dialer, which silently broke Invisible mode (§4.2: an invisible peer stops beaconing, so if it also may not dial, it can never start a conversation) for half of all identity pairs.
- **Idle timeout**: 5 minutes. Keepalive interval is deliberately **longer** than the idle timeout is short — i.e. keepalive is off for idle conversations and the connection is allowed to close; it is cheap to re-establish. (Setting a keepalive shorter than the idle timeout means the idle timeout never fires, which is a config that quietly pins every peer connection open forever.)
- **0-RTT: disabled.** QUIC 0-RTT data is replayable by construction (RFC 9001 §9.2), and §3's threat model explicitly includes a LAN adversary who can replay. Allowing `Msg` or `Ack` frames in 0-RTT would let that adversary re-deliver messages at will. The saving — one RTT, well under a millisecond on a LAN — is not worth it.
- **Migration**: enabled, **but it only rescues one side.** RFC 9000 §9 is explicit that only the *client* may initiate migration; a server cannot. So a laptop that moves from Wi-Fi to Ethernet mid-transfer keeps its QUIC connection **if it happens to be the dialing peer**, and otherwise reconnects. quinn also requires `Endpoint::rebind()` to be driven explicitly on interface change — migration is not automatic. The honest framing: QUIC migration is a nice-to-have that sometimes saves a reconnect; the thing that actually makes transfers survive a network change is **chunk-level resume** (§2.5), which works in both directions regardless of role.

Stream allocation:

| Stream | Direction | Purpose |
|---|---|---|
| Bidi 0 | Both | **Control channel.** Length-prefixed CBOR frames, u32 BE length, ≤1 MiB per frame |
| Uni (any) | Sender→Receiver | **File data streams.** Header frame then raw chunk bytes |
| Datagrams | Both | Typing indicators, presence deltas, transfer progress hints. Lossy by design |

Chat is never blocked behind a file transfer, because QUIC streams are independently flow-controlled. In IP Messenger a large transfer and a message are entirely separate connections; here they share one connection and one congestion controller, which is both simpler and fairer on the wire.

### 2.4 Control frames

CBOR maps, tagged by a `t` (type) key. Abbreviated schema:

```
Hello        { t:"hello",  proto:1, id:bytes32, name, host, caps, avatar? }
HelloAck     { t:"hack",   accepted:bool, reason? }

Msg          { t:"msg",    mid:uuid, ts:uint, body:{ text, fmt:"md"|"plain" },
                           reply_to:uuid?, attach:xfer_id?,
                           flags:{ sealed:bool, receipt:bool, urgent:bool } }
Ack          { t:"ack",    mid:uuid, kind:"delivered"|"read"|"opened", ts }
Edit         { t:"edit",   mid:uuid, body, ts }
Retract      { t:"retr",   mid:uuid, ts }
Typing       { t:"typ",    state:"start"|"stop" }          # datagram

Presence     { t:"pres",   state, status, avatar? }         # datagram

OfferFiles   { t:"offer",  xid:uuid, total_size, files:[ FileEntry ] }
FileEntry    { path, size, mode, mtime, kind:"file"|"dir"|"symlink",
               root:bytes32 }                               # BLAKE3 root hash
AcceptFiles  { t:"accept", xid, indices:[uint], have:[[idx, from, to]] }
DeclineFiles { t:"decline",xid, reason? }
ChunkReq     { t:"creq",   xid, idx:uint, ranges:[[uint,uint]] }
XferCtl      { t:"xctl",   xid, action:"pause"|"resume"|"cancel" }
XferDone     { t:"xdone",  xid, idx?, ok:bool, err? }

AvatarReq    { t:"avreq",  hash:bytes32 }
AvatarData   { t:"avdat",  hash:bytes32, mime, data:bytes }  # ≤256 KiB

RosterReq    { t:"rreq",   limit:uint }                       # to an anchor
RosterRsp    { t:"rrsp",   beacons:[bytes] }                  # signed payloads, verbatim

KeyRotate    { t:"krot",   new_id:bytes32,
                           sig_new_by_old:bytes64,
                           sig_old_by_new:bytes64 }
Error        { t:"err",    code:uint, msg:text }
```

Design notes:

- **`sealed`** is IP Messenger's "secret message," kept because it's genuinely good UX: the message renders as a sealed envelope; the sender is told the moment it's opened. Preserved as `Ack{kind:"opened"}`.
- **`receipt`** is opt-in per message, not a global setting. Read receipts are a consent question, not a config toggle.
- **`Edit`/`Retract`** are new. Retract removes from the recipient's live view and marks the stored log entry as retracted; it does **not** promise deletion, and the UI says so honestly.
- **`KeyRotate`** lets a peer that reinstalls migrate its identity without triggering a scary warning, *if* it can prove continuity. That proof is **both directions** — the new key signed by the old, *and* the old key signed by the new — each over a domain-separated input (`"wisp-keyrotate-v1\x00" || old_id || new_id`). A single one-directional signature over a bare 32-byte key is a replayable capture that a third party could present as its own rotation. Without a valid pair, it is simply a new identity and the UI says so.

### 2.5 File transfer

The most-used feature in IP Messenger, and the one with the most room to improve.

**Chunking and integrity.** BLAKE3 is a Merkle tree, so a receiver can verify each piece *as it arrives* against a single root hash using an inclusion proof — no need to receive the whole file before knowing it's intact. This is Bao-style verified streaming and it is the right primitive here. Three things the one-line version of that claim hides, all of which drive concrete decisions:

1. **It is not free in bytes.** Bao's outboard encoding costs 64 bytes per parent node. At BLAKE3's *native* 1 KiB leaf size that is ~6.25% wire overhead — on the 40 GB folder example below, about 2.5 GB of pure proof material.
2. **So the transfer unit must be aligned to a block size, not picked arbitrarily.** Wisp uses [`bao-tree`](https://docs.rs/bao-tree/) with a **chunk group of 16 KiB** (`BlockSize::from_chunk_log(4)`), which drops proof overhead to ~0.39%. The transfer unit is **1 MiB = 64 blocks**, so a chunk's proof is its subtree parents plus a log₂(n) sibling path — kilobytes, not tens of kilobytes.
3. **The `blake3` crate does not expose this.** Verified streaming needs `bao` (whose own README warns it is *"beta cryptography software… not been formally audited"*), `bao-tree` (n0-computer/iroh, actively maintained — the intended dependency), or hand-rolling on blake3's hazmat API. A document that calls a parser panic a release blocker should not quietly depend on unaudited beta crypto, so: **`bao-tree`, pinned, with the integrity path fuzzed alongside the codec.**

The root hash goes in the offer; every arriving chunk is verified against it before it touches the destination file.

**Flow.**

1. Sender builds a manifest — walks the selection, records path/size/mode/mtime, computes root hashes (streaming, off the UI thread), sends `OfferFiles`.
2. Receiver sees a file card with names, sizes, total. It picks a destination, may accept a subset, and reports any chunk ranges it already has (from an interrupted earlier attempt or from a chunk it has seen from any peer). Sends `AcceptFiles`.
3. Sender opens up to **4 concurrent unidirectional streams**, each carrying `{xid, idx, chunk}` headers followed by chunk bytes. Concurrency is adaptive: QUIC's congestion controller does the real work; the stream count just keeps the pipe full on high-BDP links.
4. Receiver writes to `<dest>/.lantern-partial/<xid>/<idx>` with a sidecar recording verified chunk ranges. On completion, verify root, `fsync`, atomically rename into place, restore mtime and mode.
5. `XferDone` both ways.

**Resume.** If the connection drops mid-transfer, nothing is lost. The partial file and its range sidecar survive process restart and machine reboot. On reconnect the receiver re-sends `AcceptFiles` with its `have` ranges and the sender ships only the gaps. IP Messenger restarts from zero.

**Dedup — and its side channel.** A content-addressed chunk cache (bounded, LRU, default 2 GiB) means re-receiving a file you already have costs almost nothing. But reporting `have` ranges keyed on content hash tells the *sender* which content you already possess, and a malicious peer can weaponize that: offer a file it merely suspects you have, watch you decline every chunk, and confirm possession without transferring a byte. This is the classic content-hash probe (the Dropbox `hash_value` attack).

So dedup is scoped: **`have` ranges are reported only for chunks previously received from that same peer.** Cross-peer dedup still happens locally on write — you don't store the same bytes twice — but it is never disclosed on the wire. This costs some bandwidth in the "three people send me the same 4 GB build" case and is worth it.

**Folders** stream entry-by-entry with no pre-archiving step, so a 40 GB folder starts transferring immediately instead of after a zip.

**Safety.** Every incoming path is normalized and rejected if it escapes the destination root (`..`, absolute paths, drive letters, NTFS ADS colons, reserved Windows names like `CON`/`NUL`, trailing dots/spaces). Symlinks are never followed on write and are only recreated if they resolve inside the transfer root. Executable bits are stripped from received files by default. This class of bug — *Zip Slip* — is the single most likely security hole in an app like this, so it gets an explicit fuzz-tested module and its own test-vector file.

### 2.6 Capability bits

Advertised in beacons and in `Hello`, so the protocol can grow without version bumps.

| Bit | Capability |
|---|---|
| 0 | Text messaging |
| 1 | File transfer v1 |
| 2 | Folder transfer |
| 3 | Resume / partial accept |
| 4 | Chunk dedup |
| 5 | Avatars |
| 6 | Typing indicators |
| 7 | Message edit / retract |
| 8 | Anchor node |
| 9 | ipmsg bridge active |
| 10 | Screenshot / clipboard send |
| 11–31 | Reserved |

---

## 3. Security model

**Threat model.** The adversary is another machine on the same LAN: a curious colleague, a compromised laptop, someone on the guest Wi-Fi. They can sniff, spoof, and replay on the local segment. They cannot break Ed25519 or TLS 1.3. Out of scope: a compromised endpoint (if they own your machine, they read your messages), and traffic analysis (they can see *that* you're transferring 4 GB to someone).

### 3.1 Identity

- One **Ed25519 keypair per device**, generated on first launch.
- Private key stored in the OS secret store: macOS Keychain, Windows DPAPI via CredMan, Linux Secret Service (libsecret) with an encrypted-file fallback for headless/minimal systems.
- The public key **is** the identity. Names are labels, not identity.

**One macOS caveat, stated plainly.** `SecKeyCreateWithData` supports RSA and NIST EC only — there is no `kSecAttrKeyTypeEd25519`. The Ed25519 private key is therefore stored as **raw bytes in a generic-password item** (with `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`), which works fine but means no Secure Enclave backing and no non-extractable-key story on macOS. ECDSA P-256 would buy Secure Enclave storage; it would also make signing RNG-dependent on the beacon path, where deterministic signatures are worth more. Ed25519 stays.

### 3.2 Fingerprints and verification

Fingerprint = BLAKE3-256 of the public key, rendered two ways:

- **Safety words** — 8 words from a 2048-word BIP-39-style list, e.g. `orbit · marina · flatten · cobra · sonnet · drifter · almanac · quill`. Encodes 88 bits, which puts a colliding fingerprint out of reach (2⁸⁸ grinding) while staying short enough to actually read aloud. The list alternates between two 1024-word halves by position, borrowing the PGP Word List's anti-transposition property — swapping two words yields an invalid sequence rather than a plausible one, and reading aloud across a desk is exactly where transposition happens. (Note: the actual PGP Word List is two 256-word lists encoding 8 bits each; this is BIP-39 sizing with PGP's alternation trick.)
- **Emoji grid** — a 4×4 grid derived from the same digest, for at-a-glance visual comparison on two screens.

Verification is a deliberate act: open the peer inspector, compare, tap **Verified**. Verified peers get a subtle badge. Unverified-but-pinned peers work normally — we do not nag — but a **key change** on a previously-seen peer produces a red banner and blocks file transfer until acknowledged.

### 3.3 Transport security

QUIC/TLS 1.3, mandatory, no plaintext mode, no cipher negotiation surface beyond TLS 1.3's fixed suites. Certificate verification is pure key pinning; the X.509 wrapper is a formality that `rustls` requires.

**No downgrade.** A native Lantern peer that fails identity verification does not fall back to anything. Legacy ipmsg peers are reached only through the bridge and are visibly, permanently marked as unencrypted.

### 3.4 At rest

- Message log in SQLite, with `PRAGMA secure_delete=ON` so "clear history" doesn't leave plaintext in freed pages. Optional **SQLCipher** encryption with the key in the OS keychain — off by default (it complicates backup and adds a build dependency), one switch to enable, and the switch explains the tradeoff in one sentence rather than a paragraph of jargon.
- The chunk cache stores content, not filenames, and is wiped on "clear history."
- Received files land in a user-chosen directory. Default: `~/Lantern` — never a hidden path, never the system temp directory.

### 3.5 Network posture

Full socket inventory — there are four, not three:

| Socket | When |
|---|---|
| UDP 3939, dual-stack — **both** Wisp beacons and QUIC (§2.1 demux) | Always |
| UDP 5353 + `224.0.0.251` / `ff02::fb` — mDNS responder | When mDNS discovery is on (default) |
| UDP + TCP 2425 | Only when the ipmsg bridge is on (default off) |
| Outbound unicast to configured **anchor** addresses | Only if anchors are configured |

Two honest qualifications to "nothing leaves the LAN":

- **Anchors are off-link by definition.** Cross-subnet discovery means routed unicast to an address you configured. That is still your network, not the internet, but it is not the local link.
- **The Markdown renderer resolves no remote resources.** `![](https://…)` in an incoming message is *not* fetched. Inline images render only from `attach` transfers and the avatar cache. Link *navigation* is user-initiated and handed to the OS browser. Without this rule, any LAN peer could use an image tag as a read-receipt and IP-disclosure beacon, and the "zero connections off the local link" claim would be false in the most-used code path in the app.

Beyond that: no external hostname resolution, no update checker, no telemetry. On a machine with no default route the app works exactly as well as on one with internet.

---

## 4. Discovery and presence

### 4.1 Four discovery paths, one roster

1. **mDNS-SD** (`_lantern._udp.local`) — the modern path. TXT record carries the **full 32-byte identity key** (44 base64 chars, comfortably inside a TXT record — truncating buys nothing and adds a collision surface) plus the QUIC port. Ship our own responder (`mdns-sd`) on **all three** platforms rather than relying on the OS: Windows has had a native mDNS resolver since 10 1703, but coverage is uneven and Windows Firewall blocks inbound 5353 for new apps by default behind a first-run prompt users routinely decline.
2. **UDP broadcast** — the ipmsg path, kept because mDNS is blocked on plenty of managed networks. Sends to each interface's directed broadcast plus `255.255.255.255`.
3. **IPv6 multicast** — the RFC 3306 unicast-prefix-based group from §2.2, link scope for the local segment and site scope for routed v6.
4. **Anchors and static peers** — explicit unicast addresses, for cross-subnet and for locked-down networks where nothing broadcasts.

All four feed one deduplicated roster keyed on the Ed25519 identity, not on IP address. A peer that changes IP is *the same peer* and its conversation, transfers, and trust state follow it. IP Messenger keys on address and loses the thread.

### 4.2 Presence

| State | Source |
|---|---|
| Active | User input within 5 min |
| Idle | OS idle timer past threshold (default 5 min) |
| Away | Manual, or screen locked |
| DND | Manual — suppresses all notification surfaces, still receives |
| Invisible | Manual — stops beaconing, still connects outbound |
| Offline | No beacon for 3 heartbeat intervals (135 s) |

Idle detection per platform: `CGEventSourceSecondsSinceLastEventType` (macOS), `GetLastInputInfo` (Windows), the `org.freedesktop.ScreenSaver` / idle-inhibit portal (Linux).

### 4.3 Offline queue

Messages to an offline peer are **stored durably** and flushed when that identity reappears — across app restarts and reboots, which IP Messenger's in-memory send queue does not survive. The UI shows queued messages in the timeline with a distinct "waiting" treatment and an honest tooltip: *this will send when they're back on the network.* Queue entries expire after a configurable 7 days.

---

## 5. UI and UX

### 5.1 The shared design system

Three native front ends will drift unless something forces them not to. That something is `design/tokens.json` — the single source of truth — compiled by `generate-tokens.py` into:

- `apps/macos/Generated/Tokens.swift` — `Color`/`Font`/`CGFloat` extensions
- `apps/windows/Generated/Tokens.xaml` — a `ResourceDictionary`
- `apps/linux/generated/tokens.css` — GTK4 CSS custom properties

**Version floors this depends on:** GTK **4.16+** for real CSS custom properties and `color-mix()` (older GTK only has `@define-color`, which cannot express the spacing, type, or motion tokens at all — on those, the generator must emit static values), and libadwaita **1.6+** for system accent color via `AdwStyleManager`.

Tokens cover: an 8-point spacing scale (4/8/12/16/24/32/48), a type scale, corner radii, elevation, motion durations and curves, and **semantic** colors (`surface`, `surface-raised`, `text-primary`, `text-secondary`, `accent`, `success`, `warning`, `danger`, `unverified`, `legacy`) each with light and dark values validated to 4.5:1 contrast.

What is **not** shared: control shapes, iconography, window chrome, animation feel, and navigation idioms. Those are the platform's job, and imitating one OS on another is exactly what makes cross-platform apps feel wrong.

### 5.2 Information architecture

```
┌─────────────┬──────────────────────────────┬──────────────┐
│  ROSTER     │        CONVERSATION          │  INSPECTOR   │
│  260px      │        flexible              │  300px       │
│             │                              │  collapsible │
│  [search]   │  ┌ toolbar: peer, verify ┐   │              │
│             │                              │  Identity    │
│ ★ Favorites │    message                   │   safety     │
│   Mira      │            message ▸         │   words      │
│   Dev-Box   │    ┌──────────────┐          │   [Verify]   │
│             │    │ file card    │          │              │
│ ● Online    │    │ ▓▓▓▓▓░░ 62%  │          │  Transfers   │
│   Kenji     │    └──────────────┘          │   3 active   │
│   Ana       │            message ▸ ✓✓      │              │
│             │                              │  Shared      │
│ ▣ Design    │  ┌──────────────────────┐    │   files      │
│   Sam       │  │ composer             │    │              │
│             │  │ [📎][⧉][🔒]    [Send]│    │  Notes       │
│ ○ Offline   │  └──────────────────────┘    │              │
│ ⚠ Legacy    │                              │              │
└─────────────┴──────────────────────────────┴──────────────┘
                     status bar: transfer mini-widget
```

Roster sections, in order: **Favorites**, **Online**, user-defined **Groups**, **Legacy** (ipmsg peers, visually distinct), **Offline**. Each row: avatar with presence ring, display name, secondary line (host · device · status), unread badge, and a verification pip.

Below the tablet breakpoint (or a narrow window), inspector collapses first, then roster becomes an overlay. All three shells implement the same two breakpoints.

### 5.3 Screen inventory

| Screen | Purpose |
|---|---|
| **Main window** | The three-pane layout above |
| **Transfer center** | All transfers, active and historical. Pause/resume/cancel, reveal in file manager, retry |
| **Peer inspector** | Identity, safety words, verification, shared history, per-peer notes and mute |
| **Command palette** | ⌘K / Ctrl+K. Jump to peer, send file, screenshot, set status, verify, toggle DND |
| **Log viewer** | Full-text search across all history (SQLite FTS5), filter by peer/date/has-file/has-image, jump-to-context |
| **Screenshot annotator** | Capture region/window/screen, then arrow, box, highlight, blur/mask, text. Send directly |
| **Settings** | Identity, network, notifications, transfers, appearance, compatibility, privacy |
| **First run** | Name, avatar, group, one screen explaining that nothing leaves the LAN, then straight into the roster |

### 5.4 Composer

Multiline, grows to 8 lines then scrolls. Markdown with live styling (bold, italic, code, links, lists, fenced code blocks with syntax highlighting). `Enter` sends, `Shift+Enter` newline, configurable both ways.

Actions on the composer bar:

- **Attach** — file picker, or drag anywhere onto the window, or paste
- **Screenshot** — invokes the annotator
- **Send clipboard** — one action for whatever is on the clipboard, image or text
- **Seal** — the sealed-envelope mode, with an "I'll be told when you open it" affordance
- **Receipt** — per-message read receipt request

Drag-and-drop shows a full-window drop target with the file count and total size, and separate drop zones for *send now* vs *add to composer*.

### 5.5 Message rendering

Rows, not chat bubbles, for own messages vs theirs — bubbles waste horizontal space and this is a desktop app where a message may contain a 40-line code block. Grouped by sender with a single avatar per run, timestamps on hover, day separators.

- **Inline images** — thumbnailed, click to expand, arrow-key through all images in the conversation.
- **File cards** — icon by type, name, size, and a progress ring that transitions indeterminate → determinate. On completion: **Open**, **Reveal**, and a checksum-verified tick.
- **Delivery states** — sending (hollow), delivered (single tick), read (double tick), queued-offline (clock), failed (retry affordance). Never silent failure.
- **Sealed messages** — a sealed envelope with the sender and time; content revealed on explicit click, which fires the `opened` ack.

### 5.6 Motion and feel

Durations from tokens: 120 ms for micro-state, 200 ms for layout, 300 ms for view transitions. Ease-out for entrances, ease-in-out for moves. List insertions spring in from the direction they arrived. Progress rings never jump backwards. **Every animation respects the OS reduce-motion setting**, in which case durations collapse to 0 and cross-fades replace slides.

### 5.7 Per-platform native integration

**macOS** — unified toolbar with the sidebar in a vibrancy material; `List` with `.sidebar` style; SF Symbols throughout; a menu bar extra showing unread count with a click-through to the last conversation; Quick Look preview for received files; drag received files out to Finder; Notification Center with inline reply; full keyboard idiom compliance (⌘1–9 for roster slots, ⌘F find, ⌘⇧A attach); Services menu entry for "Send with Lantern"; Handoff-style window restoration.

**Windows** — WinUI 3 `NavigationView` in left-compact mode; Mica backdrop on the main window, Acrylic on flyouts; Segoe Fluent icons; system tray icon with a jump list of recent peers; toast notifications with inline reply and a "Save file" action; registered as a Share target so any app can send to a peer; correct behavior under Snap Layouts and multi-DPI monitor moves; taskbar progress on the app icon during transfers.

**Linux** — GTK4 + libadwaita. The adaptive layout is **not** free: `AdwNavigationSplitView` (libadwaita 1.4 / GNOME 45) is a *two*-pane sidebar+content widget, so the three-pane layout is an `AdwOverlaySplitView` (also 1.4) for the collapsible inspector nested inside it, plus hand-written `AdwBreakpoint` setters to sequence the two-stage collapse (inspector first, then roster to overlay). Very achievable, a day's work, not a property you set. Plus: `AdwHeaderBar` with the standard hamburger; system accent color and dark preference via `AdwStyleManager` (accent needs libadwaita **1.6** / GNOME 47); XDG desktop notifications with actions; file dialogs through the XDG portal so it behaves correctly inside Flatpak; `.desktop` file with actions.

### 5.8 Accessibility

Not a cleanup phase — a definition-of-done item for every screen. Full keyboard navigation with visible focus rings. Accessibility labels on every control, tested with VoiceOver, Narrator, and Orca. No information conveyed by color alone — verification state, presence, and delivery state each have a shape as well as a hue.

**Contrast**, with the limits of the mechanism stated: `generate-tokens.py` fails the build on any *opaque* token pair below 4.5:1 for text or 3:1 for UI boundaries, in both themes. It cannot check text over the translucent materials §5.7 mandates — vibrancy, Mica, Acrylic all composite against arbitrary desktop wallpaper, and that is exactly where real contrast failures happen. So there is a companion rule the checker can't enforce and review must: **text never sits directly on a translucent material.** An opaque `surface-raised` layer goes between.

**Text sizing** differs by platform and the doc should not pretend otherwise. Windows has "Make text bigger" and display scaling; GNOME exposes `text-scaling-factor`; **macOS has no system-wide dynamic type for AppKit/SwiftUI apps** — `Font.TextStyle` maps to fixed point sizes there. macOS therefore gets an in-app text-size preference instead, and the other two follow the OS.

---

## 6. ipmsg compatibility bridge

Off by default. One switch in Settings → Compatibility, with a plain-language explanation of what it does and what it costs.

### 6.1 What it does

Binds UDP and TCP **2425** and speaks the classic protocol: `Ver:PacketNo:SenderName:SenderHost:CommandNo:Extra`.

**Encoding, done right.** `IPMSG_UTF8OPT` (0x00800000) may only be sent to a peer that advertised `IPMSG_CAPUTF8OPT` (0x01000000) in its entry packet. Setting it unilaterally — as an earlier draft of this doc said to — produces mojibake on Shift_JIS-era clients, the exact failure it was meant to prevent. So: observe the capability in `BR_ENTRY`/`ANSENTRY`, send UTF-8 only to peers that claim it, and transcode to the peer's legacy encoding otherwise.

| Classic | Mapped to |
|---|---|
| `BR_ENTRY` / `ANSENTRY` / `BR_EXIT` / `BR_ABSENCE` | Roster entries flagged `legacy: true`, `verified: never` |
| `SENDMSG` (+`SENDCHECKOPT`) | `Msg` with text body; `RECVMSG` back as `Ack{delivered}` |
| `SECRETOPT` / `READMSG` | Sealed message; `READMSG` → `Ack{opened}` |
| `FILEATTACHOPT` + attach list | `OfferFiles`; served over classic TCP `GETFILEDATA` / `GETDIRFILES` |
| `GETINFO` / `GETABSENCEINFO` | Version and status replies |
| `GETPUBKEY` / `ANSPUBKEY` and the RSA/AES layer | **Not implemented in v1.** See below |

### 6.2 What it deliberately does not do

- **No legacy crypto in v1.** Modern IP Messenger does RSA2048+AES256, and we could implement it — but the historical modes in the same negotiation (RSA-ECB, RC2-40) are weak, and a bridge that negotiates is a bridge that can be downgraded. v1 treats every legacy peer as plaintext and *says so*: legacy roster rows carry a permanent "unencrypted" badge in the warning color, the conversation header repeats it, and the composer shows an unlocked indicator. Adding RSA2048+AES256-only support is a reasonable **Phase 7b** once the honest baseline exists.
- **No resume, no dedup, no folder streaming** with legacy peers. The old protocol can't express it. The UI shows the reduced capability set rather than offering buttons that will fail.

### 6.3 Implementation notes

The bridge is a separate crate that talks to `lantern-core` through the same public API the UI uses. It has no privileged access. That constraint keeps the core's protocol handling clean and means the bridge can be compiled out entirely with a Cargo feature.

Command constants, verified against a mirrored `ipmsg.h`: `BR_ENTRY 0x01`, `BR_EXIT 0x02`, `ANSENTRY 0x03`, `BR_ABSENCE 0x04`, `SENDMSG 0x20`, `RECVMSG 0x21`, `READMSG 0x30`, `GETINFO 0x40`, `GETFILEDATA 0x60`, `RELEASEFILES 0x61`, `GETDIRFILES 0x62`. The command occupies the low 8 bits (`IPMSG_GET_MODE` masks `& 0x000000ff`); options occupy the upper 24, starting at 0x100.

One trap worth writing down: **option bit values are context-dependent.** `IPMSG_ABSENCEOPT` and `IPMSG_SENDCHECKOPT` are both `0x100`; `IPMSG_SERVEROPT` and `IPMSG_SECRETOPT` are both `0x200`. They are interpreted according to the command they accompany, so the decoder must be command-aware rather than flag-driven.

Even so, building the bridge starts with a **capture session against a live IP Messenger install**, not with code. The published protocol text defers to the header rather than listing values, real clients have accreted behavior the spec never described, and a golden-file corpus of real packets is worth more than any amount of reading.

---

## 7. Data model and storage

SQLite via `rusqlite`, WAL mode, one database file.

```sql
peers      (id BLOB PK, name, host, group_tag, device, avatar_hash,
            first_seen, last_seen, verified INT, pinned_key BLOB,
            favorite INT, muted INT, notes, legacy INT)

-- msg_rowid, not mid, is the primary key: FTS5 external-content
-- tables join on the content table's rowid, and a BLOB PK is not
-- a rowid alias. Getting this wrong silently desyncs the index.
messages   (msg_rowid INTEGER PRIMARY KEY AUTOINCREMENT,
            mid BLOB UNIQUE NOT NULL,
            peer_id BLOB, direction INT, ts INT,
            body TEXT, fmt TEXT, flags INT, state INT,
            reply_to BLOB, xid BLOB, retracted INT)

CREATE VIRTUAL TABLE messages_fts USING fts5(
    body, content=messages, content_rowid=msg_rowid);
-- plus the three sync triggers (AFTER INSERT / DELETE / UPDATE on
-- messages, writing messages_fts(messages_fts, rowid, body)).
-- Without them the index drifts from the table and search silently rots.

transfers  (xid BLOB PK, peer_id BLOB, direction INT, started, finished,
            total_size, state INT, dest_path)
xfer_files (xid BLOB, idx INT, path, size, root_hash BLOB,
            state INT, bytes_done INT, PRIMARY KEY(xid, idx))
chunks     (hash BLOB PK, size INT, refcount INT, last_used INT)
queue      (qid PK, peer_id BLOB, frame BLOB, created, expires)
kv         (k TEXT PK, v BLOB)
```

Migrations are numbered, forward-only, applied on open. Retention is user-controlled: keep everything (default), or prune messages and blobs older than N days. "Clear history" means a real `DELETE` plus `VACUUM` plus chunk-cache wipe — with `PRAGMA secure_delete=ON` set at open, so freed pages don't retain plaintext.

---

## 8. Testing

| Layer | Approach |
|---|---|
| **Codec** | `cargo-fuzz` on every parser — beacons, control frames, ipmsg packets, path normalization, **and the `bao-tree` integrity path**. A panic is a release blocker. |
| **Protocol** | Golden test vectors in `tests/vectors/` — hex fixtures with expected decodes, so any future reimplementation can prove conformance. |
| **Path safety** | Its own vector file: `..`, absolute paths, UNC, drive letters, `CON`/`NUL`/`AUX`, trailing dot/space, NTFS ADS colons, Unicode normalization tricks, symlink escapes. |
| **Integration** | `lantern-cli` spawns N instances on loopback with distinct identities; scripted scenarios assert on the event stream. Runs in CI, no GUI needed. |
| **Network conditions** | Linux `netem` for 200 ms RTT, 3% loss, 10 Mbit shaping, and mid-transfer interface flap. Test **both role assignments** — QUIC migration only rescues the dialing peer (§2.3), so a test that happens to use one identity ordering will pass while the other direction silently falls back to reconnect-and-resume. |
| **Interop** | Recorded packet captures from a real IP Messenger install, replayed against the bridge. |
| **UI** | Snapshot tests per platform where the toolkit supports it; a manual accessibility checklist per screen, signed off before a screen is "done." |

Building `lantern-cli` **first** — before any GUI — is the single highest-leverage decision in this plan. Two terminal windows discovering each other and exchanging a message is the moment the project becomes real, and it lands in Phase 1. A crude file copy over an established QUIC stream follows immediately after as a Phase 1.5 spike — deliberately *before* the full manifest/chunk/resume machinery of Phase 3 — because "I just sent a file between two machines with no server" is the demo that sustains the project through the unglamorous middle.

---

## 9. Roadmap

Sequential, not parallel. Each phase ends with something demonstrable.

**The ordering rule:** everything that is *core-side* ships before the first UI, and the first UI implements the **complete** screen surface. Otherwise every late feature gets built three times, in three toolkits, after those shells were declared done — which would undercut the whole argument for three native front ends.

| Phase | Deliverable | Gate |
|---|---|---|
| **0 — Foundations** | Workspace, CI, `lantern-proto` codec + test vectors, fuzz targets, `lantern-crypto` skeleton | Vectors round-trip; fuzzers clean for 1 M cases |
| **1 — Discovery & chat (headless)** | `lantern-discovery`, `lantern-transport`, `lantern-core`, `lantern-store` (messages, peers, kv), `lantern-cli` | Two machines on a real LAN see each other and exchange messages that survive a restart |
| **1.5 — File spike** | Crude whole-file copy over a QUIC stream. Throwaway, deliberately | A file moves between two machines. The demo exists |
| **2 — Trust** | TOFU store, safety words, key-change detection, `KeyRotate`, OS keychain on all three | Key change raises the right event; keys survive reinstall; rotation verifies both directions |
| **3 — File transfer** | `lantern-xfer` complete: manifest, `bao-tree` verified streaming, parallel streams, resume, scoped dedup, path safety. `lantern-store` gains transfers/xfer_files/chunks | 10 GB folder transfers; survives `kill -9` and a mid-transfer interface change |
| **4 — Core-side depth** | FTS5 + triggers, durable offline queue, screenshot capture pipeline, avatar cache, transfer-center state — all headless, all exercised from `lantern-cli` | Every §5.3 screen has a working core API behind it |
| **5 — Linux UI** | Full GTK4/libadwaita app covering the **complete** §5.3 screen inventory: main window, transfer center, inspector, command palette, log viewer, annotator, settings, first-run. The UX gets designed here | Accessibility checklist passed per screen; no known missing surface |
| **6 — macOS UI** | `lantern-ffi` + UniFFI XCFramework, SwiftUI app, menu bar extra, Quick Look, Notification Center, Services entry | Same surface; feels like a Mac app to someone who uses Mac apps |
| **7 — ipmsg bridge** | Capture session first, then `lantern-ipmsg`. Legacy peers appear, permanently marked | Real IP Messenger install exchanges text and files both ways |
| **7b — Legacy crypto** *(optional)* | RSA2048+AES256 only, no downgrade path | Decided by open question 3 |
| **8 — Windows UI** *(deferred)* | C ABI + callback trampolines, WinUI 3 app, tray, toasts with inline reply, share target | Same surface; Mica/Fluent conventions correct |
| **9 — Packaging** | Signed `.app`, MSIX, and **Flatpak** (portal-based file dialogs assume it). AppImage only if an unsandboxed build is genuinely needed — it changes portal behavior. Local install only | Clean install on a fresh machine of each OS |

**Platform priority (decided 17 Aug 2026): macOS and Linux first; Windows deferred.** Linux still leads (gtk4-rs needs no FFI layer, so the UX iterates fastest), macOS follows immediately, and the Windows shell — the one with the hardest FFI story — waits until both are real. A useful side effect: by the time Windows starts, the `lantern-ffi` boundary will have been battle-tested by the macOS shell, and open question 6 (csbindgen vs `uniffi-bindgen-cs`) can be answered from experience instead of speculation.

Realistic effort for one person building carefully: phases 0–3 are the interesting quarter of the work and by far the most instructive; phase 5 is where it becomes an app; 6 is a substantial chunk on its own.

Note that `tokens.json` and `generate-tokens.py` deliberately move to Phase 5 rather than Phase 0 — a design system built before there is a UI to constrain is guesswork, and Phase 0's job is the wire format.

---

## 10. Open questions for you

1. ~~**Name.**~~ **Decided (17 Aug 2026):** the app is **Lantern**, the protocol is **Wisp**. Crate names stand.
2. ~~**First platform.**~~ **Decided:** macOS and Linux first, Windows deferred to Phase 8. Linux leads for iteration speed, macOS follows immediately.
3. **Legacy crypto.** Is RSA2048+AES256 interop with modern IP Messenger worth Phase 8b, or is plaintext-and-honest enough for a LAN you control?
4. **Anchor nodes.** More than one subnet to cover, or is single-LAN enough to defer this entirely? Deferring removes the `addrs` field, the `RosterReq` frames, and a trust discussion.
5. **Encrypted-at-rest default.** On (safer, adds SQLCipher as a build dependency, breaks naive backup) or off (simpler)?
6. **Windows FFI.** Commit to the hand-written C ABI, or re-evaluate `uniffi-bindgen-cs` and possibly get one binding generator for both macOS and Windows? Deferred along with the Windows shell — by Phase 8 the UniFFI boundary will exist and be proven, which changes the calculus in favour of trying `uniffi-bindgen-cs` first.

---

---

## 11. Review log — what changed in v0.2

v0.1 was reviewed adversarially against RFC 9000/9001, the libadwaita and GTK release notes, rustls, the Bao spec, and a mirrored `ipmsg.h`. Ten defects were material. Recording them here because the *reasoning* is more useful than the corrected text alone.

| # | v0.1 said | Why it was wrong | Now |
|---|---|---|---|
| 1 | ASCII `"WISP"` magic "cannot collide" with a QUIC header on a shared port | `'W'` = `0x57`, inside RFC 9000's `0x40–0x7F` short-header range, where the low bits are header-protected and pseudorandom. ~1 in 32 short-header packets start with it | Magic `0x2A "WSP"` — Fixed Bit clear, the range RFC 9000 reserves for exactly this |
| 2 | Beacon signature covered the CBOR payload only | `type` and `flags` stayed malleable — replay a `HELLO` as a `BYE` and evict anyone from every roster, signature still valid | Signature covers header + payload, with a `"wisp-beacon-v1\0"` domain-separation prefix |
| 3 | `(boot, seq)` ordered lexicographically, `boot` random | Lexicographic order on a random nonce is meaningless. A peer that restarts and draws a smaller `boot` is blackholed **forever** — roughly half of all restarts | `boot` equality-compared, `seq` ordered within a boot |
| 4 | Lower `id` dials the higher, always | Broke Invisible mode for half of all identity pairs (stops beaconing *and* may not dial ⇒ can never talk), and made QUIC migration a coin flip on key ordering | Anyone dials; low `id` wins simultaneous-open only |
| 5 | "Migration… survives the address change. This alone justifies QUIC" | RFC 9000 §9: only the *client* may migrate. The server-role peer's connection dies exactly like TCP | Stated honestly; **resume** is what actually saves transfers |
| 6 | `ROSTER_RSP` carried "up to 256 signed beacons" in a ≤1200-byte datagram | ~110 KB of payload in a 1200-byte budget, with no reassembly scheme anywhere | Roster exchange moved to the QUIC control stream, where framing already exists |
| 7 | Anchors "cannot fabricate peers — the trust hole closed" | Beacons carried no address, so the anchor controlled identity→address mapping outright | Signed `addrs` field added; trust boundary stated precisely rather than claimed away |
| 8 | `ff02::1` for IPv6 discovery | All-nodes group — every device on the link processes it, MLD snooping can't prune it | RFC 3306 unicast-prefix-based group |
| 9 | `fts5(body, content=messages)` with `mid BLOB PRIMARY KEY` | FTS5 external content joins on **rowid**; a BLOB PK is not a rowid alias, so the index silently desyncs | `msg_rowid INTEGER PRIMARY KEY`, `content_rowid=`, plus the three sync triggers |
| 10 | Phase 8 "Depth" (log viewer, transfer center, palette, annotator) after all three UIs | Four UI screens scheduled *after* the shells were declared done ⇒ built three times, and Phase 4's "feature-complete" gate measured against a surface that kept growing | Core-side depth to Phase 4; the first shell implements the complete screen inventory |

Smaller corrections folded in: BLAKE3 verified streaming needs `bao-tree` with a named block size (~6.25% proof overhead at native 1 KiB leaves vs ~0.39% at 16 KiB) and the `blake3` crate doesn't expose it; cross-peer dedup leaks file possession via content-hash probing, so `have` reporting is scoped per-peer; 0-RTT disabled as replayable under our own threat model; `IPMSG_UTF8OPT` gated on observing `IPMSG_CAPUTF8OPT`; `AdwNavigationSplitView` is two-pane so the three-pane layout needs a nested `AdwOverlaySplitView`; version floors named (GTK 4.16 for CSS variables, libadwaita 1.6 for system accent); macOS has no dynamic type for AppKit apps; macOS Keychain can't hold an Ed25519 `SecKey`; the Markdown renderer must resolve no remote resources or the "nothing leaves the LAN" claim is false; the socket inventory was three and is actually four; and UniFFI is pre-1.0, csbindgen is stale and doesn't cover the callback direction.

Claims that survived review unchanged: the ipmsg command constants, the 1200-byte / 1280-MTU arithmetic, Ed25519 X.509 certs under rustls, UniFFI's Swift support, and `AdwNavigationSplitView`/`AdwBreakpoint` existing in libadwaita 1.4.

---

*Prior art acknowledgement: IP Messenger by H. Shirouzu, in continuous development since 1996. This project reimplements the idea, not the code, and exists for learning.*
