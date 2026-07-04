import Foundation

/// Commands the user can issue from the app. Chat commands address a session
/// of a task, mirroring the future domain protocol (SendMessage carries a
/// session id).
public enum SessionCommand: Hashable, Sendable {
    case sendText(taskID: String, sessionID: String, text: String)
    case sendImage(taskID: String, sessionID: String, data: Data, filename: String)
    case approvePermission(id: String)
    case denyPermission(id: String)
}

/// Domain-level events feeding the UI.
public enum SessionEvent: Hashable, Sendable {
    case projects([ProjectItem])
    case tasks([TaskItem])
    case docs([DocItem])
    case chat(taskID: String, sessionID: String, message: ChatMessage)
    case permissionRequested(PermissionRequest)
    case permissionResolved(id: String, approved: Bool)
    case connection(ConnectionState)
    case latency(milliseconds: Double)
    /// The daemon's CI watch changed a task's review state (capture started,
    /// verdict persisted, re-review pending or blocked by red CI).
    case reviewState(taskID: String, state: ReviewState)
}

/// Source of domain events for one daemon connection. The wire protocol does
/// not carry domain state yet (it only streams terminal frames), so the app
/// talks to this abstraction; a socket-backed implementation plugs in once
/// the daemon exposes domain snapshots and session events.
public protocol SessionBackend: Sendable {
    func events() async -> AsyncStream<SessionEvent>
    func send(_ command: SessionCommand) async
}
