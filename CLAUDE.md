# Lantern — Agent Notes

Read this first. It is the shared brief for every agent (and human) working
on this project, on any machine. Keep it current: when you finish meaningful
work, update the **Status** section here and, if your session is attached to
the "IP Messanger" Claude project, also update `claude/lantern-build-status.md`
there.

## What this is

Lantern is a **serverless LAN messenger**: encrypted chat + resumable file
transfer between machines on the same network. No server, no accounts, no
cloud. Rust engine, native shells per platform. Personal, educational
project. Inspired by IP Messenger (ipmsg.org); reimplements the idea, not
the code.

**The GitHub repo is public.** This brief used to say "private, never
published"; checked on 17 Aug 2026, the GitHub API reports
`"visibility": "public"`, and it clones anonymously. Treat everything
committed here as world-readable: no host names, no LAN addresses, no
tokens, no captures with real endpoints in them. If it was meant to be
private, change it in the repo settings and correct this paragraph.

## Where things live

- **GitHub (canonical):** `vaghasiyautsav/lantern` — **public**, see above.
- **Mac working copy:** `~/dev/lantern` (push/pull from here).
- **Design doc:** `design/DESIGN.md` — architecture + Wisp protocol spec.
  Every protocol/architecture decision traces here; §11 logs review fixes.
- **Brand:** `brand/Lantern-Brand-Guide.html` + `design/icon-philosophy.md`.
- Claude project docs (cloud sessions): `claude/lantern-design.md`,
  `claude/lantern-build-status.md`.

## Status (last update: 17 Aug 2026)

