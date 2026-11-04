import Foundation
import SwiftUI // Array.move(fromOffsets:toOffset:)

/// A saved Mac to connect to. The password is not here: it lives in the Keychain
/// under the server's id.
struct Server: Identifiable, Codable, Hashable {
    enum Codec: String, Codable, CaseIterable, Identifiable {
        case avc444, avc420
        var id: String { rawValue }
        var title: String {
            switch self {
            case .avc444: "AVC444 (четче текст)"
            case .avc420: "AVC420 (меньше трафика)"
            }
        }
    }

    enum Certificate: String, Codable, CaseIterable, Identifiable {
        /// Remember the server's certificate on the first connection and warn if it changes.
        case tofu
        /// Accept any certificate.
        case ignore
        var id: String { rawValue }
        var title: String {
            switch self {
            case .tofu: "Запомнить при первом подключении"
            case .ignore: "Не проверять"
            }
        }
    }

    var id = UUID()
    var name = ""
    var host = ""
    var port = 3389
    var user = ""
    var codec = Codec.avc444
    var certificate = Certificate.tofu
    var fullscreen = true
    /// Window size when not full screen; nil lets FreeRDP pick.
    var windowWidth: Int?
    var windowHeight: Int?
    var clipboard = true
    /// Darken the server Mac's own displays during the session. The OpenRDP
    /// server does it when the RDP alternate shell holds `curtain`.
    var curtain = false
    /// Extra FreeRDP options, one per line, for example `/network:lan`.
    var extraArguments = ""
    /// The macOS account on the server Mac, for unlocking FileVault over SSH
    /// after a power cut and for Screen Sharing. Its password is in the
    /// Keychain next to the RDP one.
    var macUser = ""
    var sshPort = 22
    /// The server Mac's Ethernet or Wi-Fi address for Wake-on-LAN, such as
    /// `a4:83:e7:12:34:56`. Learnt from the ARP table while the Mac is up.
    var wakeAddress = ""

    init() {}

    /// Fields missing from servers.json (saved by an older version) keep their
    /// defaults instead of failing the whole list.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = Server()
        id = try c.decodeIfPresent(UUID.self, forKey: .id) ?? d.id
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? d.name
        host = try c.decodeIfPresent(String.self, forKey: .host) ?? d.host
        port = try c.decodeIfPresent(Int.self, forKey: .port) ?? d.port
        user = try c.decodeIfPresent(String.self, forKey: .user) ?? d.user
        codec = try c.decodeIfPresent(Codec.self, forKey: .codec) ?? d.codec
        certificate = try c.decodeIfPresent(Certificate.self, forKey: .certificate) ?? d.certificate
        fullscreen = try c.decodeIfPresent(Bool.self, forKey: .fullscreen) ?? d.fullscreen
        windowWidth = try c.decodeIfPresent(Int.self, forKey: .windowWidth)
        windowHeight = try c.decodeIfPresent(Int.self, forKey: .windowHeight)
        clipboard = try c.decodeIfPresent(Bool.self, forKey: .clipboard) ?? d.clipboard
        curtain = try c.decodeIfPresent(Bool.self, forKey: .curtain) ?? d.curtain
        extraArguments = try c.decodeIfPresent(String.self, forKey: .extraArguments) ?? d.extraArguments
        macUser = try c.decodeIfPresent(String.self, forKey: .macUser) ?? d.macUser
        sshPort = try c.decodeIfPresent(Int.self, forKey: .sshPort) ?? d.sshPort
        wakeAddress = try c.decodeIfPresent(String.self, forKey: .wakeAddress) ?? d.wakeAddress
    }

    var displayName: String { name.isEmpty ? host : name }

    var address: String {
        let h = host.contains(":") && !host.hasPrefix("[") ? "[\(host)]" : host
        return port == 3389 ? h : "\(h):\(port)"
    }

    var extraArgumentList: [String] {
        extraArguments.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
    }
}

/// The saved servers, kept as JSON in Application Support.
@MainActor
final class ServerStore: ObservableObject {
    @Published private(set) var servers: [Server] = []

    private let url: URL

    init(directory: URL = AppPaths.support) {
        url = directory.appendingPathComponent("servers.json")
        load()
    }

    func load() {
        guard let data = try? Data(contentsOf: url) else { return }
        servers = (try? JSONDecoder().decode([Server].self, from: data)) ?? []
    }

    func save(_ server: Server) {
        if let i = servers.firstIndex(where: { $0.id == server.id }) {
            servers[i] = server
        } else {
            servers.append(server)
        }
        write()
    }

    func delete(_ server: Server) {
        servers.removeAll { $0.id == server.id }
        Keychain.deletePassword(for: server.id)
        Keychain.deletePassword(for: server.id, .mac)
        write()
    }

    func move(from source: IndexSet, to destination: Int) {
        servers.move(fromOffsets: source, toOffset: destination)
        write()
    }

    private func write() {
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try encoder.encode(servers).write(to: url, options: .atomic)
        } catch {
            NSLog("OpenRDP Client: cannot save servers: \(error)")
        }
    }
}

enum AppPaths {
    static let support: URL = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("OpenRDP Client", isDirectory: true)
    static let logs: URL = FileManager.default.urls(for: .libraryDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("Logs/OpenRDP Client", isDirectory: true)
}
