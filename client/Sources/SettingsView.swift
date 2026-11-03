import AppKit
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        Form {
            Section("FreeRDP") {
                LabeledContent("sdl-freerdp") {
                    if let binary = model.binary {
                        Text(binary.path + (model.version.map { "  (\($0))" } ?? ""))
                            .textSelection(.enabled)
                    } else {
                        Text("не найден").foregroundStyle(.red)
                    }
                }
                HStack {
                    TextField("Свой путь", text: $model.customBinaryPath,
                              prompt: Text(FreeRDP.candidates.joined(separator: " или ")))
                    Button("Выбрать…", action: choose)
                }
            }

            Section {
                Picker("Модификатор", selection: $model.hotkeyModifier) {
                    ForEach(HotkeyModifier.allCases) { Text($0.title).tag($0) }
                }
                if let error = model.hotkeyError {
                    Text(error).foregroundStyle(.red).font(.caption)
                }
            } header: {
                Text("Сочетания FreeRDP")
            } footer: {
                Text("FreeRDP по умолчанию держит правый Shift, поэтому заглавные П, К, В, Ь с правым Shift не набираются, а правый Shift+В отключает сеанс. Записывается в \(HotkeyConfig.url.path).")
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Section {
                Toggle("Передавать системные сочетания в сеанс", isOn: $model.passSystemShortcuts)
            } footer: {
                Text("Пока окно сеанса активно, ⌘Пробел (смена раскладки), ⌘Tab, ⌘Q, ⌘H, ⌘W уходят на удалённый Mac, а не обрабатываются этим. Действует со следующего подключения.")
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Section { HotkeyList() }
        }
        .formStyle(.grouped)
        .padding(.vertical, 8)
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = false
        panel.directoryURL = URL(fileURLWithPath: "/usr/local/bin")
        panel.treatsFilePackagesAsDirectories = true
        if panel.runModal() == .OK, let url = panel.url {
            model.customBinaryPath = url.path
        }
    }
}
