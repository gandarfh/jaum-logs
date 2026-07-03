import Foundation

/// Commands the user can issue from the app.
public enum SessionCommand: Hashable, Sendable {
    case sendText(String)
    case sendImage(data: Data, filename: String)
    case approvePermission(id: String)
    case denyPermission(id: String)
}

/// Domain-level events feeding the UI.
public enum SessionEvent: Hashable, Sendable {
    case board([TaskItem])
    case chat(ChatMessage)
    case chatUpdated(ChatMessage)
    case permissionRequested(PermissionRequest)
    case permissionResolved(id: String, approved: Bool)
    case connection(ConnectionState)
}

/// Source of domain events for one session. The daemon wire protocol does not
/// carry domain state yet (it only streams terminal frames), so the app talks
/// to this abstraction; a socket-backed implementation plugs in once the
/// daemon exposes domain snapshots.
public protocol SessionBackend: Sendable {
    func events() async -> AsyncStream<SessionEvent>
    func send(_ command: SessionCommand) async
}
