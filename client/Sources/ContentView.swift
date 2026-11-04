import AppKit
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var store: ServerStore
    @State private var selection: UUID?
    @State private var editing: Server?

    var body: some View {
        NavigationSplitView {
            List(selection: $selection) {
                ForEach(store.servers) { server in
                    ServerRow(server: server, session: model.session(for: server), status: model.status(of: server))
                        .tag(server.id)
                        .contextMenu { menu(for: server) }
                }
                .onMove { store.move(from: $0, to: $1) }
            }
            .navigationSplitViewColumnWidth(min: 200, ideal: 230)
            .toolbar {
                ToolbarItem {
                    Button { editing = Server() } label: { Label("Добавить Mac", systemImage: "plus") }
                }
            }
            .contextMenu(forSelectionType: UUID.self) { _ in } primaryAction: { ids in
                ids.compactMap { id in store.servers.first { $0.id == id } }.forEach(model.connect)
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
        .alert("Не удалось подключиться", isPresented: Binding(
            get: { model.launchError != nil }, set: { if !$0 { model.launchError = nil } })
        ) {
            Button("OK") {}
        } message: {
            Text(model.launchError ?? "")
        }
        .onAppear { if selection == nil { selection = store.servers.first?.id } }
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
        Button("Удалить", role: .destructive) { store.delete(server) }
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
                .foregroundStyle(sessionBox.session?.isRunning == true ? Color.green : Color.secondary)
            VStack(alignment: .leading) {
                Text(server.displayName)
                Text(server.user.isEmpty ? server.address : "\(server.user)@\(server.address)")
                    .font(.caption).foregroundStyle(.secondary)
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
                Text(status.explanation).font(.callout).foregroundStyle(.secondary)
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
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(status.color.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
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
                Label(problem, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
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
                            .foregroundStyle(.secondary).textSelection(.enabled)
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
                            .foregroundStyle(.green).labelStyle(.titleAndIcon)
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
                            .foregroundStyle(.orange)
                        if problem == FreeRDP.certificateProblem {
                            Button("Забыть сертификат сервера") { model.forgetCertificate(for: server) }
                        }
                    }
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.orange.opacity(0.1), in: RoundedRectangle(cornerRadius: 8))
                }

                GroupBox("Параметры") {
                    Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 6) {
                        row("Видео", server.codec.title)
                        row("Окно", server.fullscreen ? "Во весь экран"
                            : server.windowWidth.map { "\($0)×\(server.windowHeight ?? 0)" } ?? "Размер по умолчанию")
                        row("Сертификат", server.certificate.title)
                        row("Буфер обмена", server.clipboard ? "Общий" : "Выключен")
                        row("Экран Мака", server.curtain ? "Гаснет во время сессии" : "Не гаснет")
                        if !server.extraArgumentList.isEmpty {
                            row("Дополнительно", server.extraArgumentList.joined(separator: " "))
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(6)
                }

                HotkeyList()
            }
            .padding(24)
            .frame(maxWidth: 640, alignment: .leading)
        }
    }

    private func row(_ title: String, _ value: String) -> some View {
        GridRow {
            Text(title).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }
}

/// FreeRDP's own shortcuts inside a session, with the modifier chosen in Settings.
struct HotkeyList: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        GroupBox("Сочетания FreeRDP в сеансе") {
            VStack(alignment: .leading, spacing: 6) {
                if model.hotkeyModifier == .off {
                    Text("Выключены в настройках.").foregroundStyle(.secondary)
                } else {
                    Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 4) {
                        ForEach(freerdpShortcuts, id: \.key) { shortcut in
                            GridRow {
                                Text("\(model.hotkeyModifier.symbol) + \(shortcut.key)")
                                    .font(.body.monospaced())
                                Text(shortcut.action)
                            }
                        }
                    }
                }
                if model.passSystemShortcuts {
                    Text("⌘Пробел, ⌘Tab, ⌘Q и другие системные сочетания уходят на удалённый Mac, пока окно сеанса активно и курсор над ним. Вернуть их этому Mac: модификатор + G.")
                        .font(.caption).foregroundStyle(.secondary)
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
            Image(systemName: "desktopcomputer").font(.system(size: 48)).foregroundStyle(.secondary)
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
                Image(systemName: "arrow.down.circle.fill").foregroundStyle(.blue).font(.title2)
                Text(title).font(.headline)
                Spacer()
                UpdateControl()
            }
            .padding(12)
            .background(.blue.opacity(0.1))
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
                Text("Проверяю…").foregroundStyle(.secondary)
            case .current:
                Label("Последняя версия", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                Button("Проверить ещё раз") { updates.check() }
            case .newer(let available):
                Button("Обновить до \(available.version)") { updates.update(sessionsOpen: model.runningCount > 0) }
                    .buttonStyle(.borderedProminent)
            case .updating:
                ProgressView().controlSize(.small)
                Text("Скачиваю и проверяю…").foregroundStyle(.secondary)
            case .failed(let problem):
                Text(problem).foregroundStyle(.orange).font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Повторить") { updates.check() }
            }
        }
    }
}

private struct MissingFreeRDPBanner: View {
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.octagon.fill").foregroundStyle(.red).font(.title2)
            VStack(alignment: .leading, spacing: 6) {
                Text("sdl-freerdp не найден").font(.headline)
                Text("Искали в /usr/local/bin (Homebrew на Intel) и /opt/homebrew/bin (Apple Silicon). Установите FreeRDP через Homebrew или укажите путь в настройках.")
                    .fixedSize(horizontal: false, vertical: true)
                HStack {
                    Text("brew install freerdp").font(.body.monospaced()).textSelection(.enabled)
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
        .background(.red.opacity(0.1))
    }
}
