// Lantern — native macOS app. Pure SwiftUI/AppKit: no web view anywhere.
//
// The Rust engine (~/.lantern/bin/lantern-gui) runs as a child process and
// is driven over its localhost API — the same surface the future UniFFI
// binding will expose in-process. Every visible pixel here is SwiftUI.
//
// Compiled by install.sh with Apple's swiftc (Command Line Tools):
//   swiftc -parse-as-library -O -target <arch>-apple-macos13.0 \
//       -o Lantern Lantern.swift

import SwiftUI
import AppKit
import ImageIO
import Network
import UniformTypeIdentifiers
import UserNotifications

// MARK: - Notifications

/// Banners for messages that arrive while you're looking at something else.
///
/// Only fires when Lantern isn't the active app — a notification for a
/// message already on screen is noise. Locally built copies of Lantern are
/// ad-hoc signed, and macOS refuses notification authorisation for some
/// such builds, so every path falls back to bouncing the Dock icon rather
/// than failing silently.
enum Notifier {
    static func requestAuthorization() {
        guard Bundle.main.bundleIdentifier != nil else { return }
        UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    static func notify(title: String, body: String) {
        guard !NSApp.isActive else { return }
        guard Bundle.main.bundleIdentifier != nil else {
            bounceDock()
            return
        }
        let center = UNUserNotificationCenter.current()
        center.getNotificationSettings { settings in
            guard settings.authorizationStatus == .authorized else {
                bounceDock()
                return
            }
            let content = UNMutableNotificationContent()
            content.title = title
            content.body = body
            content.sound = .default
            center.add(UNNotificationRequest(
                identifier: UUID().uuidString, content: content, trigger: nil))
        }
    }

    private static func bounceDock() {
        DispatchQueue.main.async {
            NSApp.requestUserAttention(.informationalRequest)
        }
    }
}

/// The engine's localhost port. `LANTERN_GUI_PORT` picks another, so a second
/// copy of the app can drive a second engine on one Mac — the same env-var
/// convention the GTK shell uses for its two-instance test (see README).
let GUI_PORT = Int(ProcessInfo.processInfo.environment["LANTERN_GUI_PORT"] ?? "") ?? 3999
let BASE = "http://localhost:\(GUI_PORT)"

/// Where identity, history and the outbox live: `~/.lantern`, or
/// `LANTERN_DATA_DIR` for a second instance.
let DATA_DIR: URL = ProcessInfo.processInfo.environment["LANTERN_DATA_DIR"]
    .map { URL(fileURLWithPath: ($0 as NSString).expandingTildeInPath) }
    ?? FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".lantern")

// MARK: - Local Network permission

/// macOS Local Network privacy, asked for — and read back — by the app.
///
/// All of Lantern's LAN traffic comes from the engine, a helper binary, and
/// macOS never put the question to anyone on its behalf: the permission sat
/// at "not decided", which macOS treats as *no* for anything Lantern starts
/// while still letting it *answer* others. So every Mac saw every other one
/// — beacons arrive, and replies to them pass — and messages went nowhere,
/// because opening a connection to send one is not a reply. It failed as
/// "No route to host", deep in the engine's log.
///
/// A Bonjour browse from the app itself is what makes macOS ask, and its
/// policy-denied error is the one dependable way to read the answer: browse
/// for a throwaway service we also advertise; seeing it means allowed.
/// The permission covers the engine too — macOS charges it to this app.
final class LocalNetworkCheck: @unchecked Sendable {
    private var browser: NWBrowser?
    private var listener: NWListener?
    private let type = "_lantern-check._tcp"
    private let name = "lantern-check-" + UUID().uuidString.prefix(8)
    /// true = allowed, false = denied. Called on the main queue, on change.
    private let onChange: (Bool) -> Void
    private var last: Bool?

    init(onChange: @escaping (Bool) -> Void) { self.onChange = onChange }

    func start() {
        guard browser == nil else { return }
        if let listener = try? NWListener(using: .tcp) {
            listener.service = NWListener.Service(name: name, type: type)
            listener.newConnectionHandler = { $0.cancel() }
            listener.start(queue: .main)
            self.listener = listener
        }
        let browser = NWBrowser(for: .bonjour(type: type, domain: nil), using: .tcp)
        browser.stateUpdateHandler = { [weak self] state in
            // kDNSServiceErr_PolicyDenied: Local Network access is refused
            // (or was never decided, which macOS treats the same).
            if case .waiting(let error) = state, case .dns(let code) = error, code == -65570 {
                self?.report(false)
            }
        }
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            guard let self else { return }
            let seen = results.contains {
                if case .service(let n, _, _, _) = $0.endpoint { return n == self.name }
                return false
            }
            if seen { self.report(true) }
        }
        browser.start(queue: .main)
        self.browser = browser
    }

    private func report(_ allowed: Bool) {
        guard allowed != last else { return }
        last = allowed
        onChange(allowed)
        // Once allowed there is nothing left to watch; until then keep
        // browsing, so flipping the switch in Settings is noticed at once.
        if allowed {
            browser?.cancel()
            listener?.cancel()
        }
    }
}

// MARK: - Wire models

struct Me: Codable {
    var name: String
    var id: String
    var short: String
    var words: [String]
}

struct Peer: Codable, Identifiable, Equatable {
    var id: String
    var name: String
    var host: String
    var addr: String
    var verified: Bool
    var words: [String]
    /// Beacon seen within the last three heartbeats (DESIGN §4.2). A peer
    /// that has gone quiet stays in the roster — history and trust survive —
    /// but must not be drawn as if it were reachable.
    var online: Bool = true
    var sinceBeaconS: UInt64?
    /// "active", "away" or "dnd", as their beacon announces it.
    var state: String?
    /// Their free-text status line, usually empty.
    var status: String?
    /// Group label from their beacon; the sidebar clusters on it.
    var group: String?

    enum CodingKeys: String, CodingKey {
        case id, name, host, addr, verified, words, online, state, status, group
        case sinceBeaconS = "since_beacon_s"
    }

    var away: Bool { state == "away" }
    var dnd: Bool { state == "dnd" }

    /// The line under their name: how they've said they are, else which
    /// machine they're on — or, once they've gone quiet, when they were last
    /// heard from.
    var subtitle: String {
        guard online else { return lastSeenText }
        let mood = dnd ? "Do not disturb" : away ? "Away" : nil
        let line = (status ?? "").trimmingCharacters(in: .whitespaces)
        switch (mood, line.isEmpty) {
        case (let m?, false): return "\(m) · \(line)"
        case (let m?, true): return m
        case (nil, false): return line
        case (nil, true): return host
        }
    }

    /// "last seen 4 min ago" — plain language, no false precision.
    var lastSeenText: String {
        guard let s = sinceBeaconS else { return "not seen recently" }
        if s < 90 { return "last seen just now" }
        let mins = s / 60
        if mins < 60 { return "last seen \(mins) min ago" }
        let hours = mins / 60
        if hours < 24 { return "last seen \(hours) h ago" }
        return "last seen \(hours / 24) d ago"
    }
}

/// What this copy of Lantern was built from, and how the last update went.
struct BuildInfo: Codable {
    var commit: String
    var date: String
    /// The source checkout it was built in — nil for a copy that can't
    /// update itself.
    var repo: String?
    var last_update: UpdateStateInfo?
}

struct UpdateStateInfo: Codable {
    var state: String   // running · ok · failed
    var step: String
    var message: String
    var commit: String
    var started: String
}

/// The answer to "is there a newer Lantern on GitHub?".
struct UpdateInfo: Codable {
    var commit: String
    var date: String
    var branch: String
    var behind: Int
    var commits: [String]
    var dirty: Bool
    /// Why it can't be installed right now — already in plain language.
    var blocked: String?
    var can_update: Bool
    var summary: String
}

/// Can our beacons leave this Mac? `GET /api/link` and the "link" event.
struct LinkInfo: Codable, Equatable {
    var status: String // ok · no-network · blocked
    var detail: String?
}

struct SearchHit: Codable, Identifiable {
    var mid: String
    var peer: String
    var outgoing: Bool
    var ts: UInt64
    var text: String
    /// Arrived sealed and not opened — the hit names it without its words.
    var sealed: Bool?
    var id: String { mid }
}

struct UnreadCount: Codable {
    var peer: String
    var n: Int
}

struct HistoryMessage: Codable {
    var mid: String
    var outgoing: Bool
    var ts: UInt64
    var text: String
    var state: Int
    var reply_to: String?
    /// Arrived sealed and not yet opened. Optional so pre-seal engines parse.
    var sealed: Bool?
}

// MARK: - Chat items

enum ChatKind: Equatable {
    case text(String)
    /// A closed envelope; the words stay on the engine until opened.
    case sealed
    /// `fraction` is how far along the transfer is, 0…1, and nil while the
    /// engine has nothing measured to report — the card then shows an
    /// indeterminate bar rather than a made-up position.
    case file(
        name: String, size: UInt64, status: String,
        done: Bool, failed: Bool, fraction: Double? = nil)
}

struct ChatItem: Identifiable, Equatable {
    var id: String // mid or xid
    var outgoing: Bool
    var ts: UInt64
    var kind: ChatKind
    var delivered: Bool
    /// mid of the message this one answers.
    var replyTo: String?
}

/// What a reply quotes: resolved at render time from the timeline, so a
/// quote always shows the message as it actually is, and a reply to
/// something no longer loaded degrades to a plain message rather than a
/// dangling stub.
struct QuotedMessage: Equatable {
    var author: String
    var excerpt: String
    var outgoing: Bool
}

// MARK: - App state

@MainActor
final class Model: ObservableObject {
    @Published var me: Me?
    @Published var peers: [Peer] = []
    @Published var selected: String? {
        didSet {
            // A reply belongs to the conversation it was started in.
            replyingTo = nil
            if let s = selected {
                unread[s] = 0
                updateBadge()
                Task {
                    await self.loadHistory(s)
                    // Badges are seeded from the engine's store at launch,
                    // so reading a conversation has to tell the store too —
                    // or the count comes back next time Lantern opens.
                    _ = await self.postJSON("/api/mark-read/\(s)", [:])
                }
            }
        }
    }
    @Published var items: [ChatItem] = []
    @Published var unread: [String: Int] = [:]
    @Published var banner: String?
    @Published var draft: String = ""
    /// The message the composer is currently answering, if any.
    @Published var replyingTo: ChatItem?
    /// Composer lock: the next message goes sealed. One seal per press.
    @Published var seal = false
    @Published var showVerify = false
    @Published var engineUp = false
    /// A look-again is in flight; the button spins rather than allowing five
    /// more beacons to be fired by five more clicks.
    @Published var refreshing = false

    // -- updates ----------------------------------------------------------
    @Published var build: BuildInfo?
    @Published var showUpdates = false
    /// nil while a check is in flight.
    @Published var updateInfo: UpdateInfo?
    @Published var updateStarting = false
    /// Set once the updater is away: the app is about to quit itself, and the
    /// sheet says so rather than looking frozen.
    @Published var updateHandedOff = false

    /// Whether discovery can reach the network at all. Drives the one banner
    /// that explains an empty sidebar in terms someone can act on.
    @Published var link = LinkInfo(status: "ok")
    /// macOS Local Network permission: nil until known. `false` means
    /// people may still appear, but nothing sent can reach them.
    @Published var localNetwork: Bool?
    private var localNetworkCheck: LocalNetworkCheck?

    /// What the sidebar banner should say. macOS's own answer wins over the
    /// engine's view: a refusal that still lets beacons in looks healthy
    /// from inside the engine.
    var linkForBanner: LinkInfo {
        localNetwork == false
            ? LinkInfo(status: "blocked", detail: "Local Network access is off for Lantern")
            : link
    }
    /// What our beacons announce: "active", "away" or "dnd".
    @Published var presence = "active"
    /// Sidebar search across every conversation; non-empty swaps the roster
    /// for the matches.
    @Published var search = "" { didSet { runSearch() } }
    @Published var searchHits: [SearchHit] = []
    /// A search hit was opened: scroll to it once that history is loaded.
    @Published var pendingJump: String?
    /// The file on disk behind a transfer card, by xid — the picked file for
    /// one we sent, the saved file for one we received. Lets the card preview
    /// an image and show the file in Finder.
    @Published var filePaths: [String: URL] = [:]
    /// Bumped to put the cursor in the composer.
    @Published var focusComposer = 0

    private var ws: URLSessionWebSocketTask?
    private var fileMeta: [String: (name: String, size: UInt64)] = [:]
    private var searchTask: Task<Void, Never>?

    var currentPeer: Peer? { peers.first { $0.id == selected } }

    /// Peers clustered by the group their beacon announces — named groups
    /// first, the ungrouped last, the same order the GTK roster uses.
    var peerGroups: [(name: String, peers: [Peer])] {
        let names = Set(peers.map { $0.group ?? "" }).sorted {
            $0.isEmpty != $1.isEmpty ? !$0.isEmpty : $0.lowercased() < $1.lowercased()
        }
        return names.map { g in (g, peers.filter { ($0.group ?? "") == g }) }
    }

