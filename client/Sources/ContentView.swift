import AppKit
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var store: ServerStore
    @State private var selection: UUID?
    @State private var editing: Server?

    var body: some View {
        SplitView {
            connectOnDoubleClick(List(selection: $selection) {
                ForEach(store.servers) { server in
                    legacyDoubleClick(ServerRow(server: server, session: model.session(for: server), status: model.status(of: server))
                        .tag(server.id)
                        .contextMenu { menu(for: server) }, server: server)
                }
                .onMove { store.move(from: $0, to: $1) }
            })
            .toolbar {
                ToolbarItem {
                    Button { editing = Server() } label: { Label("Добавить Mac", systemImage: "plus") }
                }
            }
        } detail: {
            VStack(spacing: 0) {
                if model.binary == nil { MissingFreeRDPBanner() }
                UpdateBanner()
                if let server = store.servers.first(where: { $0.id == selection }) {
                    ServerDetail(server: server, edit: { editing = server })
                } else {
                    EmptyDetail(add: { editing = Server() })
                }
            }
        }
        .sheet(item: $editing) { server in
            ServerEditor(server: server) { saved in
                store.save(saved)
                selection = saved.id
            }
        }
        .messageAlert("Не удалось подключиться", message: model.launchError) { model.launchError = nil }
        .onAppear { if selection == nil { selection = store.servers.first?.id } }
    }

    /// A double click on a row connects to that Mac.
    @ViewBuilder private func connectOnDoubleClick(_ list: some View) -> some View {
        if #available(macOS 13.0, *) {
            list.contextMenu(forSelectionType: UUID.self) { _ in } primaryAction: { ids in
                ids.compactMap { id in store.servers.first { $0.id == id } }.forEach(model.connect)
            }
        } else {
            list
        }
    }

    @ViewBuilder private func legacyDoubleClick(_ row: some View, server: Server) -> some View {
        if #available(macOS 13.0, *) {
            row
        } else {
            row.simultaneousGesture(TapGesture(count: 2).onEnded { model.connect(server) })
        }
    }

    @ViewBuilder private func menu(for server: Server) -> some View {
        Button("Подключиться") { model.connect(server) }
        Button("Изменить…") { editing = server }
        Divider()
        Button("Разблокировать FileVault") { model.unlock(server) }
            .disabled(model.status(of: server) != .locked)
        Button("Разбудить") { model.wake(server) }
            .disabled(Host.macBytes(server.wakeAddress) == nil)
        Button("Общий экран") { Host.openScreenSharing(server) }
        Divider()
        destructiveButton("Удалить") { store.delete(server) }
    }
}

private struct ServerRow: View {
    let server: Server
    let status: HostStatus
    @ObservedObject private var sessionBox: SessionBox

    init(server: Server, session: Session?, status: HostStatus) {
        self.server = server
        self.status = status
        sessionBox = SessionBox(session)
    }

    var body: some View {
        HStack {
            Image(systemName: "desktopcomputer")
                .fg(sessionBox.session?.isRunning == true ? Color.green : Color.secondary)
            VStack(alignment: .leading) {
                Text(server.displayName)
                Text(server.user.isEmpty ? server.address : "\(server.user)@\(server.address)")
                    .font(.caption).fg(.secondary)
            }
            Spacer()
            StatusDot(status: status)
        }
        .padding(.vertical, 2)
    }
}

extension HostStatus {
    var color: Color {
        switch self {
        case .unknown: .gray
        case .online: .green
        case .locked: .yellow
        case .offline: .red
        }
    }

    var title: String {
        switch self {
        case .unknown: "Проверяю…"
        case .online: "В сети"
        case .locked: "Ждёт разблокировки FileVault"
        case .offline: "Не отвечает"
        }
    }

    var explanation: String {
        switch self {
        case .unknown: ""
        case .online: "RDP отвечает. Если на Mac никто не вошёл, после подключения будет окно входа."
        case .locked: "Отвечает только SSH: после включения Mac ждёт пароль FileVault или ещё загружается."
        case .offline: "Не отвечают ни RDP, ни SSH: Mac выключен, спит или не в этой сети."
        }
    }
}

private struct StatusDot: View {
    let status: HostStatus

    var body: some View {
        Circle()
            .fill(status.color)
            .frame(width: 9, height: 9)
            .help(status.title)
    }
}

/// The Mac's state on the network and what can be done about it.
private struct HostPanel: View {
    @EnvironmentObject private var model: AppModel
    let server: Server

