import Foundation

/// In-memory backend with a scripted session. Drives the UI (previews and
/// manual testing) and the view-model tests deterministically until the
/// daemon speaks domain events over the socket.
public actor PreviewBackend: SessionBackend {
    private var continuations: [AsyncStream<SessionEvent>.Continuation] = []
    private var counter = 0
    private var pendingPermissions: [String: ToolCall] = [:]

    public init() {}

    public func events() -> AsyncStream<SessionEvent> {
        let (stream, continuation) = AsyncStream.makeStream(of: SessionEvent.self)
        continuations.append(continuation)
        for event in Self.initialEvents {
            continuation.yield(event)
        }
        if case .permissionRequested(let request) = Self.initialEvents.last {
            pendingPermissions[request.id] = ToolCall(
                id: request.id,
                name: request.toolName,
                summary: request.request,
                state: .running
            )
        }
        return stream
    }

    public func send(_ command: SessionCommand) async {
        switch command {
        case .sendText(let text):
            let message = ChatMessage(
                id: nextID("msg"),
                role: .user,
                blocks: [.markdown(text)],
                timestamp: nextTimestamp()
            )
            broadcast(.chat(message))
            broadcast(
                .chat(
                    ChatMessage(
                        id: nextID("msg"),
                        role: .assistant,
                        blocks: [.markdown("Recebi sua mensagem e vou considerar na sessão.")],
                        timestamp: nextTimestamp()
                    )))
        case .sendImage(let data, let filename):
            let message = ChatMessage(
                id: nextID("msg"),
                role: .user,
                blocks: [.markdown("Imagem enviada: \(filename)"), .image(data)],
                timestamp: nextTimestamp()
            )
            broadcast(.chat(message))
        case .approvePermission(let id):
            resolvePermission(id: id, approved: true)
        case .denyPermission(let id):
            resolvePermission(id: id, approved: false)
        }
    }

    private func resolvePermission(id: String, approved: Bool) {
        guard var tool = pendingPermissions.removeValue(forKey: id) else { return }
        tool.state = approved ? .succeeded : .failed
        broadcast(.permissionResolved(id: id, approved: approved))
        broadcast(
            .chat(
                ChatMessage(
                    id: nextID("msg"),
                    role: .system,
                    blocks: [.tool(tool)],
                    timestamp: nextTimestamp()
                )))
    }

    private func broadcast(_ event: SessionEvent) {
        for continuation in continuations {
            continuation.yield(event)
        }
    }

    private func nextID(_ prefix: String) -> String {
        counter += 1
        return "\(prefix)-\(counter)"
    }

    private func nextTimestamp() -> Date {
        Date(timeIntervalSince1970: 1_700_000_000 + Double(counter))
    }

    private static let initialEvents: [SessionEvent] = {
        let base = Date(timeIntervalSince1970: 1_700_000_000)
        let board: [TaskItem] = [
            TaskItem(
                id: "socket-client",
                title: "Cliente de socket compartilhado",
                status: .merged,
                objective: "Conectar os apps nativos ao daemon local via unix socket.",
                acceptanceCriteria: ["Handshake com frame inicial", "Reconexão limpa"]
            ),
            TaskItem(
                id: "board-nativo",
                title: "Board nativo com colunas por status",
                status: .wip,
                objective: "Espelhar o backlog no app com navegação por task.",
                acceptanceCriteria: ["Colunas por status", "Detalhe com critérios"]
            ),
            TaskItem(
                id: "chat-estruturado",
                title: "Chat estruturado da sessão",
                status: .review,
                objective: "Markdown, imagens inline e cards de ferramenta no chat.",
                acceptanceCriteria: ["Markdown", "Imagens", "Cards de tool"]
            ),
            TaskItem(
                id: "editor-embutido",
                title: "Editor embutido para arquivos da sessão",
                status: .ready,
                objective: "Abrir e salvar arquivos pedidos pelo daemon sem sair do app."
            ),
            TaskItem(
                id: "atalhos-navegacao",
                title: "Atalhos de teclado estilo vim",
                status: .backlog,
                objective: "Navegação completa sem mouse."
            ),
        ]
        return [
            .connection(.connected),
            .board(board),
            .chat(
                ChatMessage(
                    id: "seed-1",
                    role: .assistant,
                    blocks: [
                        .markdown(
                            "Sessão iniciada. Vou trabalhar no **board nativo** seguindo os critérios de aceite."
                        )
                    ],
                    timestamp: base
                )),
            .chat(
                ChatMessage(
                    id: "seed-2",
                    role: .system,
                    blocks: [
                        .tool(
                            ToolCall(
                                id: "tool-1",
                                name: "cargo test",
                                summary: "Rodando a suíte de testes do workspace",
                                state: .succeeded
                            ))
                    ],
                    timestamp: base.addingTimeInterval(1)
                )),
            .permissionRequested(
                PermissionRequest(
                    id: "perm-1",
                    toolName: "git push",
                    request: "Enviar o branch de trabalho para o remoto"
                )),
        ]
    }()
}
