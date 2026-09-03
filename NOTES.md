# Lantern — Agent Notes

Read this first. It is the shared brief for anyone working on this project,
on any machine. Keep it current: when you finish meaningful work, update the
**Status** section here.

## What this is

Lantern is a **serverless LAN messenger**: encrypted chat + resumable file
transfer between machines on the same network. No server, no accounts, no
cloud. Rust engine, native shells per platform. Personal, educational
project. Inspired by IP Messenger (ipmsg.org); reimplements the idea, not
the code.

**Personal, educational, not for public use or distribution.** It is not a
product, it is not supported, and it is not meant to be handed to anyone as
something to rely on.

**But the GitHub repo is still public**, and intent is not a setting.
Checked again on 18 Aug 2026, the API reports `"visibility": "public"` and
it clones anonymously. Until that is changed in the repo settings — which
only the owner can do — treat everything committed here as world-readable:
no host names, no LAN addresses, no tokens, no packet captures with real
endpoints. When it is flipped to private, correct this paragraph.

## Where things live

- **GitHub (canonical):** `vaghasiyautsav/lantern` — **public**, see above.
- **Mac working copy:** `~/dev/lantern` (push/pull from here).
- **Design doc:** `design/DESIGN.md` — architecture + Wisp protocol spec.
  Every protocol/architecture decision traces here; §11 logs review fixes.
- **Brand:** `brand/Lantern-Brand-Guide.html` + `design/icon-philosophy.md`.

## Two machines, one repo — read before you touch anything

Lantern is built from **both** working copies: this Mac and the Ubuntu box.
Another agent may be editing the same files right now. On 18 Aug 2026 two
sessions independently built a refresh button, a clear-conversation flow, a
transfer-rate readout and an updater the same afternoon; reconciling that
cost more than the features did. So:

1. **Fetch before you write, not just before you push.**
   `git fetch origin && git status -sb`. If you are behind, fast-forward
   first (`git merge --ff-only origin/main`) so you are writing against
   what's real. Read `git log --oneline HEAD..origin/main` — that list is the
   other machine telling you what it just did.
2. **Fetch again immediately before pushing.** If origin moved while you
   worked, merge it, re-run the gate (`cargo test --workspace --exclude
   lantern-gtk`, `cargo clippy`, plus the shell you touched), and only then
   push. A rejected push means somebody else's work is sitting there — never
   `--force`.
3. **Prefer your own platform's shell.** A Mac session owns
   `apps/macos-native`; a Linux session owns `apps/linux-native`. Neither can
   compile the other's (no GTK4 on macOS, no swiftc on Linux), so a shell
   edited blind is a guess. If both sides did edit one, **the version that
   was actually built and run on that platform wins** — that is how the
   18 Aug collision was settled.
4. **Put shared behaviour in `lantern-core`, once.** Two shells solving the
   same problem separately is the collision above in miniature. Core is also
   the only place both machines can test.
5. **Commit and push in small pieces.** Every hour of unpushed work raises
   the odds of a conflict in exactly the file the other agent is in.
6. **Say what you are mid-way through** in the Status section below before
   you stop. That is the only channel between the two machines.

## Status (last update: 18 Aug 2026)