    private var status: HostStatus { model.status(of: server) }
    private var activity: HostActivity? { model.activity[server.id] }
    private var hasMacAccount: Bool { !server.macUser.isEmpty && Keychain.password(for: server.id, .mac) != nil }
    private var canWake: Bool { Host.macBytes(server.wakeAddress) != nil }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                StatusDot(status: status)
                Text(status.title).font(.headline)
                Spacer()
                if status != .online && status != .unknown {
                    Button("Проверить") { Task { await model.refreshStatuses() } }
                }
            }
            if !status.explanation.isEmpty {
                Text(status.explanation).font(.callout).fg(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let activity { ActivityLine(activity: activity, server: server) }
            HStack(spacing: 10) {
                if status == .locked {
                    Button { model.unlock(server) } label: {
                        Label("Разблокировать", systemImage: "lock.open")
                    }
                    .disabled(!hasMacAccount || activity?.isWaiting == true)
                }
                if status == .offline && canWake {
                    Button { model.wake(server) } label: {
                        Label("Разбудить", systemImage: "power")
                    }
                    .disabled(activity?.isWaiting == true)
                }
                if status == .online {
                    Button { Host.openScreenSharing(server) } label: {
                        Label("Общий экран", systemImage: "rectangle.on.rectangle")
                    }
                    .help("Запасной путь: войти на Mac через «Общий экран», если окно входа по RDP не открывается.")
                }
            }
            if status == .locked && !hasMacAccount {
                Text("Чтобы разблокировать отсюда, укажите пользователя и пароль Mac в «Изменить…».")
                    .font(.caption).fg(.secondary)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(status.color.opacity(0.08)))
    }
}

private struct ActivityLine: View {
    @EnvironmentObject private var model: AppModel
    let activity: HostActivity
    let server: Server

    var body: some View {
        switch activity {
        case .unlocking:
            HStack { ProgressView().controlSize(.small); Text("Отправляю пароль FileVault по SSH…") }
        case .starting:
            HStack { ProgressView().controlSize(.small); Text("Диск разблокирован, macOS загружается. Подключиться можно, когда точка станет зелёной.") }
        case .waking:
            HStack { ProgressView().controlSize(.small); Text("Пакет Wake-on-LAN отправлен, жду ответа…") }
        case .failed(let problem):
            VStack(alignment: .leading, spacing: 6) {
                Label(problem, systemImage: "exclamationmark.triangle.fill").fg(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                HStack {
                    if problem == Host.UnlockError.hostKeyChanged.localizedDescription {
                        Button("Забыть ключ SSH") { model.forgetHostKey(server) }
                    }
                    Button("Общий экран") { Host.openScreenSharing(server) }
                    Button("Скрыть") { model.clearActivity(server) }
                }
            }
        }
    }
}

/// Lets a view observe a session that may not exist yet.
private final class SessionBox: ObservableObject {
    let session: Session?
    init(_ session: Session?) { self.session = session }
}

private struct ServerDetail: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var store: ServerStore
    let server: Server
    let edit: () -> Void

    private var session: Session? { model.session(for: server) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                HStack(alignment: .firstTextBaseline) {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(server.displayName).font(.title.bold())
                        Text(server.user.isEmpty ? server.address : "\(server.user)@\(server.address)")
                            .fg(.secondary).selectable()
                    }
                    Spacer()
                    Button("Изменить…", action: edit)
                }

                HostPanel(server: server)

                HStack(spacing: 12) {
                    if session?.isRunning == true {
                        Button { model.disconnect(server) } label: {
                            Label("Отключиться", systemImage: "xmark.circle").frame(minWidth: 140)
                        }
                        .controlSize(.large)
                        Label("Сеанс открыт", systemImage: "circle.fill")
                            .fg(.green).titleAndIconLabel()
                    } else {
                        Button { model.connect(server) } label: {
                            Label("Подключиться", systemImage: "play.fill").frame(minWidth: 140)
                        }
                        .controlSize(.large)
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.binary == nil)
                    }
                    Spacer()
                    if session != nil {
                        Button("Журнал") { model.showLog(for: server) }
                    }
                }

                if case .ended(let problem?) = session?.state {
                    VStack(alignment: .leading, spacing: 8) {
                        Label(problem, systemImage: "exclamationmark.triangle.fill")
                            .fg(.orange)
                        if problem == FreeRDP.certificateProblem {
                            Button("Забыть сертификат сервера") { model.forgetCertificate(for: server) }
                        }
                    }
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color.orange.opacity(0.1)))
                }

                GroupBox(label: Text("Параметры")) {
                    PairGrid(rows: parameters.map { (id: $0.0, left: Text($0.0).foregroundColor(.secondary), right: Text($0.1)) })
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(6)
                }

                HotkeyList()
            }
            .padding(24)
            .frame(maxWidth: 640, alignment: .leading)
        }
    }

    private var parameters: [(String, String)] {
        var rows = [
            ("Видео", server.codec.title),
            ("Окно", server.fullscreen ? "Во весь экран"
                : server.windowWidth.map { "\($0)×\(server.windowHeight ?? 0)" } ?? "Размер по умолчанию"),
            ("Сертификат", server.certificate.title),
            ("Буфер обмена", server.clipboard ? "Общий" : "Выключен"),
            ("Экран Мака", server.curtain ? "Гаснет во время сессии" : "Не гаснет"),
            ("После отключения", server.keepForReconnect ? "Ждёт переподключения 5 минут" : "Сеанс убирается сразу"),
        ]
        if !server.extraArgumentList.isEmpty {
            rows.append(("Дополнительно", server.extraArgumentList.joined(separator: " ")))
        }
        return rows
    }
}

