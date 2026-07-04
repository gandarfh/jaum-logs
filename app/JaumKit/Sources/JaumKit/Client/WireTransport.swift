import Foundation
import Network
import Synchronization

/// Byte-level transport to the daemon. Abstracted so the client and the
/// view-models can be tested against an in-memory implementation.
public protocol WireTransport: Sendable {
    /// Establishes the connection and returns the stream of raw incoming
    /// bytes. The stream finishes on clean EOF and throws on failure.
    func connect() async throws -> AsyncThrowingStream<Data, any Error>
    func send(_ data: Data) async throws
    func close() async
}

/// Default daemon socket path, mirroring the daemon's `$HOME/jaum/daemon.sock`.
public func defaultDaemonSocketPath() -> String {
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    return home + "/jaum/daemon.sock"
}

/// Unix domain socket transport backed by Network.framework. Each `connect()`
/// starts a fresh `NWConnection` (a cancelled connection cannot be restarted),
/// so the transport survives failures and supports reconnection.
public final class UnixSocketTransport: WireTransport, Sendable {
    public enum TransportError: Error {
        case notConnected
    }

    private let path: String
    private let current = Mutex<NWConnection?>(nil)

    public init(path: String = defaultDaemonSocketPath()) {
        self.path = path
    }

    public func connect() async throws -> AsyncThrowingStream<Data, any Error> {
        let connection = NWConnection(to: .unix(path: path), using: .tcp)
        current.withLock { existing in
            existing?.cancel()
            existing = connection
        }

        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, any Error>) in
            let resumed = ResumeGuard()
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    if resumed.tryResume() { cont.resume() }
                // .waiting is treated as terminal on purpose: for a local
                // unix socket it means the daemon is not listening (refused
                // or missing), and failing fast beats hanging the pill in
                // "connecting" while NW retries. Reconnection is one click.
                case .failed(let error), .waiting(let error):
                    connection.cancel()
                    if resumed.tryResume() { cont.resume(throwing: error) }
                default:
                    break
                }
            }
            connection.start(queue: .global(qos: .userInitiated))
        }
        return AsyncThrowingStream { continuation in
            Self.receiveLoop(connection, into: continuation)
            continuation.onTermination = { _ in connection.cancel() }
        }
    }

    private static func receiveLoop(
        _ connection: NWConnection,
        into continuation: AsyncThrowingStream<Data, any Error>.Continuation
    ) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 1 << 16) {
            data, _, isComplete, error in
            if let data, !data.isEmpty {
                continuation.yield(data)
            }
            if let error {
                continuation.finish(throwing: error)
            } else if isComplete {
                continuation.finish()
            } else {
                receiveLoop(connection, into: continuation)
            }
        }
    }

    public func send(_ data: Data) async throws {
        guard let connection = current.withLock({ $0 }) else {
            throw TransportError.notConnected
        }
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, any Error>) in
            connection.send(
                content: data,
                completion: .contentProcessed { error in
                    if let error {
                        cont.resume(throwing: error)
                    } else {
                        cont.resume()
                    }
                })
        }
    }

    public func close() async {
        let connection = current.withLock { existing -> NWConnection? in
            let previous = existing
            existing = nil
            return previous
        }
        connection?.cancel()
    }
}

/// NWConnection can report failure through more than one callback; this keeps
/// the checked continuation from being resumed twice.
private final class ResumeGuard: @unchecked Sendable {
    private let lock = NSLock()
    private var resumed = false

    func tryResume() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if resumed { return false }
        resumed = true
        return true
    }
}
