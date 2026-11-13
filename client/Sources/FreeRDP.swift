import Foundation

/// Finds FreeRDP's SDL client: the one inside the app (client/build-freerdp.sh),
/// or one installed with Homebrew.
enum FreeRDP {
    /// Contents/MacOS/sdl-freerdp, with SDL3 built for the keyboard grab.
    static var bundled: URL? {
        Bundle.main.url(forAuxiliaryExecutable: "sdl-freerdp")
            .flatMap { FileManager.default.isExecutableFile(atPath: $0.path) ? $0 : nil }
    }

    /// Intel Homebrew first on an Intel Mac, Apple Silicon Homebrew first otherwise.
    static var candidates: [String] {
        #if arch(x86_64)
        ["/usr/local/bin/sdl-freerdp", "/opt/homebrew/bin/sdl-freerdp"]
        #else
        ["/opt/homebrew/bin/sdl-freerdp", "/usr/local/bin/sdl-freerdp"]
        #endif
    }

    static func locate(custom: String, preferBundled: Bool) -> URL? {
        if preferBundled, let bundled { return bundled }
        let custom = (custom as NSString).expandingTildeInPath
        let paths = custom.isEmpty ? candidates : [custom]
        return paths.first { FileManager.default.isExecutableFile(atPath: $0) }.map { URL(fileURLWithPath: $0) }
    }

    /// "3.32.2", from `sdl-freerdp --version`.
    static func version(of binary: URL) async -> String? {
        await Task.detached {
            let p = Process()
            p.executableURL = binary
            p.arguments = ["--version"]
            let out = Pipe()
            p.standardOutput = out
            p.standardError = FileHandle.nullDevice
            guard (try? p.run()) != nil else { return nil }
            let data = out.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            let text = String(decoding: data, as: UTF8.self)
            guard let range = text.range(of: #"version [0-9][^ \n]*"#, options: .regularExpression) else { return nil }
            return String(text[range].dropFirst("version ".count))
        }.value
    }

    /// The FreeRDP options for a server, one argument per element.
    ///
    /// They reach sdl-freerdp on stdin through `/args-from:stdin`, one per line,
    /// so the password never shows up in `ps` or in a file.
    static func arguments(for server: Server, password: String?, reverseScroll: Bool = false) -> [String] {
        var args = [
            "/v:\(server.address)",
            "/t:\(server.displayName)",
            "/gfx:\(server.codec.rawValue)",
            "/dynamic-resolution",
            "/cert:\(server.certificate.rawValue)",
        ]
        if !server.user.isEmpty { args.append("/u:\(server.user)") }
        if let password, !password.isEmpty { args.append("/p:\(password)") }
        if server.fullscreen {
            args.append("/f")
        } else if let w = server.windowWidth, let h = server.windowHeight, w > 0, h > 0 {
            args.append("/size:\(w)x\(h)")
        }
        args.append(server.clipboard ? "+clipboard" : "-clipboard")

        // The RDP alternate shell carries options for the OpenRDP server,
        // joined with commas into one /shell, together with any the user added.
        var shell: [String] = []
        for arg in server.extraArgumentList {
            if arg.lowercased().hasPrefix("/shell:") {
                shell.append(String(arg.dropFirst("/shell:".count)))
            } else {
                args.append(arg)
            }
        }
        if reverseScroll { shell.insert("reversescroll", at: 0) }
        if !server.keepForReconnect { shell.insert("nokeep", at: 0) }
        if server.curtain { shell.insert("curtain", at: 0) }
        if !shell.isEmpty { args.append("/shell:" + shell.joined(separator: ",")) }
        // A line break would split an argument in two, and an empty line ends the list.
        return args.map { $0.replacingOccurrences(of: "\n", with: "").replacingOccurrences(of: "\r", with: "") }
    }

    static let certificateProblem = "Сертификат сервера не совпал с запомненным. Если сервер переустанавливали, забудьте старый сертификат и подключитесь снова."

    /// What an sdl-freerdp exit code means, nil for a normal end of the session.
    static func explain(exitCode: Int32, log: String) -> String? {
        // Closing the window and the disconnect shortcut also end sdl-freerdp with
        // 131, like a failed connection. They log only ERRCONNECT_CONNECT_CANCELLED,
        // and only after the session was up.
        let connected = log.contains("gdi_init_ex")
        let failed = log.replacingOccurrences(of: "ERRCONNECT_CONNECT_CANCELLED", with: "").contains("ERRCONNECT")
        if exitCode == 131 && connected && !failed { return nil }
        switch exitCode {
        case 0, 1, 2, 11: return nil
        // FreeRDP 3.32 prints "Host key ... has changed" even when it accepts a
        // certificate on first use, so the text means something only on a failure.
        case 143 where log.contains("has changed"), 145 where log.contains("has changed"):
            return certificateProblem
        case 3: return "Сеанс закрыт из-за простоя."
        case 5: return "Сеанс перехвачен другим подключением."
        case 128: return "FreeRDP не принял параметры. Проверьте дополнительные параметры сервера."
        case 131, 141, 147: return "Не удалось подключиться: сервер не отвечает или порт закрыт."
        case 132, 134, 154, 155, 159: return "Неверное имя пользователя или пароль."
        case 139, 140: return "Адрес сервера не найден."
        case 143: return "Не удалось установить TLS-соединение (возможно, отклонён сертификат)."
        case 145: return "Подключение отменено."
        default: return "sdl-freerdp завершился с кодом \(exitCode)."
        }
    }

    /// Where FreeRDP keeps the certificate it accepted for a host (TOFU).
    static func storedCertificate(for server: Server) -> URL {
        let base = HotkeyConfig.url.deletingLastPathComponent()
        return base.appendingPathComponent("server/\(server.host)_\(server.port).pem")
    }
}

/// One running sdl-freerdp.
@MainActor
final class Session: ObservableObject, Identifiable {
    enum State: Equatable {
        case running
        case ended(problem: String?)
    }

