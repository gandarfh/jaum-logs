import Foundation
import Testing

@testable import JaumKit

@MainActor
struct TerminalModelTests {
    @Test func attachConnectsAndMirrorsFrames() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach(cols: 4, rows: 1)
        #expect(model.connection == .connected)
        #expect(try transport.sentMessages() == [.resize(cols: 4, rows: 1)])

        try transport.push(.frameFull(cols: 4, rows: 1, cells: [WireCell(x: 0, y: 0, sym: "j")]))
        try transport.push(.frameDiff([WireCell(x: 1, y: 0, sym: "a")]))
        #expect(await waitUntil { model.grid.textRows() == ["ja  "] })
    }

    @Test func attachFailureExposesTheError() async {
        let transport = FakeTransport()
        transport.failConnect = true
        let model = TerminalModel(transport: transport)
        await model.attach()
        #expect(model.connection == .disconnected)
        #expect(model.lastError != nil)
    }

    @Test func sendFailureExposesTheError() async {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        transport.failSend = true
        await model.send(.key(KeyEvent(code: .enter)))
        #expect(model.lastError != nil)
    }

    @Test func runEditorLoadsTheFileIntoTheEmbeddedEditor() async throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("jaumkit-editor-\(getpid())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("conventions.md")
        try "conteudo original".write(to: file, atomically: true, encoding: .utf8)

        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        try transport.push(.runEditor(path: file.path))
        #expect(await waitUntil { model.editorRequest != nil })
        #expect(model.editorRequest?.content == "conteudo original")

        try await model.finishEditing(content: "conteudo novo")
        #expect(model.editorRequest == nil)
        #expect(try String(contentsOf: file, encoding: .utf8) == "conteudo novo")
        #expect(try transport.sentMessages().last == .editorDone)
    }

    @Test func runEditorOnMissingFileOpensEmptyBuffer() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        try transport.push(.runEditor(path: "/nonexistent/jaumkit/file.md"))
        #expect(await waitUntil { model.editorRequest != nil })
        #expect(model.editorRequest?.content == "")
    }

    /// A file that exists but cannot be read must not open an empty editor
    /// (saving would wipe it); the daemon gets EditorDone so it moves on.
    @Test func runEditorOnUnreadableFileRefusesToOpen() async throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("jaumkit-unreadable-\(getpid())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("secreto.md")
        try "conteudo".write(to: file, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o000], ofItemAtPath: file.path)

        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        try transport.push(.runEditor(path: file.path))
        #expect(await waitUntil { model.lastError != nil })
        #expect(model.editorRequest == nil)
        #expect(try transport.sentMessages().last == .editorDone)

        try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: file.path)
        #expect(try String(contentsOf: file, encoding: .utf8) == "conteudo")
    }

    @Test func cancelEditingAnswersTheDaemonWithoutWriting() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        try transport.push(.runEditor(path: "/tmp/whatever.md"))
        #expect(await waitUntil { model.editorRequest != nil })

        await model.cancelEditing()
        #expect(model.editorRequest == nil)
        #expect(try transport.sentMessages().last == .editorDone)
    }

    @Test func finishEditingWithoutARequestIsANoOp() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        try await model.finishEditing(content: "x")
        await model.cancelEditing()
        #expect(try transport.sentMessages() == [.resize(cols: 120, rows: 40)])
    }

    @Test func detachMessageDisconnects() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        try transport.push(.detach)
        #expect(await waitUntil { model.connection == .disconnected })
    }

    @Test func transportErrorDisconnectsWithReason() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        transport.finishIncoming(error: FakeTransport.Failure.connectRefused)
        #expect(await waitUntil { model.connection == .disconnected })
        #expect(model.lastError != nil)
    }

    @Test func reattachAfterDisconnectWorks() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach(cols: 3, rows: 1)
        try transport.push(.detach)
        #expect(await waitUntil { model.connection == .disconnected })

        await model.attach(cols: 3, rows: 1)
        #expect(model.connection == .connected)
        try transport.push(.frameFull(cols: 3, rows: 1, cells: [WireCell(x: 0, y: 0, sym: "z")]))
        #expect(await waitUntil { model.grid.textRows() == ["z  "] })
    }

    @Test func detachClosesTransportAndSecondAttachIsIgnored() async throws {
        let transport = FakeTransport()
        let model = TerminalModel(transport: transport)
        await model.attach()
        await model.attach()
        #expect(try transport.sentMessages() == [.resize(cols: 120, rows: 40)])
        await model.detach()
        #expect(transport.closed)
        #expect(model.connection == .disconnected)
    }
}
