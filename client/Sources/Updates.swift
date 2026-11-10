import AppKit
import CryptoKit
import Foundation

/// Newer releases of the client on GitHub: checked at launch and from the
/// button in Settings. "Update" downloads the release's disk image, checks it
/// against the release's SHA-256, puts the new app in place of this one and
/// starts it.
///
/// Two channels: stable takes GitHub's latest release, never a pre-release;
/// beta takes the newest release of all, betas (`v1.7.0-beta.1`) included. A
/// beta build back on stable waits for the next stable release.
@MainActor
final class Updates: ObservableObject {
    struct Available: Equatable {
        let version: String
        let image: URL
        let checksum: URL
    }

    enum State: Equatable {
        case idle
        case checking
        case current
        case newer(Available)
        case updating(version: String)
        case failed(String)
    }

    enum Channel: String, CaseIterable, Identifiable {
        case stable, beta
        var id: String { rawValue }
        var title: String { self == .stable ? "Стабильные" : "Бета" }
    }

    @Published private(set) var state = State.idle
    @Published var channel: Channel {
        didSet {
            UserDefaults.standard.set(channel.rawValue, forKey: "updateChannel")
            if case .updating = state { return }
            state = .idle
        }
    }

    init() {
        channel = UserDefaults.standard.string(forKey: "updateChannel").flatMap(Channel.init(rawValue:)) ?? .stable
    }

    private static let latest = URL(string: "https://api.github.com/repos/org-event/OpenRDP/releases/latest")!
    /// The newest releases first, pre-releases included.
    private static let all = URL(string: "https://api.github.com/repos/org-event/OpenRDP/releases?per_page=30")!
    private static let downloads = "https://github.com/org-event/OpenRDP/releases/download/"

