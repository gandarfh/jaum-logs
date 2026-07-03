import Foundation
import Observation

/// View-model for the daemon connection: projects, task list grouped by
/// status, per-task session tabs, docs, permission prompt and connection
/// telemetry. Consumes a `SessionBackend` event stream and exposes intents.
@MainActor
@Observable
public final class SessionModel {
    /// The detail pane tab: the task's own detail or one of its sessions.
    public enum DetailTab: Hashable, Sendable {
        case detail
        case session(String)
    }

    public private(set) var projects: [ProjectItem] = []
    public private(set) var tasks: [TaskItem] = []
    public private(set) var docs: [DocItem] = []
    public private(set) var pendingPermission: PermissionRequest?
    public private(set) var connection: ConnectionState = .disconnected
    public private(set) var latencySamples: [Double] = []

    public var selectedProjectID: ProjectItem.ID?
    public var statusFilter: TaskStatus?
    public var selectedTaskID: TaskItem.ID? {
        didSet {
            if oldValue != selectedTaskID {
                selectedTab = .detail
            }
        }
    }
    public var selectedTab: DetailTab = .detail
    public var selectedDocID: DocItem.ID?

    private let backend: any SessionBackend
    private var consumeTask: Task<Void, Never>?

    public init(backend: any SessionBackend) {
        self.backend = backend
    }

    /// Task list sections in the fixed status order, honoring the sidebar
    /// status filter; empty groups are omitted like in the approved mock.
    public var sections: [TaskListSection] {
        TaskStatus.allCases.compactMap { status in
            if let filter = statusFilter, filter != status { return nil }
            let grouped = tasks.filter { $0.status == status }
            return grouped.isEmpty ? nil : TaskListSection(status: status, tasks: grouped)
        }
    }

    public func taskCount(for status: TaskStatus) -> Int {
        tasks.filter { $0.status == status }.count
    }

    public var selectedTask: TaskItem? {
        tasks.first { $0.id == selectedTaskID }
    }

    public var selectedDoc: DocItem? {
        docs.first { $0.id == selectedDocID } ?? docs.first
    }

    /// Average latency in milliseconds over the recent samples.
    public var averageLatency: Double? {
        guard !latencySamples.isEmpty else { return nil }
        return latencySamples.reduce(0, +) / Double(latencySamples.count)
    }

    public func session(_ id: String, of task: TaskItem) -> TaskSession? {
        task.sessions.first { $0.id == id }
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

    public func sendText(_ text: String, taskID: String, sessionID: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        dispatch(.sendText(taskID: taskID, sessionID: sessionID, text: trimmed))
    }

    public func sendImage(data: Data, filename: String, taskID: String, sessionID: String) {
        guard !data.isEmpty else { return }
        dispatch(.sendImage(taskID: taskID, sessionID: sessionID, data: data, filename: filename))
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
        case .projects(let projects):
            self.projects = projects
            if selectedProjectID == nil {
                selectedProjectID = projects.first?.id
            }
        case .tasks(let tasks):
            self.tasks = tasks
        case .docs(let docs):
            self.docs = docs
        case .chat(let taskID, let sessionID, let message):
            appendChat(taskID: taskID, sessionID: sessionID, message: message)
        case .permissionRequested(let request):
            pendingPermission = request
        case .permissionResolved(let id, _):
            if pendingPermission?.id == id {
                pendingPermission = nil
            }
        case .connection(let state):
            connection = state
        case .latency(let milliseconds):
            latencySamples.append(milliseconds)
            if latencySamples.count > 60 {
                latencySamples.removeFirst(latencySamples.count - 60)
            }
        }
    }

    private func appendChat(taskID: String, sessionID: String, message: ChatMessage) {
        guard let taskIndex = tasks.firstIndex(where: { $0.id == taskID }) else { return }
        guard
            let sessionIndex = tasks[taskIndex].sessions.firstIndex(where: { $0.id == sessionID })
        else { return }
        var session = tasks[taskIndex].sessions[sessionIndex]
        if let existing = session.messages.firstIndex(where: { $0.id == message.id }) {
            session.messages[existing] = message
        } else {
            session.messages.append(message)
        }
        tasks[taskIndex].sessions[sessionIndex] = session
    }
}
