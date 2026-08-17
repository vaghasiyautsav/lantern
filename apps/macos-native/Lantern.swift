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

let GUI_PORT = 3999
let BASE = "http://localhost:\(GUI_PORT)"

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

    enum CodingKeys: String, CodingKey {
        case id, name, host, addr, verified, words, online
        case sinceBeaconS = "since_beacon_s"
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

struct HistoryMessage: Codable {
    var mid: String
    var outgoing: Bool
    var ts: UInt64
    var text: String
    var state: Int
    var reply_to: String?
}

// MARK: - Chat items

enum ChatKind: Equatable {
    case text(String)
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
                Task { await self.loadHistory(s) }
            }
        }
    }
    @Published var items: [ChatItem] = []
    @Published var unread: [String: Int] = [:]
    @Published var banner: String?
    @Published var draft: String = ""
    /// The message the composer is currently answering, if any.
    @Published var replyingTo: ChatItem?
    @Published var showVerify = false
    @Published var engineUp = false

    // -- updates ----------------------------------------------------------
    @Published var build: BuildInfo?
    @Published var showUpdates = false
    /// nil while a check is in flight.
    @Published var updateInfo: UpdateInfo?
    @Published var updateStarting = false
    /// Set once the updater is away: the app is about to quit itself, and the
    /// sheet says so rather than looking frozen.
    @Published var updateHandedOff = false

    private var ws: URLSessionWebSocketTask?
    private var fileMeta: [String: (name: String, size: UInt64)] = [:]

    var currentPeer: Peer? { peers.first { $0.id == selected } }

    func start() {
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
            self.connectWS()
            await self.loadBuild()
        }
        Timer.scheduledTimer(withTimeInterval: 15, repeats: true) { _ in
            Task { @MainActor in await self.refreshPeers() }
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

    func loadHistory(_ peerID: String) async {
        items = []
        if let hist: [HistoryMessage] = await getJSON("/api/history/\(peerID)") {
            items = hist.map {
                ChatItem(
                    id: $0.mid, outgoing: $0.outgoing, ts: $0.ts,
                    kind: .text($0.text), delivered: $0.state >= 1,
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
        draft = ""
        replyingTo = nil
        Task {
            var body: [String: Any] = ["peer": peer, "text": text]
            if let answering { body["reply_to"] = answering }
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

    func sendFiles(_ urls: [URL]) {
        guard let peer = selected else {
            flash("Pick a person first, then drop the file.")
            return
        }
        let target = currentPeer
        if let target, !target.online {
            flash(explain(nil, peer: target))
            return
        }
        for url in urls {
            Task {
                let r = await postJSON(
                    "/api/filepath", ["peer": peer, "path": url.path])
                if let xid = r?["xid"] as? String {
                    let name = (r?["name"] as? String) ?? url.lastPathComponent
                    let attrs = try? FileManager.default
                        .attributesOfItem(atPath: url.path)
                    let size = (attrs?[.size] as? NSNumber)?.uint64Value ?? 0
                    fileMeta[xid] = (name, size)
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

        case "msg":
            let peer = ev["peer"] as? String ?? ""
            let text = ev["text"] as? String ?? ""
            let mid = ev["mid"] as? String ?? UUID().uuidString
            let ts = (ev["ts"] as? NSNumber)?.uint64Value ?? nowMS()
            let who = (ev["peer_name"] as? String)
                ?? peers.first { $0.id == peer }?.name ?? "Someone"
            if peer == selected {
                items.append(ChatItem(
                    id: mid, outgoing: false, ts: ts,
                    kind: .text(text), delivered: true,
                    replyTo: ev["reply_to"] as? String))
            } else {
                unread[peer, default: 0] += 1
                updateBadge()
            }
            NSSound(named: "Pop")?.play()
            Notifier.notify(title: who, body: text)

        case "delivered":
            if let mid = ev["mid"] as? String,
               let i = items.firstIndex(where: { $0.id == mid }) {
                items[i].delivered = true
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
            Notifier.notify(
                title: (ev["peer_name"] as? String) ?? "Incoming file",
                body: "Sending you \(name) · \(fmtSize(size))")

        case "file-received":
            let xid = ev["xid"] as? String ?? ""
            let name = ev["name"] as? String ?? "file"
            let size = (ev["size"] as? NSNumber)?.uint64Value ?? 0
            upsertFile(
                xid: xid, outgoing: false, name: name, size: size,
                status: "saved to ~/.lantern/downloads", done: true, failed: false)
            NSSound(named: "Glass")?.play()

        case "progress":
            guard let xid = ev["xid"] as? String, let meta = fileMeta[xid],
                  let done = (ev["done"] as? NSNumber)?.uint64Value,
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
                    .fill(online ? Color.green : Color(NSColor.tertiaryLabelColor))
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

    var body: some View {
        HStack(spacing: 10) {
            AvatarView(name: peer.name, colorKey: peer.id, online: peer.online)
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
                Text(peer.online ? peer.host : peer.lastSeenText)
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
        .help(peer.online
            ? "\(peer.name) on \(peer.host) · \(peer.addr)"
            : "\(peer.name) — \(peer.lastSeenText). Messages won't send.")
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
    /// Jump to the quoted original.
    var onJumpToQuoted: () -> Void = {}
    /// Delete this message from this Mac.
    var onDelete: () -> Void = {}

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

                case .file(let name, let size, let status, let done,
                           let failed, let fraction):
                    TransferCard(
                        name: name, size: size, status: status,
                        done: done, failed: failed, outgoing: item.outgoing,
                        fraction: fraction)
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

struct ConversationView: View {
    @EnvironmentObject var model: Model
    @FocusState private var composerFocused: Bool
    /// Briefly ringed after jumping to a quoted original, so the eye lands
    /// on it instead of hunting the scrollback.
    @State private var highlighted: String?
    /// Clearing a conversation can't be undone, so it asks first. Non-nil
    /// while the confirmation is up, and carries who it's about.
    @State private var confirmClear: Peer?

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
                        LazyVStack(alignment: .leading, spacing: 0) {
                            if model.items.isEmpty {
                                ConversationStarter(peer: peer)
                            }
                            ForEach(Array(model.items.enumerated()),
                                    id: \.element.id) { idx, item in
                                let prev = idx > 0 ? model.items[idx - 1] : nil
                                if needsDaySeparator(item, after: prev) {
                                    DaySeparator(ts: item.ts)
                                }
                                let next = idx + 1 < model.items.count
                                    ? model.items[idx + 1] : nil
                                MessageRow(
                                    item: item,
                                    peerName: peer.name,
                                    myName: model.me?.name ?? "Me",
                                    showsHeader: startsRun(item, after: prev),
                                    endsRun: next.map {
                                        startsRun($0, after: item)
                                    } ?? true,
                                    quoted: quoted(for: item, peerName: peer.name),
                                    onReply: {
                                        model.replyingTo = item
                                        composerFocused = true
                                    },
                                    onJumpToQuoted: {
                                        if let rid = item.replyTo {
                                            jump(to: rid, proxy)
                                        }
                                    },
                                    onDelete: { model.deleteMessage(item) })
                                    .id(item.id)
                                    .background(
                                        RoundedRectangle(cornerRadius: 10)
                                            .fill(Color.accentColor.opacity(
                                                highlighted == item.id ? 0.13 : 0)))
                            }
                        }
                        .padding(14)
                    }
                    .onChange(of: model.items) { _ in
                        if let last = model.items.last {
                            withAnimation { proxy.scrollTo(last.id, anchor: .bottom) }
                        }
                    }
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

                    TextField("Message \(peer.name)…  (or drop files here)",
                              text: $model.draft, axis: .vertical)
                        .textFieldStyle(.plain)
                        .font(.system(size: 13))
                        .lineLimit(1...6)
                        .focused($composerFocused)
                        .onSubmit { model.sendMessage() }
                        .padding(.horizontal, 10)
                        .padding(.vertical, 7)
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
                    .help("Send  (⌘↩)")
                }
                .padding(12)
                .background(.bar)
            }
            .onAppear { composerFocused = true }
            .clearChatConfirmation($confirmClear)
        } else {
            VStack(spacing: 10) {
                Image(systemName: "lamp.desk")
                    .font(.system(size: 40))
                    .foregroundColor(.secondary)
                    .symbolRenderingMode(.hierarchical)
                Text(model.peers.isEmpty
                    ? "Nobody on this network yet"
                    : "Select someone to start")
                    .font(.system(size: 16, weight: .medium))
                Text(model.peers.isEmpty
                    ? "Open Lantern on another machine on the same network "
                        + "and it appears in the sidebar on its own."
                    : "Messages and files go straight to their machine.\n"
                        + "No server, no cloud, nothing in between.")
                    .font(.system(size: 12.5))
                    .foregroundColor(.secondary)
                    .multilineTextAlignment(.center)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

struct ContentView: View {
    @EnvironmentObject var model: Model
    @State private var dropTargeted = false
    /// Who the sidebar is asking about clearing, if anyone.
    @State private var confirmClear: Peer?

    var body: some View {
        NavigationSplitView {
            VStack(alignment: .leading, spacing: 0) {
                // Right-click a person to clear that chat without opening it
                // first — the point of deleting is usually to be rid of a
                // conversation, not to go and read it again.
                List(model.peers, selection: $model.selected) { peer in
                    SidebarRow(peer: peer, unread: model.unread[peer.id] ?? 0)
                        .contextMenu {
                            Button("Clear Conversation…", role: .destructive) {
                                confirmClear = peer
                            }
                        }
                }
                .listStyle(.sidebar)
                .clearChatConfirmation($confirmClear)
                if model.peers.isEmpty {
                    // Short here on purpose — the detail pane already
                    // explains what to do; repeating it twice reads as noise.
                    Text(model.engineUp ? "No one yet." : "Starting the engine…")
                        .font(.system(size: 11.5))
                        .foregroundColor(.secondary)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                }
                Spacer(minLength: 0)
                Divider()
                if let me = model.me {
                    HStack(spacing: 8) {
                        AvatarView(name: me.name, colorKey: "me-self",
                                   size: 26, online: nil)
                        VStack(alignment: .leading, spacing: 1) {
                            HStack(spacing: 4) {
                                Text(me.name)
                                    .font(.system(size: 12, weight: .medium))
                                    .lineLimit(1)
                                Image(systemName: "lock.fill")
                                    .font(.system(size: 9))
                                    .foregroundColor(.green)
                            }
                            Text(me.short + "…")
                                .font(.system(size: 10, design: .monospaced))
                                .foregroundColor(.secondary)
                        }
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 8)
                    .contentShape(Rectangle())
                    .help("You: \(me.name)\nSafety words — anyone verifying you "
                        + "should see exactly these:\n"
                        + me.words.joined(separator: " · "))
                }
            }
            .navigationSplitViewColumnWidth(min: 220, ideal: 250)
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
        // Drop anywhere in the window — with a visible target, so it doesn't
        // depend on the user guessing that dropping works.
        .overlay {
            if dropTargeted {
                RoundedRectangle(cornerRadius: 12)
                    .strokeBorder(Color.accentColor, lineWidth: 2.5)
                    .background(
                        RoundedRectangle(cornerRadius: 12)
                            .fill(Color.accentColor.opacity(0.07)))
                    .overlay(
                        Label(model.selected == nil
                            ? "Pick a person first"
                            : "Drop to send to \(model.currentPeer?.name ?? "them")",
                              systemImage: "arrow.down.doc")
                            .font(.system(size: 13, weight: .medium))
                            .padding(.horizontal, 16)
                            .padding(.vertical, 10)
                            .background(Capsule().fill(.regularMaterial)))
                    .padding(6)
                    .allowsHitTesting(false)
            }
        }
        .onDrop(of: [UTType.fileURL], isTargeted: $dropTargeted) { providers in
            for provider in providers {
                provider.loadItem(
                    forTypeIdentifier: UTType.fileURL.identifier,
                    options: nil
                ) { data, _ in
                    if let data = data as? Data,
                       let url = URL(dataRepresentation: data, relativeTo: nil) {
                        Task { @MainActor in self.model.sendFiles([url]) }
                    }
                }
            }
            return true
        }
    }
}

// MARK: - Engine lifecycle

final class EngineDelegate: NSObject, NSApplicationDelegate {
    var engine: Process?

    func applicationDidFinishLaunching(_ notification: Notification) {
        Notifier.requestAuthorization()
        let home = FileManager.default.homeDirectoryForCurrentUser
        let bin = home.appendingPathComponent(".lantern/bin/lantern-gui")
        guard FileManager.default.isExecutableFile(atPath: bin.path) else { return }

        let logPath = home.appendingPathComponent(".lantern/gui.log").path
        if !FileManager.default.fileExists(atPath: logPath) {
            FileManager.default.createFile(atPath: logPath, contents: nil)
        }
        let proc = Process()
        proc.executableURL = bin
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