    /// "1.6.4", from the bundle; "0.1.0" in a local build.
    static var current: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
    }

    /// Asks GitHub. With `quiet`, an error leaves the state as it was: the
    /// launch check should not complain when the Mac is offline.
    func check(quiet: Bool = false) {
        if case .updating = state { return }
        if !quiet { state = .checking }
        Task {
            do {
                let beta = channel == .beta
                var request = URLRequest(url: beta ? Self.all : Self.latest)
                request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
                request.setValue("OpenRDP-Client/\(Self.current)", forHTTPHeaderField: "User-Agent")
                let (data, _) = try await URLSession.shared.data(for: request)
                let release: Release
                if beta {
                    let releases = try JSONDecoder().decode([Release].self, from: data)
                    let usable = releases.filter { $0.draft != true && $0.hasClient }
                    guard let newest = usable.max(by: { Self.isNewer($1.version, than: $0.version) }) else {
                        if !quiet { state = .current }
                        return
                    }
                    release = newest
                } else {
                    release = try JSONDecoder().decode(Release.self, from: data)
                }
                let version = release.version
                guard Self.isNewer(version, than: Self.current) else {
                    if !quiet { state = .current }
                    return
                }
                let url = { (name: String) in
                    release.assets.first { $0.name == name }
                        .flatMap { $0.url.hasPrefix(Self.downloads) ? URL(string: $0.url) : nil }
                }
                guard let image = release.clientImage,
                      let imageURL = url(image.name), let checksumURL = url(image.name + ".sha256") else {
                    if !quiet { state = .failed("В релизе \(release.tag) нет образа клиента.") }
                    return
                }
                state = .newer(Available(version: version, image: imageURL, checksum: checksumURL))
            } catch {
                if !quiet { state = .failed("Не удалось проверить обновления: \(error.localizedDescription)") }
            }
        }
    }

    /// Downloads and checks the new client, then quits; a small script waits
    /// for this process to end, swaps the app and opens the new one.
    /// `sessionsOpen` refuses while a session would be cut off.
    func update(sessionsOpen: Bool) {
        guard case .newer(let available) = state else { return }
        if sessionsOpen {
            state = .failed("Закройте открытые сеансы, потом нажмите «Обновить» снова.")
            Task { try? await Task.sleep(nanoseconds: 4_000_000_000); state = .newer(available) }
            return
        }
        state = .updating(version: available.version)
        Task {
            do {
                let staged = try await Self.stage(available)
                try Self.swapAndRelaunch(with: staged)
                NSApp.terminate(nil)
            } catch {
                state = .failed("Не удалось обновить до \(available.version): \(error.localizedDescription)")
            }
        }
    }

    /// The new app, copied out of the verified disk image into a folder of its own.
    private static func stage(_ available: Available) async throws -> URL {
        let work = FileManager.default.temporaryDirectory.appendingPathComponent("openrdp-client-update-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)
        let (downloaded, response) = try await URLSession.shared.download(from: available.image)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw UpdateError("сервер не отдал образ") }
        let image = work.appendingPathComponent("client.dmg")
        try FileManager.default.moveItem(at: downloaded, to: image)

        let (sum, _) = try await URLSession.shared.data(from: available.checksum)
        guard let expected = String(decoding: sum, as: UTF8.self).split(whereSeparator: \.isWhitespace).first?.lowercased(),
              expected.count == 64 else { throw UpdateError("в релизе нет контрольной суммы") }
        let actual = SHA256.hash(data: try Data(contentsOf: image)).map { String(format: "%02x", $0) }.joined()
        guard actual == expected else { throw UpdateError("контрольная сумма не совпала") }

        let mount = work.appendingPathComponent("mount")
        try run("/usr/bin/hdiutil", ["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint", mount.path, image.path])
        defer { try? run("/usr/bin/hdiutil", ["detach", "-quiet", mount.path]) }
        let app = mount.appendingPathComponent("OpenRDP Client.app")
        guard FileManager.default.fileExists(atPath: app.path) else { throw UpdateError("в образе нет OpenRDP Client.app") }
        let staged = work.appendingPathComponent("OpenRDP Client.app")
        try run("/usr/bin/ditto", [app.path, staged.path])
        return staged
    }

    /// Starts the swap, which waits for this process to exit.
    private static func swapAndRelaunch(with staged: URL) throws {
        let target = Bundle.main.bundleURL
        guard FileManager.default.isWritableFile(atPath: target.deletingLastPathComponent().path) else {
            throw UpdateError("нет права записи в \(target.deletingLastPathComponent().path)")
        }
        let script = """
        while kill -0 "$1" 2>/dev/null; do sleep 0.2; done
        rm -rf "$2.old" && mv "$2" "$2.old" && /usr/bin/ditto "$3" "$2" && rm -rf "$2.old" || { rm -rf "$2"; mv "$2.old" "$2"; }
        /usr/bin/xattr -dr com.apple.quarantine "$2" 2>/dev/null
        rm -rf "$(dirname "$3")"
        /usr/bin/open "$2"
        """
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        p.arguments = ["-c", script, "update", String(ProcessInfo.processInfo.processIdentifier), target.path, staged.path]
        p.standardInput = FileHandle.nullDevice
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        try p.run()
    }

    private static func run(_ tool: String, _ arguments: [String]) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: tool)
        p.arguments = arguments
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        try p.run()
        p.waitUntilExit()
        guard p.terminationStatus == 0 else { throw UpdateError("\((tool as NSString).lastPathComponent) завершился с кодом \(p.terminationStatus)") }
    }

    /// Whether version `a` is newer than `b`: dotted numbers, and a release
    /// after its pre-releases (`1.7.0-beta.2` < `1.7.0-rc.1` < `1.7.0`).
    nonisolated static func isNewer(_ a: String, than b: String) -> Bool {
        let split = { (v: String) -> ([Int], String?, Int) in
            let halves = v.split(separator: "-", maxSplits: 1).map(String.init)
            let numbers = (halves.first ?? "").split(separator: ".").map { Int($0.prefix { $0.isNumber }) ?? 0 }
            guard halves.count == 2 else { return (numbers, nil, 0) }
            let pre = halves[1].split(separator: ".", maxSplits: 1).map(String.init)
            return (numbers, pre[0].lowercased(), pre.count == 2 ? Int(pre[1]) ?? 0 : 0)
        }
        let (x, xLabel, xNumber) = split(a)
        let (y, yLabel, yNumber) = split(b)
        for i in 0..<max(x.count, y.count) {
            let (l, r) = (i < x.count ? x[i] : 0, i < y.count ? y[i] : 0)
            if l != r { return l > r }
        }
        switch (xLabel, yLabel) {
        case (nil, nil): return false
        case (nil, _): return true
        case (_, nil): return false
        case let (l?, r?): return l != r ? l > r : xNumber > yNumber
        }
    }

    private struct UpdateError: LocalizedError {
        let errorDescription: String?
        init(_ text: String) { errorDescription = text }
    }

    private struct Release: Decodable {
        let tag: String
        let draft: Bool?
        let assets: [Asset]
        enum CodingKeys: String, CodingKey {
            case tag = "tag_name"
            case draft
            case assets
        }

        var version: String { tag.hasPrefix("v") ? String(tag.dropFirst()) : tag }
        var clientImage: Asset? {
            assets.first { $0.name.hasPrefix("openrdp-client-") && $0.name.hasSuffix("-universal.dmg") }
        }
        var hasClient: Bool { clientImage != nil }
    }

    private struct Asset: Decodable {
        let name: String
        let url: String
        enum CodingKeys: String, CodingKey {
            case name
            case url = "browser_download_url"
        }
    }
}
