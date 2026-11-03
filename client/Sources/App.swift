import AppKit
import SwiftUI

@main
struct OpenRDPClientApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel()

    var body: some Scene {
        Window("OpenRDP Client", id: "main") {
            ContentView()
                .environmentObject(model)
                .environmentObject(model.store)
                .frame(minWidth: 680, minHeight: 420)
                .onAppear { delegate.model = model }
        }
        .defaultSize(width: 860, height: 620)
        .commands {
            CommandGroup(replacing: .newItem) {}
        }

        Settings {
            SettingsView()
                .environmentObject(model)
                .frame(width: 560)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    weak var model: AppModel?

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        MainActor.assumeIsolated { model?.runningCount ?? 0 } == 0
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let model = MainActor.assumeIsolated({ model }), MainActor.assumeIsolated({ model.runningCount }) > 0 else {
            return .terminateNow
        }
        let alert = NSAlert()
        alert.messageText = "Закрыть открытые сеансы?"
        alert.informativeText = "Сеансы sdl-freerdp будут отключены."
        alert.addButton(withTitle: "Отключить и выйти")
        alert.addButton(withTitle: "Отмена")
        guard alert.runModal() == .alertFirstButtonReturn else { return .terminateCancel }
        MainActor.assumeIsolated { model.disconnectAll() }
        return .terminateNow
    }
}
