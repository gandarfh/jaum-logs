import Foundation
import Testing

@testable import JaumKit

struct UnixSocketTransportTests {
    @Test func handshakeAgainstARealUnixSocket() async throws {
        let path = "/tmp/jaumkit-test-\(getpid())-handshake.sock"
        let server = try UnixSocketServer(path: path)
        defer { server.shutdown() }

        server.acceptOnce { fd in
            guard let payload = UnixSocketServer.readFrame(from: fd),
                let resize = try? JSONDecoder().decode(ClientMessage.self, from: payload),
                case .resize(let cols, let rows) = resize
            else { return }
            let frame = ServerMessage.frameFull(
                cols: cols,
                rows: rows,
                cells: [WireCell(x: 0, y: 0, sym: "j")]
            )
            if let framed = try? WireFraming.encode(frame) {
                UnixSocketServer.write(framed, to: fd)
            }
        }

        let client = DaemonClient(transport: UnixSocketTransport(path: path))
        let events = try await client.attach(cols: 80, rows: 24)

        var received: [DaemonClient.Event] = []
        for await event in events {
            received.append(event)
        }
        #expect(received.count == 2)
        #expect(
            received.first
                == .message(
                    .frameFull(cols: 80, rows: 24, cells: [WireCell(x: 0, y: 0, sym: "j")])))
        // Network.framework may surface the server-side close as an error
        // instead of a clean EOF; either way the client must end disconnected.
        guard case .disconnected = received.last else {
            Issue.record("expected disconnected, got \(String(describing: received.last))")
            return
        }
        await client.detach()
    }

    @Test func connectToMissingSocketThrows() async {
        let transport = UnixSocketTransport(path: "/tmp/jaumkit-test-missing.sock")
        let client = DaemonClient(transport: transport)
        await #expect(throws: (any Error).self) {
            _ = try await client.attach(cols: 1, rows: 1)
        }
    }

    @Test func sendWithoutConnectThrows() async {
        let transport = UnixSocketTransport(path: "/tmp/jaumkit-test-missing.sock")
        await #expect(throws: UnixSocketTransport.TransportError.self) {
            try await transport.send(Data([1]))
        }
    }

    /// A failed connect must not poison the transport: after the daemon comes
    /// up on the same path, the next connect() succeeds with a fresh
    /// NWConnection (a cancelled one cannot be restarted).
    @Test func reconnectAfterFailureWorks() async throws {
        let path = "/tmp/jaumkit-test-\(getpid())-reconnect.sock"
        let transport = UnixSocketTransport(path: path)
        await #expect(throws: (any Error).self) {
            _ = try await transport.connect()
        }

        let server = try UnixSocketServer(path: path)
        defer { server.shutdown() }
        server.acceptOnce { fd in
            guard let payload = UnixSocketServer.readFrame(from: fd) else { return }
            UnixSocketServer.write(Data([0, 0, 0, UInt8(payload.count)]) + payload, to: fd)
        }

        let incoming = try await transport.connect()
        try await transport.send(try WireFraming.encode(ClientMessage.shutdown))
        var chunks = Data()
        for try await chunk in incoming {
            chunks.append(chunk)
            if chunks.count >= 4 { break }
        }
        #expect(!chunks.isEmpty)
        await transport.close()
    }

    @Test func defaultSocketPathLivesInTheJaumHome() {
        let path = defaultDaemonSocketPath()
        #expect(path.hasSuffix("/jaum/daemon.sock"))
        #expect(path.hasPrefix(FileManager.default.homeDirectoryForCurrentUser.path))
    }
}
