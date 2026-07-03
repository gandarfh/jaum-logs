import JaumKit
import SwiftUI

/// Read-only mirror of the daemon's TUI frame buffer, useful to confirm the
/// socket wiring against a live daemon while the native views take over.
struct TerminalMirrorView: View {
    @Bindable var terminal: TerminalModel

    var body: some View {
        VStack(spacing: 0) {
            if terminal.grid.isEmpty {
                ContentUnavailableView {
                    Label("Sem sessão de terminal", systemImage: "terminal")
                } description: {
                    Text(description)
                } actions: {
                    Button(actionTitle) {
                        Task {
                            if terminal.connection == .connected {
                                await terminal.detach()
                            } else {
                                await terminal.attach()
                            }
                        }
                    }
                }
            } else {
                ScrollView([.horizontal, .vertical]) {
                    Text(terminal.grid.textRows().joined(separator: "\n"))
                        .font(.body.monospaced())
                        .textSelection(.enabled)
                        .padding()
                }
            }

            Divider()

            HStack {
                ConnectionBadge(state: terminal.connection)
                if let error = terminal.lastError {
                    Text(error)
                        .font(.footnote)
                        .foregroundStyle(.red)
                        .lineLimit(1)
                }
                Spacer()
                Button(actionTitle) {
                    Task {
                        if terminal.connection == .connected {
                            await terminal.detach()
                        } else {
                            await terminal.attach()
                        }
                    }
                }
            }
            .padding(8)
        }
        .navigationTitle("Terminal")
    }

    private var description: String {
        if let error = terminal.lastError {
            return "Não conectou ao daemon: \(error)"
        }
        return "Conecte ao daemon local para espelhar a sessão."
    }

    private var actionTitle: String {
        terminal.connection == .connected ? "Desconectar" : "Conectar"
    }
}

/// Embedded editor answering the daemon's interactive edit request.
struct EditorSheet: View {
    @Bindable var terminal: TerminalModel
    let request: EditorRequest
    @State private var content: String = ""
    @State private var saveError: String?

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text(request.path)
                    .font(.callout.monospaced())
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer()
            }
            .padding(10)

            Divider()

            TextEditor(text: $content)
                .font(.body.monospaced())
                .frame(minWidth: 560, minHeight: 360)

            Divider()

            HStack {
                if let saveError {
                    Text(saveError)
                        .font(.footnote)
                        .foregroundStyle(.red)
                }
                Spacer()
                Button("Cancelar") {
                    Task {
                        await terminal.cancelEditing()
                    }
                }
                .keyboardShortcut(.cancelAction)
                Button("Salvar e continuar") {
                    Task {
                        do {
                            try await terminal.finishEditing(content: content)
                        } catch {
                            saveError = error.localizedDescription
                        }
                    }
                }
                .keyboardShortcut(.defaultAction)
            }
            .padding(10)
        }
        .onAppear {
            content = request.content
        }
    }
}
