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

    @Test func reviewIndicatorRendersEveryGlyph() {
        let verdict = ReviewVerdict(
            reviewedSHA: "9534bae", findings: 2,
            reviewedAt: Date(timeIntervalSince1970: 0))
        let states: [ReviewState] = [
            .running, .rereviewPending, .rereviewFailed, .reviewed(verdict),
            .reviewed(ReviewVerdict(reviewedSHA: "abc1234", findings: 0, reviewedAt: Date())),
        ]
        for state in states {
            if let indicator = ReviewIndicator.make(for: state, now: Date()) {
                renderInWindow(ReviewIndicatorView(indicator: indicator))
            }
        }
    }

    @Test func detailRendersEveryTab() async {
        let session = await startedSession()
        session.selectedTaskID = "jaum-42"
        let task = sampleTask(session, "jaum-42")
        renderInWindow(TaskDetailView(session: session, task: task))
        session.selectedTab = .session("jaum-42-play")
        renderInWindow(TaskDetailView(session: session, task: task))
        session.selectedTab = .session("unknown")
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
        renderInWindow(DetailTabView(task: TaskItem(id: "empty", title: "Nothing here")))
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
            SessionTabButton(title: "Detail", isLive: false, isSelected: false) {})
    }

    @Test func sessionTabButtonActionSwitchesTheTab() async {
        let session = await startedSession()
        session.selectedTaskID = "jaum-42"
        #expect(session.selectedTab == .detail)
        let button = SessionTabButton(title: "Play", isLive: false, isSelected: false) {
            session.selectedTab = .session("jaum-42-play")
        }
        button.action()
        #expect(session.selectedTab == .session("jaum-42-play"))
    }
}
