import Foundation

/// In-memory backend with a scripted session mirroring the approved mock.
/// Drives the UI (previews and manual testing) and the view-model tests
/// deterministically until the daemon speaks domain events over the socket.
public actor PreviewBackend: SessionBackend {
    private var continuations: [UUID: AsyncStream<SessionEvent>.Continuation] = [:]
    private var counter = 0
    private var pendingPermissions: [String: ToolCall] = [:]
    private var resolvedPermissions: Set<String> = []
    private var permissionSession: (taskID: String, sessionID: String)?

    public init() {}

    public func events() -> AsyncStream<SessionEvent> {
        let (stream, continuation) = AsyncStream.makeStream(of: SessionEvent.self)
        let id = UUID()
        continuations[id] = continuation
        continuation.onTermination = { [weak self] _ in
            Task {
                await self?.forget(id)
            }
        }
        for event in Self.initialEvents {
            if case .permissionRequested(let request) = event {
                // A permission already answered in this process must not be
                // re-prompted to a late subscriber.
                guard !resolvedPermissions.contains(request.id) else { continue }
                if pendingPermissions[request.id] == nil {
                    pendingPermissions[request.id] = ToolCall(
                        id: request.id,
                        name: request.toolName,
                        summary: request.request,
                        state: .running
                    )
                    permissionSession = ("jaum-42", "jaum-42-play")
                }
            }
            continuation.yield(event)
        }
        return stream
    }

    private func forget(_ id: UUID) {
        continuations.removeValue(forKey: id)
    }

    var subscriberCount: Int {
        continuations.count
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
                        blocks: [.markdown("Got your message, I will factor it into the session.")],
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
                        blocks: [.markdown("Image sent: \(filename)"), .image(data)],
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
        resolvedPermissions.insert(id)
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
        for continuation in continuations.values {
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
        let reviewedAt = Date().addingTimeInterval(-5 * 60)
        let playChat: [ChatMessage] = [
            ChatMessage(
                id: "seed-1",
                role: .system,
                blocks: [.markdown("loading repo-map + constraints...")],
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
                blocks: [.markdown("Protocol round-trips cleanly. **Run the next step?**")],
                timestamp: base.addingTimeInterval(3)
            ),
        ]
        let tasks: [TaskItem] = [
            TaskItem(
                id: "jaum-42",
                title: "Domain protocol + headless daemon",
                status: .wip,
                objective:
                    "Swap the daemon's final stage from rendering cells to serializing domain state, keeping the brain in Rust.",
                criteria: [
                    Criterion(text: "Daemon no longer depends on ratatui", done: true),
                    Criterion(text: "DomainSnapshot serde round-trip tested", done: true),
                    Criterion(text: "PTY tee + scrollback ring buffer"),
                ],
                constraints: [
                    "Do not duplicate PR state",
                    "Extra scope becomes deferred",
                    "No coverage exclusions",
                ],
                worktree: "jaum-42",
                isParallel: true,
                isEditing: true,
                prCount: 2,
                lastActivity: "now",
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
                title: "Migrate the TUI client to the snapshot",
                status: .wip,
                objective: "The TUI client renders from the DomainSnapshot.",
                worktree: "jaum-39",
                prCount: 1,
                lastActivity: "8min",
                sessions: [
                    TaskSession(id: "jaum-39-play", kind: .play, isLive: true, toolCount: 1)
                ],
                reviewState: .running
            ),
            TaskItem(
                id: "jaum-31",
                title: "TCP transport + token",
                status: .review,
                objective: "Remote connection with a pairing token over Headscale.",
                criteria: [
                    Criterion(text: "Handshake carries the version", done: true),
                    Criterion(text: "Token required over TCP", done: true),
                    Criterion(text: "Reject closes the connection", done: true),
                    Criterion(text: "Latency measured via Ping/Pong"),
                ],
                prCount: 1,
                lastActivity: "1h",
                sessions: [
                    TaskSession(id: "jaum-31-play", kind: .play),
                    TaskSession(
                        id: "jaum-31-review",
                        kind: .review,
                        findings: [
                            Finding(
                                title: "Duplicated PR state",
                                detail:
                                    "The struct stores merge state that already comes from gh. Violates a constraint.",
                                location: "crates/cli/src/app.rs:212"
                            ),
                            Finding(
                                title: "Token read without trim",
                                detail: "The handshake compares the token with a trailing newline.",
                                location: "crates/cli/src/config.rs:88"
                            ),
                        ]
                    ),
                ],
                reviewState: .reviewed(
                    ReviewVerdict(reviewedSHA: "9534bae", findings: 2, reviewedAt: reviewedAt))
            ),
            TaskItem(
                id: "jaum-28",
                title: "macOS app MVP",
                status: .ready,
                objective: "macOS target with a window invisible to screen sharing.",
                prCount: 1,
                reviewState: .rereviewPending
            ),
            TaskItem(
                id: "jaum-44",
                title: "Embedded editor",
                status: .backlog,
                objective: "In-app file editing replacing the external $EDITOR."
            ),
            TaskItem(
                id: "jaum-8",
                title: "Golden protocol fixtures",
                status: .merged,
                prCount: 1
            ),
        ]
        let docs: [DocItem] = [
            DocItem(
                name: "conventions.md",
                subtitle: "jaum-logs \u{00B7} edited 2h ago",
                content: """
                    ## Constraints

                    The `.backlog/` directory is the single source of truth. GitHub is downstream.

                    - Do not duplicate PR state, read from `gh`, never store it.
                    - Extra scope becomes `deferred` + a new backlog item.

                    ## Style

                    Comments explain **why**, not what. No emojis.
                    """
            ),
            DocItem(
                name: "architecture.md",
                subtitle: "jaum-logs",
                content: """
                    ## Layers

                    - `jaum-core`: backlog model + store.
                    - `adapters`: git, gh, executor behind traits.
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
                ProjectItem(id: "new-site", name: "new-site", taskCount: 9),
            ]),
            .docs(docs),
            .tasks(tasks),
            // The daemon's CI watch finishes jaum-39's capture: the spinner
            // resolves to a persisted verdict.
            .reviewState(
                taskID: "jaum-39",
                state: .reviewed(
                    ReviewVerdict(reviewedSHA: "1e5e355", findings: 0, reviewedAt: reviewedAt))),
            .permissionRequested(
                PermissionRequest(
                    id: "perm-1",
                    toolName: "git push",
                    request: "Push the working branch to the remote"
                )),
        ]
    }()
}
