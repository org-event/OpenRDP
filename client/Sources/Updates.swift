import AppKit
import Foundation

/// Newer releases of the client on GitHub: checked at launch and from the
/// button in Settings. A newer disk image is downloaded to ~/Downloads and
/// opened, so installing it is the usual drag onto Applications.
@MainActor
final class Updates: ObservableObject {
    enum State: Equatable {
        case idle
        case checking
        case current
        case newer(version: String, url: URL)
        case downloading(version: String)
        case failed(String)
    }

    @Published private(set) var state = State.idle

    private static let latest = URL(string: "https://api.github.com/repos/org-event/OpenRDP/releases/latest")!
    private static let downloads = "https://github.com/org-event/OpenRDP/releases/download/"

    /// "1.6.4", from the bundle; "0.1.0" in a local build.
    static var current: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
    }

    /// Asks GitHub. With `quiet`, an error leaves the state as it was: the
    /// launch check should not complain when the Mac is offline.
    func check(quiet: Bool = false) {
        if case .downloading = state { return }
        if !quiet { state = .checking }
        Task {
            do {
                var request = URLRequest(url: Self.latest)
                request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
                request.setValue("OpenRDP-Client/\(Self.current)", forHTTPHeaderField: "User-Agent")
                let (data, _) = try await URLSession.shared.data(for: request)
                let release = try JSONDecoder().decode(Release.self, from: data)
                let version = release.tag.hasPrefix("v") ? String(release.tag.dropFirst()) : release.tag
                guard Self.isNewer(version, than: Self.current) else {
                    if !quiet { state = .current }
                    return
                }
                guard let asset = release.assets.first(where: { $0.name.hasPrefix("openrdp-client-") && $0.name.hasSuffix("-universal.dmg") }),
                      let url = URL(string: asset.url), asset.url.hasPrefix(Self.downloads) else {
                    if !quiet { state = .failed("В релизе \(release.tag) нет образа клиента.") }
                    return
                }
                state = .newer(version: version, url: url)
            } catch {
                if !quiet { state = .failed("Не удалось проверить обновления: \(error.localizedDescription)") }
            }
        }
    }

    /// Downloads the disk image into ~/Downloads and opens it.
    func download() {
        guard case .newer(let version, let url) = state else { return }
        state = .downloading(version: version)
        Task {
            do {
                let (temporary, response) = try await URLSession.shared.download(from: url)
                guard (response as? HTTPURLResponse)?.statusCode == 200 else {
                    throw URLError(.badServerResponse)
                }
                let folder = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask)[0]
                let target = folder.appendingPathComponent(url.lastPathComponent)
                try? FileManager.default.removeItem(at: target)
                try FileManager.default.moveItem(at: temporary, to: target)
                NSWorkspace.shared.open(target)
                state = .newer(version: version, url: url)
            } catch {
                state = .failed("Не удалось скачать \(version): \(error.localizedDescription)")
            }
        }
    }

    /// Whether dotted version `a` is newer than `b`.
    nonisolated static func isNewer(_ a: String, than b: String) -> Bool {
        let parts = { (v: String) in v.split(separator: ".").map { Int($0.prefix { $0.isNumber }) ?? 0 } }
        let (x, y) = (parts(a), parts(b))
        for i in 0..<max(x.count, y.count) {
            let (l, r) = (i < x.count ? x[i] : 0, i < y.count ? y[i] : 0)
            if l != r { return l > r }
        }
        return false
    }

    private struct Release: Decodable {
        let tag: String
        let assets: [Asset]
        enum CodingKeys: String, CodingKey {
            case tag = "tag_name"
            case assets
        }
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