    var hasGroups: Bool { peers.contains { !($0.group ?? "").isEmpty } }

    func start() {
        let check = LocalNetworkCheck { [weak self] allowed in
            Task { @MainActor in
                guard let self else { return }
                let was = self.localNetwork
                self.localNetwork = allowed
                // Just allowed: ask everyone to check in, so the roster and
                // the queued messages move now rather than a heartbeat later.
                if allowed && was == false {
                    _ = await self.postJSON("/api/refresh", [:])
                    await self.refreshPeers()
                }
            }
        }
        localNetworkCheck = check
        check.start()
        Task {
            // Engine may still be booting; poll until it answers.
            for _ in 0..<80 {
                if let me: Me = await getJSON("/api/me") {
                    self.me = me
                    self.engineUp = true
                    break
                }
                try? await Task.sleep(nanoseconds: 250_000_000)
            }
            await self.refreshPeers()
            await self.loadUnread()
            await self.refreshLink()
            self.connectWS()
            await self.loadBuild()
        }
        Timer.scheduledTimer(withTimeInterval: 15, repeats: true) { _ in
            Task { @MainActor in
                await self.refreshPeers()
                await self.refreshLink()
            }
        }
    }

    // -- networking helpers ---------------------------------------------

    func getJSON<T: Decodable>(_ path: String) async -> T? {
        guard let url = URL(string: BASE + path) else { return nil }
        do {
            let (data, _) = try await URLSession.shared.data(from: url)
            return try JSONDecoder().decode(T.self, from: data)
        } catch {
            return nil
        }
    }

    func postJSON(_ path: String, _ body: [String: Any]) async -> [String: Any]? {
        guard let url = URL(string: BASE + path) else { return nil }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        do {
            let (data, _) = try await URLSession.shared.data(for: req)
            return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        } catch {
            return nil
        }
    }

    /// POST that decodes a typed answer, for endpoints whose reply is a
    /// structure rather than an acknowledgement.
    func postDecoding<T: Decodable>(_ path: String, _ body: [String: Any]) async -> T? {
        guard let url = URL(string: BASE + path) else { return nil }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        do {
            let (data, _) = try await URLSession.shared.data(for: req)
            return try JSONDecoder().decode(T.self, from: data)
        } catch {
            return nil
        }
    }

    /// DELETE with no body. Returns the engine's JSON, or nil if it never
    /// answered — the difference matters, because "nothing was deleted" and
    /// "we don't know" must not read the same to the person who asked.
    func deleteJSON(_ path: String) async -> [String: Any]? {
        guard let url = URL(string: BASE + path) else { return nil }
        var req = URLRequest(url: url)
        req.httpMethod = "DELETE"
        do {
            let (data, _) = try await URLSession.shared.data(for: req)
            return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        } catch {
            return nil
        }
    }

    func refreshPeers() async {
        if let list: [Peer] = await getJSON("/api/peers") {
            // Online first, then alphabetical — the people you can actually
            // reach right now belong at the top.
            peers = list.sorted {
                $0.online == $1.online
                    ? $0.name.lowercased() < $1.name.lowercased()
                    : $0.online
            }
        }
    }

    /// Unread counts from the engine's store, so badges survive a restart
    /// instead of starting from zero every launch.
    func loadUnread() async {
        guard let list: [UnreadCount] = await getJSON("/api/unread") else { return }
        for u in list where u.peer != selected { unread[u.peer] = u.n }
        updateBadge()
    }

    func refreshLink() async {
        if let l: LinkInfo = await getJSON("/api/link") { link = l }
    }

    /// Tell the network how we are. Do Not Disturb also silences this Mac:
    /// messages still arrive and badge, but no sound and no banner.
    func setPresence(_ state: String) {
        presence = state
        Task {
            _ = await postJSON("/api/presence", ["state": state, "away": state == "away"])
        }
    }

    private func runSearch() {
        searchTask?.cancel()
        let q = search.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty else {
            searchHits = []
            return
        }
        searchTask = Task {
            // One query per pause in typing, not one per keystroke.
            try? await Task.sleep(nanoseconds: 150_000_000)
            guard !Task.isCancelled else { return }
            // Alphanumerics only: a bare "+" in a query string decodes as a
            // space, which would quietly search "c  " for "c++".
            let enc = q.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? ""
            guard let hits: [SearchHit] = await getJSON("/api/search?q=\(enc)"),
                  !Task.isCancelled
            else { return }
            searchHits = hits
        }
    }

    /// Open the conversation a hit belongs to, scrolled to the hit.
    func openHit(_ hit: SearchHit) {
        pendingJump = hit.mid
        search = ""
        selected = hit.peer
    }

    func loadHistory(_ peerID: String) async {
        items = []
        if let hist: [HistoryMessage] = await getJSON("/api/history/\(peerID)") {
            items = hist.map {
                ChatItem(
                    id: $0.mid, outgoing: $0.outgoing, ts: $0.ts,
                    kind: ($0.sealed ?? false) && !$0.outgoing
                        ? .sealed : .text($0.text),
                    delivered: $0.state >= 1,
                    replyTo: $0.reply_to)
            }
        }
    }

    // -- actions ----------------------------------------------------------

    /// Turns an engine error into something a person can act on. The engine
    /// speaks in transport terms ("quic: timed out"); only the shell knows
    /// the peer went quiet, which is nearly always the actual reason.
    private func explain(_ raw: String?, peer: Peer?) -> String {
        if let peer, !peer.online {
            return "\(peer.name) is offline — \(peer.lastSeenText). "
                + "Nothing was sent."
        }
        guard let raw else { return "The engine didn't answer." }
        if raw.contains("timed out") {
            return "No answer from \(peer?.name ?? "them") — they may have "
                + "just quit, or a firewall is blocking the connection."
        }
        return raw
    }

