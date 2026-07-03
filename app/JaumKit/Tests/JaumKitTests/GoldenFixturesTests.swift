import Foundation
import Testing

@testable import JaumKit

/// Decodes the exact JSON fixtures committed by the Rust protocol tests and
/// re-encodes them, proving both directions of the Swift mirror are lossless.
/// The fixtures live in the Rust crate; this suite reads the same files.
struct GoldenFixturesTests {
    static let fixtureDirectory: URL = {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 {
            url.deleteLastPathComponent()
        }
        return url.appendingPathComponent("crates/cli/tests/fixtures/protocol")
    }()

    private func fixtureData(_ name: String) throws -> Data {
        try Data(contentsOf: Self.fixtureDirectory.appendingPathComponent("\(name).json"))
    }

    private func jsonObject(_ data: Data) throws -> NSObject {
        let object = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        return try #require(object as? NSObject)
    }

    /// Decode the fixture, re-encode the result and compare at the JSON value
    /// level (whitespace and key order insensitive), like the Rust golden test.
    @discardableResult
    private func roundtrip<T: Codable>(_ name: String, as type: T.Type) throws -> T {
        let data = try fixtureData(name)
        let decoded = try JSONDecoder().decode(type, from: data)
        let reencoded = try JSONEncoder().encode(decoded)
        #expect(try jsonObject(reencoded) == jsonObject(data), "roundtrip of \(name) is lossy")
        return decoded
    }

    @Test func clientKey() throws {
        let decoded = try roundtrip("client_key", as: ClientMessage.self)
        #expect(decoded == .key(KeyEvent(code: .char("p"), modifiers: .control)))
    }

    @Test func clientMouse() throws {
        let decoded = try roundtrip("client_mouse", as: ClientMessage.self)
        #expect(decoded == .mouse(MouseEvent(kind: .down(.left), column: 12, row: 5)))
    }

    @Test func clientResize() throws {
        let decoded = try roundtrip("client_resize", as: ClientMessage.self)
        #expect(decoded == .resize(cols: 120, rows: 40))
    }

    @Test func clientEditorDone() throws {
        let decoded = try roundtrip("client_editor_done", as: ClientMessage.self)
        #expect(decoded == .editorDone)
    }

    @Test func clientShutdown() throws {
        let decoded = try roundtrip("client_shutdown", as: ClientMessage.self)
        #expect(decoded == .shutdown)
    }

    @Test func serverFrameFull() throws {
        let decoded = try roundtrip("server_frame_full", as: ServerMessage.self)
        guard case .frameFull(let cols, let rows, let cells) = decoded else {
            Issue.record("expected FrameFull, got \(decoded)")
            return
        }
        #expect(cols == 80)
        #expect(rows == 24)
        #expect(cells == Self.sampleCells)
    }

    @Test func serverFrameDiff() throws {
        let decoded = try roundtrip("server_frame_diff", as: ServerMessage.self)
        #expect(decoded == .frameDiff(Self.sampleCells))
    }

    @Test func serverDetach() throws {
        let decoded = try roundtrip("server_detach", as: ServerMessage.self)
        #expect(decoded == .detach)
    }

    @Test func serverRunEditor() throws {
        let decoded = try roundtrip("server_run_editor", as: ServerMessage.self)
        #expect(decoded == .runEditor(path: "/tmp/conventions.md"))
    }

    @Test func handshake() throws {
        struct Handshake: Codable {
            var client: ClientMessage
            var server: ServerMessage
        }
        let decoded = try roundtrip("handshake", as: Handshake.self)
        #expect(decoded.client == .resize(cols: 80, rows: 24))
        guard case .frameFull(80, 24, Self.sampleCells) = decoded.server else {
            Issue.record("expected FrameFull, got \(decoded.server)")
            return
        }
    }

    /// The two cells pinned by the Rust fixtures, covering RGB, indexed and
    /// named colors plus modifier combinations.
    static let sampleCells: [WireCell] = [
        WireCell(x: 0, y: 0, sym: "j", fg: .rgb(180, 142, 255), mods: .bold),
        WireCell(
            x: 1,
            y: 0,
            sym: "◆",
            fg: .indexed(42),
            bg: .named(.black),
            underline: .named(.cyan),
            mods: [.italic, .underlined]
        ),
    ]
}