/// FreeRDP's own shortcuts inside a session, with the modifier chosen in Settings.
struct HotkeyList: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        GroupBox(label: Text("Сочетания FreeRDP в сеансе")) {
            VStack(alignment: .leading, spacing: 6) {
                if model.hotkeyModifier == .off {
                    Text("Выключены в настройках.").fg(.secondary)
                } else {
                    PairGrid(rows: freerdpShortcuts.map { shortcut in
                        (id: shortcut.key,
                         left: Text("\(model.hotkeyModifier.symbol) + \(shortcut.key)").font(.system(.body, design: .monospaced)),
                         right: Text(shortcut.action))
                    }, verticalSpacing: 4)
                }
                if model.passSystemShortcuts {
                    Text("⌘Пробел, ⌘Tab, ⌘Q и другие системные сочетания уходят на удалённый Mac, пока окно сеанса активно и курсор над ним\(model.localControlShortcuts ? " (сочетания с одним ⌃, как ⌃← и ⌃→, остаются этому Mac)" : ""). Вернуть их этому Mac: модификатор + G.")
                        .font(.caption).fg(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(6)
        }
    }
}

private struct EmptyDetail: View {
    @EnvironmentObject private var store: ServerStore
    let add: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "desktopcomputer").font(.system(size: 48)).fg(.secondary)
            Text(store.servers.isEmpty ? "Нет сохранённых Mac" : "Выберите Mac слева").font(.title3)
            if store.servers.isEmpty {
                Button("Добавить Mac…", action: add).controlSize(.large)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// Shown when the launch check found a newer client.
private struct UpdateBanner: View {
    @EnvironmentObject private var updates: Updates

    var body: some View {
        switch updates.state {
        case .newer(let available):
            banner("Доступна версия \(available.version)")
        case .updating(let version):
            banner("Обновляю до \(version)…")
        default:
            EmptyView()
        }
    }

    private func banner(_ title: String) -> some View {
        HStack(spacing: 10) {
                Image(systemName: "arrow.down.circle.fill").fg(.blue).font(.title2)
                Text(title).font(.headline)
                Spacer()
                UpdateControl()
            }
            .padding(12)
            .background(Color.blue.opacity(0.1))
    }
}

/// The check or download button and what came of it.
struct UpdateControl: View {
    @EnvironmentObject private var updates: Updates
    @EnvironmentObject private var model: AppModel

    var body: some View {
        HStack(spacing: 8) {
            switch updates.state {
            case .idle:
                Button("Проверить обновления") { updates.check() }
            case .checking:
                ProgressView().controlSize(.small)
                Text("Проверяю…").fg(.secondary)
            case .current:
                Label("Последняя версия", systemImage: "checkmark.circle.fill").fg(.green)
                Button("Проверить ещё раз") { updates.check() }
            case .newer(let available):
                Button("Обновить до \(available.version)") { updates.update(sessionsOpen: model.runningCount > 0) }
                    .prominentButton()
            case .updating:
                ProgressView().controlSize(.small)
                Text("Скачиваю и проверяю…").fg(.secondary)
            case .failed(let problem):
                Text(problem).fg(.orange).font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Повторить") { updates.check() }
            }
        }
    }
}

private struct MissingFreeRDPBanner: View {
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.octagon.fill").fg(.red).font(.title2)
            VStack(alignment: .leading, spacing: 6) {
                Text("sdl-freerdp не найден").font(.headline)
                Text("Искали в /usr/local/bin (Homebrew на Intel) и /opt/homebrew/bin (Apple Silicon). Установите FreeRDP через Homebrew или укажите путь в настройках.")
                    .fixedSize(horizontal: false, vertical: true)
                HStack {
                    Text("brew install freerdp").font(.system(.body, design: .monospaced)).selectable()
                    Button("Скопировать") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString("brew install freerdp", forType: .string)
                    }
                    if #available(macOS 14, *) {
                        SettingsLink { Text("Настройки…") }
                    } else {
                        Button("Настройки…") { NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil) }
                    }
                }
            }
            Spacer()
        }
        .padding(14)
        .background(Color.red.opacity(0.1))
    }
}
