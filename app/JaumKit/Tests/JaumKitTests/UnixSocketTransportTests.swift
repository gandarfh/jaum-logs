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

    @Test func defaultSocketPathLivesInTheJaumHome() {
        let path = defaultDaemonSocketPath()
        #expect(path.hasSuffix("/jaum/daemon.sock"))
        #expect(path.hasPrefix(FileManager.default.homeDirectoryForCurrentUser.path))
    }
}
