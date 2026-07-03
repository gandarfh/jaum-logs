import Foundation
import Observation

/// An embedded-editor request coming from the daemon (`RunEditor`): the app
/// edits the file in place and answers `EditorDone` on save.
public struct EditorRequest: Identifiable, Hashable, Sendable {
    public var path: String
    public var content: String

    public var id: String { path }

    public init(path: String, content: String) {
        self.path = path
        self.content = content
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

    public init(transport: any WireTransport) {
        self.client = DaemonClient(transport: transport)
    }

    public func attach(cols: UInt16 = 120, rows: UInt16 = 40) async {
        guard consumeTask == nil else { return }
        connection = .connecting
        do {
            // A previous stream may still be tearing down asynchronously;
            // detaching first makes manual reconnection deterministic.
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
    /// interactive step finished. I/O runs off the main actor.
    public func finishEditing(content: String) async throws {
        guard let request = editorRequest else { return }
        let path = request.path
        try await Task.detached(priority: .userInitiated) {
            try content.write(toFile: path, atomically: true, encoding: .utf8)
        }.value
        editorRequest = nil
        await send(.editorDone)
    }

    public func cancelEditing() async {
        guard editorRequest != nil else { return }
        editorRequest = nil
        await send(.editorDone)
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
            editorRequest = EditorRequest(path: path, content: content ?? "")
        } catch {
            lastError = "Não deu para ler \(path): \(error.localizedDescription)"
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
    /// (the transport builds a fresh connection per connect).
    private func handleStreamEnd(reason: String?) {
        connection = .disconnected
        lastError = reason
        consumeTask?.cancel()
        consumeTask = nil
        let client = self.client
        Task {
            await client.detach()
        }
    }
}
