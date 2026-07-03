import Foundation
import Testing

@testable import JaumKit

struct DaemonClientTests {
    @Test func attachSendsResizeAndStreamsMessages() async throws {
        let transport = FakeTransport()
        let client = DaemonClient(transport: transport)
        let events = try await client.attach(cols: 80, rows: 24)
        #expect(try transport.sentMessages() == [.resize(cols: 80, rows: 24)])

        try transport.push(.detach)
        try transport.push(.runEditor(path: "/tmp/c.md"))
        transport.finishIncoming()

        var received: [DaemonClient.Event] = []
        for await event in events {
            received.append(event)
        }
        #expect(
            received == [
                .message(.detach),
                .message(.runEditor(path: "/tmp/c.md")),
                .disconnected(reason: nil),
            ])
    }

    @Test func messagesSplitAcrossChunksAreReassembled() async throws {
        let transport = FakeTransport()
        let client = DaemonClient(transport: transport)
        let events = try await client.attach(cols: 10, rows: 5)

        let framed = try WireFraming.encode(
            ServerMessage.frameDiff([WireCell(x: 0, y: 0, sym: "a")]))
        transport.pushRaw(framed.prefix(3))
        transport.pushRaw(framed.suffix(framed.count - 3))
        transport.finishIncoming()

        var received: [DaemonClient.Event] = []
        for await event in events {
            received.append(event)
        }
        #expect(
            received == [
                .message(.frameDiff([WireCell(x: 0, y: 0, sym: "a")])),
                .disconnected(reason: nil),
            ])
    }

    @Test func transportFailureSurfacesAsDisconnected() async throws {
        let transport = FakeTransport()
        let client = DaemonClient(transport: transport)
        let events = try await client.attach(cols: 10, rows: 5)
        transport.finishIncoming(error: FakeTransport.Failure.connectRefused)

        var received: [DaemonClient.Event] = []
        for await event in events {
            received.append(event)
        }
        #expect(received.count == 1)
        guard case .disconnected(let reason) = received[0] else {
            Issue.record("expected disconnected, got \(received[0])")
            return
        }
        #expect(reason != nil)
    }

    @Test func malformedIncomingBytesEndTheStream() async throws {
        let transport = FakeTransport()
        let client = DaemonClient(transport: transport)
        let events = try await client.attach(cols: 10, rows: 5)
        transport.pushRaw(Data([0, 0, 0, 2]) + Data("{}".utf8))

        var received: [DaemonClient.Event] = []
        for await event in events {
            received.append(event)
        }
        #expect(received.count == 1)
        guard case .disconnected(let reason) = received[0] else {
            Issue.record("expected disconnected, got \(received[0])")
            return
        }
        #expect(reason != nil)
    }

    @Test func sendFramesClientMessages() async throws {
        let transport = FakeTransport()
        let client = DaemonClient(transport: transport)
        _ = try await client.attach(cols: 1, rows: 1)
        try await client.send(.key(KeyEvent(code: .enter)))
        try await client.send(.shutdown)
        #expect(
            try transport.sentMessages() == [
                .resize(cols: 1, rows: 1),
                .key(KeyEvent(code: .enter)),
                .shutdown,
            ])
    }

    @Test func detachClosesTheTransport() async throws {
        let transport = FakeTransport()
        let client = DaemonClient(transport: transport)
        _ = try await client.attach(cols: 1, rows: 1)
        await client.detach()
        #expect(transport.closed)
    }

    @Test func connectFailurePropagates() async {
        let transport = FakeTransport()
        transport.failConnect = true
        let client = DaemonClient(transport: transport)
        await #expect(throws: FakeTransport.Failure.self) {
            _ = try await client.attach(cols: 1, rows: 1)
        }
    }
}
