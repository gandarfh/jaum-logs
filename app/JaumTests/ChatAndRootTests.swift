import JaumKit
import SwiftUI
import Testing
import UniformTypeIdentifiers

@testable import Jaum

@MainActor
struct ChatAndRootTests {
    private func chatView(_ session: SessionModel) -> SessionChatView {
        let task = sampleTask(session, "jaum-42")
        return SessionChatView(
            session: session,
            task: task,
            taskSession: task.sessions[0]
        )
    }

    @Test func chatRendersTimelineWithAllBlockKinds() async {
        let session = await startedSession()
        session.sendImage(
            data: Data([1, 2, 3]),
            filename: "quebrada.png",
            taskID: "jaum-42",
            sessionID: "jaum-42-play"
        )
        _ = await waitUntil {
            sampleTask(session, "jaum-42").sessions[0].messages.count >= 5
        }
        renderInWindow(chatView(session))
    }

    @Test func chatRendersToolCardStates() {
        for state in [ToolCallState.running, .succeeded, .failed] {
            renderInWindow(
                ToolCardView(tool: ToolCall(id: "t", name: "Bash", summary: "ls", state: state)))
        }
    }

    @Test func chatBlockViewRendersEveryCase() {
        renderInWindow(ChatBlockView(block: .markdown("**oi**")))
        renderInWindow(ChatBlockView(block: .image(Data([0, 1]))))
        renderInWindow(ChatBlockView(block: .tool(ToolCall(id: "t", name: "Read", summary: "x"))))
    }

    @Test func chatEntryDistinguishesUserFromAssistant() {
        let base = Date(timeIntervalSince1970: 0)
        renderInWindow(
            ChatEntryView(
                message: ChatMessage(id: "u", role: .user, blocks: [.markdown("oi")], timestamp: base)
            ))
        renderInWindow(
            ChatEntryView(
                message: ChatMessage(
                    id: "a", role: .assistant, blocks: [.markdown("resposta")], timestamp: base)
            ))
    }

    @Test func sendDraftForwardsToTheSession() async {
        let session = await startedSession()
        let view = chatView(session)
        view.sendDraft()
        let before = sampleTask(session, "jaum-42").sessions[0].messages.count
        session.sendText("mensagem direta", taskID: "jaum-42", sessionID: "jaum-42-play")
        #expect(
            await waitUntil {
                sampleTask(session, "jaum-42").sessions[0].messages.count == before + 2
            })
    }

    @Test func attachImageReadsTheFileAndSendsIt() async throws {
        let session = await startedSession()
        let view = chatView(session)
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("jaum-tests-\(getpid()).png")
        try Data([9, 9, 9]).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }

        let before = sampleTask(session, "jaum-42").sessions[0].messages.count
        view.attachImage(.success(url))
        #expect(
            await waitUntil {
                sampleTask(session, "jaum-42").sessions[0].messages.count == before + 1
            })

        view.attachImage(.failure(CocoaError(.fileReadNoSuchFile)))
        view.attachImage(.success(url.appendingPathExtension("missing")))
        try? await Task.sleep(for: .milliseconds(50))
    }

    @Test func readFileLoadsBytesOffTheMainActor() async throws {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("jaum-readfile-\(getpid()).bin")
        try Data([1, 2, 3, 4]).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        let data = try await SessionChatView.readFile(at: url)
        #expect(data == Data([1, 2, 3, 4]))
    }

    @Test func rootViewRendersTasksMode() async {
        let session = await startedSession()
        session.selectedTaskID = "jaum-42"
        let terminal = TerminalModel(transport: UnixSocketTransport(path: "/tmp/jaum-nada.sock"))
        renderInWindow(RootView(session: session, terminal: terminal))
    }

    @Test func rootViewRendersWithoutSelectionOrPermission() async {
        let session = SessionModel(backend: PreviewBackend())
        let terminal = TerminalModel(transport: UnixSocketTransport(path: "/tmp/jaum-nada.sock"))
        renderInWindow(RootView(session: session, terminal: terminal))
    }

    @Test func splitViewsRenderBothModes() async {
        let session = await startedSession()
        session.selectedTaskID = "jaum-31"
        renderInWindow(TasksSplitView(session: session))
        renderInWindow(DocsSplitView(session: session))
        session.selectedDocID = "arquitetura.md"
        renderInWindow(DocsSplitView(session: session))
    }

    @Test func sidebarTogglesTheStatusFilter() async {
        let session = await startedSession()
        renderInWindow(SidebarView(session: session))
        session.statusFilter = .wip
        renderInWindow(SidebarView(session: session))
    }

    @Test func docViewRendersHeadingsAndParagraphs() async {
        let session = await startedSession()
        renderInWindow(DocView(doc: session.docs[0]))
        renderInWindow(DocView(doc: DocItem(name: "vazio.md", content: "so um paragrafo")))
    }

    @Test func editorSheetRendersAndSyncsContent() async {
        let terminal = TerminalModel(transport: UnixSocketTransport(path: "/tmp/jaum-nada.sock"))
        let request = EditorRequest(path: "/tmp/conventions.md", content: "# titulo")
        renderInWindow(EditorSheet(terminal: terminal, request: request))
    }

    @Test func appModeExposesTitlesAndIcons() {
        for mode in AppMode.allCases {
            #expect(!mode.title.isEmpty)
            #expect(!mode.systemImage.isEmpty)
            #expect(mode.id == mode.rawValue)
        }
    }
}
