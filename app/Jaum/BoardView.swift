import JaumKit
import SwiftUI

struct BoardView: View {
    @Bindable var session: SessionModel

    var body: some View {
        ScrollView(.horizontal) {
            HStack(alignment: .top, spacing: 12) {
                ForEach(session.columns) { column in
                    BoardColumnView(column: column, selection: $session.selectedTaskID)
                }
            }
            .padding()
        }
        .navigationTitle("Board")
        .navigationSubtitle("\(session.tasks.count) tasks")
    }
}

struct BoardColumnView: View {
    let column: BoardColumn
    @Binding var selection: TaskItem.ID?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(column.status.displayName)
                    .font(.headline)
                Spacer()
                Text("\(column.tasks.count)")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            .padding(.horizontal, 4)

            if column.tasks.isEmpty {
                Text("Sem tasks")
                    .font(.callout)
                    .foregroundStyle(.tertiary)
                    .frame(maxWidth: .infinity, minHeight: 60)
            } else {
                ForEach(column.tasks) { task in
                    TaskCardView(task: task, isSelected: selection == task.id)
                        .onTapGesture { selection = task.id }
                }
            }
            Spacer(minLength: 0)
        }
        .frame(width: 240)
    }
}

struct TaskCardView: View {
    let task: TaskItem
    let isSelected: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(task.title)
                .font(.body.weight(.medium))
                .lineLimit(3)
            HStack {
                Text(task.kind.displayName)
                    .font(.caption)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(.quaternary, in: Capsule())
                Spacer()
                Text(task.id)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(Color(nsColor: .controlBackgroundColor))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .strokeBorder(isSelected ? Color.accentColor : .clear, lineWidth: 2)
        )
    }
}

struct TaskDetailView: View {
    let task: TaskItem

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                VStack(alignment: .leading, spacing: 6) {
                    Text(task.title)
                        .font(.title2.weight(.semibold))
                    HStack(spacing: 8) {
                        Text(task.kind.displayName)
                        Text(task.status.displayName)
                            .foregroundStyle(.secondary)
                        Text(task.id)
                            .font(.callout.monospaced())
                            .foregroundStyle(.secondary)
                    }
                    .font(.callout)
                }

                if !task.objective.isEmpty {
                    GroupBox("Objetivo") {
                        MarkdownText(task.objective)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }

                if !task.acceptanceCriteria.isEmpty {
                    GroupBox("Critérios de aceite") {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(task.acceptanceCriteria, id: \.self) { criterion in
                                Label(criterion, systemImage: "checkmark.circle")
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }

                if !task.constraints.isEmpty {
                    GroupBox("Restrições") {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(task.constraints, id: \.self) { constraint in
                                Label(constraint, systemImage: "exclamationmark.triangle")
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                Spacer()
            }
            .padding()
        }
        .navigationTitle(task.title)
    }
}
