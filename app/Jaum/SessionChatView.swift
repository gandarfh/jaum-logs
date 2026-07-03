import JaumKit
import SwiftUI
import UniformTypeIdentifiers

/// A session tab: structured chat timeline (assistant markdown, tool cards,
/// inline images) inside a framed panel with header and composer.
struct SessionChatView: View {
    @Bindable var session: SessionModel
    let task: TaskItem
    let taskSession: TaskSession

    @State var draft: String
    @State private var showingImagePicker = false

    init(session: SessionModel, task: TaskItem, taskSession: TaskSession, draft: String = "") {
        self.session = session
        self.task = task
        self.taskSession = taskSession
        _draft = State(initialValue: draft)
    }

    var body: some View {
        VStack(spacing: 0) {
            panelHeader

            Divider()

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(taskSession.messages) { message in
                            ChatEntryView(message: message)
                                .id(message.id)
                        }
                    }
                    .padding(13)
                }
                .onChange(of: taskSession.messages.count) {
                    if let last = taskSession.messages.last {
                        withAnimation {
                            proxy.scrollTo(last.id, anchor: .bottom)
                        }
                    }
                }
            }

            Divider()

            composer
        }
        .background(
            RoundedRectangle(cornerRadius: 11)
                .strokeBorder(.separator)
        )
        .padding(18)
        .fileImporter(
            isPresented: $showingImagePicker,
            allowedContentTypes: [.png, .jpeg, .gif, .heic]
        ) { result in
            session.attachImage(result, taskID: task.id, sessionID: taskSession.id)
        }
        .alert(
            "Não deu para anexar a imagem",
            isPresented: Binding(
                get: { session.attachmentError != nil },
                set: { if !$0 { session.clearAttachmentError() } }
            )
        ) {
            Button("Entendi", role: .cancel) {}
        } message: {
            Text(session.attachmentError ?? "")
        }
    }

    private var panelHeader: some View {
        HStack(spacing: 8) {
            Image(systemName: "terminal")
                .font(.caption)
            Text("\(taskSession.kind.rawValue) · ")
                .font(.caption)
                .foregroundStyle(.secondary)
                + Text(task.id)
                .font(.caption.weight(.semibold))
                + Text(task.worktree != nil ? " · worktree" : "")
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
            if taskSession.isLive {
                HStack(spacing: 6) {
                    Image(systemName: "waveform.path.ecg")
                    Text("ao vivo · \(taskSession.toolCount) tools")
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 13)
        .padding(.vertical, 9)
    }

    private var composer: some View {
        HStack(spacing: 8) {
            Button {
                showingImagePicker = true
            } label: {
                Image(systemName: "photo.badge.plus")
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .help("Enviar imagem")

            TextField("Digite pra conversar", text: $draft, axis: .vertical)
                .textFieldStyle(.plain)
                .font(.callout)
                .lineLimit(1...5)
                .onSubmit(sendDraft)

            Button("Enviar", action: sendDraft)
                .buttonStyle(GhostButtonStyle())
                .keyboardShortcut(.return, modifiers: .command)
                .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .padding(10)
    }

    func sendDraft() {
        session.sendText(draft, taskID: task.id, sessionID: taskSession.id)
        draft = ""
    }
}

/// One timeline entry. User messages get a subtle "você" label; assistant
/// and system entries flow as a plain timeline like the approved mock.
struct ChatEntryView: View {
    let message: ChatMessage

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if message.role == .user {
                Text("você")
                    .font(.caption2.weight(.bold))
                    .foregroundStyle(.secondary)
            }
            ForEach(Array(message.blocks.enumerated()), id: \.offset) { _, block in
                ChatBlockView(block: block)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, message.role == .user ? 10 : 0)
        .padding(.vertical, message.role == .user ? 8 : 0)
        .background(
            message.role == .user
                ? AnyShapeStyle(Color.primary.opacity(0.06)) : AnyShapeStyle(.clear),
            in: RoundedRectangle(cornerRadius: 8)
        )
    }
}

struct ChatBlockView: View {
    let block: ChatBlock

    var body: some View {
        switch block {
        case .markdown(let text):
            MarkdownText(text)
                .font(.callout)
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

/// Tool card: monochrome, state told by icon shape.
struct ToolCardView: View {
    let tool: ToolCall

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: icon)
                .font(.callout)
                .foregroundStyle(tool.state == .running ? .secondary : .primary)
            VStack(alignment: .leading, spacing: 2) {
                Text(tool.name)
                    .font(.caption.monospaced().weight(.semibold))
                Text(tool.summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .padding(8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .strokeBorder(.separator)
        )
    }

    private var icon: String {
        switch tool.state {
        case .running: "clock"
        case .succeeded: "checkmark.circle"
        case .failed: "xmark.circle"
        }
    }
}