    func sendMessage() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, let peer = selected else { return }
        let target = currentPeer
        // Known-offline peers fail here rather than after a ~25 s QUIC
        // timeout. Same outcome, said straight away, and what they typed
        // stays in the box.
        if let target, !target.online {
            flash(explain(nil, peer: target))
            return
        }
        let answering = replyingTo?.id
        let sealNow = seal
        seal = false
        draft = ""
        replyingTo = nil
        Task {
            var body: [String: Any] = ["peer": peer, "text": text]
            if let answering { body["reply_to"] = answering }
            if sealNow { body["sealed"] = true }
            let r = await postJSON("/api/msg", body)
            if let mid = r?["mid"] as? String {
                items.append(ChatItem(
                    id: mid, outgoing: true, ts: nowMS(),
                    kind: .text(text), delivered: false,
                    replyTo: answering))
            } else {
                flash(explain(r?["error"] as? String, peer: target))
                // Don't silently swallow what they typed, or which message
                // they were answering.
                draft = text
                if let answering {
                    replyingTo = items.first { $0.id == answering }
                }
            }
        }
    }

    /// Send files to the open conversation, or to `peerID` — a drop onto
    /// someone in the sidebar opens their conversation and sends there.
    func sendFiles(_ urls: [URL], to peerID: String? = nil) {
        if let peerID, peerID != selected { selected = peerID }
        guard let peer = selected else {
            flash("Pick a person first, then drop the file — or drop it "
                + "straight onto someone in the sidebar.")
            return
        }
        let target = currentPeer
        if let target, !target.online {
            flash(explain(nil, peer: target))
            return
        }
        // The engine streams one file; a folder would fail halfway into the
        // offer with an error about reading a directory. Say it up front.
        let folders = urls.filter { $0.hasDirectoryPath }
        if !folders.isEmpty {
            flash(folders.count == 1
                ? "\(folders[0].lastPathComponent) is a folder — Lantern sends "
                    + "files. Compress it first (right-click → Compress)."
                : "Folders can't be sent yet — compress them first.")
        }
        for url in urls where !url.hasDirectoryPath {
            Task {
                let r = await postJSON(
                    "/api/filepath", ["peer": peer, "path": url.path])
                if let xid = r?["xid"] as? String {
                    let name = (r?["name"] as? String) ?? url.lastPathComponent
                    let attrs = try? FileManager.default
                        .attributesOfItem(atPath: url.path)
                    let size = (attrs?[.size] as? NSNumber)?.uint64Value ?? 0
                    fileMeta[xid] = (name, size)
                    filePaths[xid] = url
                    upsertFile(
                        xid: xid, outgoing: true, name: name, size: size,
                        status: "sending…", done: false, failed: false)
                } else {
                    flash("Couldn't send \(url.lastPathComponent) — "
                        + explain(r?["error"] as? String, peer: target))
                }
            }
        }
    }

    /// A pasted screenshot or picture is a file transfer like any other —
    /// ipmsg's inline images ride its file mechanism too. It's saved under
    /// the data directory's outbox first, because the engine streams from
    /// disk, and named for when it was pasted, the way macOS names
    /// screenshots.
    func sendImage(_ image: NSImage) {
        guard let tiff = image.tiffRepresentation,
              let png = NSBitmapImageRep(data: tiff)?
                .representation(using: .png, properties: [:])
        else {
            flash("Couldn't read that image from the clipboard.")
            return
        }
        let dir = DATA_DIR.appendingPathComponent("outbox")
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd 'at' HH.mm.ss"
        let url = dir.appendingPathComponent("Pasted image \(f.string(from: Date())).png")
        do {
            try FileManager.default.createDirectory(
                at: dir, withIntermediateDirectories: true)
            try png.write(to: url)
        } catch {
            flash("Couldn't save the pasted image: \(error.localizedDescription)")
            return
        }
        sendFiles([url])
    }

    // -- deleting ---------------------------------------------------------
    //
    // Deletion is local, always. Wisp has no "delete for everyone" frame, so
    // the other machine keeps its copy and every string below says so. There
    // is no undo either — the engine zeroes the pages it frees — which is why
    // clearing a whole conversation asks first and a single message doesn't
    // (one message, one deliberate pick, from a menu attached to it).

    /// Remove one message from this Mac. Transfer cards aren't in the message
    /// log at all, so those are dropped from the timeline only, and the file
    /// on disk is left exactly where it is.
    func deleteMessage(_ item: ChatItem) {
        if replyingTo?.id == item.id { replyingTo = nil }
        if case .file = item.kind {
            items.removeAll { $0.id == item.id }
            flash("Removed that transfer from the list. The file itself is "
                + "still on disk — nothing was deleted from either machine.")
            return
        }
        Task {
            let r = await deleteJSON("/api/message/\(item.id)")
            guard r != nil else {
                flash("Couldn't delete that message — the engine didn't answer.")
                return
            }
            // The engine reports deleted:false for a message it never had
            // (an unsent one, say). Either way it should leave the timeline.
            items.removeAll { $0.id == item.id }
        }
    }

    /// Delete every stored message with one peer from this Mac.
    func clearConversation(_ peerID: String) {
        let name = peers.first { $0.id == peerID }?.name ?? "them"
        Task {
            guard let r = await deleteJSON("/api/history/\(peerID)") else {
                flash("Couldn't clear the conversation — the engine didn't answer.")
                return
            }
            if selected == peerID { items = [] }
            unread[peerID] = 0
            updateBadge()
            let n = (r["deleted"] as? NSNumber)?.intValue ?? 0
            flash(n == 0
                ? "Nothing was stored for \(name) — the conversation was "
                    + "already empty here."
                : "Deleted \(n) message\(n == 1 ? "" : "s") from this Mac. "
                    + "\(name) still has their copy.")
        }
    }

    // -- updates ----------------------------------------------------------

    /// Read the build stamp, and report the outcome of an update this app
    /// started before it quit — the only place that result can be shown,
    /// since the app that asked for the update is not the app that sees it
    /// finish.
    func loadBuild() async {
        guard let info: BuildInfo = await getJSON("/api/version") else { return }
        build = info
        guard let last = info.last_update, !last.started.isEmpty else { return }
        // Announce each update once, not on every launch afterwards.
        let seenKey = "lastSeenUpdate"
        guard UserDefaults.standard.string(forKey: seenKey) != last.started
        else { return }
        UserDefaults.standard.set(last.started, forKey: seenKey)
        if last.state == "ok" {
            flash("Updated — now running build \(info.commit). \(last.message)")
        } else if last.state == "failed" {
            flash("The last update didn't finish: \(last.message) "
                + "Your previous Lantern is still what's running. "
                + "Details in ~/.lantern/update.log.")
        }
    }

    func checkForUpdates() {
        updateInfo = nil
        Task {
            // Deliberately not cached: "up to date" is only worth saying if
            // it was just checked.
            guard let info: UpdateInfo = await postDecoding(
                "/api/update/check", [:])
            else {
                flash("Couldn't check for updates — the engine didn't answer.")
                showUpdates = false
                return
            }
            updateInfo = info
        }
    }

    /// Hand off to the detached updater, then get out of its way: it can't
    /// replace the binaries this app is running from while it's running.
    func applyUpdate() {
        updateStarting = true
        Task {
            let r = await postJSON("/api/update/apply", [:])
            updateStarting = false
            if let error = r?["error"] as? String {
                flash("Can't update: \(error)")
                return
            }
            guard r?["started"] as? Bool == true else {
                flash("Couldn't start the update — the engine didn't answer.")
                return
            }
            updateHandedOff = true
            // Long enough to read the sheet, short enough not to feel stuck.
            try? await Task.sleep(nanoseconds: 2_500_000_000)
            NSApp.terminate(nil)
        }
    }

    /// Ask everyone on the link to check in, then reload the roster.
    ///
    /// Discovery already runs on its own; this is for the case where someone
    /// has just opened Lantern on the other machine, or the network moved
    /// under you, and waiting out a heartbeat feels like being stuck.
    func refreshNow() {
        guard !refreshing else { return }
        refreshing = true
        Task {
            // The engine waits for beacon replies before answering, so by the
            // time this returns the roster is the refreshed one.
            let r = await postJSON("/api/refresh", [:])
            await refreshPeers()
            refreshing = false
            guard let r, r["asked"] as? Bool == true else {
                flash("Couldn't reach the engine to look again.")
                return
            }
            let online = (r["online"] as? NSNumber)?.intValue ?? peers.filter(\.online).count
            if online == 0 {
                flash("Nobody answered. Check the other machine has Lantern "
                    + "open and is on this same network.")
            }
        }
    }

    func markVerified() {
        guard let peer = selected else { return }
        Task {
            _ = await postJSON("/api/trust", ["peer": peer])
            await refreshPeers()
            showVerify = false
        }
    }

    func flash(_ text: String) {
        banner = text
        Task {
            try? await Task.sleep(nanoseconds: 9_000_000_000)
            if banner == text { banner = nil }
        }
    }

    private func updateBadge() {
        let total = unread.values.reduce(0, +)
        NSApp.dockTile.badgeLabel = total > 0 ? String(total) : ""
    }

    private func upsertFile(
        xid: String, outgoing: Bool, name: String, size: UInt64,
        status: String, done: Bool, failed: Bool, fraction: Double? = nil
    ) {
        let kind = ChatKind.file(
            name: name, size: size, status: status, done: done,
            failed: failed, fraction: fraction)
        if let i = items.firstIndex(where: { $0.id == xid }) {
            items[i].kind = kind
        } else {
            items.append(ChatItem(
                id: xid, outgoing: outgoing, ts: nowMS(),
                kind: kind, delivered: done))
        }
    }

    /// Break a seal: the engine reveals it and tells the sender; the
    /// envelope becomes the text when history reloads.
    func openSealed(_ mid: String) {
        guard let peer = selected else { return }
        Task {
            _ = await postJSON("/api/open/\(mid)", [:])
            await loadHistory(peer)
        }
    }

    // -- live events ------------------------------------------------------

    func connectWS() {
        guard let url = URL(string: "ws://localhost:\(GUI_PORT)/ws") else { return }
        let task = URLSession.shared.webSocketTask(with: url)
        ws = task
        task.resume()
        listen(task)
    }

    nonisolated private func listen(_ task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let message):
                if case .string(let s) = message {
                    Task { @MainActor in self.handleEvent(s) }
                }
                self.listen(task)
            case .failure:
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 1_500_000_000)
                    self.connectWS()
                }
            }
        }
    }

    private func handleEvent(_ raw: String) {
        guard let data = raw.data(using: .utf8),
              let ev = (try? JSONSerialization.jsonObject(with: data))
                as? [String: Any],
              let type = ev["type"] as? String
        else { return }

        switch type {
        case "peer":
            Task { await refreshPeers() }

        case "link":
            link = LinkInfo(
                status: ev["status"] as? String ?? "ok",
                detail: ev["detail"] as? String)

        case "msg":
            let peer = ev["peer"] as? String ?? ""
            let text = ev["text"] as? String ?? ""
            let mid = ev["mid"] as? String ?? UUID().uuidString
            let ts = (ev["ts"] as? NSNumber)?.uint64Value ?? nowMS()
            let who = (ev["peer_name"] as? String)
                ?? peers.first { $0.id == peer }?.name ?? "Someone"
            let sealed = ev["sealed"] as? Bool ?? false
            if peer == selected {
                items.append(ChatItem(
                    id: mid, outgoing: false, ts: ts,
                    kind: sealed ? .sealed : .text(text), delivered: true,
                    replyTo: ev["reply_to"] as? String))
                // On screen is read — in the store too, or it reappears as
                // unread at the next launch.
                Task { _ = await postJSON("/api/mark-read/\(peer)", [:]) }
            } else {
                unread[peer, default: 0] += 1
                updateBadge()
            }
            guard presence != "dnd" else { break }
            NSSound(named: "Pop")?.play()
            // A sealed message's whole point is that its words are not
            // sitting in a notification.
            Notifier.notify(
                title: who, body: sealed ? "sent a sealed message" : text)

        case "delivered", "opened":
            if let mid = ev["mid"] as? String,
               let i = items.firstIndex(where: { $0.id == mid }) {
                items[i].delivered = true
            }

        case "file-offer-pending":
            // A stranger's file over the engine's cap: nothing has been
            // fetched. NSAlert keeps this working in every layout without
            // threading bindings through the views.
            let who = ev["peer_name"] as? String ?? "Someone"
            let name = ev["name"] as? String ?? "a file"
            let size = (ev["size"] as? NSNumber)?.uint64Value ?? 0
            let xid = ev["xid"] as? String ?? ""
            let alert = NSAlert()
            alert.messageText = "\(who) wants to send \"\(name)\" (\(fmtSize(size)))"
            alert.informativeText = "They are not verified. Accept only if "
                + "you expected this — nothing downloads until you say so."
            alert.addButton(withTitle: "Decline")
            alert.addButton(withTitle: "Accept")
            let accept = alert.runModal() == .alertSecondButtonReturn
            Task {
                _ = await postJSON(
                    "/api/xfer/\(xid)/\(accept ? "accept" : "decline")", [:])
            }

        case "file-offered":
            let peer = ev["peer"] as? String ?? ""
            let xid = ev["xid"] as? String ?? ""
            let name = ev["name"] as? String ?? "file"
            let size = (ev["size"] as? NSNumber)?.uint64Value ?? 0
            fileMeta[xid] = (name, size)
            if peer == selected {
                upsertFile(
                    xid: xid, outgoing: false, name: name, size: size,
                    status: "receiving…", done: false, failed: false)
            } else {
                unread[peer, default: 0] += 1
                updateBadge()
            }
            guard presence != "dnd" else { break }
            Notifier.notify(
                title: (ev["peer_name"] as? String) ?? "Incoming file",
                body: "Sending you \(name) · \(fmtSize(size))")

        case "file-received":
            let xid = ev["xid"] as? String ?? ""
            let name = ev["name"] as? String ?? "file"
            let size = (ev["size"] as? NSNumber)?.uint64Value ?? 0
            // Report the folder the core actually wrote to — the destination
            // follows the user's Downloads folder now, so a literal here
            // would go stale.
            let path = ev["path"] as? String ?? ""
            let folder = path.isEmpty
                ? "the download folder"
                : (path as NSString).deletingLastPathComponent
                    .replacingOccurrences(of: NSHomeDirectory(), with: "~")
            if !path.isEmpty { filePaths[xid] = URL(fileURLWithPath: path) }
            upsertFile(
                xid: xid, outgoing: false, name: name, size: size,
                status: "saved to " + folder, done: true, failed: false)
            if presence != "dnd" { NSSound(named: "Glass")?.play() }

        case "progress":
            guard let xid = ev["xid"] as? String, let meta = fileMeta[xid],
                  let done = (ev["bytes"] as? NSNumber)?.uint64Value,
                  let total = (ev["total"] as? NSNumber)?.uint64Value,
                  total > 0
            else { break }
            let outgoing = ev["outgoing"] as? Bool ?? true
            // A finished transfer keeps its own wording (delivered & verified,
            // saved to…), so don't let a late progress event overwrite it.
            if case .file(_, _, _, true, _, _)? = items
                .first(where: { $0.id == xid })?.kind { break }
            upsertFile(
                xid: xid, outgoing: outgoing, name: meta.name, size: meta.size,
                status: transferLine(
                    done: done, total: total,
                    bps: (ev["bps"] as? NSNumber)?.uint64Value,
                    etaS: (ev["eta_s"] as? NSNumber)?.uint64Value),
                done: false, failed: false,
                fraction: min(1, Double(done) / Double(total)))

        case "chunks-sent":
            if let xid = ev["xid"] as? String, let meta = fileMeta[xid] {
                let sent = (ev["sent"] as? NSNumber)?.intValue ?? 0
                let total = (ev["total"] as? NSNumber)?.intValue ?? 0
                let status = sent < total
                    ? "resume — only \(sent) of \(total) chunks needed"
                    : "streaming \(total) chunks…"
                upsertFile(
                    xid: xid, outgoing: true, name: meta.name, size: meta.size,
                    status: status, done: false, failed: false)
            }

        case "file-sent":
            if let xid = ev["xid"] as? String {
                let ok = ev["ok"] as? Bool ?? false
                let meta = fileMeta[xid] ?? ("file", 0)
                let err = ev["err"] as? String
                upsertFile(
                    xid: xid, outgoing: true, name: meta.name, size: meta.size,
                    status: ok ? "delivered & verified" : (err ?? "refused"),
                    done: ok, failed: !ok)
            }

        case "trust-warning":
            flash("⚠ " + ((ev["detail"] as? String) ?? "identity warning"))

        default:
            break
        }
    }
}

func nowMS() -> UInt64 {
    UInt64(Date().timeIntervalSince1970 * 1000)
}

/// Collects drop results across item-provider callbacks, which arrive on
/// arbitrary queues.
private final class URLCollector: @unchecked Sendable {
    private let lock = NSLock()
    private var urls: [URL] = []
    func add(_ url: URL) { lock.lock(); urls.append(url); lock.unlock() }
    var all: [URL] { lock.lock(); defer { lock.unlock() }; return urls }
}

/// The file URLs in a drop, delivered once, on the main queue — so a drop of
/// five files is one send with at most one complaint, not five.
func loadFileURLs(_ providers: [NSItemProvider], _ done: @escaping ([URL]) -> Void) {
    let group = DispatchGroup()
    let found = URLCollector()
    for p in providers where p.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
        group.enter()
        _ = p.loadObject(ofClass: URL.self) { url, _ in
            if let url, url.isFileURL { found.add(url) }
            group.leave()
        }
    }
    group.notify(queue: .main) { done(found.all) }
}

let IMAGE_EXTENSIONS: Set<String> = ["png", "jpg", "jpeg", "gif", "heic", "webp", "tiff", "tif", "bmp"]

func isImageFile(_ url: URL) -> Bool {
    IMAGE_EXTENSIONS.contains(url.pathExtension.lowercased())
}

/// Downsampled previews for image transfers. Decoding a 12-megapixel photo
/// on every redraw of the timeline would make scrolling stutter, so each is
/// decoded once, small, and kept.
enum Thumbnails {
    private static let cache = NSCache<NSURL, NSImage>()

    static func image(for url: URL, maxPixels: Int = 720) -> NSImage? {
        if let hit = cache.object(forKey: url as NSURL) { return hit }
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
              let cg = CGImageSourceCreateThumbnailAtIndex(src, 0, [
                  kCGImageSourceCreateThumbnailFromImageAlways: true,
                  kCGImageSourceCreateThumbnailWithTransform: true,
                  kCGImageSourceThumbnailMaxPixelSize: maxPixels,
              ] as CFDictionary)
        else { return nil }
        // Pixels to points at 2x, so a preview is sharp on Retina.
        let img = NSImage(cgImage: cg, size: NSSize(
            width: CGFloat(cg.width) / 2, height: CGFloat(cg.height) / 2))
        cache.setObject(img, forKey: url as NSURL)
        return img
    }
}

func fmtSize(_ b: UInt64) -> String {
    let units = ["B", "KB", "MB", "GB"]
    var v = Double(b)
    var u = 0
    while v >= 1024 && u < units.count - 1 { v /= 1024; u += 1 }
    return u == 0 ? "\(b) B" : String(format: "%.1f %@", v, units[u])
}

/// "12.0 MB of 40.0 MB · 8.4 MB/s · 3 s left" — the speed and the time left
/// are simply left out when the engine hasn't measured them yet, rather than
/// shown as a zero or a guess.
func transferLine(
    done: UInt64, total: UInt64, bps: UInt64?, etaS: UInt64?
) -> String {
    var parts = ["\(fmtSize(done)) of \(fmtSize(total))"]
    if let bps, bps > 0 { parts.append("\(fmtSize(bps))/s") }
    if let etaS, etaS > 0 { parts.append("\(fmtTimeLeft(etaS)) left") }
    return parts.joined(separator: " · ")
}

/// Seconds are only useful for about a minute; past that people want minutes.
func fmtTimeLeft(_ seconds: UInt64) -> String {
    if seconds < 60 { return "\(seconds) s" }
    let mins = seconds / 60
    if mins < 60 { return "\(mins) min" }
    return "\(mins / 60) h \(mins % 60) min"
}

