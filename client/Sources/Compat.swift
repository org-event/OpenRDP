import SwiftUI

// What macOS 11 and 12 lack, wrapped so that macOS 13 and later keep running the
// newer SwiftUI exactly as before; only the older systems take the fallbacks.

extension View {
    /// foregroundStyle where it exists, foregroundColor before macOS 12.
    @ViewBuilder func fg(_ color: Color) -> some View {
        if #available(macOS 12.0, *) {
            foregroundStyle(color)
        } else {
            foregroundColor(color)
        }
    }

    /// Text the user can select and copy (macOS 12 and later).
    @ViewBuilder func selectable() -> some View {
        if #available(macOS 12.0, *) {
            textSelection(.enabled)
        } else {
            self
        }
    }

    @ViewBuilder func prominentButton() -> some View {
        if #available(macOS 12.0, *) {
            buttonStyle(.borderedProminent)
        } else {
            self
        }
    }

    @ViewBuilder func titleAndIconLabel() -> some View {
        if #available(macOS 11.3, *) {
            labelStyle(.titleAndIcon)
        } else {
            self
        }
    }

    /// An alert with one OK button and a message.
    @ViewBuilder func messageAlert(_ title: String, message: String?, dismiss: @escaping () -> Void) -> some View {
        let shown = Binding(get: { message != nil }, set: { if !$0 { dismiss() } })
        if #available(macOS 12.0, *) {
            alert(title, isPresented: shown) {
                Button("OK") {}
            } message: {
                Text(message ?? "")
            }
        } else {
            alert(isPresented: shown) {
                Alert(title: Text(title), message: Text(message ?? ""), dismissButton: .default(Text("OK")))
            }
        }
    }
}

/// A button shown in red where roles exist.
@ViewBuilder func destructiveButton(_ title: String, action: @escaping () -> Void) -> some View {
    if #available(macOS 12.0, *) {
        Button(title, role: .destructive, action: action)
    } else {
        Button(title, action: action)
    }
}

/// Two columns of text: a Grid on macOS 13 and later, rows of a fixed-width
/// first column before it.
struct PairGrid: View {
    let rows: [(id: String, left: Text, right: Text)]
    var verticalSpacing: CGFloat = 6
    var legacyLeftWidth: CGFloat = 150

    var body: some View {
        if #available(macOS 13.0, *) {
            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: verticalSpacing) {
                ForEach(rows, id: \.id) { row in
                    GridRow {
                        row.left
                        row.right.selectable()
                    }
                }
            }
        } else {
            VStack(alignment: .leading, spacing: verticalSpacing) {
                ForEach(rows, id: \.id) { row in
                    HStack(alignment: .firstTextBaseline, spacing: 16) {
                        row.left.frame(width: legacyLeftWidth, alignment: .leading)
                        row.right.selectable()
                    }
                }
            }
        }
    }
}

/// A sidebar and a detail pane: NavigationSplitView on macOS 13 and later,
/// NavigationView before it.
struct SplitView<Sidebar: View, Detail: View>: View {
    @ViewBuilder let sidebar: () -> Sidebar
    @ViewBuilder let detail: () -> Detail

    var body: some View {
        if #available(macOS 13.0, *) {
            NavigationSplitView {
                sidebar().navigationSplitViewColumnWidth(min: 200, ideal: 230)
            } detail: {
                detail()
            }
        } else {
            NavigationView {
                sidebar().frame(minWidth: 200, idealWidth: 230)
                detail()
            }
        }
    }
}

extension View {
    @ViewBuilder func groupedForm() -> some View {
        if #available(macOS 13.0, *) {
            formStyle(.grouped)
        } else {
            self
        }
    }
}

/// A text field with a grey hint inside where prompts exist (macOS 12 and later).
@ViewBuilder func promptField(_ title: String, text: Binding<String>, prompt: String) -> some View {
    if #available(macOS 12.0, *) {
        TextField(title, text: text, prompt: Text(prompt))
    } else {
        TextField(title, text: text)
    }
}

@ViewBuilder func promptSecureField(_ title: String, text: Binding<String>, prompt: String) -> some View {
    if #available(macOS 12.0, *) {
        SecureField(title, text: text, prompt: Text(prompt))
    } else {
        SecureField(title, text: text)
    }
}

/// A whole number without thousands separators.
@ViewBuilder func numberField(_ title: String, value: Binding<Int>) -> some View {
    if #available(macOS 12.0, *) {
        TextField(title, value: value, format: .number.grouping(.never))
    } else {
        TextField(title, text: Binding(get: { String(value.wrappedValue) },
                                       set: { if let number = Int($0) { value.wrappedValue = number } }))
    }
}

@ViewBuilder func numberField(_ title: String, value: Binding<Int?>) -> some View {
    if #available(macOS 12.0, *) {
        TextField(title, value: value, format: .number.grouping(.never))
    } else {
        TextField(title, text: Binding(get: { value.wrappedValue.map(String.init) ?? "" },
                                       set: { value.wrappedValue = Int($0) }))
    }
}

/// A form row with a title: LabeledContent on macOS 13 and later.
struct LabeledRow<Content: View>: View {
    let title: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        if #available(macOS 13.0, *) {
            LabeledContent(title, content: content)
        } else {
            HStack {
                Text(title)
                Spacer()
                content()
            }
        }
    }
}
