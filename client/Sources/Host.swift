import AppKit
import Foundation
import Network

/// What a saved Mac looks like from here, by the ports that answer.
enum HostStatus: Equatable {
    /// Not checked yet.
    case unknown
    /// The RDP port answers: the desktop or the login window is up.
    case online
    /// Only SSH answers: the FileVault screen after a power cut (macOS 26 and
    /// later take the disk password over SSH), or a Mac still starting.
    case locked
    /// Nothing answers: off, asleep or out of reach.
    case offline
}

enum Host {
    /// Whether a TCP connection to `host:port` opens within `timeout`.
    static func isOpen(_ host: String, port: Int, timeout: TimeInterval = 2) async -> Bool {
        guard let nwPort = NWEndpoint.Port(rawValue: UInt16(clamping: port)), !host.isEmpty else { return false }
        let connection = NWConnection(host: NWEndpoint.Host(host), port: nwPort, using: .tcp)
        return await withCheckedContinuation { continuation in
            let once = Once()
            let finish: (Bool) -> Void = { open in
                once.run {
                    connection.cancel()
                    continuation.resume(returning: open)
                }
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready: finish(true)
                // A refused port leaves the connection waiting for a better path.
                case .failed, .waiting, .cancelled: finish(false)
                default: break
                }
            }
            connection.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) { finish(false) }
        }
    }

    static func status(of server: Server) async -> HostStatus {
        async let rdp = isOpen(server.host, port: server.port)
        async let ssh = isOpen(server.host, port: server.sshPort)
        if await rdp { return .online }
        return await ssh ? .locked : .offline
    }

    /// The first IPv4 address of `host`.
    static func ipv4(of host: String) -> String? {
        var hints = addrinfo(ai_flags: 0, ai_family: AF_INET, ai_socktype: SOCK_STREAM, ai_protocol: 0,
                             ai_addrlen: 0, ai_canonname: nil, ai_addr: nil, ai_next: nil)
        var result: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, nil, &hints, &result) == 0, let info = result else { return nil }
        defer { freeaddrinfo(result) }
        var address = info.pointee.ai_addr.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { $0.pointee.sin_addr }
        var buffer = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
        guard inet_ntop(AF_INET, &address, &buffer, socklen_t(INET_ADDRSTRLEN)) != nil else { return nil }
        return String(cString: buffer)
    }

    /// The hardware address of `host` from the ARP table, while it is up on
    /// this network. Nil across a router or VPN, where Wake-on-LAN would not
    /// reach it anyway.
    static func hardwareAddress(of host: String) async -> String? {
        await Task.detached {
            guard let ip = ipv4(of: host) else { return nil }
            // `arp -n <ip>` misses scoped entries when run from an app, so
            // read the whole table.
            let line = run("/usr/sbin/arp", ["-an"]).split(separator: "\n").first { $0.contains("(\(ip)) at ") }
            guard let out = line.map(String.init),
                  let range = out.range(of: #" at ([0-9a-f]{1,2}:){5}[0-9a-f]{1,2} "#, options: .regularExpression) else {
                return nil
            }
            // arp drops leading zeros: "a4:3:e7:…".
            return out[range].dropFirst(4).dropLast().split(separator: ":")
                .map { $0.count == 1 ? "0\($0)" : String($0) }.joined(separator: ":")
        }.value
    }

    /// The six bytes of "a4:83:e7:12:34:56" (or with dashes).
    static func macBytes(_ text: String) -> [UInt8]? {
        let parts = text.trimmingCharacters(in: .whitespaces).split(whereSeparator: { $0 == ":" || $0 == "-" })
        let bytes = parts.compactMap { UInt8($0, radix: 16) }
        return parts.count == 6 && bytes.count == 6 ? bytes : nil
    }

    /// Sends a Wake-on-LAN magic packet to the broadcast address and to the
    /// host itself, on ports 9 and 7.
    static func wake(_ server: Server) -> Bool {
        guard let mac = macBytes(server.wakeAddress) else { return false }
        let packet = [UInt8](repeating: 0xFF, count: 6) + Array([[UInt8]](repeating: mac, count: 16).joined())
        let fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_BROADCAST, &on, socklen_t(MemoryLayout<Int32>.size))
        var sent = false
        for target in ["255.255.255.255", ipv4(of: server.host)].compactMap({ $0 }) {
            for port in [9, 7] as [UInt16] {
                var addr = sockaddr_in()
                addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
                addr.sin_family = sa_family_t(AF_INET)
                addr.sin_port = port.bigEndian
                inet_pton(AF_INET, target, &addr.sin_addr)
                let n = packet.withUnsafeBytes { bytes in
                    withUnsafePointer(to: &addr) {
                        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                            sendto(fd, bytes.baseAddress, bytes.count, 0, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
                        }
                    }
                }
                sent = sent || n == packet.count
            }
        }
        return sent
    }

    enum UnlockError: LocalizedError {
        case noAccount
        case wrongPassword
        case hostKeyChanged
        case failed(String)

        var errorDescription: String? {
            switch self {
            case .noAccount: "Укажите пользователя и пароль Mac в настройках этого Mac."
            case .wrongPassword: "Mac не принял пароль."
            case .hostKeyChanged: "Ключ SSH этого Mac изменился. Если вы его переустанавливали, нажмите «Забыть ключ SSH»."
            case .failed(let text): text.isEmpty ? "ssh не смог подключиться." : text
            }
        }
    }

    /// Where the client keeps the SSH host keys it has seen, apart from
    /// ~/.ssh: the FileVault screen has a key of its own.
    static var knownHosts: URL { AppPaths.support.appendingPathComponent("known_hosts") }

    /// Signs in over SSH with the Mac account. On the FileVault screen that
    /// unlocks the disk and macOS goes on starting; on a running Mac it opens
    /// a shell that ends at once. The password reaches ssh through an askpass
    /// helper reading a file only this user can read, deleted right after.
    static func unlock(_ server: Server) async throws {
        guard !server.macUser.isEmpty, let password = Keychain.password(for: server.id, .mac) else {
            throw UnlockError.noAccount
        }
        let (status, output) = try await Task.detached { () throws -> (Int32, String) in
            let fm = FileManager.default
            let dir = fm.temporaryDirectory.appendingPathComponent("openrdp-askpass-\(UUID().uuidString)")
            try fm.createDirectory(at: dir, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
            defer { try? fm.removeItem(at: dir) }
            let secret = dir.appendingPathComponent("secret")
            fm.createFile(atPath: secret.path, contents: Data(password.utf8), attributes: [.posixPermissions: 0o600])
            let askpass = dir.appendingPathComponent("askpass")
            let script = "#!/bin/sh\nexec /bin/cat '\(secret.path)'\n"
            fm.createFile(atPath: askpass.path, contents: Data(script.utf8), attributes: [.posixPermissions: 0o700])
            try fm.createDirectory(at: AppPaths.support, withIntermediateDirectories: true)

            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
            p.arguments = [
                "-T",
                "-p", String(server.sshPort),
                "-o", "ConnectTimeout=10",
                "-o", "StrictHostKeyChecking=accept-new",
                "-o", "UserKnownHostsFile=\(knownHosts.path)",
                "-o", "PubkeyAuthentication=no",
                "-o", "PreferredAuthentications=keyboard-interactive,password",
                "-o", "NumberOfPasswordPrompts=1",
                "-l", server.macUser,
                server.host,
            ]
            var env = ProcessInfo.processInfo.environment
            env["SSH_ASKPASS"] = askpass.path
            env["SSH_ASKPASS_REQUIRE"] = "force"
            env["DISPLAY"] = env["DISPLAY"] ?? ":0"
            p.environment = env
            p.standardInput = FileHandle.nullDevice
            let err = Pipe()
            p.standardError = err
            p.standardOutput = FileHandle.nullDevice
            try p.run()
            let data = err.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            return (p.terminationStatus, String(decoding: data, as: UTF8.self))
        }.value
        if output.contains("Permission denied") || output.contains("Too many authentication failures") {
            throw UnlockError.wrongPassword
        }
        if output.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || output.contains("Host key verification failed") {
            throw UnlockError.hostKeyChanged
        }
        // The FileVault screen drops the connection once the disk is
        // unlocked, so ssh may end with 255 even when it worked; a refused or
        // timed-out connection says so on stderr.
        if status != 0, output.contains("Connection refused") || output.contains("timed out")
            || output.contains("No route to host") || output.contains("Could not resolve") {
            throw UnlockError.failed(output.trimmingCharacters(in: .whitespacesAndNewlines))
        }
    }

    /// Drops the SSH keys remembered for this Mac.
    static func forgetHostKey(_ server: Server) {
        let name = server.sshPort == 22 ? server.host : "[\(server.host)]:\(server.sshPort)"
        _ = run("/usr/bin/ssh-keygen", ["-R", name, "-f", knownHosts.path])
    }

    /// Opens Screen Sharing on the Mac, signed in with the Mac account when
    /// its password is saved.
    static func openScreenSharing(_ server: Server) {
        var components = URLComponents()
        components.scheme = "vnc"
        components.host = server.host
        if !server.macUser.isEmpty {
            components.user = server.macUser
            if let password = Keychain.password(for: server.id, .mac) { components.password = password }
        }
        if let url = components.url { NSWorkspace.shared.open(url) }
    }

    @discardableResult
    static func run(_ path: String, _ arguments: [String]) -> String {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: path)
        p.arguments = arguments
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        guard (try? p.run()) != nil else { return "" }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return String(decoding: data, as: UTF8.self)
    }
}

/// Runs a closure once, from whichever thread gets there first.
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false

    func run(_ body: () -> Void) {
        lock.lock()
        let first = !done
        done = true
        lock.unlock()
        if first { body() }
    }
}