func dateOf(_ ts: UInt64) -> Date {
    Date(timeIntervalSince1970: Double(ts) / 1000)
}

func fmtTime(_ ts: UInt64) -> String {
    let f = DateFormatter()
    f.timeStyle = .short
    return f.string(from: dateOf(ts))
}

/// "Today" / "Yesterday" / "12 August" — a date you'd say out loud.
func fmtDay(_ ts: UInt64) -> String {
    let d = dateOf(ts)
    let cal = Calendar.current
    if cal.isDateInToday(d) { return "Today" }
    if cal.isDateInYesterday(d) { return "Yesterday" }
    let f = DateFormatter()
    f.setLocalizedDateFormatFromTemplate(
        cal.isDate(d, equalTo: Date(), toGranularity: .year) ? "dMMMM" : "dMMMMyyyy")
    return f.string(from: d)
}

func avatarColor(_ id: String) -> Color {
    let palette: [Color] = [
        Color(red: 0.42, green: 0.36, blue: 0.91),
        Color(red: 0.06, green: 0.54, blue: 0.45),
        Color(red: 0.76, green: 0.25, blue: 0.05),
        Color(red: 0.49, green: 0.23, blue: 0.93),
        Color(red: 0.01, green: 0.41, blue: 0.63),
        Color(red: 0.75, green: 0.09, blue: 0.36),
    ]
    let n = id.prefix(4).reduce(0) { $0 + Int($1.unicodeScalars.first?.value ?? 0) }
    return palette[n % palette.count]
}

func initials(_ name: String) -> String {
    let parts = name.split(separator: " ")
    let letters = parts.prefix(2).compactMap { $0.first }
    return String(letters).uppercased()
}

// MARK: - Views

struct AvatarView: View {
    let name: String
    let colorKey: String
    var size: CGFloat = 34
    /// nil = draw no presence dot at all (used for your own messages, where
    /// presence is meaningless).
    var online: Bool?
    /// "away" turns the dot amber, "dnd" red; anything else is green.
    var state: String?

    private var dotColor: Color {
        guard online == true else { return Color(NSColor.tertiaryLabelColor) }
        switch state {
        case "dnd": return .red
        case "away": return .orange
        default: return .green
        }
    }

    var body: some View {
        ZStack(alignment: .bottomTrailing) {
            Circle()
                .fill(avatarColor(colorKey).gradient)
                .frame(width: size, height: size)
                .overlay(
                    Text(initials(name))
                        .font(.system(size: size * 0.38, weight: .semibold))
                        .foregroundColor(.white))
                .saturation(online == false ? 0.25 : 1)
                .opacity(online == false ? 0.75 : 1)
            if let online {
                // Presence has a shape as well as a hue — filled for online,
                // hollow ring for offline — so it survives colour blindness
                // and greyscale (DESIGN §5.6).
                Circle()
                    .fill(dotColor)
                    .frame(width: size * 0.3, height: size * 0.3)
                    .overlay(
                        Circle()
                            .fill(Color(NSColor.windowBackgroundColor))
                            .frame(width: size * 0.13, height: size * 0.13)
                            .opacity(online ? 0 : 1))
                    .overlay(Circle().stroke(Color(NSColor.windowBackgroundColor),
                                             lineWidth: 2))
            }
        }
    }
}

struct SidebarRow: View {
    let peer: Peer
    let unread: Int
    /// A file is being dragged over this row: dropping sends it to them.
    var dropTargeted = false

    var body: some View {
        HStack(spacing: 10) {
            AvatarView(name: peer.name, colorKey: peer.id, online: peer.online,
                       state: peer.state)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(peer.name)
                        .fontWeight(.medium)
                        .lineLimit(1)
                        .foregroundColor(peer.online ? .primary : .secondary)
                    if peer.verified {
                        Image(systemName: "checkmark.shield.fill")
                            .font(.system(size: 11))
                            .foregroundColor(.green)
                            .help("Identity verified by safety words")
                    }
                }
                Text(peer.subtitle)
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            if unread > 0 {
                Text(String(unread))
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundColor(.white)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Capsule().fill(Color.accentColor))
            }
        }
        .padding(.vertical, 3)
        .padding(.horizontal, 4)
        .background(
            RoundedRectangle(cornerRadius: 7)
                .strokeBorder(Color.accentColor, lineWidth: dropTargeted ? 2 : 0)
                .background(RoundedRectangle(cornerRadius: 7)
                    .fill(Color.accentColor.opacity(dropTargeted ? 0.12 : 0))))
        .padding(.horizontal, -4)
        .help(peer.online
            ? "\(peer.name) on \(peer.host) · \(peer.addr)\nDrop a file here to send it to them."
            : "\(peer.name) — \(peer.lastSeenText). Messages won't send.")
    }
}

/// One search match in the sidebar: who, when, and the line itself.
struct SearchHitRow: View {
    let hit: SearchHit
    let peerName: String

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 4) {
                Text(hit.outgoing ? "You → \(peerName)" : peerName)
                    .font(.system(size: 11.5, weight: .semibold))
                    .lineLimit(1)
                Spacer(minLength: 4)
                Text(fmtDay(hit.ts))
                    .font(.system(size: 10.5))
                    .foregroundColor(.secondary)
            }
            // A sealed message is found, but not read out: the seal is the
            // point, and a search result is no place to break it.
            Text(hit.sealed == true && !hit.outgoing ? "🔒 Sealed message" : hit.text)
                .font(.system(size: 12))
                .foregroundColor(.secondary)
                .lineLimit(2)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 3)
        .contentShape(Rectangle())
    }
}

/// The one thing that explains an empty sidebar *and* can be fixed from
/// here. macOS asks once whether Lantern may use the local network; until
/// that's answered yes, every datagram to the LAN fails, nobody appears, and
/// nothing else on screen says why.
struct LinkBanner: View {
    let link: LinkInfo

    private static let settingsURL = URL(string:
        "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork")

    var body: some View {
        if link.status == "blocked" || link.status == "no-network" {
            VStack(alignment: .leading, spacing: 6) {
                Label(link.status == "blocked"
                        ? "macOS is blocking Lantern"
                        : "Not connected to a network",
                      systemImage: link.status == "blocked"
                        ? "hand.raised.fill" : "wifi.slash")
                    .font(.system(size: 12, weight: .semibold))
                Text(link.status == "blocked"
                    ? "Lantern isn't allowed to use the local network. People "
                        + "may still show up here, but nothing you send can "
                        + "reach them. Turn Lantern on under Privacy & Security "
                        + "→ Local Network; it reconnects by itself."
                    : "People appear as soon as Wi-Fi or Ethernet is back.")
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                if link.status == "blocked", let url = Self.settingsURL {
                    Button("Open Local Network Settings") {
                        NSWorkspace.shared.open(url)
                    }
                    .controlSize(.small)
                }
            }
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 8)
                .fill(Color.orange.opacity(0.12)))
            .overlay(RoundedRectangle(cornerRadius: 8)
                .stroke(Color.orange.opacity(0.35)))
            .padding(.horizontal, 10)
            .padding(.bottom, 6)
            .help(link.detail.map { "The system said: \($0)" } ?? "")
        }
    }
}

/// Shown over the conversation while a file is dragged across it.
struct DropHint: View {
    let title: String
    let subtitle: String

    var body: some View {
        RoundedRectangle(cornerRadius: 12)
            .strokeBorder(Color.accentColor,
                          style: StrokeStyle(lineWidth: 2.5, dash: [9, 5]))
            .background(RoundedRectangle(cornerRadius: 12)
                .fill(Color.accentColor.opacity(0.07)))
            .overlay(
                VStack(spacing: 7) {
                    Image(systemName: "arrow.down.doc.fill")
                        .font(.system(size: 30))
                        .foregroundColor(.accentColor)
                    Text(title)
                        .font(.system(size: 14, weight: .semibold))
                    Text(subtitle)
                        .font(.system(size: 11.5))
                        .foregroundColor(.secondary)
                        .multilineTextAlignment(.center)
                }
                .padding(.horizontal, 22)
                .padding(.vertical, 16)
                .background(RoundedRectangle(cornerRadius: 14)
                    .fill(.regularMaterial)))
            .padding(8)
            .allowsHitTesting(false)
            .transition(.opacity)
    }
}

/// One message, drawn the way a chat app should: your own on the right in
/// the accent colour, theirs on the left in grey. Side and colour say who
/// spoke, so the timeline carries no repeated names at all.
///
/// Consecutive messages from one sender form a run — tight spacing, and
/// only the last of the run carries the time and delivery state, so a
/// burst of three lines reads as one turn rather than three.
struct MessageRow: View {
    let item: ChatItem
    let peerName: String
    let myName: String
    /// First of a run — earns the gap above it.
    var showsHeader: Bool = true
    /// Last of a run — earns the timestamp beneath it.
    var endsRun: Bool = true
    /// The message this one answers, already resolved against the timeline.
    var quoted: QuotedMessage?
    var onReply: () -> Void = {}
    /// Break the seal on a sealed message.
    var onOpenSealed: () -> Void = {}
    /// Jump to the quoted original.
    var onJumpToQuoted: () -> Void = {}
    /// Delete this message from this Mac.
    var onDelete: () -> Void = {}
    /// The file on disk behind a transfer card, once there is one.
    var filePath: URL?

    /// The reply affordance appears on hover. It can't live in a context
    /// menu alone: selectable text hands right-click to AppKit's own
    /// Look Up / Copy menu, so a right-click on a bubble never reaches ours.
    /// Everything else a message can do lives in the ⋯ menu next to it, for
    /// the same reason — a delete you can only reach by right-click on a
    /// bubble that swallows right-clicks is a delete nobody can find.
    @State private var hovering = false

    /// iMessage's proportions: a bubble never runs the full width, so the
    /// eye always has the ragged edge to track who is speaking.
    private let maxBubble: CGFloat = 420

    private var bubbleColor: Color {
        // .primary at low opacity tracks light and dark automatically —
        // grey on white, lighter grey on black — with no second palette.
        item.outgoing ? Color.accentColor : Color.primary.opacity(0.09)
    }

    private var replyButton: some View {
        Button(action: onReply) {
            Image(systemName: "arrowshape.turn.up.left.fill")
                .font(.system(size: 11))
                .foregroundColor(.secondary)
                .frame(width: 22, height: 22)
                .background(Circle().fill(Color.primary.opacity(0.07)))
        }
        .buttonStyle(.plain)
        .help("Reply to this message")
        .opacity(hovering ? 1 : 0)
        .animation(.easeOut(duration: 0.1), value: hovering)
    }

    /// Copy and Delete, in a menu that opens on a plain left-click so it
    /// works over selectable text.
    private var moreMenu: some View {
        Menu {
            Button("Reply", action: onReply)
            if case .text(let text) = item.kind {
                Button("Copy Text") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(text, forType: .string)
                }
            }
            if let filePath {
                Button("Open") { NSWorkspace.shared.open(filePath) }
                Button("Show in Finder") {
                    NSWorkspace.shared.activateFileViewerSelecting([filePath])
                }
            }
            Divider()
            Button(deleteLabel, role: .destructive, action: onDelete)
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 11, weight: .semibold))
                .foregroundColor(.secondary)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .frame(width: 22, height: 22)
        .background(Circle().fill(Color.primary.opacity(0.07)))
        .help("More — reply, copy, delete")
        .opacity(hovering ? 1 : 0)
        .animation(.easeOut(duration: 0.1), value: hovering)
    }

    /// A transfer card isn't in the message log, so deleting one only takes
    /// it off the list. Say which of the two this is before it's clicked.
    private var deleteLabel: String {
        if case .file = item.kind { return "Remove from List" }
        return "Delete Message (this Mac only)"
    }

    var body: some View {
        HStack(spacing: 6) {
            if item.outgoing {
                Spacer(minLength: 56)
                moreMenu
                replyButton
            }

            VStack(alignment: item.outgoing ? .trailing : .leading, spacing: 3) {
                // The quote sits above its bubble and slightly inset, the
                // way every chat app draws it — so the answer reads as
                // attached to something, before you read a word of it.
                if let quoted {
                    Button(action: onJumpToQuoted) {
                        QuoteStrip(quoted: quoted, compact: true)
                            .frame(maxWidth: maxBubble - 20,
                                   alignment: item.outgoing ? .trailing : .leading)
                    }
                    .buttonStyle(.plain)
                    .help("Jump to the message this answers")
                }

                switch item.kind {
                case .text(let text):
                    // Background before frame, deliberately: the fill must
                    // hug the text. Framed first, every "ok" would paint a
                    // bubble the full 420pt wide.
                    Text(text)
                        .font(.system(size: 13))
                        .foregroundColor(item.outgoing ? .white : .primary)
                        .textSelection(.enabled)
                        .padding(.horizontal, 11)
                        .padding(.vertical, 7)
                        .background(
                            RoundedRectangle(cornerRadius: 16, style: .continuous)
                                .fill(bubbleColor))
                        .frame(maxWidth: maxBubble,
                               alignment: item.outgoing ? .trailing : .leading)

                case .sealed:
                    Button(action: onOpenSealed) {
                        Label("Sealed message — click to open",
                              systemImage: "envelope.fill")
                    }
                    .buttonStyle(.bordered)

                case .file(let name, let size, let status, let done,
                           let failed, let fraction):
                    TransferCard(
                        name: name, size: size, status: status,
                        done: done, failed: failed, outgoing: item.outgoing,
                        fraction: fraction, path: filePath)
                }

                if endsRun {
                    Text(metaLine)
                        .font(.system(size: 10.5))
                        .foregroundColor(.secondary)
                        .padding(.horizontal, 4)
                }
            }
            .contextMenu {
                Button("Reply", action: onReply)
                if case .text(let text) = item.kind {
                    Button("Copy") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(text, forType: .string)
                    }
                }
                if let filePath {
                    Button("Open") { NSWorkspace.shared.open(filePath) }
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([filePath])
                    }
                }
                Divider()
                Button(deleteLabel, role: .destructive, action: onDelete)
            }

            if !item.outgoing {
                replyButton
                moreMenu
                Spacer(minLength: 56)
            }
        }
        .padding(.top, showsHeader ? 8 : 2)
        .onHover { hovering = $0 }
    }

    /// "9:53 pm · Delivered" — the state spelled out, not a tick to decode.
    private var metaLine: String {
        let time = fmtTime(item.ts)
        guard item.outgoing else { return time }
        if case .file = item.kind { return time }
        return item.delivered ? "\(time) · Delivered" : "\(time) · Sending…"
    }
}

