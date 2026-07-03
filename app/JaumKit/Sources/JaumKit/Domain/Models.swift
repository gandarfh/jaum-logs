import Foundation

/// Task lifecycle mirroring the backlog store statuses. Display names follow
/// the approved mock (monochrome, status told by shape and weight).
public enum TaskStatus: String, CaseIterable, Codable, Sendable, Identifiable {
    case wip
    case review
    case ready
    case backlog
    case merged

    public var id: String { rawValue }

    public var displayName: String {
        switch self {
        case .wip: "Em progresso"
        case .review: "Review"
        case .ready: "Pronto"
        case .backlog: "Backlog"
        case .merged: "Merged"
        }
    }
}

public enum TaskKind: String, Codable, Sendable {
    case implementation = "impl"
    case spike

    public var displayName: String {
        switch self {
        case .implementation: "Implementação"
        case .spike: "Spike"
        }
    }
}

public struct ProjectItem: Identifiable, Hashable, Sendable {
    public var id: String
    public var name: String
    public var taskCount: Int

    public init(id: String, name: String, taskCount: Int) {
        self.id = id
        self.name = name
        self.taskCount = taskCount
    }
}

public struct Criterion: Identifiable, Hashable, Sendable {
    public var text: String
    public var done: Bool

    public var id: String { text }

    public init(text: String, done: Bool = false) {
        self.text = text
        self.done = done
    }
}

/// A review finding shown in the review session tab.
public struct Finding: Identifiable, Hashable, Sendable {
    public var title: String
    public var detail: String
    public var location: String

    public var id: String { title }

    public init(title: String, detail: String, location: String) {
        self.title = title
        self.detail = detail
        self.location = location
    }
}

public enum SessionKind: String, Codable, Sendable {
    case play
    case review

    public var displayName: String {
        switch self {
        case .play: "Play"
        case .review: "Review"
        }
    }
}

/// One session of a task. The detail pane tabs are the task's own sessions
/// (Detalhe plus one tab per session), never fixed global tabs.
public struct TaskSession: Identifiable, Hashable, Sendable {
    public var id: String
    public var kind: SessionKind
    public var isLive: Bool
    public var toolCount: Int
    public var messages: [ChatMessage]
    public var findings: [Finding]

    public init(
        id: String,
        kind: SessionKind,
        isLive: Bool = false,
        toolCount: Int = 0,
        messages: [ChatMessage] = [],
        findings: [Finding] = []
    ) {
        self.id = id
        self.kind = kind
        self.isLive = isLive
        self.toolCount = toolCount
        self.messages = messages
        self.findings = findings
    }
}

/// A backlog task as shown in the grouped list and the detail pane.
public struct TaskItem: Identifiable, Hashable, Sendable {
    public var id: String
    public var title: String
    public var kind: TaskKind
    public var status: TaskStatus
    public var objective: String
    public var criteria: [Criterion]
    public var constraints: [String]
    public var worktree: String?
    public var isParallel: Bool
    public var isEditing: Bool
    public var prCount: Int
    public var lastActivity: String?
    public var sessions: [TaskSession]

    public init(
        id: String,
        title: String,
        kind: TaskKind = .implementation,
        status: TaskStatus = .backlog,
        objective: String = "",
        criteria: [Criterion] = [],
        constraints: [String] = [],
        worktree: String? = nil,
        isParallel: Bool = false,
        isEditing: Bool = false,
        prCount: Int = 0,
        lastActivity: String? = nil,
        sessions: [TaskSession] = []
    ) {
        self.id = id
        self.title = title
        self.kind = kind
        self.status = status
        self.objective = objective
        self.criteria = criteria
        self.constraints = constraints
        self.worktree = worktree
        self.isParallel = isParallel
        self.isEditing = isEditing
        self.prCount = prCount
        self.lastActivity = lastActivity
        self.sessions = sessions
    }

    public var findingsCount: Int {
        sessions.reduce(0) { $0 + $1.findings.count }
    }

    public var hasLiveSession: Bool {
        sessions.contains { $0.isLive }
    }

    public var doneCriteriaCount: Int {
        criteria.filter(\.done).count
    }
}

/// A section of the task list: one status group, in the fixed status order.
public struct TaskListSection: Identifiable, Hashable, Sendable {
    public var status: TaskStatus
    public var tasks: [TaskItem]

    public var id: TaskStatus { status }

    public init(status: TaskStatus, tasks: [TaskItem]) {
        self.status = status
        self.tasks = tasks
    }
}

/// A project document rendered by the Docs screen.
public struct DocItem: Identifiable, Hashable, Sendable {
    public var name: String
    public var subtitle: String
    public var content: String

    public var id: String { name }

    public init(name: String, subtitle: String = "", content: String) {
        self.name = name
        self.subtitle = subtitle
        self.content = content
    }
}

public enum ChatRole: String, Hashable, Sendable {
    case user
    case assistant
    case system
}

public enum ToolCallState: Hashable, Sendable {
    case running
    case succeeded
    case failed
}

/// A tool invocation rendered as a structured card in the chat timeline.
public struct ToolCall: Identifiable, Hashable, Sendable {
    public var id: String
    public var name: String
    public var summary: String
    public var state: ToolCallState

    public init(id: String, name: String, summary: String, state: ToolCallState = .running) {
        self.id = id
        self.name = name
        self.summary = summary
        self.state = state
    }
}

/// One block inside a chat message: markdown text, an inline image or a
/// tool card.
public enum ChatBlock: Hashable, Sendable {
    case markdown(String)
    case image(Data)
    case tool(ToolCall)
}

public struct ChatMessage: Identifiable, Hashable, Sendable {
    public var id: String
    public var role: ChatRole
    public var blocks: [ChatBlock]
    public var timestamp: Date

    public init(id: String, role: ChatRole, blocks: [ChatBlock], timestamp: Date) {
        self.id = id
        self.role = role
        self.blocks = blocks
        self.timestamp = timestamp
    }
}

/// A tool waiting for the user's decision. Any connected client may answer.
public struct PermissionRequest: Identifiable, Hashable, Sendable {
    public var id: String
    public var toolName: String
    public var request: String

    public init(id: String, toolName: String, request: String) {
        self.id = id
        self.toolName = toolName
        self.request = request
    }
}

public enum ConnectionState: Hashable, Sendable {
    case disconnected
    case connecting
    case connected

    public var displayName: String {
        switch self {
        case .disconnected: "Desconectado"
        case .connecting: "Conectando"
        case .connected: "Conectado"
        }
    }
}
