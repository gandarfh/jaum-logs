import Foundation
import Synchronization

@testable import JaumKit

/// In-memory transport: captures outgoing frames and lets tests push
/// incoming bytes or fail the connection.
final class FakeTransport: WireTransport, Sendable {
    enum Failure: Error {
        case connectRefused
        case sendRefused
    }

    private struct State {
        var sent: [Data] = []
        var continuation: AsyncThrowingStream<Data, any Error>.Continuation?
        var closed = false
        var failConnect = false
        var failSend = false
    }

    private let state = Mutex(State())

    var failConnect: Bool {
        get { state.withLock { $0.failConnect } }
        set { state.withLock { $0.failConnect = newValue } }
    }

    var failSend: Bool {
        get { state.withLock { $0.failSend } }
        set { state.withLock { $0.failSend = newValue } }
    }

    func connect() async throws -> AsyncThrowingStream<Data, any Error> {
        if failConnect { throw Failure.connectRefused }
        let (stream, continuation) = AsyncThrowingStream.makeStream(of: Data.self)
        state.withLock { $0.continuation = continuation }
        return stream
    }

    func send(_ data: Data) async throws {
        if failSend { throw Failure.sendRefused }
        state.withLock { $0.sent.append(data) }
    }

    func close() async {
        let continuation = state.withLock {
            $0.closed = true
            return $0.continuation
        }
        continuation?.finish()
    }

    var sent: [Data] {
        state.withLock { $0.sent }
    }

    var closed: Bool {
        state.withLock { $0.closed }
    }

    /// Decodes every captured outgoing frame as a client message.
    func sentMessages() throws -> [ClientMessage] {
        var decoder = WireFrameDecoder()
        var messages: [ClientMessage] = []
        for data in sent {
            messages.append(contentsOf: try decoder.feed(data, as: ClientMessage.self))
        }
        return messages
    }

    func push(_ message: ServerMessage) throws {
        let framed = try WireFraming.encode(message)
        state.withLock { $0.continuation }?.yield(framed)
    }

    func pushRaw(_ data: Data) {
        state.withLock { $0.continuation }?.yield(data)
    }

    func finishIncoming(error: (any Error)? = nil) {
        let continuation = state.withLock { $0.continuation }
        if let error {
            continuation?.finish(throwing: error)
        } else {
            continuation?.finish()
        }
    }
}

/// Polls an async condition until it holds or the timeout hits, yielding to
/// the cooperative pool between checks. Duplicated in the app test bundle on
/// purpose (the two targets share no test support product); keep the
/// generous timeout in sync so slow CI runners do not flake.
func waitUntil(
    timeout: TimeInterval = 15,
    _ condition: @MainActor @escaping () -> Bool
) async -> Bool {
    let deadline = ContinuousClock.now.advanced(by: .seconds(timeout))
    while ContinuousClock.now < deadline {
        if await MainActor.run(body: condition) { return true }
        try? await Task.sleep(for: .milliseconds(10))
    }
    return await MainActor.run(body: condition)
}
