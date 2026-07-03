import Foundation

/// In-memory backend with a scripted session mirroring the approved mock.
/// Drives the UI (previews and manual testing) and the view-model tests
/// deterministically until the daemon speaks domain events over the socket.
public actor PreviewBackend: SessionBackend {
    private var continuations: [AsyncStream<SessionEvent>.Continuation] = []
    private var counter = 0
    private var pendingPermissions: [String: ToolCall] = [:]
    private var permissionSession: (taskID: String, sessionID: String)?

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
            permissionSession = ("jaum-42", "jaum-42-play")
        }
        return stream
    }

    public func send(_ command: SessionCommand) async {
        switch command {
        case .sendText(let taskID, let sessionID, let text):
            broadcast(
                .chat(
                    taskID: taskID,
                    sessionID: sessionID,
                    message: ChatMessage(
                        id: nextID("msg"),
                        role: .user,
                        blocks: [.markdown(text)],
                        timestamp: nextTimestamp()
                    )))
            broadcast(
                .chat(
                    taskID: taskID,
                    sessionID: sessionID,
                    message: ChatMessage(
                        id: nextID("msg"),
                        role: .assistant,
                        blocks: [.markdown("Recebi sua mensagem e vou considerar na sessão.")],
                        timestamp: nextTimestamp()
                    )))
        case .sendImage(let taskID, let sessionID, let data, let filename):
            broadcast(
                .chat(
                    taskID: taskID,
                    sessionID: sessionID,
                    message: ChatMessage(
                        id: nextID("msg"),
                        role: .user,
                        blocks: [.markdown("Imagem enviada: \(filename)"), .image(data)],
                        timestamp: nextTimestamp()
                    )))
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
        if let target = permissionSession {
            broadcast(
                .chat(
                    taskID: target.taskID,
                    sessionID: target.sessionID,
                    message: ChatMessage(
                        id: nextID("msg"),
                        role: .system,
                        blocks: [.tool(tool)],
                        timestamp: nextTimestamp()
                    )))
        }
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
        let playChat: [ChatMessage] = [
            ChatMessage(
                id: "seed-1",
                role: .system,
                blocks: [.markdown("carregando repo-map + constraints...")],
                timestamp: base
            ),
            ChatMessage(
                id: "seed-2",
                role: .system,
                blocks: [
                    .tool(
                        ToolCall(
                            id: "tool-1",
                            name: "Read",
                            summary: "crates/cli/src/daemon.rs",
                            state: .succeeded
                        ))
                ],
                timestamp: base.addingTimeInterval(1)
            ),
            ChatMessage(
                id: "seed-3",
                role: .system,
                blocks: [
                    .tool(
                        ToolCall(
                            id: "tool-2",
                            name: "Bash",
                            summary: "cargo test -p jaum-cli (8 passed, 0 failed)",
                            state: .succeeded
                        ))
                ],
                timestamp: base.addingTimeInterval(2)
            ),
            ChatMessage(
                id: "seed-4",
                role: .assistant,
                blocks: [.markdown("Protocolo serializando redondo. **Rodo o próximo passo?**")],
                timestamp: base.addingTimeInterval(3)
            ),
        ]
        let tasks: [TaskItem] = [
            TaskItem(
                id: "jaum-42",
                title: "Protocolo de domínio + daemon headless",
                status: .wip,
                objective:
                    "Trocar a etapa final do daemon de renderizar células por serializar estado de domínio, mantendo o cérebro em Rust.",
                criteria: [
                    Criterion(text: "Daemon deixa de depender de ratatui", done: true),
                    Criterion(text: "DomainSnapshot roundtrip serde testado", done: true),
                    Criterion(text: "Tee de PTY + ring buffer de scrollback"),
                ],
                constraints: [
                    "Não duplicar estado de PR",
                    "Escopo extra vira deferred",
                    "Sem exclusão de cobertura",
                ],
                worktree: "jaum-42",
                isParallel: true,
                isEditing: true,
                prCount: 2,
                lastActivity: "agora",
                sessions: [
                    TaskSession(
                        id: "jaum-42-play",
                        kind: .play,
                        isLive: true,
                        toolCount: 3,
                        messages: playChat
                    )
                ]
            ),
            TaskItem(
                id: "jaum-39",
                title: "Migrar cliente TUI pro snapshot",
                status: .wip,
                objective: "Cliente TUI passa a renderizar a partir do DomainSnapshot.",
                worktree: "jaum-39",
                lastActivity: "8min",
                sessions: [
                    TaskSession(id: "jaum-39-play", kind: .play, isLive: true, toolCount: 1)
                ]
            ),
            TaskItem(
                id: "jaum-31",
                title: "Transporte TCP + token",
                status: .review,
                objective: "Conexão remota com token de pareamento sobre Headscale.",
                criteria: [
                    Criterion(text: "Handshake com versão", done: true),
                    Criterion(text: "Token obrigatório no TCP", done: true),
                    Criterion(text: "Reject fecha a conexão", done: true),
                    Criterion(text: "Latência medida por Ping/Pong"),
                ],
                lastActivity: "1h",
                sessions: [
                    TaskSession(id: "jaum-31-play", kind: .play),
                    TaskSession(
                        id: "jaum-31-review",
                        kind: .review,
                        findings: [
                            Finding(
                                title: "Estado de PR duplicado",
                                detail:
                                    "A struct guarda merge state que já vem do gh. Viola constraint.",
                                location: "crates/cli/src/app.rs:212"
                            ),
                            Finding(
                                title: "Token lido sem trim",
                                detail: "Handshake compara token com quebra de linha no fim.",
                                location: "crates/cli/src/config.rs:88"
                            ),
                        ]
                    ),
                ]
            ),
            TaskItem(
                id: "jaum-28",
                title: "App macOS MVP",
                status: .ready,
                objective: "Alvo macOS com janela invisível em screen share."
            ),
            TaskItem(
                id: "jaum-44",
                title: "Editor embutido",
                status: .backlog,
                objective: "Edição de arquivo in-app substituindo o $EDITOR externo."
            ),
            TaskItem(
                id: "jaum-8",
                title: "Fixtures golden do protocolo",
                status: .merged,
                prCount: 1
            ),
        ]
        let docs: [DocItem] = [
            DocItem(
                name: "conventions.md",
                subtitle: "jaum-logs · editado 2h atrás",
                content: """
                    ## Constraints

                    O diretório `.backlog/` é a única fonte de verdade. GitHub é downstream.

                    - Não duplicar estado de PR, ler de `gh`, nunca guardar.
                    - Escopo extra vira `deferred` + novo backlog.

                    ## Estilo

                    Comentários explicam **porquê**, não o quê. Sem emojis.
                    """
            ),
            DocItem(
                name: "arquitetura.md",
                subtitle: "jaum-logs",
                content: """
                    ## Camadas

                    - `jaum-core`: modelo + store do backlog.
                    - `adapters`: git, gh, executor atrás de traits.
                    - `flows`: play, review, ingest.
                    - `cli`: daemon + TUI.
                    """
            ),
        ]
        return [
            .connection(.connected),
            .latency(milliseconds: 24),
            .latency(milliseconds: 22),
            .latency(milliseconds: 31),
            .latency(milliseconds: 18),
            .latency(milliseconds: 26),
            .latency(milliseconds: 21),
            .latency(milliseconds: 24),
            .projects([
                ProjectItem(id: "jaum-logs", name: "jaum-logs", taskCount: tasks.count),
                ProjectItem(id: "site-novo", name: "site-novo", taskCount: 9),
            ]),
            .docs(docs),
            .tasks(tasks),
            .permissionRequested(
                PermissionRequest(
                    id: "perm-1",
                    toolName: "git push",
                    request: "Enviar o branch de trabalho para o remoto"
                )),
        ]
    }()
}
