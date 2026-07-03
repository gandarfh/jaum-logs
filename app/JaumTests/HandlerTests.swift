import JaumKit
import SwiftUI
import Testing

@testable import Jaum

@MainActor
struct HandlerTests {
    private func terminal() -> TerminalModel {
        TerminalModel(transport: UnixSocketTransport(path: "/tmp/jaum-nada.sock"))
    }

    @Test func editorSaveWritesTheFileAndClearsTheRequest() async throws {
        let model = terminal()
        let file = FileManager.default.temporaryDirectory
            .appendingPathComponent("jaum-editor-\(getpid()).md")
        defer { try? FileManager.default.removeItem(at: file) }
        let request = EditorRequest(path: file.path, content: "antes")
        model.editorRequest = request

        let sheet = EditorSheet(terminal: model, request: request)
        await sheet.save()
        #expect(try String(contentsOf: file, encoding: .utf8) == "antes")
        #expect(model.editorRequest == nil)
    }

    @Test func editorSaveFailureKeepsTheSheetAlive() async {
        let model = terminal()
        let request = EditorRequest(path: "/diretorio/inexistente/arquivo.md", content: "x")
        model.editorRequest = request
        let sheet = EditorSheet(terminal: model, request: request)
        await sheet.save()
        #expect(model.editorRequest != nil)
    }

    @Test func editorCancelAnswersTheDaemon() async {
        let model = terminal()
        let request = EditorRequest(path: "/tmp/x.md", content: "x")
        model.editorRequest = request
        let sheet = EditorSheet(terminal: model, request: request)
        await sheet.cancel()
        #expect(model.editorRequest == nil)
    }

    @Test func editorSheetRendersErrorAndFileName() {
        let request = EditorRequest(path: "/tmp/pasta/conventions.md", content: "corpo")
        let sheet = EditorSheet(
            terminal: terminal(), request: request, saveError: "sem espaço em disco")
        #expect(sheet.fileName == "conventions.md")
        renderInWindow(sheet)
    }

    @Test func rootDecidePermissionDispatchesExactlyOneDecision() async {
        let session = await startedSession()
        let root = RootView(session: session, terminal: terminal())
        root.decidePermission(approved: true)
        #expect(await waitUntil { session.pendingPermission == nil })
    }

    @Test func rootDenyAlsoResolves() async {
        let session = await startedSession()
        let root = RootView(session: session, terminal: terminal())
        root.decidePermission(approved: false)
        #expect(await waitUntil { session.pendingPermission == nil })
    }

    @Test func reconnectOnlyFiresWhenDisconnected() async {
        let session = await startedSession()
        let model = terminal()
        let root = RootView(session: session, terminal: model)
        root.reconnectIfNeeded()
        _ = await waitUntil { model.lastError != nil }
        #expect(model.connection == .disconnected)
    }

    @Test func permissionPresentedBindingIsPresentationOnly() async {
        let session = await startedSession()
        let root = RootView(session: session, terminal: terminal())
        let binding = root.permissionPresented
        binding.wrappedValue = false
        try? await Task.sleep(for: .milliseconds(50))
        #expect(session.pendingPermission != nil)
    }
}