**Working, tested:** engine — signed-beacon discovery (see the 17 Aug fix
below — cross-machine discovery was broken until then), identity-pinned
QUIC/TLS 1.3 sessions, chunked resumable BLAKE3-verified transfer (survives
kill -9), TOFU trust with safety words, SQLite history. 21 tests, clippy
clean. Shells: **native SwiftUI app** (macOS, runs on Utsav's Mac), **native
GTK4 app** (`apps/linux-native`, links core directly), localhost web GUI
(`lantern-gui`, also the SwiftUI shell's local API), CLI. Installers:
`install.sh` (mac+linux), `packaging/make-dmg.sh`, `packaging/make-deb.sh`.
Icon: the **laltain** (final, user-approved — do NOT redesign unasked).

**Linux is now a first-class working copy** (`~/dev/lantern` on the Ubuntu
box), not just a build target — engine, CLI, doctor and the GTK app all
build and run there.

**Next, in order:** (1) GitHub Actions CI → .dmg / .deb / Windows .exe —
closes the Windows gap; (2) Phase 4 core depth (FTS search, durable offline
queue); (3) shell parity screens (transfer center, log viewer, palette —
DESIGN §5.3); (4) ipmsg compat bridge (Phase 7 — start with a packet
capture session).

*(GTK app polish — unread badges, verified badge, drag-drop — was item 2 and
is done; see below.)*

## Crate map

```
lantern-proto/       wire format: beacon codec, control frames. No I/O.
lantern-crypto/      Ed25519 identity, BLAKE3 fingerprints, safety words.
lantern-discovery/   UDP beacons: send fan-out, receive, replay filter.
  src/net.rs         which addresses a beacon must go to. Read this first
                     for anything shaped like "peers don't show up".
lantern-transport/   QUIC endpoint, identity-pinned mutual TLS.
lantern-store/       SQLite: peers, messages, transfers.
lantern-core/        orchestration. Roster, sessions, transfers, events.
lantern-cli/         `lantern` (headless) + `lantern-doctor` (diagnostic).
lantern-gui/         localhost web interface.
apps/macos-native/   native SwiftUI shell.
apps/linux-native/   native GTK4 shell (crate `lantern-gtk`).
```

Shells live under `apps/` — one folder per platform, named for the platform
rather than the toolkit. `apps/linux-native` is a workspace member like any
crate; only its folder moved, so the package and binary are still
`lantern-gtk`.

**One core, several shells.** If you find yourself putting protocol logic,
file I/O policy, or a trust decision in a shell, it belongs in `lantern-core`.

## Invariants — do not "simplify" these away

Each exists because the alternative was tried or was found exploitable in
review. `design/DESIGN.md` §11 has the full reasoning.

1. No decoder may panic on hostile input. A network-reachable panic is a
   release blocker.
2. The beacon signature covers the **header**, not just the payload —
   otherwise a captured HELLO is replayable as a BYE that evicts the victim
   from every roster on the link, signature still valid.
3. `boot` is compared for **equality**; `seq` is ordered within a boot.
   Ordering `(boot, seq)` lexicographically blackholes a peer forever after
   roughly half of all restarts.
4. The roster keys on the Ed25519 identity, never on IP address.
5. Identity is what the TLS certificate **proves**, not what a frame claims.
   A `Hello` disagreeing with the cert must close the connection.
6. Beacons go to **every interface's directed broadcast**, not only
   `255.255.255.255`. See `lantern-discovery/src/net.rs`.
7. Nothing connects off the local link — no update check, no telemetry, no
   remote resource fetched by the message renderer.
8. Received paths are normalized and confined to the destination root.

## Fixed 17 Aug 2026 — LAN discovery never left the machine

**Symptom:** Linux and macOS both running Lantern on one network, neither saw
the other. Empty roster, no error, no log line.

**Cause:** beacons were sent only to `255.255.255.255` — the *limited*
broadcast, which the kernel emits on exactly **one** interface, whichever the
routing table picks. With `docker0`, `virbr0`, a VPN, or both Wi-Fi and
Ethernet up, that is usually not the LAN. `sendto` returns success either
way, so it failed silently. DESIGN §4.1 already specified per-interface
directed broadcast; the code never did it.

**Fix:** `lantern-discovery::net` enumerates interfaces and beacons to each
one's directed broadcast (`ip | !netmask`) plus `255.255.255.255`, cached
30 s. Socket binds `SO_REUSEADDR`+`SO_REUSEPORT` (without them a second
instance hits EADDRINUSE and a macOS `TIME_WAIT` socket blocks restart for
two minutes — which reads as "discovery stopped working"). Send failures are
now reported instead of discarded. `lantern-cli --broadcast` defaults to
true, matching the GUI shells. New `lantern-doctor` binary.

**Verified:** unit tests for /24, /16, /22, /32; two `lantern-doctor`
instances on one host discover each other.

**Verified 17 Aug 2026 on the Linux box** (Ubuntu 24.04) — and that machine
is the exact shape the bug needed, so the result counts for something.
`lantern-doctor` enumerated three interfaces: loopback, Wi-Fi on a /24, and
**a `docker0` bridge on a /16**.

That docker bridge is the whole point. With it up, the old
limited-broadcast-only code had a live chance of putting every beacon on the
bridge and nothing on the LAN. Beacon targets now come out as the Wi-Fi
subnet's directed broadcast, the docker bridge's, and `255.255.255.255` —
all sends `ok`. Two `lantern-gtk` instances on that host discovered each
other, and a third (`lantern-cli`) delivered messages to one of them over
QUIC.

Run `lantern-doctor` yourself for the live numbers; they are deliberately
not pasted here, because this repo is public.

**Still not verified:** the real Linux ↔ macOS pair — the Mac was not on the
network during this session. The Linux half is now known good, so if the
pair still fails, suspect the Mac or the access point (see the doctor's own
summary list), not the interface enumeration.

**Changing discovery or transport? The failure mode is silence, not an
error.** Tests will not tell you it works — run `lantern-doctor` on two
machines and record the result here.

**Do not trust `lantern-doctor` on a port a live instance already holds.**
It binds `SO_REUSEPORT`, so it joins that port's reuseport group and the
kernel hands each incoming datagram to *one* socket in the group. Running
the doctor on 3939 next to a running app made it report "distinct peers
heard 0" while the app's own roster was filling normally. Give the doctor a
free port, or stop the app first.

## Linux shell — 17 Aug 2026

Moved `crates/lantern-gtk` → `apps/linux-native`, so both native shells sit
under `apps/` named for their platform. It is still an ordinary workspace
member and the package and binary are both still `lantern-gtk` — `install.sh`,
`make-deb.sh` and the `.desktop` entry needed no change.

Roadmap item 2 (GTK polish) is done:

- **Verified badge.** `lantern_core_is_verified()` in the shell was a stub
  returning `false`, so the tick never appeared no matter what the user
  verified. It now calls `Core::is_verified()`, which existed all along.
  The tick means safety words were compared out of band — never merely
  "the transport is encrypted". Do not weaken that.
- **Unread badges.** Per-peer counts in `UiState.unread`, incremented when a
  message or file offer lands for a peer that is not the open conversation,
  cleared on selection.
- **Drag-drop.** A `DropTarget` on the conversation accepts `FileList` and
  `File` and routes to the same `send_path` the paperclip uses, so a dropped
  file and a picked file take one code path. Local paths only — invariant 7.

Two things had to be fixed to make that work:

- The roster is rebuilt wholesale on every `PeerSeen`, which silently
  **dropped the open conversation** every time a beacon arrived. The rebuild
  now records where the selected peer landed and re-selects it, and the
  selection handler no-ops when the peer is already open (otherwise each
  beacon reloaded the same history and reset the scroll).
- `gtk::Application` is single-instance, so a second `lantern-gtk` handed off
  to the first and exited — **two instances on one machine were impossible**.
  Setting `LANTERN_DATA_DIR` now selects `NON_UNIQUE`: an explicit second
  profile gets its own process, while a plain double-click on the launcher
  still just focuses the running window.

## Open defects

1. **Safety words encode 80 bits, not 88.** `safety_words` takes 11 bits per
   position then does `idx % 1024`, discarding the top bit of each group; two
   fingerprints differing only in those 8 bits render identically. DESIGN
   §3.2 is itself inconsistent ("2048-word list … two 1024-word halves" *and*
   "88 bits" cannot both hold). Not a break — 2⁸⁰ is unreachable — but pick
   one: shrink the claim to 80, or grow the list to 4096 words in two
   2048-word halves. Changes every displayed fingerprint, so it is a product
   decision.
2. **Wordlist is a procedural CVCV placeholder**, not screened for
   near-homophones. DESIGN §3.2 requires a curated list before real use.
3. **Discovery cannot cross a subnet.** Broadcast only; mDNS-SD and anchors
   unimplemented. Two VLANs will never see each other.
4. **No IPv6 discovery** — the RFC 3306 group from §2.2 is designed, absent.

## Conventions (non-negotiable)

- **Commits:** author is Utsav Vaghasiya <admin@upplus.com.au>. **No Claude
  co-author trailers** — owner's explicit requirement for this repo.
- **Quality bar:** `cargo test` green and `cargo clippy` clean before any
  commit. Parsers never panic on hostile input.
- **Honesty in UX copy:** plain language, limits stated (see brand guide §06).
- **Brand:** icon/wordmark rules in the brand guide are binding. Laltain
  green is reserved for the mark. Product UI uses platform-native type.

## Build & test quickstart

```
cargo test                        # 21 tests
cargo build --release             # engine + CLI + web GUI
bash install.sh                   # platform install (mac: SwiftUI app too)
lantern-doctor                    # diagnose "nobody shows up" — run on BOTH
# two instances on one machine:
lantern-gui --targets 3939,3940
lantern-gui --name Second --data-dir ~/.lantern2 \
    --discovery-port 3940 --targets 3939,3940 --gui-port 4000

# same, for the GTK app — it reads env vars, not flags. LANTERN_DATA_DIR is
# what allows the second process to exist at all (see the Linux shell notes).
LANTERN_NAME=A LANTERN_TARGETS=3939,3940 lantern-gtk &
LANTERN_NAME=B LANTERN_DATA_DIR=~/.lantern2 LANTERN_PORT=3940 \
    LANTERN_TARGETS=3939,3940 lantern-gtk &
```

Every beacon target is a **port**, and a node only learns about peers whose
beacons reach the port it listens on. A third instance on 3941 stays
invisible until the others list 3941 in `--targets`/`LANTERN_TARGETS` too.

## Environment gotchas (learned the hard way)

- **Cloud Claude sessions cannot push to this repo** — the git/API proxy is
  bound to a configured repo set (403 regardless of credentials). Flow:
  commit in the cloud workspace → `git bundle` → user's machine → push.
- **Desktop-bridge VM** (device_bash) has git but **no network**; `git`
  operations directly on mounted folders hit lock errors (clone in VM $HOME,
  `cp -a` to the mount); `rm` is not permitted on mounts (use `mv` to a
  `_to_delete/` folder).
- Ubuntu 22.04 ships GTK 4.6 — too old for `lantern-gtk` (needs 4.10+);
  install.sh detects and falls back gracefully.
