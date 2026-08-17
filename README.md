# Lantern

A serverless messenger for your local network. No accounts, no cloud, no
server — open it and everyone else running Lantern on the same network
simply appears. Messages and files go machine-to-machine over QUIC/TLS 1.3,
end-to-end, with device identities you can verify by reading eight words
aloud.

Educational project, local use only. Inspired by (not copied from) IP
Messenger (ipmsg.org).

- `CLAUDE.md` — start here: status, crate map, invariants, open defects.
- `design/DESIGN.md` — the design document and why the protocol is shaped
  this way.

## Install (macOS / Linux)

```bash
tar xzf lantern-mac.tar.gz
cd lantern
bash install.sh
```

The installer checks for Apple's Command Line Tools and Rust (installing
via the official rustup if missing), builds the engine **and compiles the
native SwiftUI app**, and creates **Lantern.app** in `/Applications`.
Double-click it — a real Mac window, no browser involved.

To make a shareable disk image: `bash packaging/make-dmg.sh` → `Lantern.dmg`
(drag-to-Applications; on other Macs the unsigned app needs one
right-click → Open the first time).

On Linux, `bash packaging/make-deb.sh` after a release build produces
`lantern_0.1.0_amd64.deb` installing the native GTK4 app (`lantern-gtk`),
the localhost web interface (`lantern-gui`), and the CLI.

Windows: the engine is portable Rust, but building the `.exe` and its
native shell needs a Windows machine (or CI) — deferred per the roadmap.

## Using it

- People on the same network appear in the left sidebar automatically.
- Click a person → type → Enter. ✓✓ means delivered.
- Drop a file anywhere on the window to send it. Files are BLAKE3-verified
  chunk by chunk; interrupted transfers resume from where they stopped.
- **Verify identity** shows their eight safety words — read them aloud to
  each other once; if they match, mark verified. If someone's key ever
  changes, Lantern warns you loudly.
- Received files land in `~/.lantern/downloads`.

## When nobody shows up

Run the diagnostic on **both** machines at the same time:

```bash
lantern-doctor            # or ~/.lantern/bin/lantern-doctor
```

It prints every interface, the exact broadcast addresses it beacons to,
whether each send succeeded, and every datagram that arrives — then tells you
which of the four usual causes you have. Quit Lantern first for the cleanest
read, though the doctor sets `SO_REUSEPORT` and can run alongside it.

Known causes, in the order the doctor checks them:

1. **The two machines are on different subnets.** Compare the interface IPs
   the doctor prints. Broadcast does not cross a router.
2. **Wireless client isolation** on the access point. Common on guest SSIDs and
   on some mesh systems; it silently drops broadcast between clients. Test by
   putting both machines on the same wired segment.
3. **A local firewall dropping inbound UDP 3939.** macOS: System Settings →
   Network → Firewall. The doctor catches this as "not even our own broadcast
   came back".
4. **One side running an old build.** Before the LAN-discovery fix, beacons went
   only to `255.255.255.255`, which the kernel emits on a single interface — on
   a machine with Docker, libvirt or a VPN up, usually the wrong one. The CLI
   also defaulted `--broadcast` to false. Both are fixed; rebuild both sides.

## Same-machine test (two instances)

```bash
# instance 1 (your main one, restarted with a second target port)
~/.lantern/bin/lantern-gui --targets 3939,3940

# instance 2 in another terminal
~/.lantern/bin/lantern-gui --name Second --data-dir ~/.lantern2 \
    --discovery-port 3940 --targets 3939,3940 --gui-port 4000
```

Open http://localhost:3999 and http://localhost:4000 side by side.

## CLI

A headless client ships too: `~/.lantern/bin/lantern --name You --broadcast`
with `/peers`, `/msg`, `/send`, `/verify`, `/trust` commands.

## Uninstall

```bash
rm -rf ~/.lantern ~/.lantern2 ~/Applications/Lantern.app
```

## Status

Working today: signed-beacon discovery, encrypted chat with delivery acks,
chunked resumable file transfer (survives kill -9), TOFU identity trust
with safety words, message history in SQLite, this GUI.

Shells: **native SwiftUI app** on macOS (`apps/macos-native`),
**native GTK4 app** on Linux (`crates/lantern-gtk`, links the core
directly), plus the localhost web interface (`lantern-gui`) — which also
serves as the local API the SwiftUI shell drives — and the CLI.
