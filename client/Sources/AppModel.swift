import AppKit
import Combine
import Foundation

@MainActor
final class AppModel: ObservableObject {
    let store = ServerStore()
    @Published private(set) var sessions: [UUID: Session] = [:]
    @Published private(set) var binary: URL?
    @Published private(set) var version: String?
    @Published var launchError: String?

    @Published var customBinaryPath: String {
        didSet { defaults.set(customBinaryPath, forKey: "customBinaryPath"); locateBinary() }
    }
    /// Pass Cmd+Space, Cmd+Tab, Cmd+Q and other system shortcuts to the session
    /// (see Keyboard/openrdp_keyboard.m).
    @Published var passSystemShortcuts: Bool {
        didSet { defaults.set(passSystemShortcuts, forKey: "passSystemShortcuts") }
    }
    @Published var hotkeyModifier: HotkeyModifier {
        didSet { defaults.set(hotkeyModifier.rawValue, forKey: "hotkeyModifier"); applyHotkeys() }
    }
    @Published private(set) var hotkeyError: String?

    private let defaults = UserDefaults.standard
    private var cancellables: [UUID: AnyCancellable] = [:]

    init() {
        customBinaryPath = defaults.string(forKey: "customBinaryPath") ?? ""
        passSystemShortcuts = defaults.object(forKey: "passSystemShortcuts") as? Bool ?? true
        hotkeyModifier = defaults.string(forKey: "hotkeyModifier").flatMap(HotkeyModifier.init(rawValue:)) ?? .rightCommandOption
        locateBinary()
        applyHotkeys()
    }

    func locateBinary() {
        binary = FreeRDP.locate(custom: customBinaryPath)
        version = nil
        guard let binary else { return }
        Task { version = await FreeRDP.version(of: binary) }
    }

    /// Makes sdl-freerdp.json match the chosen modifier. Done on every start too,
    /// in case something else rewrote the file.
    func applyHotkeys() {
        guard HotkeyConfig.current() != hotkeyModifier else { hotkeyError = nil; return }
        do {
            try HotkeyConfig.write(hotkeyModifier)
            hotkeyError = nil
        } catch {
            hotkeyError = "Не удалось записать \(HotkeyConfig.url.path): \(error.localizedDescription)"
        }
    }

    func session(for server: Server) -> Session? { sessions[server.id] }

    func connect(_ server: Server) {
        if let running = sessions[server.id], running.isRunning { return }
        guard let binary else {
            launchError = "sdl-freerdp не найден. Установите FreeRDP: brew install freerdp"
            return
        }
        applyHotkeys()
        do {
            let session = try Session(server: server, binary: binary,
                                      password: Keychain.password(for: server.id),
                                      passSystemShortcuts: passSystemShortcuts)
            sessions[server.id] = session
            // Re-publish so views that list sessions see their state change.
            cancellables[server.id] = session.objectWillChange.sink { [weak self] in self?.objectWillChange.send() }
        } catch {
            launchError = "Не удалось запустить sdl-freerdp: \(error.localizedDescription)"
        }
    }

    func disconnect(_ server: Server) {
        sessions[server.id]?.disconnect()
    }

    func forgetCertificate(for server: Server) {
        try? FileManager.default.removeItem(at: FreeRDP.storedCertificate(for: server))
    }

    func showLog(for server: Server) {
        guard let url = sessions[server.id]?.logURL else { return }
        NSWorkspace.shared.open(url)
    }

    var runningCount: Int { sessions.values.filter(\.isRunning).count }

    func disconnectAll() {
        sessions.values.forEach { $0.disconnect() }
    }
}