/// The quoted original — a coloured rule, the author, and one line of what
/// they said. Used in two places (above a reply bubble, and above the
/// composer while you're writing one) so a reply looks the same before and
/// after you send it.
struct QuoteStrip: View {
    let quoted: QuotedMessage
    var compact = false

    var body: some View {
        HStack(spacing: 7) {
            RoundedRectangle(cornerRadius: 1.5)
                .fill(Color.accentColor)
                .frame(width: 3)
            VStack(alignment: .leading, spacing: 1) {
                Text(quoted.author)
                    .font(.system(size: 10.5, weight: .semibold))
                    .foregroundColor(.accentColor)
                Text(quoted.excerpt)
                    .font(.system(size: 11.5))
                    .foregroundColor(.secondary)
                    .lineLimit(compact ? 1 : 2)
            }
            // Above a bubble the strip hugs its text; above the composer it
            // spans the box it belongs to.
            if !compact { Spacer(minLength: 0) }
        }
        .padding(.leading, 6)
        .padding(.trailing, 8)
        .padding(.vertical, 4)
        .frame(height: compact ? 34 : 40)
        .background(
            RoundedRectangle(cornerRadius: 7, style: .continuous)
                .fill(Color.primary.opacity(0.055)))
    }
}

struct TransferCard: View {
    let name: String
    let size: UInt64
    let status: String
    let done: Bool
    let failed: Bool
    let outgoing: Bool
    /// 0…1 once the engine is reporting bytes moved; nil before that.
    var fraction: Double?
    /// The local file, when there is one: what we picked, or what was saved.
    var path: URL?

    /// Pictures show as pictures — the transfer is the same, only the card
    /// differs, as with the GTK shell's image bubbles.
    private var preview: NSImage? {
        guard let path, isImageFile(path) else { return nil }
        return Thumbnails.image(for: path)
    }

    private var icon: String {
        if done { return "checkmark.circle.fill" }
        if failed { return "exclamationmark.triangle.fill" }
        return outgoing ? "arrow.up.circle" : "arrow.down.circle"
    }

    private var tint: Color {
        if done { return .green }
        if failed { return .red }
        return .accentColor
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            if let preview {
                Image(nsImage: preview)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .frame(maxWidth: 300, maxHeight: 220, alignment: .leading)
                    .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
                    .onTapGesture { if let path { NSWorkspace.shared.open(path) } }
                    .help("Click to open")
            }
            details
        }
        .padding(11)
        .frame(maxWidth: 380, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 10)
                .fill(Color(NSColor.controlBackgroundColor)))
        .overlay(
            RoundedRectangle(cornerRadius: 10)
                .stroke(failed
                    ? Color.red.opacity(0.45)
                    : Color(NSColor.separatorColor)))
    }

    private var details: some View {
        HStack(spacing: 11) {
            Image(systemName: icon)
                .font(.system(size: 22))
                .foregroundColor(tint)
                .frame(width: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(name)
                    .fontWeight(.medium)
                    .font(.system(size: 13))
                    .lineLimit(1)
                    .truncationMode(.middle)
                HStack(spacing: 6) {
                    Text(fmtSize(size))
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                    Text("·")
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                    Text(status)
                        .font(.system(size: 11))
                        .foregroundColor(failed ? .red : .secondary)
                        .lineLimit(2)
                }
                // The bar is determinate only while the engine is actually
                // reporting bytes moved. Before the first measurement it
                // spins instead of parking at 0% — which reads as stuck.
                if !done && !failed {
                    Group {
                        if let fraction {
                            ProgressView(value: fraction)
                        } else {
                            ProgressView()
                        }
                    }
                    .progressViewStyle(.linear)
                    .frame(width: 190)
                    .controlSize(.small)
                }
            }
            Spacer(minLength: 0)
            if let path, done || outgoing {
                Button {
                    NSWorkspace.shared.activateFileViewerSelecting([path])
                } label: {
                    Image(systemName: "folder")
                        .font(.system(size: 12))
                        .foregroundColor(.secondary)
                        .frame(width: 24, height: 24)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Show in Finder")
            }
        }
    }
}

struct DaySeparator: View {
    let ts: UInt64

    var body: some View {
        HStack(spacing: 10) {
            VStack { Divider() }
            Text(fmtDay(ts))
                .font(.system(size: 10.5, weight: .medium))
                .foregroundColor(.secondary)
                .lineLimit(1)
            VStack { Divider() }
        }
        .padding(.vertical, 10)
    }
}

/// Shown instead of a blank pane before the first message. States the one
/// thing worth knowing about this conversation, and nothing else.
struct ConversationStarter: View {
    let peer: Peer

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            AvatarView(name: peer.name, colorKey: peer.id, size: 44,
                       online: peer.online)
            Text("This is the start of your conversation with \(peer.name).")
                .font(.system(size: 13, weight: .medium))
            Text(peer.verified
                ? "Identity verified. Messages and files go straight to "
                    + "\(peer.host) — encrypted end to end, never through a server."
                : "Messages and files go straight to \(peer.host) — encrypted "
                    + "end to end, never through a server. Compare safety words "
                    + "to be sure it's really them.")
                .font(.system(size: 12))
                .foregroundColor(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 14)
        .frame(maxWidth: 460, alignment: .leading)
    }
}

struct VerifySheet: View {
    @EnvironmentObject var model: Model

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Verify \(model.currentPeer?.name ?? "identity")")
                .font(.title3).fontWeight(.semibold)
            Text("Read these words aloud to each other. If they match what "
                + "\(model.currentPeer?.name ?? "they") sees for themselves, "
                + "this connection cannot be an impostor's.")
                .foregroundColor(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let words = model.currentPeer?.words {
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())],
                          alignment: .leading, spacing: 6) {
                    ForEach(Array(words.enumerated()), id: \.offset) { i, w in
                        Text("\(i + 1)  \(w)")
                            .font(.system(.body, design: .monospaced))
                    }
                }
                .padding(12)
                .background(RoundedRectangle(cornerRadius: 8)
                    .fill(Color(NSColor.controlBackgroundColor)))
            }
            HStack {
                Spacer()
                Button("Close") { model.showVerify = false }
                Button("They match — mark verified") { model.markVerified() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(22)
        .frame(width: 420)
    }
}

/// Updates: what's running, what's on GitHub, and one button to take it.
///
/// The whole panel is written to be readable by someone who doesn't know or
/// care that this app builds itself from a git checkout — but it never hides
/// that either, because when an update can't happen the reason is always
/// something about the checkout, and a vague "update failed" would leave
/// nothing to act on.
struct UpdateSheet: View {
    @EnvironmentObject var model: Model

    private var currentLine: String {
        guard let b = model.build else { return "Checking this build…" }
        return "Build \(b.commit) · \(b.date)"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Lantern updates")
                .font(.title3).fontWeight(.semibold)

            Text(currentLine)
                .font(.system(size: 12, design: .monospaced))
                .foregroundColor(.secondary)

            if model.updateHandedOff {
                Label("Lantern will close now, rebuild itself from GitHub, "
                    + "and reopen when it's done. It takes a few minutes; "
                    + "progress is written to ~/.lantern/update.log.",
                      systemImage: "arrow.triangle.2.circlepath")
                    .font(.system(size: 12.5))
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else if let info = model.updateInfo {
                if let blocked = info.blocked {
                    Label(blocked, systemImage: "exclamationmark.triangle.fill")
                        .font(.system(size: 12.5))
                        .foregroundColor(.orange)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else if info.behind == 0 {
                    Label("Up to date — nothing new on \(info.branch).",
                          systemImage: "checkmark.circle.fill")
                        .font(.system(size: 12.5))
                        .foregroundColor(.green)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Text(info.behind == 1
                        ? "1 new commit on \(info.branch):"
                        : "\(info.behind) new commits on \(info.branch):")
                        .font(.system(size: 12.5, weight: .medium))
                    // Newest first, and only what's actually there — no
                    // invented release notes.
                    ScrollView {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(info.commits, id: \.self) { line in
                                Text(line)
                                    .font(.system(size: 11.5, design: .monospaced))
                                    .foregroundColor(.secondary)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                        }
                    }
                    .frame(maxHeight: 132)
                    Text("Updating rebuilds Lantern from source on this Mac, "
                        + "so it takes a few minutes. Your identity, history "
                        + "and verified peers are untouched.")
                        .font(.system(size: 11.5))
                        .foregroundColor(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            } else {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Asking GitHub…").font(.system(size: 12.5))
                }
            }

            HStack {
                if let repo = model.build?.repo {
                    Text(repo)
                        .font(.system(size: 10.5, design: .monospaced))
                        .foregroundColor(Color(NSColor.tertiaryLabelColor))
                        .lineLimit(1)
                        .truncationMode(.head)
                        .help("The source checkout this copy builds from")
                }
                Spacer()
                Button("Close") { model.showUpdates = false }
                    .disabled(model.updateHandedOff)
                if model.updateInfo?.can_update == true && !model.updateHandedOff {
                    Button("Update and Reopen") { model.applyUpdate() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.updateStarting)
                }
            }
        }
        .padding(22)
        .frame(width: 460)
    }
}

/// The one confirmation for clearing a chat, reached from the sidebar and
/// from the conversation header. Kept in a single place so both routes say
/// exactly the same thing about what deletion does and doesn't reach.
struct ClearChatConfirmation: ViewModifier {
    @EnvironmentObject var model: Model
    @Binding var peer: Peer?

    func body(content: Content) -> some View {
        content.confirmationDialog(
            Text("Delete every message with \(peer?.name ?? "them") "
                + "from this Mac?"),
            isPresented: Binding(
                get: { peer != nil },
                set: { if !$0 { peer = nil } }),
            titleVisibility: .visible,
            presenting: peer
        ) { target in
            Button("Delete on This Mac", role: .destructive) {
                model.clearConversation(target.id)
            }
            Button("Cancel", role: .cancel) {}
        } message: { target in
            Text("This can't be undone. \(target.name) keeps their own copy — "
                + "Lantern can only delete what's stored here. Files you've "
                + "already sent or received stay on disk.")
        }
    }
}

extension View {
    func clearChatConfirmation(_ peer: Binding<Peer?>) -> some View {
        modifier(ClearChatConfirmation(peer: peer))
    }
}

// MARK: - Composer

/// The message box. An NSTextView rather than a SwiftUI TextField, for three
/// things a TextField can't be talked out of:
///
/// * A file dropped on it is **sent**. The field editor behind a focused
///   TextField takes file drags itself and types the file's *path* into the
///   message — and the composer is focused whenever a chat is open, so that
///   is exactly where people drop. It read as "dropping files doesn't work".
/// * ⌘V with a screenshot or a copied file sends it, as Ctrl-V does in the
///   GTK shell; text always pastes as plain text.
/// * Return sends and ⇧↩ / ⌥↩ start a new line — except while an input
///   method is composing (Japanese, Chinese, dead-key accents), which needs
///   Return for itself.
final class ComposerTextView: NSTextView {
    var onSubmit: () -> Void = {}
    var onEscape: () -> Void = {}
    var onFiles: ([URL]) -> Void = { _ in }
    var onImage: (NSImage) -> Void = { _ in }
    var onDropTarget: (Bool) -> Void = { _ in }
    var placeholder = "" { didSet { if placeholder != oldValue { needsDisplay = true } } }

    static func fileURLs(_ pb: NSPasteboard) -> [URL] {
        (pb.readObjects(forClasses: [NSURL.self],
                        options: [.urlReadingFileURLsOnly: true]) as? [URL]) ?? []
    }

    override func keyDown(with event: NSEvent) {
        let isReturn = event.keyCode == 36 || event.keyCode == 76
        if isReturn && !hasMarkedText() {
            let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            if mods.contains(.shift) || mods.contains(.option) {
                insertNewlineIgnoringFieldEditor(nil)
            } else if !mods.contains(.command) {
                onSubmit()
            } else {
                super.keyDown(with: event)
            }
            return
        }
        super.keyDown(with: event)
    }

    /// Esc cancels a reply. NSTextView would otherwise open its word
    /// completion popup, which nobody writing a chat message wants.
    override func cancelOperation(_ sender: Any?) { onEscape() }

    override func paste(_ sender: Any?) {
        let pb = NSPasteboard.general
        let urls = Self.fileURLs(pb)
        if !urls.isEmpty {
            onFiles(urls)
            return
        }
        // An image with no text beside it is a screenshot or a copied
        // picture. An image *with* text is nearly always a selection from a
        // web page, and then the person meant the words.
        if !pb.canReadObject(forClasses: [NSString.self], options: nil),
           let image = NSImage(pasteboard: pb) {
            onImage(image)
            return
        }
        pasteAsPlainText(sender)
    }

    // -- drag and drop: files are sent, anything else is text as usual ---

    override var acceptableDragTypes: [NSPasteboard.PasteboardType] {
        super.acceptableDragTypes + [.fileURL]
    }

    private func carriesFiles(_ info: NSDraggingInfo) -> Bool {
        !Self.fileURLs(info.draggingPasteboard).isEmpty
    }

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        guard carriesFiles(sender) else { return super.draggingEntered(sender) }
        onDropTarget(true)
        return .copy
    }

    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
        carriesFiles(sender) ? .copy : super.draggingUpdated(sender)
    }

