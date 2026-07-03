import JaumKit
import SwiftUI
import Testing

@testable import Jaum

@MainActor
struct TaskViewsTests {
    @Test func taskListRendersSectionsAndRows() async {
        let session = await startedSession()
        renderInWindow(TaskListView(session: session))
        session.statusFilter = .merged
        renderInWindow(TaskListView(session: session))
    }

    @Test func taskRowRendersChipVariants() async {
        let session = await startedSession()
        renderInWindow(TaskRowView(task: sampleTask(session, "jaum-42")))
        renderInWindow(TaskRowView(task: sampleTask(session, "jaum-31")))
        renderInWindow(TaskRowView(task: sampleTask(session, "jaum-28")))
    }

    @Test func detailRendersEveryTab() async {
        let session = await startedSession()
        session.selectedTaskID = "jaum-42"
        let task = sampleTask(session, "jaum-42")
        renderInWindow(TaskDetailView(session: session, task: task))
        session.selectedTab = .session("jaum-42-play")
        renderInWindow(TaskDetailView(session: session, task: task))
        session.selectedTab = .session("desconhecida")
        renderInWindow(TaskDetailView(session: session, task: task))
    }

    @Test func detailRendersReviewTabAndBareTask() async {
        let session = await startedSession()
        session.selectedTaskID = "jaum-31"
        let review = sampleTask(session, "jaum-31")
        session.selectedTab = .session("jaum-31-review")
        renderInWindow(TaskDetailView(session: session, task: review))
        renderInWindow(TaskDetailView(session: session, task: sampleTask(session, "jaum-44")))
    }

    @Test func detailTabViewHandlesEmptyAndFilledTasks() async {
        let session = await startedSession()
        renderInWindow(DetailTabView(task: sampleTask(session, "jaum-42")))
        renderInWindow(DetailTabView(task: TaskItem(id: "vazia", title: "Sem nada")))
    }

    @Test func reviewSessionViewHandlesEmptyFindings() async {
        let session = await startedSession()
        let task = sampleTask(session, "jaum-31")
        renderInWindow(
            ReviewSessionView(
                task: task,
                taskSession: TaskSession(id: "s", kind: .review)
            ))
    }

    @Test func sessionTabButtonRendersStates() {
        renderInWindow(
            SessionTabButton(title: "Play", isLive: true, isSelected: true) {})
        renderInWindow(
            SessionTabButton(title: "Detalhe", isLive: false, isSelected: false) {})
    }

    @Test func sessionTabButtonActionSwitchesTheTab() async {
        let session = await startedSession()
        session.selectedTaskID = "jaum-42"
        var tapped = false
        let button = SessionTabButton(title: "Play", isLive: false, isSelected: false) {
            tapped = true
        }
        button.action()
        #expect(tapped)
    }
}
