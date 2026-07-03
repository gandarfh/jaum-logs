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
    /// interactive step finished.
    public func finishEditing(content: String) async throws {
        guard let request = editorRequest else { return }
        try content.write(toFile: request.path, atomically: true, encoding: .utf8)
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
            let content = (try? String(contentsOfFile: path, encoding: .utf8)) ?? ""
            editorRequest = EditorRequest(path: path, content: content)
        case .message(.detach):
            connection = .disconnected
        case .message(let frame):
            grid.apply(frame)
        case .disconnected(let reason):
            connection = .disconnected
            lastError = reason
        }
    }
}
