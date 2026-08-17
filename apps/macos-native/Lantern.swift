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
}

struct HistoryMessage: Codable {
    var mid: String
    var outgoing: Bool
    var ts: UInt64
    var text: String
    var state: Int
}

// MARK: - Chat items

enum ChatKind: Equatable {
    case text(String)
    case file(name: String, size: UInt64, status: String, done: Bool, failed: Bool)
}

struct ChatItem: Identifiable, Equatable {
    var id: String // mid or xid
    var outgoing: Bool
    var ts: UInt64
    var kind: ChatKind
    var delivered: Bool
}

// MARK: - App state

@MainActor
final class Model: ObservableObject {
    @Published var me: Me?
    @Published var peers: [Peer] = []
    @Published var selected: String? {
        didSet {
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
    @Published var showVerify = false
    @Published var engineUp = false

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

    func refreshPeers() async {
        if let list: [Peer] = await getJSON("/api/peers") {
            peers = list.sorted { $0.name.lowercased() < $1.name.lowercased() }
        }
    }

    func loadHistory(_ peerID: String) async {
        items = []
        if let hist: [HistoryMessage] = await getJSON("/api/history/\(peerID)") {
            items = hist.map {
                ChatItem(
                    id: $0.mid, outgoing: $0.outgoing, ts: $0.ts,
                    kind: .text($0.text), delivered: $0.state >= 1)
            }
        }
    }

    // -- actions ----------------------------------------------------------

    func sendMessage() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, let peer = selected else { return }
        draft = ""
        Task {
            if let r = await postJSON("/api/msg", ["peer": peer, "text": text]),
               let mid = r["mid"] as? String {
                items.append(ChatItem(
                    id: mid, outgoing: true, ts: nowMS(),
                    kind: .text(text), delivered: false))
            } else {
                flash("Message didn't send — peer unreachable?")
            }
        }
    }

    func sendFiles(_ urls: [URL]) {
        guard let peer = selected else {
            flash("Pick a person first, then drop the file.")
            return
        }
        for url in urls {
            Task {
                if let r = await postJSON(
                    "/api/filepath", ["peer": peer, "path": url.path]),
                    let xid = r["xid"] as? String {
                    let name = (r["name"] as? String) ?? url.lastPathComponent
                    let attrs = try? FileManager.default
                        .attributesOfItem(atPath: url.path)
                    let size = (attrs?[.size] as? NSNumber)?.uint64Value ?? 0
                    fileMeta[xid] = (name, size)
                    upsertFile(
                        xid: xid, outgoing: true, name: name, size: size,
                        status: "sending…", done: false, failed: false)
                } else {
                    flash("Couldn't send \(url.lastPathComponent)")
                }
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
            try? await Task.sleep(nanoseconds: 6_000_000_000)
            if banner == text { banner = nil }
        }
    }

    private func updateBadge() {
        let total = unread.values.reduce(0, +)
        NSApp.dockTile.badgeLabel = total > 0 ? String(total) : ""
    }

    private func upsertFile(
        xid: String, outgoing: Bool, name: String, size: UInt64,
        status: String, done: Bool, failed: Bool
    ) {
        let kind = ChatKind.file(
            name: name, size: size, status: status, done: done, failed: failed)
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
            if peer == selected {
                items.append(ChatItem(
                    id: mid, outgoing: false, ts: ts,
                    kind: .text(text), delivered: true))
            } else {
                unread[peer, default: 0] += 1
                updateBadge()
            }
            NSSound(named: "Pop")?.play()

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

        case "file-received":
            let xid = ev["xid"] as? String ?? ""
            let name = ev["name"] as? String ?? "file"
            let size = (ev["size"] as? NSNumber)?.uint64Value ?? 0
            upsertFile(
                xid: xid, outgoing: false, name: name, size: size,
                status: "saved to ~/.lantern/downloads", done: true, failed: false)
            NSSound(named: "Glass")?.play()

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

func fmtTime(_ ts: UInt64) -> String {
    let d = Date(timeIntervalSince1970: Double(ts) / 1000)
    let f = DateFormatter()
    f.timeStyle = .short
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

    var body: some View {
        ZStack(alignment: .bottomTrailing) {
            Circle()
                .fill(avatarColor(colorKey))
                .frame(width: size, height: size)
                .overlay(
                    Text(initials(name))
                        .font(.system(size: size * 0.38, weight: .semibold))
                        .foregroundColor(.white))
            Circle()
                .fill(Color.green)
                .frame(width: size * 0.3, height: size * 0.3)
                .overlay(Circle().stroke(Color(NSColor.windowBackgroundColor),
                                         lineWidth: 2))
        }
    }
}

struct SidebarRow: View {
    let peer: Peer
    let unread: Int

    var body: some View {
        HStack(spacing: 10) {
            AvatarView(name: peer.name, colorKey: peer.id)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(peer.name).fontWeight(.medium).lineLimit(1)
                    if peer.verified {
                        Image(systemName: "checkmark.shield.fill")
                            .font(.system(size: 11))
                            .foregroundColor(.green)
                    }
                }
                Text(peer.host)
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
    }
}

struct MessageRow: View {
    let item: ChatItem
    let peerName: String
    let myName: String

    var senderName: String { item.outgoing ? myName : peerName }
    var senderKey: String { item.outgoing ? "me-self" : peerName }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            AvatarView(name: senderName, colorKey: senderKey)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(senderName).fontWeight(.semibold)
                    Text(fmtTime(item.ts))
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                    if item.outgoing {
                        Text(item.delivered ? "✓✓" : "◷")
                            .font(.system(size: 11))
                            .foregroundColor(item.delivered ? .accentColor : .secondary)
                    }
                }
                switch item.kind {
                case .text(let text):
                    Text(text)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                case .file(let name, let size, let status, let done, let failed):
                    HStack(spacing: 10) {
                        Image(systemName: done
                            ? "doc.circle.fill"
                            : (failed ? "xmark.circle.fill" : "arrow.up.arrow.down.circle"))
                            .font(.system(size: 26))
                            .foregroundColor(done ? .green : (failed ? .red : .accentColor))
                        VStack(alignment: .leading, spacing: 1) {
                            Text(name).fontWeight(.medium)
                            Text("\(fmtSize(size)) · \(status)")
                                .font(.system(size: 11))
                                .foregroundColor(.secondary)
                        }
                    }
                    .padding(10)
                    .background(
                        RoundedRectangle(cornerRadius: 8)
                            .fill(Color(NSColor.controlBackgroundColor)))
                    .overlay(
                        RoundedRectangle(cornerRadius: 8)
                            .stroke(Color.secondary.opacity(0.25)))
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 4)
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

struct ConversationView: View {
    @EnvironmentObject var model: Model
    @FocusState private var composerFocused: Bool

    var body: some View {
        if let peer = model.currentPeer {
            VStack(spacing: 0) {
                // Header
                HStack(spacing: 10) {
                    AvatarView(name: peer.name, colorKey: peer.id, size: 30)
                    VStack(alignment: .leading, spacing: 0) {
                        Text(peer.name).fontWeight(.semibold)
                        Text("\(peer.host) · \(peer.addr)")
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
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 9)
                Divider()

                // Messages
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(model.items) { item in
                                MessageRow(
                                    item: item,
                                    peerName: peer.name,
                                    myName: model.me?.name ?? "Me")
                                    .id(item.id)
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
                // Composer
                HStack(alignment: .bottom, spacing: 8) {
                    Button {
                        let panel = NSOpenPanel()
                        panel.allowsMultipleSelection = true
                        panel.canChooseDirectories = false
                        if panel.runModal() == .OK {
                            model.sendFiles(panel.urls)
                        }
                    } label: {
                        Image(systemName: "paperclip")
                    }
                    .buttonStyle(.borderless)
                    .help("Send a file")

                    TextField("Message \(peer.name)…  (or drop files here)",
                              text: $model.draft, axis: .vertical)
                        .textFieldStyle(.plain)
                        .lineLimit(1...5)
                        .focused($composerFocused)
                        .onSubmit { model.sendMessage() }

                    Button("Send") { model.sendMessage() }
                        .keyboardShortcut(.return, modifiers: [.command])
                        .disabled(model.draft.trimmingCharacters(
                            in: .whitespacesAndNewlines).isEmpty)
                }
                .padding(12)
            }
            .onAppear { composerFocused = true }
        } else {
            VStack(spacing: 8) {
                Image(systemName: "lamp.desk")
                    .font(.system(size: 42))
                    .foregroundColor(.secondary)
                Text("Select someone to start")
                    .font(.title3)
                Text("Messages and files go straight to their machine.\nNo server, no cloud, nothing in between.")
                    .foregroundColor(.secondary)
                    .multilineTextAlignment(.center)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

struct ContentView: View {
    @EnvironmentObject var model: Model

    var body: some View {
        NavigationSplitView {
            VStack(alignment: .leading, spacing: 0) {
                List(model.peers, selection: $model.selected) { peer in
                    SidebarRow(peer: peer, unread: model.unread[peer.id] ?? 0)
                }
                .listStyle(.sidebar)
                if model.peers.isEmpty {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(model.engineUp
                            ? "Nobody else on this network yet."
                            : "Starting the engine…")
                            .font(.system(size: 12))
                            .foregroundColor(.secondary)
                        if model.engineUp {
                            Text("Open Lantern on another machine on the same "
                                + "network and it appears here automatically.")
                                .font(.system(size: 11))
                                .foregroundColor(.secondary)
                        }
                    }
                    .padding(12)
                }
                Spacer(minLength: 0)
                Divider()
                if let me = model.me {
                    VStack(alignment: .leading, spacing: 2) {
                        HStack(spacing: 5) {
                            Image(systemName: "lock.fill")
                                .font(.system(size: 10))
                                .foregroundColor(.green)
                            Text("\(me.name) · \(me.short)…")
                                .font(.system(size: 11))
                                .foregroundColor(.secondary)
                        }
                        Text(me.words.joined(separator: " · "))
                            .font(.system(size: 9, design: .monospaced))
                            .foregroundColor(.secondary)
                            .lineLimit(2)
                            .help("Your safety words — someone verifying you should see exactly these")
                    }
                    .padding(10)
                }
            }
            .navigationSplitViewColumnWidth(min: 220, ideal: 250)
        } detail: {
            ConversationView()
        }
        .frame(minWidth: 760, minHeight: 500)
        .sheet(isPresented: $model.showVerify) { VerifySheet() }
        .overlay(alignment: .top) {
            if let banner = model.banner {
                Text(banner)
                    .font(.system(size: 12.5))
                    .padding(.horizontal, 14)
                    .padding(.vertical, 7)
                    .background(Capsule().fill(Color.orange.opacity(0.92)))
                    .foregroundColor(.white)
                    .padding(.top, 10)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
        .onDrop(of: [UTType.fileURL], isTargeted: nil) { providers in
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
    }
}
