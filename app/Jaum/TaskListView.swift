import JaumKit
import SwiftUI

/// Middle column: tasks grouped by status with pills and indicators.
struct TaskListView: View {
    @Bindable var session: SessionModel

    var body: some View {
        List(selection: $session.selectedTaskID) {
            ForEach(session.sections) { section in
                Section {
                    ForEach(section.tasks) { task in
                        TaskRowView(task: task)
                            .tag(task.id)
                    }
                } header: {
                    HStack {
                        Text(section.status.displayName)
                            .font(.subheadline.weight(.bold))
                            .foregroundStyle(.primary)
                        Spacer()
                        Text("\(section.tasks.count)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                }
            }
        }
        .listStyle(.inset)
        .overlay {
            if session.sections.isEmpty {
                ContentUnavailableView(
                    "No tasks",
                    systemImage: "tray",
                    description: Text("No tasks under this filter.")
                )
            }
        }
        .navigationTitle("Tasks")
    }
}

struct TaskRowView: View {
    let task: TaskItem

    var body: some View {
        HStack(alignment: .top, spacing: 11) {
            StatusDot(status: task.status)
                .padding(.top, 4)
            VStack(alignment: .leading, spacing: 2) {
                Text(task.id)
                    .font(.callout.weight(.semibold))
                Text(task.title)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                if hasChips {
                    HStack(spacing: 7) {
                        if let worktree = task.worktree {
                            Chip(text: worktree, systemImage: "arrow.triangle.branch")
                        }
                        if task.isParallel {
                            Chip(text: "parallel", systemImage: "equal")
                        }
                        if task.findingsCount > 0 {
                            Chip(text: "\(task.findingsCount) findings", systemImage: "flag")
                        }
                    }
                    .padding(.top, 5)
                }
                if let indicator = ReviewIndicator.make(for: task.reviewState, now: Date()) {
                    ReviewIndicatorView(indicator: indicator)
                        .padding(.top, 5)
                }
            }
            Spacer(minLength: 0)
            VStack(alignment: .trailing, spacing: 6) {
                if let activity = task.lastActivity {
                    Text(activity)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                if task.hasLiveSession {
                    Image(systemName: "record.circle")
                        .font(.caption)
                        .foregroundStyle(.primary)
                        .accessibilityLabel("Live session")
                }
            }
        }
        .padding(.vertical, 4)
    }

    private var hasChips: Bool {
        task.worktree != nil || task.isParallel || task.findingsCount > 0
    }
}

/// Board glyph plus label for a task's automatic review state.
struct ReviewIndicatorView: View {
    let indicator: ReviewIndicator

    var body: some View {
        HStack(spacing: 5) {
            glyph
                .font(.caption2)
                .frame(width: 12)
            Text(indicator.label)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder
    private var glyph: some View {
        switch indicator.glyph {
        case .running:
            ProgressView()
                .controlSize(.mini)
        case .rereviewPending:
            Image(systemName: "hourglass")
                .foregroundStyle(.secondary)
        case .rereviewFailed:
            Image(systemName: "xmark.circle")
                .foregroundStyle(.primary)
        case .reviewed(let hasFindings):
            Image(systemName: hasFindings ? "flag.checkered" : "checkmark.seal")
                .foregroundStyle(.primary)
        }
    }
}