    override func draggingExited(_ sender: NSDraggingInfo?) {
        onDropTarget(false)
        super.draggingExited(sender)
    }

    override func prepareForDragOperation(_ sender: NSDraggingInfo) -> Bool {
        carriesFiles(sender) ? true : super.prepareForDragOperation(sender)
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        let urls = Self.fileURLs(sender.draggingPasteboard)
        guard !urls.isEmpty else { return super.performDragOperation(sender) }
        onDropTarget(false)
        onFiles(urls)
        return true
    }

    override func concludeDragOperation(_ sender: NSDraggingInfo?) {
        onDropTarget(false)
        super.concludeDragOperation(sender)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard string.isEmpty, !placeholder.isEmpty else { return }
        let x = textContainerInset.width + (textContainer?.lineFragmentPadding ?? 0)
        (placeholder as NSString).draw(
            at: NSPoint(x: x, y: textContainerInset.height),
            withAttributes: [
                .font: font ?? NSFont.systemFont(ofSize: 13),
                .foregroundColor: NSColor.placeholderTextColor,
            ])
    }
}

/// SwiftUI face of [`ComposerTextView`]: grows from one line to six, then
/// scrolls.
struct ComposerField: NSViewRepresentable {
    @Binding var text: String
    @Binding var height: CGFloat
    var placeholder: String
    /// Changing this puts the cursor in the box.
    var focusToken: Int
    var onSubmit: () -> Void
    var onEscape: () -> Void
    var onFiles: ([URL]) -> Void
    var onImage: (NSImage) -> Void
    var onDropTarget: (Bool) -> Void

    static let minHeight: CGFloat = 30
    static let maxHeight: CGFloat = 124

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        // TextKit 1, built by hand: the height is measured from the layout
        // manager, and asking a TextKit 2 view for one silently switches it.
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(containerSize: NSSize(
            width: 0, height: CGFloat.greatestFiniteMagnitude))
        container.widthTracksTextView = true
        layout.addTextContainer(container)

        let tv = ComposerTextView(frame: .zero, textContainer: container)
        tv.isRichText = false
        tv.importsGraphics = false
        tv.allowsUndo = true
        tv.font = .systemFont(ofSize: 13)
        tv.textColor = .labelColor
        tv.insertionPointColor = .controlAccentColor
        tv.drawsBackground = false
        tv.textContainerInset = NSSize(width: 6, height: 7)
        tv.isVerticallyResizable = true
        tv.isHorizontallyResizable = false
        tv.autoresizingMask = [.width]
        tv.minSize = NSSize(width: 0, height: Self.minHeight)
        tv.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude,
                            height: CGFloat.greatestFiniteMagnitude)
        tv.delegate = context.coordinator

        let scroll = NSScrollView()
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.documentView = tv
        context.coordinator.textView = tv
        wire(tv)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let tv = context.coordinator.textView else { return }
        wire(tv)
        tv.placeholder = placeholder
        if tv.string != text {
            tv.string = text
            tv.needsDisplay = true
        }
        context.coordinator.measure()
        if context.coordinator.focusToken != focusToken {
            context.coordinator.focusToken = focusToken
            DispatchQueue.main.async { tv.window?.makeFirstResponder(tv) }
        }
    }

    private func wire(_ tv: ComposerTextView) {
        tv.onSubmit = onSubmit
        tv.onEscape = onEscape
        tv.onFiles = onFiles
        tv.onImage = onImage
        tv.onDropTarget = onDropTarget
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ComposerField
        weak var textView: ComposerTextView?
        var focusToken = Int.min

        init(_ parent: ComposerField) { self.parent = parent }

        func textDidChange(_ notification: Notification) {
            guard let tv = textView else { return }
            parent.text = tv.string
            // The placeholder lives outside the glyphs' dirty rect.
            tv.needsDisplay = true
            measure()
        }

        func measure() {
            guard let tv = textView, let lm = tv.layoutManager,
                  let tc = tv.textContainer else { return }
            lm.ensureLayout(for: tc)
            let used = lm.usedRect(for: tc).height + tv.textContainerInset.height * 2
            let h = min(max(used, ComposerField.minHeight), ComposerField.maxHeight)
            guard abs(h - parent.height) > 0.5 else { return }
            // Not during the view update that asked for the measurement.
            DispatchQueue.main.async { self.parent.height = h }
        }
    }
}

/// Keeps a SwiftUI ScrollView pinned to its end while the person is reading
/// the end, and leaves it alone once they scroll away. Put it in the
/// background of a marker at the very end of the scroll view's content.
///
/// This has to be AppKit, not SwiftUI. On macOS a GeometryReader isn't
/// re-measured when the view only scrolls, so SwiftUI can't tell a scroll
/// from content growing. And `ScrollViewProxy.scrollTo` can land a little
/// short of the end, leaving part of the last message under the composer.
/// The NSScrollView behind every SwiftUI ScrollView knows exactly where
/// the scroll position and the end are.
struct ScrollWatcher: NSViewRepresentable {
    let handle: ScrollEndHandle
    /// Called whenever following may have changed: true while the view is
    /// following the end of the content.
    var onChange: (_ following: Bool) -> Void

    func makeNSView(context: Context) -> WatcherView {
        let v = WatcherView()
        v.onChange = onChange
        handle.view = v
        return v
    }

    func updateNSView(_ v: WatcherView, context: Context) {
        v.onChange = onChange
        handle.view = v
    }

    /// Lives in the content, so its own frame is the position of the end
    /// marker it's the background of.
    final class WatcherView: NSView {
        var onChange: ((Bool) -> Void)?
        /// Scrolled to the end — by the person or by us — and not scrolled
        /// away since. While this holds, anything that changes size (a
        /// message arriving, the composer growing, a preview loading) is
        /// followed down to the new end at once.
        private(set) var following = true
        private weak var watched: NSScrollView?
        private var last: (offset: CGFloat, visible: CGFloat, content: CGFloat)?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            attach()
            // The scroll view may not be assembled around us yet.
            DispatchQueue.main.async { [weak self] in self?.attach() }
        }

        private func attach() {
            guard let sv = enclosingScrollView, sv !== watched else { return }
            let center = NotificationCenter.default
            center.removeObserver(self)
            watched = sv
            last = nil
            let clip = sv.contentView
            clip.postsBoundsChangedNotifications = true
            clip.postsFrameChangedNotifications = true
            center.addObserver(self, selector: #selector(changed),
                               name: NSView.boundsDidChangeNotification, object: clip)
            center.addObserver(self, selector: #selector(changed),
                               name: NSView.frameDidChangeNotification, object: clip)
            if let doc = sv.documentView {
                doc.postsFrameChangedNotifications = true
                center.addObserver(self, selector: #selector(changed),
                                   name: NSView.frameDidChangeNotification, object: doc)
            }
            // The marker moving down as messages arrive.
            postsFrameChangedNotifications = true
            center.addObserver(self, selector: #selector(changed),
                               name: NSView.frameDidChangeNotification, object: self)
            changed()
        }

        /// Puts the end marker at the bottom edge of the visible area.
        func scrollToEnd() {
            following = true
            guard let sv = watched, let doc = sv.documentView else { return }
            let clip = sv.contentView
            let visible = clip.documentVisibleRect
            let marker = convert(bounds, to: doc)
            let y = doc.isFlipped ? marker.maxY - visible.height : marker.minY
            let top = max(0, doc.frame.height - visible.height)
            let target = NSPoint(x: visible.minX, y: min(max(0, y), top))
            if abs(target.y - visible.minY) > 0.5 {
                clip.scroll(to: target)
                sv.reflectScrolledClipView(clip)
            }
            onChange?(following)
        }

        @objc private func changed() {
            guard let sv = watched, let doc = sv.documentView else { return }
            let visible = sv.contentView.documentVisibleRect
            let marker = convert(bounds, to: doc)
            let content = doc.frame.height
            let offset = doc.isFlipped ? visible.minY : content - visible.maxY
            // Points of content below the visible area.
            let distance = doc.isFlipped
                ? marker.maxY - visible.maxY
                : visible.minY - marker.minY
            // A scroll is the position moving while the visible area and the
            // content both keep their size. When the composer grows or
            // shrinks AppKit nudges the position to fit; that isn't the
            // person scrolling away, and mustn't stop the following.
            var scrolled = true
            if let last {
                scrolled = abs(last.offset - offset) > 0.5
                    && abs(last.visible - visible.height) < 0.5
                    && abs(last.content - content) < 0.5
            }
            last = (offset, visible.height, content)
            if scrolled {
                following = distance <= 60
            } else if following && distance > 0.5 {
                scrollToEnd()
                return
            }
            onChange?(following)
        }

        deinit { NotificationCenter.default.removeObserver(self) }
    }
}

/// Lets SwiftUI ask the ScrollWatcher for an exact scroll to the end.
final class ScrollEndHandle {
    weak var view: ScrollWatcher.WatcherView?
    func scrollToEnd() { view?.scrollToEnd() }
}

struct ConversationView: View {
    @EnvironmentObject var model: Model
    /// A file is being dragged over the conversation, or over the composer
    /// (which takes its own drops — see ComposerTextView).
    @State private var dropTargeted = false
    @State private var composerDropTargeted = false
    @State private var composerHeight = ComposerField.minHeight
    /// Briefly ringed after jumping to a quoted original, so the eye lands
    /// on it instead of hunting the scrollback.
    @State private var highlighted: String?
    /// Clearing a conversation can't be undone, so it asks first. Non-nil
    /// while the confirmation is up, and carries who it's about.
    @State private var confirmClear: Peer?
    /// The view is following the end of the conversation: it was scrolled
    /// to the end, by the person or by us, and hasn't been scrolled away
    /// since. An incoming message moves the view only then; someone reading
    /// back through history isn't yanked down, they get the pill instead.
    @State private var following = true
    /// Incoming messages that arrived while scrolled up — the pill's count.
    @State private var unseenBelow = 0
    /// The last message the view has already reacted to. nil while a
    /// conversation is (re)loading, so the list that arrives reads as
    /// "opened", not as one very large new message.
    @State private var shownLast: String?
    /// Reaches the ScrollWatcher, for an exact scroll to the end.
    @State private var scroller = ScrollEndHandle()

    private static let bottomID = "lantern-conversation-bottom"

