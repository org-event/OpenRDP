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
    /// Extra FreeRDP options, one per line, for example `/network:lan`.
    var extraArguments = ""

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
