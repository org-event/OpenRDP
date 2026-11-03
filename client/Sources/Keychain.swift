import Foundation
import Security

/// Server passwords as generic Keychain items, one per server id.
enum Keychain {
    private static let service = "OpenRDP Client"

    private static func query(for id: UUID) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: id.uuidString,
        ]
    }

    static func password(for id: UUID) -> String? {
        var q = query(for: id)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: AnyObject?
        guard SecItemCopyMatching(q as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    @discardableResult
    static func setPassword(_ password: String, for id: UUID) -> Bool {
        guard !password.isEmpty else { return deletePassword(for: id) }
        let data = Data(password.utf8)
        let status = SecItemUpdate(query(for: id) as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            var q = query(for: id)
            q[kSecValueData as String] = data
            q[kSecAttrLabel as String] = "OpenRDP Client"
            q[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlocked
            return SecItemAdd(q as CFDictionary, nil) == errSecSuccess
        }
        return status == errSecSuccess
    }

    @discardableResult
    static func deletePassword(for id: UUID) -> Bool {
        let status = SecItemDelete(query(for: id) as CFDictionary)
        return status == errSecSuccess || status == errSecItemNotFound
    }
}