    let id = UUID()
    let server: Server
    let logURL: URL
    @Published private(set) var state = State.running
    private let process = Process()
    private var disconnectRequested = false

    init(server: Server, binary: URL, password: String?, passSystemShortcuts: Bool, localControlShortcuts: Bool, reverseScroll: Bool) throws {
        self.server = server
        try FileManager.default.createDirectory(at: AppPaths.logs, withIntermediateDirectories: true)
        let safeName = server.displayName.replacingOccurrences(of: "/", with: "-")
        logURL = AppPaths.logs.appendingPathComponent("\(safeName).log")
        FileManager.default.createFile(atPath: logURL.path, contents: nil, attributes: [.posixPermissions: 0o600])
        let log = try FileHandle(forWritingTo: logURL)

        process.executableURL = binary
        process.arguments = ["/args-from:stdin"]
        var env = ProcessInfo.processInfo.environment
        env.removeValue(forKey: "DYLD_INSERT_LIBRARIES")
        if passSystemShortcuts, let lib = Bundle.main.url(forResource: "libopenrdp-keyboard", withExtension: "dylib") {
            env["DYLD_INSERT_LIBRARIES"] = lib.path
            env["OPENRDP_LOCAL_CONTROL_SHORTCUTS"] = localControlShortcuts ? "1" : "0"
        }
        // FreeRDP enters full screen with SDL_SetWindowFullscreen, and SDL then
        // hides the menu bar for good. With this the pointer at the top edge
        // brings down the menu bar and the window's title bar, as in any Mac
        // app in full screen: the green button leaves it, the red one closes.
        env["SDL_VIDEO_MAC_FULLSCREEN_MENU_VISIBILITY"] = "1"
        process.environment = env
        let input = Pipe()
        process.standardInput = input
        process.standardOutput = log
        process.standardError = log
        process.terminationHandler = { [weak self] p in
            let code = p.terminationStatus
            try? log.close()
            Task { @MainActor in self?.finished(exitCode: code) }
        }
        try process.run()

        var args = FreeRDP.arguments(for: server, password: password, reverseScroll: reverseScroll)
        // Without the grab the bundled SDL leaves system shortcuts to this Mac too.
        if !passSystemShortcuts { args.append("-grab-keyboard") }
        let lines = args.joined(separator: "\n") + "\n\n"
        input.fileHandleForWriting.write(Data(lines.utf8))
        try? input.fileHandleForWriting.close()
    }

    var isRunning: Bool { state == .running }

    func disconnect() {
        guard process.isRunning else { return }
        disconnectRequested = true
        process.terminate()
    }

    private func finished(exitCode: Int32) {
        let text = (try? String(contentsOf: logURL, encoding: .utf8)) ?? ""
        let tail = String(text.suffix(20_000))
        // sdl-freerdp catches SIGTERM and exits with 131, like a lost connection.
        state = .ended(problem: disconnectRequested ? nil : FreeRDP.explain(exitCode: exitCode, log: tail))
    }
}
