import Foundation

/// The modifier FreeRDP's SDL client holds for its own shortcuts (SDL_KeyModMask
/// in sdl-freerdp.json). FreeRDP's default is right Shift, which breaks typing
/// capital П, К, В and Ь in the Russian layout (they sit on G, R, D and M) and
/// disconnects on right Shift+D.
enum HotkeyModifier: String, CaseIterable, Identifiable {
    case rightCommandOption
    case controlOptionCommand
    case rightShift
    case off

    var id: String { rawValue }

    var title: String {
        switch self {
        case .rightCommandOption: "Правые ⌘ + ⌥"
        case .controlOptionCommand: "⌃ + ⌥ + ⌘ (левые)"
        case .rightShift: "Правый ⇧ (как в FreeRDP, мешает набору)"
        case .off: "Выключены"
        }
    }

    /// How the modifier is shown in front of each key in the shortcut list.
    var symbol: String {
        switch self {
        case .rightCommandOption: "правые ⌘⌥"
        case .controlOptionCommand: "⌃⌥⌘"
        case .rightShift: "правый ⇧"
        case .off: ""
        }
    }

    var mask: [String] {
        switch self {
        case .rightCommandOption: ["KMOD_RGUI", "KMOD_RALT"]
        case .controlOptionCommand: ["KMOD_LCTRL", "KMOD_LALT", "KMOD_LGUI"]
        case .rightShift: ["KMOD_RSHIFT"]
        case .off: ["KMOD_NONE"]
        }
    }

    init?(mask: [String]) {
        let normalized = Set(mask.map { $0.replacingOccurrences(of: "SDL_KMOD_", with: "KMOD_") })
        guard let match = Self.allCases.first(where: { Set($0.mask) == normalized }) else { return nil }
        self = match
    }
}

/// FreeRDP's SDL client shortcuts, with the keys it uses by default.
let freerdpShortcuts: [(key: String, action: String)] = [
    ("Enter", "полный экран / окно"),
    ("G", "захват клавиатуры вкл./выкл."),
    ("R", "окно меняет размер вкл./выкл."),
    ("M", "свернуть окно"),
    ("D", "отключиться"),
]

/// Reads and writes the per-user sdl-freerdp.json, keeping keys we do not own.
///
/// FreeRDP looks it up with freerdp_GetConfigFilePath(false, "sdl-freerdp.json"):
/// $XDG_CONFIG_HOME/freerdp/sdl-freerdp.json, or ~/.config/freerdp/sdl-freerdp.json
/// (`sdl-freerdp /help` prints the location it uses).
enum HotkeyConfig {
    static var url: URL {
        let base: URL
        if let xdg = ProcessInfo.processInfo.environment["XDG_CONFIG_HOME"], !xdg.isEmpty {
            base = URL(fileURLWithPath: xdg, isDirectory: true)
        } else {
            base = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".config", isDirectory: true)
        }
        return base.appendingPathComponent("freerdp/sdl-freerdp.json")
    }

    private static func readObject() -> [String: Any] {
        guard let data = try? Data(contentsOf: url),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return [:] }
        return object
    }

    /// The modifier in the file, nil when the file leaves FreeRDP's default
    /// (right Shift) or holds a combination we do not offer.
    static func current() -> HotkeyModifier? {
        guard let mask = readObject()["SDL_KeyModMask"] as? [String] else { return nil }
        return HotkeyModifier(mask: mask)
    }

    static func write(_ modifier: HotkeyModifier) throws {
        var object = readObject()
        object["SDL_KeyModMask"] = modifier.mask
        let data = try JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys])
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try data.write(to: url, options: .atomic)
    }
}
