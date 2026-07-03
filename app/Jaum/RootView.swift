import JaumKit
import SwiftUI

enum AppMode: String, CaseIterable, Identifiable {
    case tasks
    case docs

    var id: String { rawValue }

    var title: String {
        switch self {
        case .tasks: "Tasks"
        case .docs: "Docs"
        }
    }

    var systemImage: String {
        switch self {
        case .tasks: "list.bullet"
        case .docs: "doc.text"
        }
    }
}

struct RootView: View {
    @Bindable var session: SessionModel
    @Bindable var terminal: TerminalModel
    @State private var mode: AppMode = .tasks

    var body: some View {
        Group {
            switch mode {
            case .tasks:
                TasksSplitView(session: session)
            case .docs:
                DocsSplitView(session: session)
            }
        }
        .toolbar {
            ToolbarItem(placement: .navigation) {
                Picker("Modo", selection: $mode) {
                    ForEach(AppMode.allCases) { mode in
                        Label(mode.title, systemImage: mode.systemImage)
                            .tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .labelStyle(.titleAndIcon)
            }
            ToolbarItem(placement: .primaryAction) {
                ConnectionPill(
                    state: session.connection,
                    averageLatency: session.averageLatency,
                    samples: session.latencySamples
                )
            }
        }
        .alert(item: permissionBinding) { request in
            Alert(
                title: Text("Permitir \(request.toolName)?"),
                message: Text(request.request),
                primaryButton: .default(Text("Aprovar")) {
                    session.approvePendingPermission()
                },
                secondaryButton: .cancel(Text("Negar")) {
                    session.denyPendingPermission()
                }
            )
        }
        .sheet(item: $terminal.editorRequest) { request in
            EditorSheet(terminal: terminal, request: request)
        }
    }

    /// The alert dismisses itself by writing nil; the decision buttons answer
    /// the daemon, so plain dismissal (Esc) counts as denial.
    private var permissionBinding: Binding<PermissionRequest?> {
        Binding(
            get: { session.pendingPermission },
            set: { newValue in
                if newValue == nil, session.pendingPermission != nil {
                    session.denyPendingPermission()
                }
            }
        )
    }
}

/// Approved task layout: sidebar (projects and status filters), task list
/// grouped by status, detail pane with the task's session tabs.
struct TasksSplitView: View {
    @Bindable var session: SessionModel

    var body: some View {
        NavigationSplitView {
            SidebarView(session: session)
                .navigationSplitViewColumnWidth(min: 170, ideal: 210)
        } content: {
            TaskListView(session: session)
                .navigationSplitViewColumnWidth(min: 250, ideal: 296)
        } detail: {
            if let task = session.selectedTask {
                TaskDetailView(session: session, task: task)
            } else {
                ContentUnavailableView(
                    "Nenhuma task selecionada",
                    systemImage: "sidebar.right",
                    description: Text("Escolha uma task na lista para ver o detalhe.")
                )
            }
        }
    }
}

struct SidebarView: View {
    @Bindable var session: SessionModel

    var body: some View {
        List(selection: $session.selectedProjectID) {
            Section("Projetos") {
                ForEach(session.projects) { project in
                    HStack {
                        Label(project.name, systemImage: "folder")
                        Spacer()
                        Text("\(project.taskCount)")
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                    .tag(project.id)
                }
            }
            Section("Status") {
                ForEach(TaskStatus.allCases) { status in
                    Button {
                        session.statusFilter = session.statusFilter == status ? nil : status
                    } label: {
                        HStack(spacing: 9) {
                            StatusDot(status: status)
                            Text(status.displayName)
                            Spacer()
                            Text("\(session.taskCount(for: status))")
                                .foregroundStyle(.secondary)
                                .monospacedDigit()
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .listRowBackground(
                        session.statusFilter == status
                            ? Color.primary.opacity(0.08) : Color.clear
                    )
                }
            }
        }
        .listStyle(.sidebar)
    }
}

/// Docs screen: document list in the sidebar, rendered markdown on the right.
struct DocsSplitView: View {
    @Bindable var session: SessionModel

    var body: some View {
        NavigationSplitView {
            List(selection: $session.selectedDocID) {
                Section("Documentos") {
                    ForEach(session.docs) { doc in
                        Label(doc.name, systemImage: "doc.text")
                            .tag(doc.id)
                    }
                }
            }
            .listStyle(.sidebar)
            .navigationSplitViewColumnWidth(min: 170, ideal: 210)
        } detail: {
            if let doc = session.selectedDoc {
                DocView(doc: doc)
            } else {
                ContentUnavailableView("Sem documentos", systemImage: "doc.text")
            }
        }
    }
}

struct DocView: View {
    let doc: DocItem

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                Text(doc.name)
                    .font(.title3.weight(.bold))
                if !doc.subtitle.isEmpty {
                    Text(doc.subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Divider()
                    .padding(.vertical, 6)
                ForEach(Array(doc.content.split(separator: "\n\n").enumerated()), id: \.offset) {
                    _, paragraph in
                    let block = String(paragraph)
                    if block.hasPrefix("## ") {
                        Text(block.dropFirst(3))
                            .font(.headline)
                            .padding(.top, 8)
                    } else {
                        MarkdownText(block)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(22)
        }
        .navigationTitle(doc.name)
    }
}
