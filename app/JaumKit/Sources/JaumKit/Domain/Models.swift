import Foundation

/// Task lifecycle mirroring the backlog store statuses.
public enum TaskStatus: String, CaseIterable, Codable, Sendable, Identifiable {
    case backlog
    case ready
    case wip
    case review
    case merged

    public var id: String { rawValue }

    public var displayName: String {
        switch self {
        case .backlog: "Backlog"
        case .ready: "Pronta"
        case .wip: "Em curso"
        case .review: "Review"
        case .merged: "Mergeada"
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

/// A backlog task as shown on the board and in the detail pane.
public struct TaskItem: Identifiable, Hashable, Sendable {
    public var id: String
    public var title: String
    public var kind: TaskKind
    public var status: TaskStatus
    public var objective: String
    public var acceptanceCriteria: [String]
    public var constraints: [String]

    public init(
        id: String,
        title: String,
        kind: TaskKind = .implementation,
        status: TaskStatus = .backlog,
        objective: String = "",
        acceptanceCriteria: [String] = [],
        constraints: [String] = []
    ) {
        self.id = id
        self.title = title
        self.kind = kind
        self.status = status
        self.objective = objective
        self.acceptanceCriteria = acceptanceCriteria
        self.constraints = constraints
    }
}

public struct BoardColumn: Identifiable, Hashable, Sendable {
    public var status: TaskStatus
    public var tasks: [TaskItem]

    public var id: TaskStatus { status }

    public init(status: TaskStatus, tasks: [TaskItem]) {
        self.status = status
        self.tasks = tasks
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

/// A tool invocation rendered as a structured card in the chat.
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

/// A tool waiting for the user's decision.
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
