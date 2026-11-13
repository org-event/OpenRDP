import AppKit
import SwiftUI

@main
enum Main {
    static func main() {
        if #available(macOS 13.0, *) {
            OpenRDPClientApp.main()
        } else {
            LegacyClientApp.main()
        }
    }
}

/// The main window's content, the same in both apps.
private struct MainWindow: View {
    let model: AppModel
    let delegate: AppDelegate

    var body: some View {
        ContentView()
            .environmentObject(model)
            .environmentObject(model.store)
            .environmentObject(model.updates)
            .frame(minWidth: 680, minHeight: 420)
            .onAppear { delegate.model = model }
    }
}

private struct SettingsWindow: View {
    let model: AppModel

    var body: some View {
        SettingsView()
            .environmentObject(model)
            .environmentObject(model.updates)
            .frame(width: 560)
    }
}

@available(macOS 13.0, *)
struct OpenRDPClientApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel()

    var body: some Scene {
        Window("OpenRDP Client", id: "main") {
            MainWindow(model: model, delegate: delegate)
        }
        .defaultSize(width: 860, height: 620)
        .commands {
            CommandGroup(replacing: .newItem) {}
        }

        Settings { SettingsWindow(model: model) }
    }
}

/// macOS 11 and 12 have no single-window scene: a WindowGroup without File > New.
struct LegacyClientApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup("OpenRDP Client") {
            MainWindow(model: model, delegate: delegate)
                .frame(idealWidth: 860, idealHeight: 620)
        }
        .commands {
            CommandGroup(replacing: .newItem) {}
        }

        Settings { SettingsWindow(model: model) }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    weak var model: AppModel?

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        MainActor.assumeIsolated { model?.showClient() }
        return true
    }

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
