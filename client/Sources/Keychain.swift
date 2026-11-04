import Foundation
import Security

/// Server passwords as generic Keychain items, one per server id: the RDP
/// password under the id itself, the Mac account's password (for unlocking
/// FileVault and for Screen Sharing) under `<id>.mac`.
enum Keychain {
    enum Kind {
        case rdp, mac
    }

    private static let service = "OpenRDP Client"

    private static func query(for id: UUID, _ kind: Kind) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: kind == .rdp ? id.uuidString : "\(id.uuidString).mac",
        ]
    }

    static func password(for id: UUID, _ kind: Kind = .rdp) -> String? {
        var q = query(for: id, kind)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: AnyObject?
        guard SecItemCopyMatching(q as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    @discardableResult
    static func setPassword(_ password: String, for id: UUID, _ kind: Kind = .rdp) -> Bool {
        guard !password.isEmpty else { return deletePassword(for: id, kind) }
        let data = Data(password.utf8)
        let status = SecItemUpdate(query(for: id, kind) as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            var q = query(for: id, kind)
            q[kSecValueData as String] = data
            q[kSecAttrLabel as String] = "OpenRDP Client"
            q[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlocked
            return SecItemAdd(q as CFDictionary, nil) == errSecSuccess
        }
        return status == errSecSuccess
    }

    @discardableResult
    static func deletePassword(for id: UUID, _ kind: Kind = .rdp) -> Bool {
        let status = SecItemDelete(query(for: id, kind) as CFDictionary)
        return status == errSecSuccess || status == errSecItemNotFound
    }
}
