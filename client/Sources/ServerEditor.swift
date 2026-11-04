import SwiftUI

struct ServerEditor: View {
    @Environment(\.dismiss) private var dismiss
    @State private var server: Server
    @State private var password: String
    @State private var macPassword: String
    @State private var customSize: Bool
    private let isNew: Bool
    private let onSave: (Server) -> Void

    init(server: Server, onSave: @escaping (Server) -> Void) {
        _server = State(initialValue: server)
        _password = State(initialValue: Keychain.password(for: server.id) ?? "")
        _macPassword = State(initialValue: Keychain.password(for: server.id, .mac) ?? "")
        _customSize = State(initialValue: server.windowWidth != nil)
        isNew = server.host.isEmpty
        self.onSave = onSave
    }

    private var problem: String? {
        if server.host.trimmingCharacters(in: .whitespaces).isEmpty { return "Укажите адрес." }
        if !(1...65535).contains(server.port) { return "Порт должен быть от 1 до 65535." }
        if password.contains(where: \.isNewline) || macPassword.contains(where: \.isNewline) {
            return "Пароль не может содержать перевод строки."
        }
        if !(1...65535).contains(server.sshPort) { return "Порт SSH должен быть от 1 до 65535." }
        if !server.wakeAddress.isEmpty && Host.macBytes(server.wakeAddress) == nil {
            return "MAC-адрес вида a4:83:e7:12:34:56."
        }
        return nil
    }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    TextField("Имя", text: $server.name, prompt: Text("Mac-Ivan"))
                    TextField("Адрес", text: $server.host, prompt: Text("192.168.2.100 или mac.local"))
                    TextField("Порт", value: $server.port, format: .number.grouping(.never))
                    TextField("Пользователь", text: $server.user)
                    SecureField("Пароль", text: $password, prompt: Text("хранится в Связке ключей"))
                }
                Section {
                    TextField("Пользователь Mac", text: $server.macUser, prompt: Text("учётная запись macOS"))
                    SecureField("Пароль Mac", text: $macPassword, prompt: Text("хранится в Связке ключей"))
                    TextField("Порт SSH", value: $server.sshPort, format: .number.grouping(.never))
                    TextField("MAC-адрес", text: $server.wakeAddress, prompt: Text("узнается сам, когда Mac в сети"))
                } header: {
                    Text("После включения")
                } footer: {
                    Text("После пропадания питания Mac с FileVault ждёт пароль, и RDP недоступен. Клиент разблокирует диск по SSH (macOS 26 и новее, включите «Удалённый вход»), будит Mac по Wake-on-LAN и входит в «Общий экран» этой учётной записью.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section("Сеанс") {
                    Picker("Видео", selection: $server.codec) {
                        ForEach(Server.Codec.allCases) { Text($0.title).tag($0) }
                    }
                    Toggle("Во весь экран", isOn: $server.fullscreen)
                    if !server.fullscreen {
                        Toggle("Задать размер окна", isOn: $customSize)
                        if customSize {
                            LabeledContent("Размер") {
                                HStack {
                                    TextField("Ширина", value: $server.windowWidth, format: .number.grouping(.never))
                                    Text("×")
                                    TextField("Высота", value: $server.windowHeight, format: .number.grouping(.never))
                                }
                                .labelsHidden()
                                .frame(width: 160)
                            }
                        }
                    }
                    Toggle("Общий буфер обмена", isOn: $server.clipboard)
                    Toggle("Гасить экран Мака во время сессии", isOn: $server.curtain)
                    Picker("Сертификат", selection: $server.certificate) {
                        ForEach(Server.Certificate.allCases) { Text($0.title).tag($0) }
                    }
                }
                Section {
                    TextEditor(text: $server.extraArguments)
                        .font(.body.monospaced())
                        .frame(height: 60)
                } header: {
                    Text("Дополнительные параметры FreeRDP")
                } footer: {
                    Text("По одному на строку, например /network:lan. Добавляются после /v /u /gfx:\(server.codec.rawValue) /dynamic-resolution /cert:\(server.certificate.rawValue).")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            .formStyle(.grouped)

            HStack {
                if let problem { Text(problem).foregroundStyle(.secondary).font(.callout) }
                Spacer()
                Button("Отмена") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(isNew ? "Добавить" : "Сохранить") { save() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(problem != nil)
            }
            .padding()
        }
        .frame(width: 480, height: 680)
    }

    private func save() {
        server.host = server.host.trimmingCharacters(in: .whitespaces)
        server.user = server.user.trimmingCharacters(in: .whitespaces)
        server.macUser = server.macUser.trimmingCharacters(in: .whitespaces)
        server.wakeAddress = server.wakeAddress.trimmingCharacters(in: .whitespaces).lowercased()
        if !customSize {
            server.windowWidth = nil
            server.windowHeight = nil
        }
        Keychain.setPassword(password, for: server.id)
        Keychain.setPassword(macPassword, for: server.id, .mac)
        onSave(server)
        dismiss()
    }
}
