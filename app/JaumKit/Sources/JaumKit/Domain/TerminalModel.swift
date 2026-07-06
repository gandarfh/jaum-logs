import Foundation
import Observation

/// An embedded-editor request coming from the daemon (`RunEditor`): the app
/// edits the file in place and answers `EditorDone` on save. Identity
/// includes a per-request revision, so a second RunEditor for the same path
/// recreates the sheet with fresh content instead of showing a stale buffer.
public struct EditorRequest: Identifiable, Hashable, Sendable {
    public var path: String
    public var content: String
    public var revision: Int

    public var id: String { "\(revision):\(path)" }

    public init(path: String, content: String, revision: Int = 0) {
        self.path = path
        self.content = content
        self.revision = revision
    }
}

/// View-model for the live daemon connection: mirrors the terminal frame
/// buffer and drives the embedded editor round-trip.
@MainActor
@Observable
public final class TerminalModel {
    public private(set) var grid = FrameGrid()
    public private(set) var connection: ConnectionState = .disconnected
    public private(set) var lastError: String?
    public var editorRequest: EditorRequest?

    private let client: DaemonClient
    private var consumeTask: Task<Void, Never>?
    private var cleanupTask: Task<Void, Never>?
    private var editorRevision = 0

    public init(transport: any WireTransport) {
        self.client = DaemonClient(transport: transport)
    }

    public func attach(cols: UInt16 = 120, rows: UInt16 = 40) async {
        guard consumeTask == nil else { return }
        connection = .connecting
        do {
            // A previous stream may still be tearing down asynchronously;
            // awaiting its cleanup keeps the stray detach from killing the
            // connection this attach is about to open.
            await cleanupTask?.value
            cleanupTask = nil
            await client.detach()
            let events = try await client.attach(cols: cols, rows: rows)
            connection = .connected
            lastError = nil
            consumeTask = Task { [weak self] in
                for await event in events {
                    guard let self else { return }
                    self.apply(event)
                }
            }
        } catch {
            connection = .disconnected
            lastError = error.localizedDescription
        }
    }

    public func detach() async {
        consumeTask?.cancel()
        consumeTask = nil
        await client.detach()
        connection = .disconnected
    }

    public func send(_ message: ClientMessage) async {
        do {
            try await client.send(message)
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Saves the embedded editor buffer back to disk and tells the daemon the
    /// interactive step finished. I/O runs off the main actor. The request is
    /// only cleared after EditorDone is confirmed sent, so a failure keeps
    /// the sheet open for retry instead of leaving the daemon hanging.
    public func finishEditing(content: String) async throws {
        guard let request = editorRequest else { return }
        let path = request.path
        try await Task.detached(priority: .userInitiated) {
            try content.write(toFile: path, atomically: true, encoding: .utf8)
        }.value
        try await client.send(.editorDone)
        editorRequest = nil
    }

    /// Cancel answers with the same `EditorDone` as save, only without a write.
    /// The current wire protocol has no distinct cancel signal, so the daemon
    /// cannot tell the two apart; it just resumes. If cancel ever needs to
    /// differ (for example, to skip a post-edit reload), the protocol needs a
    /// dedicated message.
    public func cancelEditing() async throws {
        guard editorRequest != nil else { return }
        try await client.send(.editorDone)
        editorRequest = nil
    }

    func apply(_ event: DaemonClient.Event) {
        switch event {
        case .message(.runEditor(let path)):
            Task {
                await openEditor(path: path)
            }
        case .message(.detach):
            handleStreamEnd(reason: nil)
        case .message(let frame):
            grid.apply(frame)
        case .disconnected(let reason):
            handleStreamEnd(reason: reason)
        }
    }

    /// A missing file opens an empty buffer (the daemon may be creating it);
    /// a file that exists but cannot be read must NOT open an editor, or
    /// saving would overwrite it with emptiness. The daemon is answered with
    /// EditorDone so the flow is not left hanging.
    private func openEditor(path: String) async {
        do {
            let content = try await Self.readIfExists(path)
            editorRevision += 1
            editorRequest = EditorRequest(
                path: path, content: content ?? "", revision: editorRevision)
        } catch {
            lastError = "Could not read \(path): \(error.localizedDescription)"
            await send(.editorDone)
        }
    }

    private nonisolated static func readIfExists(_ path: String) async throws -> String? {
        try await Task.detached(priority: .userInitiated) {
            guard FileManager.default.fileExists(atPath: path) else { return nil }
            return try String(contentsOfFile: path, encoding: .utf8)
        }.value
    }

    /// Releases the dead connection so a later `attach()` can start over
    /// (the transport builds a fresh connection per connect). The detach is
    /// tracked in `cleanupTask` so the next attach awaits it instead of
    /// racing it.
    private func handleStreamEnd(reason: String?) {
        connection = .disconnected
        lastError = reason
        consumeTask?.cancel()
        consumeTask = nil
        let client = self.client
        cleanupTask = Task {
            await client.detach()
        }
    }
}
