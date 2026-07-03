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

    @Test func startLoadsProjectsTasksDocsAndPermission() async {
        let model = await startedModel()
        #expect(model.connection == .connected)
        #expect(model.projects.count == 2)
        #expect(model.selectedProjectID == "jaum-logs")
        #expect(model.tasks.count == 6)
        #expect(model.docs.count == 2)
        #expect(model.pendingPermission?.toolName == "git push")
    }

    /// A duplicated stream would double every echo; probing with a real
    /// message makes the check deterministic (commands are ordered, so the
    /// probe's echoes arrive after any duplicate's).
    @Test func startTwiceDoesNotDuplicateTheStream() async {
        let model = await startedModel()
        model.start()
        let before = playMessages(model).count
        model.sendText("sonda", taskID: "jaum-42", sessionID: "jaum-42-play")
        _ = await waitUntil { self.playMessages(model).count >= before + 2 }
        #expect(playMessages(model).count == before + 2)
    }

    @Test func sectionsFollowStatusOrderAndOmitEmptyGroups() async {
        let model = await startedModel()
        #expect(model.sections.map(\.status) == [.wip, .review, .ready, .backlog, .merged])
        #expect(model.sections.first?.tasks.map(\.id) == ["jaum-42", "jaum-39"])
    }

    @Test func statusFilterNarrowsTheSections() async {
        let model = await startedModel()
        model.statusFilter = .review
        #expect(model.sections.map(\.status) == [.review])
        #expect(model.sections.first?.tasks.map(\.id) == ["jaum-31"])
        model.statusFilter = nil
        #expect(model.sections.count == 5)
    }

    @Test func taskCountsFeedTheSidebar() async {
        let model = await startedModel()
        #expect(model.taskCount(for: .wip) == 2)
        #expect(model.taskCount(for: .review) == 1)
        #expect(model.taskCount(for: .merged) == 1)
    }

    @Test func selectingATaskResetsTheTabToDetail() async {
        let model = await startedModel()
        model.selectedTaskID = "jaum-42"
        model.selectedTab = .session("jaum-42-play")
        model.selectedTaskID = "jaum-31"
        #expect(model.selectedTab == .detail)
        #expect(model.selectedTask?.status == .review)
        model.selectedTaskID = "jaum-31"
        model.selectedTab = .session("jaum-31-review")
        #expect(model.selectedTab == .session("jaum-31-review"))
    }

    @Test func sessionLookupResolvesTabs() async {
        let model = await startedModel()
        let task = model.tasks.first { $0.id == "jaum-31" }!
        #expect(model.session("jaum-31-review", of: task)?.kind == .review)
        #expect(model.session("nope", of: task) == nil)
    }

    @Test func sendTextAppendsToTheAddressedSession() async {
        let model = await startedModel()
        let before = playMessages(model).count
        model.sendText("  bora revisar  ", taskID: "jaum-42", sessionID: "jaum-42-play")
        #expect(await waitUntil { self.playMessages(model).count == before + 2 })
        let messages = playMessages(model)
        #expect(messages[before].role == .user)
        #expect(messages[before].blocks == [.markdown("bora revisar")])
        #expect(messages[before + 1].role == .assistant)
    }

    /// The blank send must produce nothing; the probe that follows it is
    /// ordered after, so if the blank had gone through the first new user
    /// message would be the blank, not the probe.
    @Test func blankTextIsIgnored() async {
        let model = await startedModel()
        let before = playMessages(model).count
        model.sendText("   \n ", taskID: "jaum-42", sessionID: "jaum-42-play")
        model.sendText("sonda", taskID: "jaum-42", sessionID: "jaum-42-play")
        _ = await waitUntil { self.playMessages(model).count >= before + 2 }
        #expect(playMessages(model).count == before + 2)
        guard case .markdown(let text) = playMessages(model)[before].blocks.first else {
            Issue.record("expected markdown block")
            return
        }
        #expect(text == "sonda")
    }

    @Test func sendImageAppendsAnImageBlock() async {
        let model = await startedModel()
        let before = playMessages(model).count
        model.sendImage(
            data: Data([1, 2, 3]),
            filename: "captura.png",
            taskID: "jaum-42",
            sessionID: "jaum-42-play"
        )
        #expect(await waitUntil { self.playMessages(model).count == before + 1 })
        #expect(playMessages(model).last?.blocks.contains(.image(Data([1, 2, 3]))) == true)
    }

    @Test func attachImageReadsAndSendsTheFile() async throws {
        let model = await startedModel()
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("jaumkit-attach-\(getpid()).png")
        try Data([9, 9, 9]).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }

        let before = playMessages(model).count
        model.attachImage(.success(url), taskID: "jaum-42", sessionID: "jaum-42-play")
        #expect(await waitUntil { self.playMessages(model).count == before + 1 })
        #expect(playMessages(model).last?.blocks.contains(.image(Data([9, 9, 9]))) == true)
        #expect(model.attachmentError == nil)
    }

    @Test func attachImageFailuresSurfaceTheError() async {
        let model = await startedModel()
        model.attachImage(
            .failure(CocoaError(.fileReadNoSuchFile)), taskID: "jaum-42",
            sessionID: "jaum-42-play")
        #expect(model.attachmentError != nil)
        model.clearAttachmentError()
        #expect(model.attachmentError == nil)

        let missing = FileManager.default.temporaryDirectory
            .appendingPathComponent("jaumkit-attach-\(getpid())-missing.png")
        model.attachImage(.success(missing), taskID: "jaum-42", sessionID: "jaum-42-play")
        #expect(await waitUntil { model.attachmentError != nil })
    }

    @Test func emptyImageIsIgnored() async {
        let model = await startedModel()
        let before = playMessages(model).count
        model.sendImage(
            data: Data(), filename: "vazio.png", taskID: "jaum-42", sessionID: "jaum-42-play")
        model.sendText("sonda", taskID: "jaum-42", sessionID: "jaum-42-play")
        _ = await waitUntil { self.playMessages(model).count >= before + 2 }
        #expect(playMessages(model).count == before + 2)
        #expect(playMessages(model)[before].blocks == [.markdown("sonda")])
    }

    @Test func chatToUnknownTaskOrSessionIsDropped() async {
        let model = await startedModel()
        model.apply(
            .chat(
                taskID: "nope",
                sessionID: "x",
                message: ChatMessage(
                    id: "m", role: .user, blocks: [], timestamp: Date(timeIntervalSince1970: 0))
            ))
        model.apply(
            .chat(
                taskID: "jaum-42",
                sessionID: "nope",
                message: ChatMessage(
                    id: "m", role: .user, blocks: [], timestamp: Date(timeIntervalSince1970: 0))
            ))
        #expect(playMessages(model).allSatisfy { $0.id != "m" })
    }

    @Test func chatWithKnownIDReplacesTheMessage() async {
        let model = await startedModel()
        var updated = playMessages(model)[0]
        updated.blocks = [.markdown("editado")]
        model.apply(.chat(taskID: "jaum-42", sessionID: "jaum-42-play", message: updated))
        #expect(playMessages(model)[0].blocks == [.markdown("editado")])
    }

    @Test func approvingResolvesThePermissionAndLogsAToolCard() async {
        let model = await startedModel()
        let before = playMessages(model).count
        model.approvePendingPermission()
        #expect(await waitUntil { model.pendingPermission == nil })
        #expect(await waitUntil { self.playMessages(model).count == before + 1 })
        guard case .tool(let tool) = playMessages(model).last?.blocks.first else {
            Issue.record("expected tool block")
            return
        }
        #expect(tool.state == .succeeded)
    }

    @Test func denyingResolvesThePermissionAsFailed() async {
        let model = await startedModel()
        model.denyPendingPermission()
        #expect(await waitUntil { model.pendingPermission == nil })
        #expect(
            await waitUntil {
                if case .tool(let tool)? = self.playMessages(model).last?.blocks.first {
                    return tool.state == .failed
                }
                return false
            })
    }

    @Test func decisionsWithoutAPendingPermissionAreNoOps() async {
        let model = SessionModel(backend: PreviewBackend())
        model.approvePendingPermission()
        model.denyPendingPermission()
        model.sendText("", taskID: "t", sessionID: "s")
        #expect(model.tasks.isEmpty)
    }

    @Test func permissionResolvedForAnotherRequestKeepsThePending() async {
        let model = SessionModel(backend: PreviewBackend())
        model.apply(.permissionRequested(PermissionRequest(id: "p1", toolName: "t", request: "r")))
        model.apply(.permissionResolved(id: "outro", approved: true))
        #expect(model.pendingPermission?.id == "p1")
        model.apply(.permissionResolved(id: "p1", approved: true))
        #expect(model.pendingPermission == nil)
    }

    @Test func latencySamplesFeedTheAverageAndAreCapped() async throws {
        let model = await startedModel()
        #expect(model.latencySamples.count == 7)
        let average = try #require(model.averageLatency)
        #expect(average > 20 && average < 30)

        for _ in 0..<70 {
            model.apply(.latency(milliseconds: 10))
        }
        #expect(model.latencySamples.count == 60)

        let empty = SessionModel(backend: PreviewBackend())
        #expect(empty.averageLatency == nil)
    }

    @Test func selectedDocFallsBackToTheFirst() async {
        let model = await startedModel()
        #expect(model.selectedDoc?.name == "conventions.md")
        model.selectedDocID = "arquitetura.md"
        #expect(model.selectedDoc?.name == "arquitetura.md")
        model.selectedDocID = "nope.md"
        #expect(model.selectedDoc?.name == "conventions.md")
    }

    @Test func stopDisconnects() async {
        let model = await startedModel()
        model.stop()
        #expect(model.connection == .disconnected)
    }

    @Test func stoppedSubscribersAreForgottenByTheBackend() async {
        let backend = PreviewBackend()
        let model = SessionModel(backend: backend)
        model.start()
        _ = await waitUntil { !model.tasks.isEmpty }
        model.stop()
        var count = await backend.subscriberCount
        for _ in 0..<200 where count != 0 {
            try? await Task.sleep(for: .milliseconds(10))
            count = await backend.subscriberCount
        }
        #expect(count == 0)
    }

    @Test func resolvedPermissionIsNotRepromptedToLateSubscribers() async {
        let backend = PreviewBackend()
        let first = SessionModel(backend: backend)
        first.start()
        _ = await waitUntil { first.pendingPermission != nil }
        first.approvePendingPermission()
        _ = await waitUntil { first.pendingPermission == nil }

        let second = SessionModel(backend: backend)
        second.start()
        _ = await waitUntil { !second.tasks.isEmpty }
        #expect(second.pendingPermission == nil)
    }

    @Test func commandsReachTheBackendInOrder() async {
        let model = await startedModel()
        let before = playMessages(model).count
        for index in 1...4 {
            model.sendText("mensagem \(index)", taskID: "jaum-42", sessionID: "jaum-42-play")
        }
        #expect(await waitUntil { self.playMessages(model).count == before + 8 })
        let userTexts = playMessages(model).suffix(8)
            .filter { $0.role == .user }
            .compactMap { message -> String? in
                if case .markdown(let text) = message.blocks.first { return text }
                return nil
            }
        #expect(userTexts == ["mensagem 1", "mensagem 2", "mensagem 3", "mensagem 4"])
    }

    @Test func taskDerivedFieldsForTheList() async {
        let model = await startedModel()
        let wip = model.tasks.first { $0.id == "jaum-42" }!
        #expect(wip.hasLiveSession)
        #expect(wip.doneCriteriaCount == 2)
        let review = model.tasks.first { $0.id == "jaum-31" }!
        #expect(review.findingsCount == 2)
        #expect(!review.hasLiveSession)
    }

    @Test func displayNamesAreExposedForTheUI() {
        #expect(TaskStatus.wip.displayName == "Em progresso")
        #expect(TaskStatus.review.displayName == "Review")
        #expect(TaskStatus.ready.displayName == "Pronto")
        #expect(TaskStatus.backlog.displayName == "Backlog")
        #expect(TaskStatus.merged.displayName == "Merged")
        #expect(TaskKind.spike.displayName == "Spike")
        #expect(TaskKind.implementation.displayName == "Implementação")
        #expect(SessionKind.play.displayName == "Play")
        #expect(SessionKind.review.displayName == "Review")
        #expect(ConnectionState.connecting.displayName == "Conectando")
        #expect(ConnectionState.connected.displayName == "Conectado")
        #expect(ConnectionState.disconnected.displayName == "Desconectado")
        #expect(TaskStatus.allCases.map(\.id) == ["wip", "review", "ready", "backlog", "merged"])
    }

    private func playMessages(_ model: SessionModel) -> [ChatMessage] {
        model.tasks.first { $0.id == "jaum-42" }?
            .sessions.first { $0.id == "jaum-42-play" }?
            .messages ?? []
    }
}
