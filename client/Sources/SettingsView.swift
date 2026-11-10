import AppKit
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var updates: Updates

    var body: some View {
        Form {
            Section {
                LabeledContent("OpenRDP Client \(Updates.current)") {
                    UpdateControl()
                }
                Picker("Канал", selection: $updates.channel) {
                    ForEach(Updates.Channel.allCases) { Text($0.title).tag($0) }
                }
            } header: {
                Text("Обновления")
            } footer: {
                Text("Новая версия проверяется и при запуске. «Обновить» скачивает её, сверяет контрольную сумму, ставит на место этой и перезапускает клиент. Бета — новые версии раньше всех, для проверки; с беты на стабильные клиент перейдёт, когда выйдет следующая стабильная версия.")
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Section("FreeRDP") {
                if FreeRDP.bundled != nil {
                    Toggle("Встроенный FreeRDP", isOn: $model.preferBundled)
                }
                LabeledContent("sdl-freerdp") {
                    if let binary = model.binary {
                        Text(binary.path + (model.version.map { "  (\($0))" } ?? ""))
                            .textSelection(.enabled)
                    } else {
                        Text("не найден").foregroundStyle(.red)
                    }
                }
                if !model.isBundled {
                    HStack {
                        TextField("Свой путь", text: $model.customBinaryPath,
                                  prompt: Text(FreeRDP.candidates.joined(separator: " или ")))
                        Button("Выбрать…", action: choose)
                    }
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

            Section {
                Toggle("Обратная прокрутка", isOn: $model.reverseScroll)
            } header: {
                Text("Трекпад и мышь")
            } footer: {
                Text("Прокрутка в сеансе идёт в другую сторону, вверх‑вниз и влево‑вправо, для трекпада и мыши. Делает сервер OpenRDP; действует со следующего подключения.")
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