**Working, tested:** engine — signed-beacon discovery (see the 17 Aug fix
below — cross-machine discovery was broken until then), identity-pinned
QUIC/TLS 1.3 sessions, chunked resumable BLAKE3-verified transfer (survives
kill -9), TOFU trust with safety words, SQLite history, **presence**
(`PeerView.online`, offline after 3 missed heartbeats — DESIGN §4.2),
**history deletion** (per message or whole conversation, local only, WAL
checkpointed), **live transfer speed + ETA** (`CoreEvent::TransferProgress`,
`core::rate`), **update from GitHub** (`core::update` +
`packaging/update.sh`, also the standalone `lantern-update`), **look-again**
(`BeaconType::Ping` via `core.refresh()` / `POST /api/refresh`, behind a
refresh button in every shell). 43 tests workspace-wide, clippy
clean. Shells: **native SwiftUI app** (macOS, runs on Utsav's Mac), **native
GTK4 app** (`apps/linux-native`, links core directly), localhost web GUI
(`lantern-gui`, also the SwiftUI shell's local API), CLI. Installers:
`install.sh` (mac+linux), `packaging/make-dmg.sh`, `packaging/make-deb.sh`.
Icon: the **laltain** (final, user-approved — do NOT redesign unasked).

**Three rules the new features encode** — they cost real debugging, so don't
undo them:

1. **Deleting is local, and says so.** Wisp has no delete-for-everyone
   frame. Every string in every shell says "this machine"; none imply the
   peer's copy goes too. Deletion also checkpoints the WAL — `secure_delete`
   zeroes the freed page, but `lantern.db-wal` keeps the *old* page image, so
   without the checkpoint deleted message text is still greppable on disk.
2. **`send_file` returns as soon as the offer is away**, then streams in a
   spawned task. It used to return after the last byte, which meant no shell
   could learn the xid until the transfer was already over — a progress bar
   was impossible by construction. Failures after the offer arrive as
   `FileSent { ok: false }`, not as a return value.
3. **An update never touches uncommitted work.** A dirty checkout is refused
   with the reason. The updater runs detached (`packaging/update.sh`) because
   it replaces the binaries the app is running from — Linux refuses to write
   a busy executable at all — and `install.sh` therefore installs by
   copy-then-rename and stages `Lantern.app` before swapping it in.

**Linux is now a first-class working copy** (`~/dev/lantern` on the Ubuntu
box), not just a build target — engine, CLI, doctor and the GTK app all
build and run there.

**Next, in order:** (1) confirm the first green CI run and try the Windows
zip on a real Windows machine; (2) Phase 4 core depth (FTS search, durable
offline queue); (3) shell parity screens (transfer center, log viewer,
palette — DESIGN §5.3); (4) ipmsg compat bridge (Phase 7 — start with a
packet capture session); (5) a native Windows shell, if the .exe proves
useful.

**GTK shell, still to do** (18 Aug — two agents converged on this file the
same day; the Linux-built version won, and these are what it doesn't have
yet): **per-message delete** (`core.delete_message`, the macOS ⋯ menu's
equivalent) and the **update panel** (`core::update` + `check_update` /
`start_update` / `take_unseen_result`, which reports how the last update
went — the app that starts one never sees it finish). Everything else is
there: refresh, clear conversation, live transfer rate. A Mac session cannot
compile this crate (no GTK4), so write it on Linux or expect a fixup pass.

## CI — `.github/workflows/ci.yml` (18 Aug 2026)

Five jobs: `check` (test + clippy, the bar above, with `-D warnings` on the
clippy invocation only — putting it in `RUSTFLAGS` would fail the build on a
*dependency's* warning), `linux` → `.deb`, `macos` → `.dmg`, `windows` →
`.zip` (cross-built on Linux by `packaging/make-windows.sh`), and
`windows-msvc`, which builds natively and ships nothing — it proves the
engine compiles with Microsoft's toolchain, a different claim from "we can
package it". Tagging `v*` publishes the three packages to a GitHub release
via the preinstalled `gh`, so no third-party release action and no extra
token.

The matrix also closes a hole this project keeps falling into: a Mac session
cannot compile `apps/linux-native`, a Linux session cannot compile the
SwiftUI shell, so either can ship a shell it never built. CI compiles all
three.

**Windows had never been compiled before this.** There is no native Windows
shell, so that job builds the engine, CLI, doctor and web interface, and not
`lantern-gtk` (GTK4 on Windows needs msys2/vcpkg — separate work). Three
things were fixed to give it a chance:

- `hostname()` now reads `COMPUTERNAME` on Windows. `HOSTNAME` is a Unix
  shell variable, so Windows would otherwise have shipped the exact
  "unknown" bug just fixed for macOS.
- `user_download_dir()` uses `USERPROFILE`; Windows has no `HOME`, so the
  function returned `None` and files would have gone to the data directory.
- `SO_REUSEPORT` was already cfg-gated in `lantern-discovery`, and
  `lantern-discovery` cross-checks clean for `x86_64-pc-windows-gnu`.

`rusqlite` is vendored and builds SQLite from source under MSVC, so no
system SQLite is needed. Whether it links and runs under MSVC is what the
first CI run answers.

**It also builds without CI, and did.** `bash packaging/make-windows.sh`
cross-compiles from this Mac (or the Ubuntu box) with
`x86_64-pc-windows-gnu` + mingw-w64 and stages
`dist/lantern-windows-x64.zip` — the three exes, a `Start-Lantern.cmd` that
launches the engine and opens `localhost:3999`, and a README (CRLF, because
Notepad renders LF-only text as one line) that says there is no native
window yet. 52 s, clean, first attempt, on arm64: `ring` and bundled SQLite
both build under mingw, and the interface is a browser rather than a native
toolkit. So the gnu target links today; MSVC is what CI still has to prove.
The CI job ships the same package rather than three bare .exes — without a
launcher the first thing a person meets is a console that appears to do
nothing.

**Still untested on real Windows.** It compiles and packages; nobody has
double-clicked it.

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
7. Nothing connects off the local link **on its own**. No check at launch, no
   polling, no telemetry, no remote resource fetched by the message renderer.
   The single exception is an update a person explicitly asks for — by
   clicking Update and confirming, or by running `lantern-update` — and even
   then the network step happens in `packaging/update.sh`, not in the app's
   own code. Nothing schedules that. Do not answer "how do I ship updates?"
   with a background checker, and do not add a second network path.

**Amended 18 Aug 2026** (owner asked for an in-app updater; original wording
allowed no update path inside the app at all). What the wording protects is
unchanged: Lantern must never reveal who is running it, from where, or how
often, to anyone the user didn't decide to talk to. A fetch that happens only
when someone clicks a button, in a script the app hands off to, keeps that.
An automatic one wouldn't — so if a future session is tempted by "just check
once a day in the background", the answer is still no.
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

**Linux ↔ macOS confirmed, both directions, 17 Aug 2026.** The Mac came onto
the network later the same evening and the pair worked without further
changes: the Mac's roster picked up the Linux instances, and `lantern-doctor`
on the Linux box heard both Mac instances (the Mac was running two, on 3939
and 3940). The doctor's own verdict — "This machine hears other Lantern
nodes. Discovery works here." The long-standing gate on this fix is closed.

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

### Conversation UI — 18 Aug 2026

The thread is now **two-sided**: our own messages sit right with a stronger
bubble tint, the peer's sit left, each with a coloured initials avatar, the
time, and — on ours — a delivery tick. Both bubble tints derive from the
theme's own foreground via `alpha(currentColor, …)`, so they track light and
dark without a hardcoded palette, and laltain green stays reserved for the
mark.

**Note for whoever does the macOS side:** the SwiftUI shell is *not*
two-sided. `MessageRow` gives every message an avatar and a trailing
`Spacer()`, so incoming and outgoing both sit left, Slack-style. The shells
genuinely differ here now. Bringing the Mac across is a straight port of the
layout below; the avatar colour and initials helpers were written to match
`Lantern.swift` exactly (same six colours, same first-four-character sum) so
one peer looks identical on both.

Also carried over from the Mac shell: avatars in the roster, the peer's
`host · addr` in the conversation header, the verify button reflecting
verified state, the numbered two-column safety-word grid, a proper empty
state, a transient error banner (`flash`), and a desktop notification in
place of the Mac's dock badge — sent only when the window is not already
focused.

**Transfer speed.** `CoreEvent::TransferProgress { xid, outgoing, bytes,
total }` is emitted per chunk in both directions; the file card reads
"12.3 MB of 50.0 MB · 8.4 MB/s". The event deliberately carries **no rate** —
the shell knows when it last painted and the core does not, so the shell
divides. Repaints are throttled to 250 ms, which doubles as the interval the
rate is measured over; the smoothing is a 0.7/0.3 EMA because raw per-chunk
deltas are unreadable. On the receive side progress counts *verified* bytes,
so it can only advance on data that passed its hash.

**Refresh — `BeaconType::Ping` is now implemented.** It had a slot in the
wire format and the DESIGN §4.2 table from the start, and nothing ever sent
one. It matters because a peer only answers a beacon from someone *new* to
it, and the roster is in-memory: re-announcing at a node that already knows
us gets no reply, so "look for devices now" via `announce()` would do
nothing for exactly the peers you can already see. `Core::refresh()` sends a
Ping; a node receiving one answers with its Hello. Exposed as the header-bar
refresh button and the CLI's `/refresh`.

**Ping replies are rate limited to one per two seconds, and must stay that
way.** One broadcast Ping draws a reply from every node on the link, so
without a limit an attacker spends one packet to cost the link N — a cheap
amplifier. Signing does not help, since anyone can mint an identity.

**Clear conversation** (`Store::clear_messages` / `Core::clear_history`)
deletes one peer's messages and returns the count. **The peer row
deliberately survives**: it holds the pinned key and the verified flag, and
dropping it would silently downgrade a verified contact to first-contact
trust, so the next connection would be accepted as new rather than checked
against what we pinned. A store test covers that. The confirmation text says
plainly that this is local only and the peer keeps their copy — a "clear"
that reads like a recall but is not would be the wrong thing to be vague
about. Neither the SwiftUI shell nor the web GUI has this yet.

### Fixed 18 Aug 2026 — every Mac announced itself as "unknown"

`hostname()` tried `$HOSTNAME`, then `/etc/hostname`. **macOS has neither**:
there is no `/etc/hostname`, and `HOSTNAME` is a *shell* variable that a GUI
app launched from Finder never inherits. So it fell through to its last
resort and every Mac told the whole network its name was the literal string
"unknown" — which is what every peer then showed in its roster. Linux only
worked by landing on `/etc/hostname`.

It now asks the kernel via `gethostname(3)`, which is POSIX and answers on
both platforms, keeping the old sources as fallbacks. The short name is used
(`Some-MacBook.local` → `Some-MacBook`), matching what Linux reports for the
same machine.

`lantern-doctor` had **a second copy** of the same guess, so the tool whose
entire job is comparing two machines would have labelled the Mac "unknown"
too. It now calls `lantern_core::hostname()`; there is one implementation.
If you need the host name anywhere else, call that — do not write a third.

**App icon.** A GNOME/Wayland window is matched to its launcher by
`app_id`, and the icon comes from `<app_id>.desktop`. The app announces
`local.lantern.gtk` but `install.sh` wrote `lantern.desktop`, so the running
app got a generic fallback icon while search — which reads the file directly
— showed the right one. The file is now named for the app id, with
`StartupWMClass` covering X11. **If `APP_ID` ever changes, rename the desktop
file with it.**

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

## Received files go to the user's Downloads folder — 17 Aug 2026

They used to land in `<data_dir>/downloads`, i.e. inside `~/.lantern`. On
Linux that is a hidden directory: the file manager does not list it, so the
person who just accepted a file had no way to find it.

`CoreConfig.download_dir: Option<PathBuf>` now names the destination.
**`None` keeps the old in-data-dir behaviour, and that is deliberate** — the
integration tests pass `None` so a test run can never write into the real
user's Downloads folder. Shells pass `lantern_core::user_download_dir()`.

`user_download_dir()` reads the XDG user-dirs setting on Linux rather than
hardcoding `~/Downloads`, because the folder is localised — on a French
desktop it is `Téléchargements`, and writing to a literal `Downloads` would
quietly create a second, wrong folder beside the real one. macOS has no such
indirection. Five unit tests cover the parsing.

Two things that fell out of this, worth not undoing:

- `finalize` used a bare `std::fs::rename`, which **cannot cross a
  filesystem**. The destination is now user-configurable and may be another
  mount, so it falls back to copy-and-delete. Without that, a transfer that
  had downloaded and verified every chunk would fail at the last step.
- No shell hardcodes the destination in its UI any more; each reads `path`
  off `CoreEvent::FileReceived`, which every shell already received. A
  literal would go stale the moment the folder is configurable. `Core::
  download_dir()` is there for anything that needs the path up front.

Invariant 8 is unaffected: `sanitize_filename` strips separators and rejects
`..`, so the received name is still confined to the destination root
whatever that root is.

## Updating — `lantern-update`, never the app

`update.sh` is installed as `~/.lantern/bin/lantern-update`, with the source
checkout baked in at install time. It fetches, fast-forwards, reruns
`install.sh`, and tells you to restart. `--check` reports without changing
anything.

It exists as a separate tool because invariant 7 names update checks
specifically: an app that polls GitHub at launch tells a third party who runs
Lantern, from where, and how often — the one thing the product promises it
does not do. Putting the network access in a command a person runs
deliberately keeps the app binary honest and the README's "no accounts, no
cloud, no server" true. This was a considered decision (17 Aug 2026), not an
oversight.

Two guards worth keeping: it refuses to run with a dirty working tree
(rebasing over someone's edits loses work), and it only fast-forwards (a
merge commit made behind your back is a surprise; diverged history is a
person's problem).

`install.sh` unlinks each binary before copying it. Overwriting a *running*
executable fails with `ETXTBSY`, and since the updater normally runs while
Lantern is open, that was a guaranteed failure on the common path.

### Fixed 18 Aug 2026 — the update check could hang forever

**Symptom:** on macOS, "check for updates" left the sheet spinning and the
app looking wedged.

**Cause:** `update::git()` ran git with the environment it inherited. An
engine launched from Finder or a desktop launcher has **no terminal**, so
when git decided it needed a credential it did not fail — it blocked, on a
username prompt nobody could see or answer. `check_blocking` never returned,
so the localhost API request never returned, so the sheet spun. `ssh` could
stall the same way on a passphrase prompt, and a connection accepted but
never answered had nothing to end it either.

**Fix:** every git invocation now runs with `GIT_TERMINAL_PROMPT=0`,
`SSH_ASKPASS_REQUIRE=never`, `GIT_SSH_COMMAND` carrying `BatchMode=yes` and
`ConnectTimeout=10`, and the HTTP low-speed limits. Each turns a hang into an
error the caller can show. `packaging/update.sh` exports the same set — it
runs detached during an apply, where a silent block would leave the state
file reading "running" forever with nothing to report.

**Keep this in mind for anything that shells out** from a GUI-launched
process: the absence of a terminal turns "prompt the user" into "wait
forever". It is not specific to git.

### Windows packaging — 18 Aug 2026

The v0.1.0 release attached three loose `.exe` files, because the CI job
uploaded binaries rather than a package: a Windows user's first experience
would have been a console that appears to do nothing. CI now runs
`packaging/make-windows.sh`, the same script a person runs by hand, which
cross-builds with mingw and produces `lantern-windows-x64.zip` carrying
`Start-Lantern.cmd` and a README. The native MSVC job stays as
`windows-msvc` and ships nothing — it exists to prove the engine compiles
with Microsoft's toolchain, which is a different claim from "we can package
it".

**The version is hand-maintained in five places** — `Cargo.toml`,
`packaging/make-deb.sh`, `packaging/make-dmg.sh`, and twice in `install.sh`.
Nothing checks that they agree. Bump them together or a `.deb` will claim a
version the binary inside it does not.

### 18 Aug 2026, evening — the ipmsg-gap batch

Measured against IP Messenger's help doc and built better where its design
was worth beating:

- **Resident background mode (Linux).** ipmsg's "popups with no window
  open" is just a resident tray process; same here now. Closing the GTK
  window hides it (`app.hold()` keeps the loop alive), the launcher click
  re-presents it via `connect_activate`, and install.sh writes an autostart
  entry that starts it hidden (`LANTERN_START_HIDDEN=1`). Notifications
  already fired when unfocused, so messages now pop from login onwards.
- **Presence.** `Core::set_presence` announces Away/Dnd + a status line —
  the beacon fields existed from day one, nothing ever set them. GTK: away
  toggle in the header; rows show away/dnd/offline and dim offline (30 s
  roster poll, since nobody beacons "I left"). PeerSeen and PeerView carry
  `state`/`status` now.
- **Unread survives restart** — counts derive from a `read` column, seeded
  at startup (`unread_counts`), cleared by `mark_read` when a conversation
  opens. RAM-only badges died with the process.
- **FTS5 search** across all conversations (`Store::search`, triggers keep
  the index in step). The input is one quoted phrase-prefix — hostile input
  is a term, never FTS syntax. **The delete trigger is load-bearing**: FTS
  keeps its own copy of the text, so without it a secure-deleted message
  stayed greppable in the index. GTK: sidebar search; a hit opens that
  conversation.
- **Transfer consent, better than ipmsg's.** Theirs gates every download on
  a click; ours integrates TOFU: verified peers fetch as before, a
  *stranger's* offer over `auto_accept_limit` (GTK: 25 MB) is held —
  `FileOfferPending` → `accept_file`/`decline_file`. Without a cap, anyone
  on the LAN could fill every disk. `reply_to_offer` is the one shared
  accept path so resume can't drift between auto and consent.

Also fixed here: `make_beacon` locked the presence mutex twice inside one
struct literal — both temporary guards live to the end of the expression,
std's Mutex is not reentrant, so every announce() self-deadlocked. If a test
run ever hangs rather than fails, look for exactly this shape.

**Not carried to macOS/web yet:** consent dialog, search UI, away toggle,
unread seeding (the core APIs are there; shells behave as before with
`auto_accept_limit: None`). ipmsg features still open: groups/multi-send,
sealed messages, inline images, offline send queue (Phase 4), cross-subnet.

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

- **Commits:** author is Utsav Vaghasiya <admin@upplus.com.au>. **No
  co-author trailers of any kind** — owner's explicit requirement.
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

- **Some sandboxed environments cannot push to this repo** — their git/API
  proxy is bound to a configured repo set and returns 403 regardless of
  credentials. Flow: commit there → `git bundle` → a machine that can push.
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

## Pending: the Windows CI job should ship a package, not three .exes

`packaging/make-windows.sh` stages `Start-Lantern.cmd` and a README beside
the binaries; the `windows` job in `ci.yml` uploads the bare .exes. Without a
launcher the first thing a Windows user meets is a console that appears to do
nothing, so the job should build the same package the script does.

Not done from the Mac: that machine's GitHub token has no `workflow` scope,
so it cannot push any change under `.github/workflows/`. **Whoever is on the
Linux box can — do it there**, or grant the Mac's token the scope.
