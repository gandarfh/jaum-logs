import Foundation
import Testing

@testable import JaumKit

@MainActor
struct SessionModelTests {
    private func startedModel() async -> SessionModel {
        let model = SessionModel(backend: PreviewBackend())
        model.start()
        _ = await waitUntil { !model.tasks.isEmpty && model.pendingPermission != nil }
        return model
    }

    @Test func startLoadsBoardChatAndPermission() async {
        let model = await startedModel()
        #expect(model.connection == .connected)
        #expect(model.tasks.count == 5)
        #expect(model.messages.count == 2)
        #expect(model.pendingPermission?.toolName == "git push")
    }

    @Test func startTwiceDoesNotDuplicateTheStream() async {
        let model = await startedModel()
        model.start()
        try? await Task.sleep(for: .milliseconds(50))
        #expect(model.messages.count == 2)
    }

    @Test func columnsFollowTheStatusOrderAndKeepEmptyOnes() async {
        let model = await startedModel()
        #expect(model.columns.map(\.status) == TaskStatus.allCases)
        let bySlug = Dictionary(
            uniqueKeysWithValues: model.columns.map { ($0.status, $0.tasks.map(\.id)) })
        #expect(bySlug[.wip] == ["board-nativo"])
        #expect(bySlug[.merged] == ["socket-client"])
    }

    @Test func taskSelectionResolvesTheItem() async {
        let model = await startedModel()
        model.selectedTaskID = "chat-estruturado"
        #expect(model.selectedTask?.status == .review)
        model.selectedTaskID = "nope"
        #expect(model.selectedTask == nil)
    }

    @Test func sendTextAppendsUserAndAssistantMessages() async {
        let model = await startedModel()
        model.sendText("  bora revisar o board  ")
        #expect(await waitUntil { model.messages.count == 4 })
        guard case .markdown(let text) = model.messages[2].blocks.first else {
            Issue.record("expected markdown block")
            return
        }
        #expect(text == "bora revisar o board")
        #expect(model.messages[2].role == .user)
        #expect(model.messages[3].role == .assistant)
    }

    @Test func blankTextIsIgnored() async {
        let model = await startedModel()
        model.sendText("   \n  ")
        try? await Task.sleep(for: .milliseconds(50))
        #expect(model.messages.count == 2)
    }

    @Test func sendImageAppendsAnImageBlock() async {
        let model = await startedModel()
        model.sendImage(data: Data([1, 2, 3]), filename: "captura.png")
        #expect(await waitUntil { model.messages.count == 3 })
        #expect(model.messages[2].blocks.contains(.image(Data([1, 2, 3]))))
    }

    @Test func emptyImageIsIgnored() async {
        let model = await startedModel()
        model.sendImage(data: Data(), filename: "vazio.png")
        try? await Task.sleep(for: .milliseconds(50))
        #expect(model.messages.count == 2)
    }

    @Test func approvingResolvesThePermissionAndLogsAToolCard() async {
        let model = await startedModel()
        model.approvePendingPermission()
        #expect(await waitUntil { model.pendingPermission == nil })
        #expect(await waitUntil { model.messages.count == 3 })
        guard case .tool(let tool) = model.messages[2].blocks.first else {
            Issue.record("expected tool block")
            return
        }
        #expect(tool.state == .succeeded)
    }

    @Test func denyingResolvesThePermissionAsFailed() async {
        let model = await startedModel()
        model.denyPendingPermission()
        #expect(await waitUntil { model.pendingPermission == nil })
        #expect(await waitUntil { model.messages.count == 3 })
        guard case .tool(let tool) = model.messages[2].blocks.first else {
            Issue.record("expected tool block")
            return
        }
        #expect(tool.state == .failed)
    }

    @Test func decisionsWithoutAPendingPermissionAreNoOps() async {
        let model = SessionModel(backend: PreviewBackend())
        model.approvePendingPermission()
        model.denyPendingPermission()
        model.sendText("")
        #expect(model.messages.isEmpty)
    }

    @Test func stopDisconnects() async {
        let model = await startedModel()
        model.stop()
        #expect(model.connection == .disconnected)
    }

    @Test func chatUpdatedReplacesOrAppends() async {
        let model = SessionModel(backend: PreviewBackend())
        let original = ChatMessage(
            id: "m1",
            role: .assistant,
            blocks: [.markdown("antes")],
            timestamp: Date(timeIntervalSince1970: 0)
        )
        model.apply(.chat(original))
        var updated = original
        updated.blocks = [.markdown("depois")]
        model.apply(.chatUpdated(updated))
        #expect(model.messages == [updated])

        let fresh = ChatMessage(
            id: "m2",
            role: .system,
            blocks: [.markdown("novo")],
            timestamp: Date(timeIntervalSince1970: 1)
        )
        model.apply(.chatUpdated(fresh))
        #expect(model.messages.count == 2)
    }

    @Test func permissionResolvedForAnotherRequestKeepsThePending() async {
        let model = SessionModel(backend: PreviewBackend())
        model.apply(.permissionRequested(PermissionRequest(id: "p1", toolName: "t", request: "r")))
        model.apply(.permissionResolved(id: "outro", approved: true))
        #expect(model.pendingPermission?.id == "p1")
        model.apply(.permissionResolved(id: "p1", approved: true))
        #expect(model.pendingPermission == nil)
    }

    @Test func displayNamesAreExposedForTheUI() {
        #expect(TaskStatus.wip.displayName == "Em curso")
        #expect(TaskKind.spike.displayName == "Spike")
        #expect(TaskKind.implementation.displayName == "Implementação")
        #expect(ConnectionState.connecting.displayName == "Conectando")
        #expect(ConnectionState.connected.displayName == "Conectado")
        #expect(ConnectionState.disconnected.displayName == "Desconectado")
        #expect(TaskStatus.allCases.map(\.id) == ["backlog", "ready", "wip", "review", "merged"])
    }
}
