import AppKit
import Combine
import Foundation

@MainActor
final class AppModel: ObservableObject {
    let store = ServerStore()
    let updates = Updates()
    @Published private(set) var sessions: [UUID: Session] = [:]
    @Published private(set) var binary: URL?
    @Published private(set) var version: String?
    @Published var launchError: String?

    @Published var customBinaryPath: String {
        didSet { defaults.set(customBinaryPath, forKey: "customBinaryPath"); locateBinary() }
    }
    /// Use the sdl-freerdp inside the app when there is one (stage 1 used Homebrew's).
    @Published var preferBundled: Bool {
        didSet { defaults.set(preferBundled, forKey: "preferBundled"); locateBinary() }
    }
    var isBundled: Bool { binary != nil && binary == FreeRDP.bundled }
    /// Pass Cmd+Space, Cmd+Tab, Cmd+Q and other system shortcuts to the session
    /// (see Keyboard/openrdp_keyboard.m).
    @Published var passSystemShortcuts: Bool {
        didSet { defaults.set(passSystemShortcuts, forKey: "passSystemShortcuts") }
    }
    /// Turn the vertical scroll of the trackpad and the mouse the other way in
    /// the session; the OpenRDP server does it (`reversescroll`).
    @Published var reverseScroll: Bool {
        didSet { defaults.set(reverseScroll, forKey: "reverseScroll") }
    }
    @Published var hotkeyModifier: HotkeyModifier {
        didSet { defaults.set(hotkeyModifier.rawValue, forKey: "hotkeyModifier"); applyHotkeys() }
    }
    @Published private(set) var hotkeyError: String?
    /// What each saved Mac answers on the network, checked every few seconds.
    @Published private(set) var statuses: [UUID: HostStatus] = [:]
    /// Unlocking or waking in progress, or how it went.
    @Published private(set) var activity: [UUID: HostActivity] = [:]

    private let defaults = UserDefaults.standard
    private var cancellables: [UUID: AnyCancellable] = [:]

    init() {
        customBinaryPath = defaults.string(forKey: "customBinaryPath") ?? ""
        preferBundled = defaults.object(forKey: "preferBundled") as? Bool ?? true
        passSystemShortcuts = defaults.object(forKey: "passSystemShortcuts") as? Bool ?? true
        reverseScroll = defaults.bool(forKey: "reverseScroll")
        hotkeyModifier = defaults.string(forKey: "hotkeyModifier").flatMap(HotkeyModifier.init(rawValue:)) ?? .rightCommandOption
        locateBinary()
        applyHotkeys()
        Task { await watchHosts() }
        updates.check(quiet: true)
    }

    func status(of server: Server) -> HostStatus { statuses[server.id] ?? .unknown }

    /// Checks every saved Mac: every 2 seconds while one is being unlocked or
    /// woken, every 6 otherwise.
    private func watchHosts() async {
        while !Task.isCancelled {
            await refreshStatuses()
            let busy = activity.values.contains { $0.isWaiting }
            try? await Task.sleep(nanoseconds: (busy ? 2 : 6) * 1_000_000_000)
        }
    }

    func refreshStatuses() async {
        let servers = store.servers
        let results = await withTaskGroup(of: (Server, HostStatus).self) { group in
            for server in servers {
                group.addTask { (server, await Host.status(of: server)) }
            }
            var results: [(Server, HostStatus)] = []
            for await result in group { results.append(result) }
            return results
        }
        for (server, status) in results {
            statuses[server.id] = status
            if status == .online {
                if activity[server.id]?.isWaiting == true { activity[server.id] = nil }
                if server.wakeAddress.isEmpty { await learnWakeAddress(server) }
            }
            if case .waking(let since) = activity[server.id], status == .locked || Date().timeIntervalSince(since) > 120 {
                activity[server.id] = status == .locked ? nil : .failed("Mac не проснулся за 2 минуты.")
            }
            if case .starting(let since) = activity[server.id], Date().timeIntervalSince(since) > 300 {
                activity[server.id] = .failed("Mac не запустил RDP за 5 минут после разблокировки. Попробуйте «Общий экран».")
            }
        }
    }

    private func learnWakeAddress(_ server: Server) async {
        guard let address = await Host.hardwareAddress(of: server.host),
              var current = store.servers.first(where: { $0.id == server.id }),
              current.wakeAddress.isEmpty else { return }
        current.wakeAddress = address
        store.save(current)
    }

    /// Unlocks FileVault over SSH with the Mac account, then waits for RDP.
    func unlock(_ server: Server) {
        guard activity[server.id]?.isWaiting != true else { return }
        activity[server.id] = .unlocking
        Task {
            do {
                try await Host.unlock(server)
                activity[server.id] = .starting(since: Date())
            } catch {
                activity[server.id] = .failed(error.localizedDescription)
            }
            await refreshStatuses()
        }
    }

    func wake(_ server: Server) {
        if Host.wake(server) {
            activity[server.id] = .waking(since: Date())
        } else {
            activity[server.id] = .failed("Не удалось отправить пакет Wake-on-LAN: проверьте MAC-адрес в настройках этого Mac.")
        }
    }

    func forgetHostKey(_ server: Server) {
        Host.forgetHostKey(server)
        activity[server.id] = nil
    }

    func clearActivity(_ server: Server) { activity[server.id] = nil }

    func locateBinary() {
        binary = FreeRDP.locate(custom: customBinaryPath, preferBundled: preferBundled)
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
                                      passSystemShortcuts: passSystemShortcuts,
                                      reverseScroll: reverseScroll)
            sessions[server.id] = session
            // Re-publish so views that list sessions see their state change.
            cancellables[server.id] = session.objectWillChange.sink { [weak self] in
                self?.objectWillChange.send()
                // The session's state changes right after this fires.
                DispatchQueue.main.async { self?.updateDockIcon() }
            }
            updateDockIcon()
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

    /// While a session is open its window, sdl-freerdp, has a Dock icon of its
    /// own, with this app's icon, so the client steps out of the Dock to leave
    /// one. Opening the client again (Launchpad, Applications) brings it back.
    func updateDockIcon() {
        let policy: NSApplication.ActivationPolicy = runningCount > 0 ? .accessory : .regular
        guard NSApp.activationPolicy() != policy else { return }
        NSApp.setActivationPolicy(policy)
    }

    /// The client opened again during a session: back in the Dock, in front.
    func showClient() {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        NSApp.windows.first { $0.identifier?.rawValue == "main" || $0.title == "OpenRDP Client" }?.makeKeyAndOrderFront(nil)
    }

    func disconnectAll() {
        sessions.values.forEach { $0.disconnect() }
    }
}

enum HostActivity: Equatable {
    case unlocking
    /// FileVault took the password; macOS is starting.
    case starting(since: Date)
    case waking(since: Date)
    case failed(String)

    var isWaiting: Bool {
        if case .failed = self { return false }
        return true
    }
}
