import Foundation

/// Session-level client: frames outgoing messages, decodes incoming ones and
/// runs the attach handshake (announce size, receive the first full frame).
public actor DaemonClient {
    public enum Event: Sendable, Hashable {
        case message(ServerMessage)
        case disconnected(reason: String?)
    }

    public enum ClientError: Error {
        case alreadyAttached
    }

    private let transport: any WireTransport
    private var decoder = WireFrameDecoder()
    private var receiveTask: Task<Void, Never>?

    public init(transport: any WireTransport) {
        self.transport = transport
    }

    /// Connects, announces the terminal size and returns the event stream.
    /// Call `detach()` before attaching again.
    public func attach(cols: UInt16, rows: UInt16) async throws -> AsyncStream<Event> {
        guard receiveTask == nil else {
            throw ClientError.alreadyAttached
        }
        decoder = WireFrameDecoder()
        let incoming = try await transport.connect()
        try await send(.resize(cols: cols, rows: rows))
        let (stream, continuation) = AsyncStream.makeStream(of: Event.self)
        receiveTask = Task {
            do {
                for try await chunk in incoming {
                    for message in try self.decode(chunk) {
                        continuation.yield(.message(message))
                    }
                }
                continuation.yield(.disconnected(reason: nil))
            } catch {
                continuation.yield(.disconnected(reason: error.localizedDescription))
            }
            continuation.finish()
        }
        return stream
    }

    public func send(_ message: ClientMessage) async throws {
        try await transport.send(WireFraming.encode(message))
    }

    public func detach() async {
        receiveTask?.cancel()
        receiveTask = nil
        await transport.close()
    }

    private func decode(_ chunk: Data) throws -> [ServerMessage] {
        try decoder.feed(chunk, as: ServerMessage.self)
    }
}
