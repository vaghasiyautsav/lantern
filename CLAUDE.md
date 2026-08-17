# Lantern — Agent Notes

Read this first. It is the shared brief for every agent (and human) working
on this project, on any machine. Keep it current: when you finish meaningful
work, update the **Status** section here and, if your session is attached to
the "IP Messanger" Claude project, also update `claude/lantern-build-status.md`
there.

## What this is

Lantern is a **serverless LAN messenger**: encrypted chat + resumable file
transfer between machines on the same network. No server, no accounts, no
cloud. Rust engine, native shells per platform. Private, educational project
— never published. Inspired by IP Messenger (ipmsg.org); reimplements the
idea, not the code.

## Where things live

- **GitHub (canonical):** `vaghasiyautsav/lantern` (private).
- **Mac working copy:** `~/dev/lantern` (push/pull from here).
- **Design doc:** `design/DESIGN.md` — architecture + Wisp protocol spec.
  Every protocol/architecture decision traces here; §11 logs review fixes.
- **Brand:** `brand/Lantern-Brand-Guide.html` + `design/icon-philosophy.md`.
- Claude project docs (cloud sessions): `claude/lantern-design.md`,
  `claude/lantern-build-status.md`.

## Status (last update: 17 Aug 2026)

**Working, tested:** engine — signed-beacon discovery, identity-pinned
QUIC/TLS 1.3 sessions, chunked resumable BLAKE3-verified transfer (survives
kill -9), TOFU trust with safety words, SQLite history, **presence**
(`PeerView.online`, offline after 3 missed heartbeats — DESIGN §4.2). 16
tests, clippy clean. Shells: **native SwiftUI app** (macOS, runs on Utsav's Mac), **native
GTK4 app** (`crates/lantern-gtk`, links core directly), localhost web GUI
(`lantern-gui`, also the SwiftUI shell's local API), CLI. Installers:
`install.sh` (mac+linux), `packaging/make-dmg.sh`, `packaging/make-deb.sh`.
Icon: the **laltain** (final, user-approved — do NOT redesign unasked).

**Next, in order:** (1) GitHub Actions CI → .dmg / .deb / Windows .exe —
closes the Windows gap; (2) GTK app polish (unread badges, verified badge,
drag-drop); (3) Phase 4 core depth (FTS search, durable offline queue);
(4) shell parity screens (transfer center, log viewer, palette — DESIGN §5.3);
(5) ipmsg compat bridge (Phase 7 — start with a packet capture session).

## Conventions (non-negotiable)

- **Commits:** author is Utsav Vaghasiya <admin@upplus.com.au>. **No Claude
  co-author trailers** — owner's explicit requirement for this private repo.
- **Quality bar:** `cargo test` green and `cargo clippy` clean before any
  commit. Parsers never panic on hostile input.
- **Honesty in UX copy:** plain language, limits stated (see brand guide §06).
- **Brand:** icon/wordmark rules in the brand guide are binding. Laltain
  green is reserved for the mark. Product UI uses platform-native type.

## Build & test quickstart

```
cargo test                        # 16 tests
cargo build --release             # engine + CLI + web GUI
bash install.sh                   # platform install (mac: SwiftUI app too)
# two instances on one machine:
lantern-gui --targets 3939,3940
lantern-gui --name Second --data-dir ~/.lantern2 \
    --discovery-port 3940 --targets 3939,3940 --gui-port 4000
```

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
- **SwiftUI: `.fixedSize(horizontal: false, vertical: true)` on a `Text` in
  the `NavigationSplitView` *detail* pane silently blanks the whole
  **sidebar** column** — no crash, no log, the data is fine and the detail
  pane still draws. Cost hours on 17 Aug 2026. Give such text a width
  (`.frame(maxWidth: .infinity, alignment: .leading)`) or a `lineLimit`
  instead. If a pane ever renders empty, bisect the *other* pane.
- `cargo test` at the workspace root fails on macOS because `lantern-gtk`
  needs pkg-config/GTK. Use `cargo test --workspace --exclude lantern-gtk`.
