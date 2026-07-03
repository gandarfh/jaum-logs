import JaumKit
import SwiftUI
import UniformTypeIdentifiers

struct ChatView: View {
    @Bindable var session: SessionModel
    @State private var draft = ""
    @State private var showingImagePicker = false
    @State private var attachmentError: String?

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 12) {
                        ForEach(session.messages) { message in
                            ChatMessageView(message: message)
                                .id(message.id)
                        }
                    }
                    .padding()
                }
                .onChange(of: session.messages.count) {
                    if let last = session.messages.last {
                        withAnimation {
                            proxy.scrollTo(last.id, anchor: .bottom)
                        }
                    }
                }
            }

            Divider()

            HStack(alignment: .center, spacing: 8) {
                Button {
                    showingImagePicker = true
                } label: {
                    Image(systemName: "photo.badge.plus")
                }
                .help("Enviar imagem")

                TextField("Mensagem para a sessão", text: $draft, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(1...5)
                    .onSubmit(sendDraft)

                Button("Enviar", action: sendDraft)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
            .padding(10)
        }
        .navigationTitle("Chat da sessão")
        .fileImporter(
            isPresented: $showingImagePicker,
            allowedContentTypes: [.png, .jpeg, .gif, .heic]
        ) { result in
            attachImage(result)
        }
        .alert(
            "Não deu para anexar a imagem",
            isPresented: Binding(
                get: { attachmentError != nil },
                set: { if !$0 { attachmentError = nil } }
            )
        ) {
            Button("Entendi", role: .cancel) {}
        } message: {
            Text(attachmentError ?? "")
        }
    }

    private func sendDraft() {
        session.sendText(draft)
        draft = ""
    }

    private func attachImage(_ result: Result<URL, any Error>) {
        switch result {
        case .success(let url):
            let secured = url.startAccessingSecurityScopedResource()
            defer {
                if secured {
                    url.stopAccessingSecurityScopedResource()
                }
            }
            do {
                let data = try Data(contentsOf: url)
                session.sendImage(data: data, filename: url.lastPathComponent)
            } catch {
                attachmentError = error.localizedDescription
            }
        case .failure(let error):
            attachmentError = error.localizedDescription
        }
    }
}

struct ChatMessageView: View {
    let message: ChatMessage

    var body: some View {
        HStack {
            if message.role == .user {
                Spacer(minLength: 40)
            }
            VStack(alignment: .leading, spacing: 8) {
                ForEach(Array(message.blocks.enumerated()), id: \.offset) { _, block in
                    ChatBlockView(block: block)
                }
            }
            .padding(10)
            .background(background, in: RoundedRectangle(cornerRadius: 10))
            if message.role != .user {
                Spacer(minLength: 40)
            }
        }
    }

    private var background: some ShapeStyle {
        switch message.role {
        case .user: AnyShapeStyle(Color.accentColor.opacity(0.18))
        case .assistant: AnyShapeStyle(.quaternary)
        case .system: AnyShapeStyle(.quinary)
        }
    }
}

struct ChatBlockView: View {
    let block: ChatBlock

    var body: some View {
        switch block {
        case .markdown(let text):
            MarkdownText(text)
                .textSelection(.enabled)
        case .image(let data):
            if let image = NSImage(data: data) {
                Image(nsImage: image)
                    .resizable()
                    .scaledToFit()
                    .frame(maxWidth: 360, maxHeight: 280)
                    .clipShape(RoundedRectangle(cornerRadius: 6))
            } else {
                Label("Imagem inválida", systemImage: "photo.trianglebadge.exclamationmark")
                    .foregroundStyle(.secondary)
            }
        case .tool(let tool):
            ToolCardView(tool: tool)
        }
    }
}

struct ToolCardView: View {
    let tool: ToolCall

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: icon)
                .foregroundStyle(color)
            VStack(alignment: .leading, spacing: 2) {
                Text(tool.name)
                    .font(.callout.monospaced().weight(.semibold))
                Text(tool.summary)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .strokeBorder(color.opacity(0.4))
        )
    }

    private var icon: String {
        switch tool.state {
        case .running: "clock"
        case .succeeded: "checkmark.circle.fill"
        case .failed: "xmark.circle.fill"
        }
    }

    private var color: Color {
        switch tool.state {
        case .running: .orange
        case .succeeded: .green
        case .failed: .red
        }
    }
}

/// Inline markdown (bold, code, links) with whitespace preserved; falls back
/// to plain text when parsing fails.
struct MarkdownText: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        if let attributed = try? AttributedString(
            markdown: text,
            options: AttributedString.MarkdownParsingOptions(
                interpretedSyntax: .inlineOnlyPreservingWhitespace
            )
        ) {
            Text(attributed)
        } else {
            Text(text)
        }
    }
}