    private var canSend: Bool {
        !model.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    /// Resolve a reply reference against what's loaded. Returns nil when the
    /// original isn't in the timeline — the reply then reads as an ordinary
    /// message rather than quoting a blank.
    private func quoted(for item: ChatItem, peerName: String) -> QuotedMessage? {
        guard let rid = item.replyTo,
              let original = model.items.first(where: { $0.id == rid })
        else { return nil }
        let excerpt: String
        switch original.kind {
        case .text(let t): excerpt = t
        case .sealed: excerpt = "🔒 sealed message"
        case .file(let name, _, _, _, _, _): excerpt = "📎 \(name)"
        }
        return QuotedMessage(
            author: original.outgoing ? (model.me?.name ?? "You") : peerName,
            excerpt: excerpt,
            outgoing: original.outgoing)
    }

    private func jump(to mid: String, _ proxy: ScrollViewProxy) {
        withAnimation { proxy.scrollTo(mid, anchor: .center) }
        highlighted = mid
        Task {
            try? await Task.sleep(nanoseconds: 1_600_000_000)
            if highlighted == mid { withAnimation { highlighted = nil } }
        }
    }

    /// Bring the end of the conversation fully into view, above the composer.
    ///
    /// In passes, on purpose. A new row is laid out *after* the change that
    /// added it has been handled, so a scroll issued straight away aims at
    /// the old bottom and stops one message short — the message just sent
    /// or received, left hidden behind the composer. So SwiftUI scrolls a
    /// run-loop turn later, and the watcher then puts the end exactly at
    /// the bottom edge. Once that has happened the watcher keeps it there
    /// by itself.
    private func scrollToBottom(_ proxy: ScrollViewProxy, animated: Bool) {
        unseenBelow = 0
        DispatchQueue.main.async {
            if animated {
                withAnimation(.easeOut(duration: 0.2)) {
                    proxy.scrollTo(Self.bottomID, anchor: .bottom)
                }
            } else {
                proxy.scrollTo(Self.bottomID, anchor: .bottom)
                DispatchQueue.main.async { scroller.scrollToEnd() }
            }
        }
        // After any slide has finished.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
            scroller.scrollToEnd()
        }
    }

    /// Called on every change to the timeline: decides whether this one
    /// moves the view, and where to.
    private func itemsChanged(_ proxy: ScrollViewProxy) {
        // History is cleared before it reloads; nothing to aim at until it's
        // back, and what comes back is a conversation being opened.
        guard let last = model.items.last else {
            shownLast = nil
            unseenBelow = 0
            return
        }
        // Opened from a search hit: land on the hit, not the bottom. A hit
        // older than the loaded history just opens the conversation.
        if let target = model.pendingJump {
            model.pendingJump = nil
            if model.items.contains(where: { $0.id == target }) {
                shownLast = last.id
                DispatchQueue.main.async { jump(to: target, proxy) }
                return
            }
        }
        if shownLast == nil {
            // Just opened: start at the latest message, without a slide.
            shownLast = last.id
            scrollToBottom(proxy, animated: false)
        } else if last.id != shownLast {
            shownLast = last.id
            // Your own message always comes into view. Theirs does when you
            // were following the end; otherwise it waits behind the pill.
            // (The watcher's answer, not the state mirrored from it: this
            // runs before SwiftUI has re-rendered.)
            if last.outgoing || (scroller.view?.following ?? true) {
                scrollToBottom(proxy, animated: true)
            } else {
                unseenBelow += 1
            }
        }
        // Same messages with new heights — a delivery tick, a transfer card
        // finishing with its preview — need nothing here: the watcher
        // follows any change of size while the end is being read.
    }

    /// Mirrors the watcher's answer into state, for the pill — touching
    /// state only when it actually flips.
    private func followingChanged(_ now: Bool) {
        if now != following { following = now }
        if now && unseenBelow > 0 { unseenBelow = 0 }
    }

    /// A new run starts on a change of sender, or after a five-minute gap —
    /// long enough that the next line reads as a new thought.
    private func startsRun(_ item: ChatItem, after prev: ChatItem?) -> Bool {
        guard let prev else { return true }
        if prev.outgoing != item.outgoing { return true }
        if case .file = item.kind { return true }
        if case .file = prev.kind { return true }
        return item.ts &- prev.ts > 5 * 60 * 1000
    }

    private func needsDaySeparator(_ item: ChatItem, after prev: ChatItem?) -> Bool {
        guard let prev else { return true }
        let cal = Calendar.current
        return !cal.isDate(dateOf(prev.ts), inSameDayAs: dateOf(item.ts))
    }

    /// How many of the newest messages are built eagerly, below the lazily
    /// built history. See the comment where the two stacks meet.
    private static let eagerTail = 50

    /// One message as the timeline shows it, with the day separator above
    /// it when it starts a new day. `idx` is its place in `model.items`.
    @ViewBuilder
    private func messageRow(_ idx: Int, _ item: ChatItem, peer: Peer,
                            proxy: ScrollViewProxy) -> some View {
        let prev = idx > 0 ? model.items[idx - 1] : nil
        if needsDaySeparator(item, after: prev) {
            DaySeparator(ts: item.ts)
        }
        let next = idx + 1 < model.items.count ? model.items[idx + 1] : nil
        MessageRow(
            item: item,
            peerName: peer.name,
            myName: model.me?.name ?? "Me",
            showsHeader: startsRun(item, after: prev),
            endsRun: next.map { startsRun($0, after: item) } ?? true,
            quoted: quoted(for: item, peerName: peer.name),
            onReply: {
                model.replyingTo = item
                model.focusComposer += 1
            },
            onOpenSealed: {
                model.openSealed(item.id)
            },
            onJumpToQuoted: {
                if let rid = item.replyTo {
                    jump(to: rid, proxy)
                }
            },
            onDelete: { model.deleteMessage(item) },
            filePath: model.filePaths[item.id])
            .id(item.id)
            .background(
                RoundedRectangle(cornerRadius: 10)
                    .fill(Color.accentColor.opacity(
                        highlighted == item.id ? 0.13 : 0)))
    }

