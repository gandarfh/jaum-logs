import Foundation
import Observation

/// View-model for one session: board, chat, permission prompt and connection
/// state. Consumes a `SessionBackend` event stream and exposes user intents.
@MainActor
@Observable
public final class SessionModel {
    public private(set) var tasks: [TaskItem] = []
    public private(set) var messages: [ChatMessage] = []
    public private(set) var pendingPermission: PermissionRequest?
    public private(set) var connection: ConnectionState = .disconnected
    public var selectedTaskID: TaskItem.ID?

    private let backend: any SessionBackend
    private var consumeTask: Task<Void, Never>?

    public init(backend: any SessionBackend) {
        self.backend = backend
    }

    /// Board columns in a fixed status order, empty columns included so the
    /// layout is stable.
    public var columns: [BoardColumn] {
        TaskStatus.allCases.map { status in
            BoardColumn(status: status, tasks: tasks.filter { $0.status == status })
        }
    }

    public var selectedTask: TaskItem? {
        tasks.first { $0.id == selectedTaskID }
    }

    public func start() {
        guard consumeTask == nil else { return }
        connection = .connecting
        let backend = self.backend
        consumeTask = Task { [weak self] in
            for await event in await backend.events() {
                guard let self else { return }
                self.apply(event)
            }
        }
    }

    public func stop() {
        consumeTask?.cancel()
        consumeTask = nil
        connection = .disconnected
    }

    public func sendText(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        dispatch(.sendText(trimmed))
    }

    public func sendImage(data: Data, filename: String) {
        guard !data.isEmpty else { return }
        dispatch(.sendImage(data: data, filename: filename))
    }

    public func approvePendingPermission() {
        guard let pending = pendingPermission else { return }
        dispatch(.approvePermission(id: pending.id))
    }

    public func denyPendingPermission() {
        guard let pending = pendingPermission else { return }
        dispatch(.denyPermission(id: pending.id))
    }

    private func dispatch(_ command: SessionCommand) {
        let backend = self.backend
        Task {
            await backend.send(command)
        }
    }

    func apply(_ event: SessionEvent) {
        switch event {
        case .board(let tasks):
            self.tasks = tasks
        case .chat(let message):
            messages.append(message)
        case .chatUpdated(let message):
            if let index = messages.firstIndex(where: { $0.id == message.id }) {
                messages[index] = message
            } else {
                messages.append(message)
            }
        case .permissionRequested(let request):
            pendingPermission = request
        case .permissionResolved(let id, _):
            if pendingPermission?.id == id {
                pendingPermission = nil
            }
        case .connection(let state):
            connection = state
        }
    }
}
