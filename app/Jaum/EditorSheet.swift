import JaumKit
import SwiftUI

/// Embedded editor answering the daemon's interactive edit request: header
/// with the file name and Cancel/Save, monospaced buffer below.
struct EditorSheet: View {
    @Bindable var terminal: TerminalModel
    let request: EditorRequest
    @State var content: String
    @State var saveError: String?

    init(terminal: TerminalModel, request: EditorRequest, saveError: String? = nil) {
        self.terminal = terminal
        self.request = request
        _content = State(initialValue: request.content)
        _saveError = State(initialValue: saveError)
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 9) {
                Image(systemName: "pencil")
                    .font(.caption)
                Text("Editing \u{00B7} \(fileName)")
                    .font(.callout.weight(.bold))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(request.path)
                Spacer()
                if let saveError {
                    Text(saveError)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Button("Cancel") {
                    Task { await cancel() }
                }
                .buttonStyle(GhostButtonStyle())
                .keyboardShortcut(.cancelAction)
                Button("Save", systemImage: "checkmark") {
                    Task { await save() }
                }
                .buttonStyle(PrimaryButtonStyle())
                .keyboardShortcut(.defaultAction)
            }
            .padding(14)

            Divider()

            TextEditor(text: $content)
                .font(.callout.monospaced())
                .scrollContentBackground(.hidden)
                .padding(8)
                .frame(minWidth: 560, minHeight: 340)
        }
    }

    func save() async {
        do {
            try await terminal.finishEditing(content: content)
        } catch {
            saveError = error.localizedDescription
        }
    }

    func cancel() async {
        do {
            try await terminal.cancelEditing()
        } catch {
            saveError = error.localizedDescription
        }
    }

    var fileName: String {
        (request.path as NSString).lastPathComponent
    }
}