    var body: some View {
        if let peer = model.currentPeer {
            VStack(spacing: 0) {
                // Header
                HStack(spacing: 10) {
                    AvatarView(name: peer.name, colorKey: peer.id, size: 30,
                               online: peer.online)
                    VStack(alignment: .leading, spacing: 0) {
                        Text(peer.name).fontWeight(.semibold)
                        Text(peer.online
                            ? "\(peer.host) · \(peer.addr)"
                            : peer.lastSeenText)
                            .font(.system(size: 11))
                            .foregroundColor(.secondary)
                    }
                    Spacer()
                    Button {
                        model.showVerify = true
                    } label: {
                        if peer.verified {
                            Label("Verified", systemImage: "checkmark.shield.fill")
                                .foregroundColor(.green)
                        } else {
                            Label("Verify identity", systemImage: "shield")
                        }
                    }
                    .help(peer.verified
                        ? "Safety words already confirmed"
                        : "Compare safety words to rule out an impostor")

                    Menu {
                        Button("Clear Conversation…", role: .destructive) {
                            confirmClear = peer
                        }
                        .keyboardShortcut(.delete, modifiers: [.command, .shift])
                    } label: {
                        Image(systemName: "ellipsis.circle")
                            .font(.system(size: 14))
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .frame(width: 26, height: 22)
                    .help("Conversation options — clear this chat (⇧⌘⌫)")
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 9)
                .background(.bar)
                Divider()

                if !peer.online {
                    // Say it once, at the top, instead of letting every send
                    // fail into a timeout.
                    HStack(spacing: 7) {
                        Image(systemName: "moon.zzz.fill")
                            .font(.system(size: 11))
                        Text("\(peer.name) is offline — \(peer.lastSeenText). "
                            + "Messages and files can't be delivered until "
                            + "Lantern is running again on that machine.")
                            .font(.system(size: 11.5))
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .foregroundColor(.secondary)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 7)
                    .background(Color(NSColor.controlBackgroundColor))
                    Divider()
                }

                // Messages
                ScrollViewReader { proxy in
                    ScrollView {
                      VStack(spacing: 0) {
                        let indexed = Array(model.items.enumerated())
                        let split = max(0, indexed.count - Self.eagerTail)
                        VStack(alignment: .leading, spacing: 0) {
                            if model.items.isEmpty {
                                ConversationStarter(peer: peer)
                            }
                            // Older history, built as it scrolls into view.
                            LazyVStack(alignment: .leading, spacing: 0) {
                                ForEach(indexed[..<split], id: \.element.id) { idx, item in
                                    messageRow(idx, item, peer: peer, proxy: proxy)
                                }
                            }
                            // The newest messages, built eagerly. A LazyVStack
                            // sizes the scroll view from *estimates* for rows
                            // it hasn't built, and with bubbles of every
                            // height the estimate came up short: the end of
                            // the scrollable area sat up to 120pt above the
                            // real last message, so nothing could scroll it
                            // into view and it stayed behind the composer.
                            // Laid out for real, the end is where it says.
                            // Not everything: 500 messages built eagerly
                            // took 1.5s to open and 0.3s per message sent.
                            VStack(alignment: .leading, spacing: 0) {
                                ForEach(indexed[split...], id: \.element.id) { idx, item in
                                    messageRow(idx, item, peer: peer, proxy: proxy)
                                }
                            }
                        }
                        .padding(14)

                        // The end of the conversation, as something to scroll
                        // to. Below the padding, so the last message lands
                        // with room under it instead of flush against the
                        // composer. It also carries the scroll watcher.
                        Color.clear
                            .frame(height: 1)
                            .id(Self.bottomID)
                            .background(ScrollWatcher(handle: scroller) { now in
                                followingChanged(now)
                            })
                      }
                    }
                    .onChange(of: model.items) { _ in itemsChanged(proxy) }
                    // The composer growing, or the reply strip appearing,
                    // takes height from this view; the watcher sees the
                    // visible area shrink and keeps the end in view.
                    .overlay(alignment: .bottom) {
                        if unseenBelow > 0 && !following {
                            Button {
                                scrollToBottom(proxy, animated: true)
                            } label: {
                                Label(unseenBelow == 1
                                        ? "1 new message"
                                        : "\(unseenBelow) new messages",
                                      systemImage: "arrow.down")
                                    .font(.system(size: 12, weight: .medium))
                                    .foregroundColor(.white)
                                    .padding(.horizontal, 12)
                                    .padding(.vertical, 6)
                                    .background(Capsule().fill(Color.accentColor))
                                    .shadow(color: .black.opacity(0.18), radius: 3, y: 1)
                            }
                            .buttonStyle(.plain)
                            .padding(.bottom, 10)
                            .transition(.opacity)
                            .help("Jump to the latest message")
                        }
                    }
                    .animation(.easeOut(duration: 0.15), value: unseenBelow > 0 && !following)
                }

                Divider()

                // What you're answering, if anything — shown above the box
                // so it's impossible to send a reply without seeing what it
                // attaches to.
                if let target = model.replyingTo,
                   let q = quoted(for: ChatItem(
                        id: "", outgoing: false, ts: 0,
                        kind: .text(""), delivered: false,
                        replyTo: target.id), peerName: peer.name) {
                    HStack(spacing: 8) {
                        QuoteStrip(quoted: q)
                        Button {
                            model.replyingTo = nil
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .font(.system(size: 14))
                                .foregroundColor(.secondary)
                        }
                        .buttonStyle(.plain)
                        .keyboardShortcut(.cancelAction)
                        .help("Cancel reply  (esc)")
                    }
                    .padding(.horizontal, 12)
                    .padding(.top, 9)
                }

                // Composer
                HStack(alignment: .bottom, spacing: 8) {
                    Button {
                        let panel = NSOpenPanel()
                        panel.allowsMultipleSelection = true
                        panel.canChooseDirectories = false
                        panel.message = "Send to \(peer.name) — straight to "
                            + "their machine, encrypted. Nothing leaves the LAN."
                        panel.prompt = "Send"
                        if panel.runModal() == .OK {
                            model.sendFiles(panel.urls)
                        }
                    } label: {
                        Image(systemName: "paperclip")
                            .font(.system(size: 15))
                            .foregroundColor(.secondary)
                            .frame(width: 26, height: 26)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help("Send a file to \(peer.name)")

                    Button {
                        model.seal.toggle()
                    } label: {
                        Image(systemName: model.seal ? "lock.fill" : "lock.open")
                            .font(.system(size: 14))
                            .foregroundColor(model.seal ? .accentColor : .secondary)
                            .frame(width: 26, height: 26)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help("Seal — \(peer.name) sees a closed envelope until "
                        + "they open it, and you see when they do")

                    ComposerField(
                        text: $model.draft,
                        height: $composerHeight,
                        placeholder: "Message \(peer.name)  ·  drop or paste files to send",
                        focusToken: model.focusComposer,
                        onSubmit: { model.sendMessage() },
                        onEscape: { model.replyingTo = nil },
                        onFiles: { model.sendFiles($0) },
                        onImage: { model.sendImage($0) },
                        onDropTarget: { composerDropTargeted = $0 })
                        .frame(height: composerHeight)
                        .padding(.horizontal, 3)
                        .background(
                            RoundedRectangle(cornerRadius: 9)
                                .fill(Color(NSColor.textBackgroundColor)))
                        .overlay(
                            RoundedRectangle(cornerRadius: 9)
                                .stroke(Color(NSColor.separatorColor)))

                    Button { model.sendMessage() } label: {
                        Image(systemName: "arrow.up.circle.fill")
                            .font(.system(size: 22))
                            .foregroundColor(canSend
                                ? .accentColor
                                : Color(NSColor.tertiaryLabelColor))
                    }
                    .buttonStyle(.plain)
                    .keyboardShortcut(.return, modifiers: [.command])
                    .disabled(!canSend)
                    .help("Send  (↩ · new line ⇧↩)")
                }
                .padding(12)
                .background(.bar)
            }
            // The drop target lives here, inside the detail column, and not
            // on the window: NavigationSplitView hosts each column in its own
            // AppKit view, and a drop handler attached outside the split view
            // never saw a drag that was over a column — which is all of them.
            .onDrop(of: [UTType.fileURL], isTargeted: $dropTargeted) { providers in
                loadFileURLs(providers) { model.sendFiles($0) }
                return true
            }
            .overlay {
                if dropTargeted || composerDropTargeted {
                    DropHint(
                        title: "Drop to send to \(peer.name)",
                        subtitle: peer.online
                            ? "Straight to \(peer.host), encrypted. Nothing leaves the LAN."
                            : "\(peer.name) is offline — nothing can be sent right now.")
                }
            }
            .animation(.easeOut(duration: 0.12), value: dropTargeted || composerDropTargeted)
            .onAppear { model.focusComposer += 1 }
            .clearChatConfirmation($confirmClear)
        } else {
            EmptyConversation()
                .onDrop(of: [UTType.fileURL], isTargeted: $dropTargeted) { providers in
                    loadFileURLs(providers) { model.sendFiles($0) }
                    return true
                }
                .overlay {
                    if dropTargeted {
                        DropHint(
                            title: "Pick someone first",
                            subtitle: "Or drop the file straight onto a person in the sidebar.")
                    }
                }
        }
    }
}

/// The detail pane before a conversation is open. When nobody has appeared,
/// it says the few things that are actually worth checking, in the order
/// they turn out to be the answer.
struct EmptyConversation: View {
    @EnvironmentObject var model: Model

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: model.peers.isEmpty
                ? "antenna.radiowaves.left.and.right" : "bubble.left.and.bubble.right")
                .font(.system(size: 38))
                .foregroundColor(.secondary)
                .symbolRenderingMode(.hierarchical)
            Text(model.peers.isEmpty
                ? (model.engineUp ? "Looking for people on this network…" : "Starting Lantern…")
                : "Select someone to start")
                .font(.system(size: 16, weight: .medium))
            if model.peers.isEmpty && model.engineUp {
                VStack(alignment: .leading, spacing: 7) {
                    tip("1.circle", "Lantern is open on the other machine.")
                    tip("2.circle", "Both are on the same network — not a guest Wi-Fi, "
                        + "which usually keeps devices apart.")
                    tip("3.circle", "If this Mac shows a warning in the sidebar, "
                        + "follow it: macOS may be blocking Lantern.")
                }
                .frame(maxWidth: 380, alignment: .leading)
                .padding(.top, 2)
                Button {
                    model.refreshNow()
                } label: {
                    Label("Look Again", systemImage: "arrow.clockwise")
                }
                .disabled(model.refreshing)
                .padding(.top, 4)
            } else {
                Text("Messages and files go straight to their machine.\n"
                    + "No server, no cloud, nothing in between.")
                    .font(.system(size: 12.5))
                    .foregroundColor(.secondary)
                    .multilineTextAlignment(.center)
            }
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func tip(_ icon: String, _ text: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: icon)
                .font(.system(size: 13))
                .foregroundColor(.secondary)
            Text(text)
                .font(.system(size: 12.5))
                .foregroundColor(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct ContentView: View {
    @EnvironmentObject var model: Model
    /// Who the sidebar is asking about clearing, if anyone.
    @State private var confirmClear: Peer?
    /// The row a file is being dragged over, if any.
    @State private var rowDropTarget: String?

    private var onlineCount: Int { model.peers.filter(\.online).count }

    var body: some View {
        NavigationSplitView {
            VStack(alignment: .leading, spacing: 0) {
                // Sidebar header: names what the list is, and holds the one
                // control that belongs to the list rather than a conversation.
                HStack(spacing: 6) {
                    Text(onlineCount > 0
                        ? "On this network · \(onlineCount)"
                        : "On this network")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundColor(.secondary)
                    Spacer(minLength: 0)
                    Button { model.refreshNow() } label: {
                        if model.refreshing {
                            ProgressView().controlSize(.small)
                        } else {
                            Image(systemName: "arrow.clockwise")
                                .font(.system(size: 11, weight: .semibold))
                        }
                    }
                    .buttonStyle(.plain)
                    .foregroundColor(.secondary)
                    .frame(width: 18, height: 18)
                    .contentShape(Rectangle())
                    .keyboardShortcut("r", modifiers: .command)
                    .disabled(model.refreshing || !model.engineUp)
                    .help("Look for people again now  (⌘R) — Lantern also "
                        + "checks on its own, and again whenever the Mac wakes "
                        + "or changes network")
                }
                .padding(.horizontal, 12)
                .padding(.top, 8)
                .padding(.bottom, 4)

                LinkBanner(link: model.linkForBanner)

                if !model.search.isEmpty {
                    searchResults
                } else {
                    roster
                }

                Divider()
                if let me = model.me { footer(me) }
            }
            .searchable(text: $model.search, placement: .sidebar,
                        prompt: "Search messages")
            .navigationSplitViewColumnWidth(min: 230, ideal: 260)
        } detail: {
            ConversationView()
        }
        .frame(minWidth: 760, minHeight: 500)
        .sheet(isPresented: $model.showVerify) { VerifySheet() }
        .sheet(isPresented: $model.showUpdates) { UpdateSheet() }
        .overlay(alignment: .top) {
            if let banner = model.banner {
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: "exclamationmark.circle.fill")
                        .font(.system(size: 12))
                    Text(banner)
                        .font(.system(size: 12.5))
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Button {
                        withAnimation { model.banner = nil }
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 10, weight: .semibold))
                    }
                    .buttonStyle(.plain)
                }
                .foregroundColor(.white)
                .padding(.horizontal, 14)
                .padding(.vertical, 9)
                .frame(maxWidth: 460)
                .background(
                    RoundedRectangle(cornerRadius: 10)
                        .fill(Color.orange.opacity(0.95))
                        .shadow(radius: 8, y: 2))
                .padding(.top, 10)
                .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
    }

    // Right-click a person to clear that chat without opening it first —
    // the point of deleting is usually to be rid of a conversation, not to
    // go and read it again. Drop a file on a person to send it to them.
    @ViewBuilder private var roster: some View {
        List(selection: $model.selected) {
            if model.hasGroups {
                ForEach(model.peerGroups, id: \.name) { group in
                    Section(group.name.isEmpty ? "Everyone else" : group.name) {
                        ForEach(group.peers) { peer in row(peer) }
                    }
                }
            } else {
                ForEach(model.peers) { peer in row(peer) }
            }
        }
        .listStyle(.sidebar)
        .clearChatConfirmation($confirmClear)
        if model.peers.isEmpty {
            // Short here on purpose — the detail pane already explains what
            // to do; repeating it twice reads as noise.
            Text(model.engineUp ? "No one yet." : "Starting the engine…")
                .font(.system(size: 11.5))
                .foregroundColor(.secondary)
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
        }
        Spacer(minLength: 0)
    }

    private func row(_ peer: Peer) -> some View {
        SidebarRow(peer: peer, unread: model.unread[peer.id] ?? 0,
                   dropTargeted: rowDropTarget == peer.id)
            .tag(peer.id)
            .onDrop(of: [UTType.fileURL], isTargeted: Binding(
                get: { rowDropTarget == peer.id },
                set: { on in
                    if on { rowDropTarget = peer.id }
                    else if rowDropTarget == peer.id { rowDropTarget = nil }
                })
            ) { providers in
                loadFileURLs(providers) { model.sendFiles($0, to: peer.id) }
                return true
            }
            .contextMenu {
                Button("Send File…") { pickFiles(for: peer) }
                Divider()
                Button("Clear Conversation…", role: .destructive) {
                    confirmClear = peer
                }
            }
    }

    @ViewBuilder private var searchResults: some View {
        if model.searchHits.isEmpty {
            Text("No messages match “\(model.search)”.")
                .font(.system(size: 11.5))
                .foregroundColor(.secondary)
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
            Spacer(minLength: 0)
        } else {
            List(model.searchHits) { hit in
                Button { model.openHit(hit) } label: {
                    SearchHitRow(
                        hit: hit,
                        peerName: model.peers.first { $0.id == hit.peer }?.name
                            ?? String(hit.peer.prefix(8)))
                }
                .buttonStyle(.plain)
            }
            .listStyle(.sidebar)
        }
    }

    /// You, with how you're showing to everyone else, and the control to
    /// change it.
    private func footer(_ me: Me) -> some View {
        HStack(spacing: 8) {
            AvatarView(name: me.name, colorKey: "me-self", size: 26,
                       online: true, state: model.presence)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(me.name)
                        .font(.system(size: 12, weight: .medium))
                        .lineLimit(1)
                    Image(systemName: "lock.fill")
                        .font(.system(size: 9))
                        .foregroundColor(.green)
                }
                Text(presenceLabel)
                    .font(.system(size: 10.5))
                    .foregroundColor(.secondary)
            }
            Spacer(minLength: 0)
            Menu {
                Picker("Show me as", selection: Binding(
                    get: { model.presence },
                    set: { model.setPresence($0) })
                ) {
                    Text("Available").tag("active")
                    Text("Away").tag("away")
                    Text("Do Not Disturb — no sounds or banners").tag("dnd")
                }
                .pickerStyle(.inline)
                Divider()
                Button("Copy My Safety Words") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(
                        me.words.joined(separator: " "), forType: .string)
                }
            } label: {
                Image(systemName: "ellipsis.circle")
                    .font(.system(size: 13))
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .frame(width: 24, height: 22)
            .help("How you show to others, and your safety words")
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .contentShape(Rectangle())
        .help("You: \(me.name) · \(me.short)…\nSafety words — anyone verifying you "
            + "should see exactly these:\n"
            + me.words.joined(separator: " · "))
    }

    private var presenceLabel: String {
        switch model.presence {
        case "away": return "Away"
        case "dnd": return "Do not disturb"
        default: return "Available"
        }
    }

    private func pickFiles(for peer: Peer) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.message = "Send to \(peer.name) — straight to their machine, "
            + "encrypted. Nothing leaves the LAN."
        panel.prompt = "Send"
        if panel.runModal() == .OK {
            model.sendFiles(panel.urls, to: peer.id)
        }
    }
}

// MARK: - Engine lifecycle

final class EngineDelegate: NSObject, NSApplicationDelegate {
    var engine: Process?

    func applicationDidFinishLaunching(_ notification: Notification) {
        Notifier.requestAuthorization()
        let env = ProcessInfo.processInfo.environment
        let home = FileManager.default.homeDirectoryForCurrentUser
        let bin = env["LANTERN_ENGINE"].map { URL(fileURLWithPath: $0) }
            ?? home.appendingPathComponent(".lantern/bin/lantern-gui")
        guard FileManager.default.isExecutableFile(atPath: bin.path) else { return }

        try? FileManager.default.createDirectory(
            at: DATA_DIR, withIntermediateDirectories: true)
        let logPath = DATA_DIR.appendingPathComponent("gui.log").path
        if !FileManager.default.fileExists(atPath: logPath) {
            FileManager.default.createFile(atPath: logPath, contents: nil)
        }
        let proc = Process()
        proc.executableURL = bin
        // The GTK shell's env vars, passed on as the engine's flags, so a
        // second profile on one Mac is one command (README, same-machine).
        var args: [String] = []
        if GUI_PORT != 3999 { args += ["--gui-port", String(GUI_PORT)] }
        if env["LANTERN_DATA_DIR"] != nil { args += ["--data-dir", DATA_DIR.path] }
        if let p = env["LANTERN_PORT"] { args += ["--discovery-port", p] }
        if let t = env["LANTERN_TARGETS"] { args += ["--targets", t] }
        if let n = env["LANTERN_NAME"] { args += ["--name", n] }
        if let g = env["LANTERN_GROUP"] { args += ["--group", g] }
        if let b = env["LANTERN_BROADCAST"] {
            args += ["--broadcast", b == "false" ? "false" : "true"]
        }
        proc.arguments = args
        if let log = FileHandle(forWritingAtPath: logPath) {
            log.seekToEndOfFile()
            proc.standardOutput = log
            proc.standardError = log
        }
        // If an engine is already running the child exits on bind failure
        // and the existing one serves us. Either way the UI polls until
        // /api/me answers.
        try? proc.run()
        engine = proc
    }

    func applicationShouldTerminateAfterLastWindowClosed(
        _ sender: NSApplication
    ) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        engine?.terminate()
    }
}

// MARK: - Entry

@main
struct LanternApp: App {
    @NSApplicationDelegateAdaptor(EngineDelegate.self) var engineDelegate
    @StateObject private var model = Model()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
                .onAppear { model.start() }
        }
        .defaultSize(width: 1240, height: 820)
        // Where macOS users look for it: the app menu, under About.
        .commands {
            CommandGroup(after: .appInfo) {
                Button("Check for Updates…") {
                    model.showUpdates = true
                    model.checkForUpdates()
                }
                .keyboardShortcut("u", modifiers: .command)
            }
        }
    }
}
