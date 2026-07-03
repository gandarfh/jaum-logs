import JaumKit
import SwiftUI

/// Detail pane: header with actions, metadata chips and the task's session
/// tabs (Detalhe plus one tab per session, never fixed global tabs).
struct TaskDetailView: View {
    @Bindable var session: SessionModel
    let task: TaskItem

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
                .padding(.horizontal, 18)
                .padding(.top, 15)
                .padding(.bottom, 12)

            if !task.objective.isEmpty {
                Text(task.objective)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 18)
                    .padding(.bottom, 12)
            }

            metadataChips
                .padding(.horizontal, 18)
                .padding(.bottom, 14)

            tabPicker
                .padding(.horizontal, 18)

            tabContent
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
        .navigationTitle(task.id)
    }

    private var header: some View {
        HStack(spacing: 10) {
            Text(task.id)
                .font(.title3.weight(.bold))
            HStack(spacing: 6) {
                StatusDot(status: task.status)
                Text(task.status.displayName)
                    .font(.caption.weight(.bold))
            }
            .padding(.horizontal, 9)
            .padding(.vertical, 3)
            .background(Capsule().fill(Color.primary.opacity(0.08)))
            Spacer()
            HStack(spacing: 7) {
                Button("Review", systemImage: "flag") {}
                    .buttonStyle(GhostButtonStyle())
                    .disabled(true)
                Button("Finish", systemImage: "arrow.triangle.merge") {}
                    .buttonStyle(GhostButtonStyle())
                    .disabled(true)
                Button("Play", systemImage: "play.fill") {}
                    .buttonStyle(PrimaryButtonStyle())
                    .disabled(true)
            }
            .help("Ações chegam com o protocolo de domínio do daemon")
        }
    }

    private var metadataChips: some View {
        HStack(spacing: 7) {
            if let worktree = task.worktree {
                Chip(text: "worktree \(worktree)", systemImage: "arrow.triangle.branch")
            }
            if task.isEditing {
                Chip(text: "editando", systemImage: "pencil")
            }
            if task.prCount > 0 {
                Chip(text: "\(task.prCount) PRs", systemImage: "link")
            }
            if !task.constraints.isEmpty {
                Chip(text: "\(task.constraints.count) constraints")
            }
        }
    }

    private var tabPicker: some View {
        HStack(spacing: 2) {
            SessionTabButton(
                title: "Detalhe",
                isLive: false,
                isSelected: session.selectedTab == .detail
            ) {
                session.selectedTab = .detail
            }
            ForEach(task.sessions) { taskSession in
                SessionTabButton(
                    title: taskSession.kind.displayName,
                    isLive: taskSession.isLive,
                    isSelected: session.selectedTab == .session(taskSession.id)
                ) {
                    session.selectedTab = .session(taskSession.id)
                }
            }
        }
        .padding(3)
        .background(RoundedRectangle(cornerRadius: 9).strokeBorder(.separator))
    }

    @ViewBuilder
    private var tabContent: some View {
        switch session.selectedTab {
        case .detail:
            DetailTabView(task: task)
        case .session(let sessionID):
            if let taskSession = session.session(sessionID, of: task) {
                if taskSession.kind == .review {
                    ReviewSessionView(task: task, taskSession: taskSession)
                } else {
                    SessionChatView(session: session, task: task, taskSession: taskSession)
                }
            } else {
                DetailTabView(task: task)
            }
        }
    }
}

/// One tab of the session tab strip, styled like the mock's segmented strip
/// (the system segmented Picker cannot render the live dot inside a segment).
struct SessionTabButton: View {
    let title: String
    let isLive: Bool
    let isSelected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 5) {
                Text(title)
                    .font(.caption.weight(.semibold))
                if isLive {
                    StatusDot(status: .wip, size: 7)
                }
            }
            .padding(.horizontal, 13)
            .padding(.vertical, 5)
            .background(
                RoundedRectangle(cornerRadius: 6)
                    .fill(isSelected ? Color.primary.opacity(0.1) : Color.clear)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(isSelected ? .primary : .secondary)
    }
}

/// The Detalhe tab: acceptance criteria checklist and constraints.
struct DetailTabView: View {
    let task: TaskItem

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                if !task.criteria.isEmpty {
                    Text("Critérios de aceite")
                        .font(.callout.weight(.bold))
                        .padding(.bottom, 4)
                    ForEach(Array(task.criteria.enumerated()), id: \.offset) { _, criterion in
                        HStack(spacing: 9) {
                            Image(systemName: criterion.done ? "checkmark" : "clock")
                                .font(.caption)
                                .foregroundStyle(criterion.done ? .primary : .secondary)
                                .frame(width: 14)
                            Text(criterion.text)
                                .font(.callout)
                                .foregroundStyle(.secondary)
                            Spacer()
                        }
                        .padding(.vertical, 8)
                        .overlay(alignment: .bottom) {
                            Divider().opacity(0.5)
                        }
                    }
                }
                if !task.constraints.isEmpty {
                    Text("Constraints")
                        .font(.callout.weight(.bold))
                        .padding(.top, 16)
                        .padding(.bottom, 4)
                    ForEach(Array(task.constraints.enumerated()), id: \.offset) { _, constraint in
                        HStack(spacing: 9) {
                            Image(systemName: "exclamationmark.triangle")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .frame(width: 14)
                            Text(constraint)
                                .font(.callout)
                                .foregroundStyle(.secondary)
                            Spacer()
                        }
                        .padding(.vertical, 8)
                        .overlay(alignment: .bottom) {
                            Divider().opacity(0.5)
                        }
                    }
                }
                if task.criteria.isEmpty && task.constraints.isEmpty {
                    Text("Sem critérios registrados.")
                        .font(.callout)
                        .foregroundStyle(.tertiary)
                        .padding(.top, 8)
                }
            }
            .padding(18)
        }
    }
}

/// The Review tab: findings summary and list.
struct ReviewSessionView: View {
    let task: TaskItem
    let taskSession: TaskSession

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    Image(systemName: "flag")
                    Text("\(taskSession.findings.count) findings")
                        .font(.callout.weight(.bold))
                    Text("· critérios \(task.doneCriteriaCount)/\(task.criteria.count)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.bottom, 10)

                ForEach(Array(taskSession.findings.enumerated()), id: \.offset) { _, finding in
                    HStack(alignment: .top, spacing: 11) {
                        Image(systemName: "flag")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .padding(.top, 1)
                        VStack(alignment: .leading, spacing: 3) {
                            Text(finding.title)
                                .font(.callout.weight(.semibold))
                            Text(finding.detail)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Text(finding.location)
                                .font(.caption.monospaced())
                                .foregroundStyle(.tertiary)
                                .padding(.top, 2)
                        }
                        Spacer()
                    }
                    .padding(.vertical, 12)
                    .overlay(alignment: .bottom) {
                        Divider().opacity(0.5)
                    }
                }
                if taskSession.findings.isEmpty {
                    Text("Sem findings.")
                        .font(.callout)
                        .foregroundStyle(.tertiary)
                }
            }
            .padding(18)
        }
    }
}
