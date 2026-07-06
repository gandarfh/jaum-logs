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
    /// True while the session data comes from the scripted preview backend,
    /// so the UI labels it instead of passing fabricated data off as real.
    var showsPreviewBadge = false
    @State private var mode: AppMode = .tasks
    @State private var permissionPrompt: PermissionRequest?
    @State private var permissionDecided = false

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
                Picker("Mode", selection: $mode) {
                    ForEach(AppMode.allCases) { mode in
                        Label(mode.title, systemImage: mode.systemImage)
                            .tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .labelStyle(.titleAndIcon)
            }
            if showsPreviewBadge {
                ToolbarItem(placement: .status) {
                    Chip(text: "demo data", systemImage: "sparkles")
                        .help(
                            "Tasks, chat and permissions are scripted; the daemon domain protocol does not exist yet"
                        )
                }
            }
            ToolbarItem(placement: .primaryAction) {
                Button {
                    reconnectIfNeeded()
                } label: {
                    // Latency stays hidden until the daemon protocol carries
                    // real Ping/Pong telemetry; the scripted samples must not
                    // masquerade as response time of the live socket.
                    ConnectionPill(
                        state: terminal.connection,
                        averageLatency: nil,
                        samples: []
                    )
                }
                .buttonStyle(.plain)
                .help(
                    terminal.connection == .disconnected
                        ? "Click to connect to the daemon" : "Daemon connection"
                )
            }
        }
        .onChange(of: session.pendingPermission) { _, newValue in
            permissionPrompt = newValue
            if newValue != nil {
                permissionDecided = false
            }
        }
        .onAppear {
            permissionPrompt = session.pendingPermission
        }
        .alert(
            "Allow \(permissionPrompt?.toolName ?? "tool")?",
            isPresented: permissionPresented,
            presenting: permissionPrompt
        ) { _ in
            Button("Approve") {
                decidePermission(approved: true)
            }
            Button("Deny", role: .cancel) {
                decidePermission(approved: false)
            }
        } message: { request in
            Text(request.request)
        }
        .sheet(item: $terminal.editorRequest) { request in
            EditorSheet(terminal: terminal, request: request)
        }
    }

    func reconnectIfNeeded() {
        guard terminal.connection == .disconnected else { return }
        let terminal = self.terminal
        Task {
            await terminal.attach()
        }
    }

    /// Exactly one decision per prompt: the buttons carry it, and Esc lands
    /// on the cancel button (deny).
    func decidePermission(approved: Bool) {
        permissionDecided = true
        if approved {
            session.approvePendingPermission()
        } else {
            session.denyPendingPermission()
        }
    }

    /// Presentation-only binding: dismissal never dispatches a decision.
    /// External resolution closes the alert via onChange. A dismissal that
    /// carried no decision (scene teardown) re-presents shortly after, so a
    /// pending request can never be orphaned behind a closed alert.
    ///
    /// The delay is not load-bearing for correctness, only for ordering
    /// robustness: SwiftUI gives no guarantee whether the button action or
    /// this setter runs first, so the re-present check waits long enough for
    /// `permissionDecided` to be set by a button tap. Re-presenting is
    /// idempotent, and `onAppear` restores the prompt anyway if the scene
    /// was rebuilt in the meantime.
    var permissionPresented: Binding<Bool> {
        Binding(
            get: { permissionPrompt != nil },
            set: { presented in
                guard !presented else { return }
                permissionPrompt = nil
                Task { @MainActor in
                    try? await Task.sleep(for: .milliseconds(300))
                    if !permissionDecided, let pending = session.pendingPermission {
                        permissionPrompt = pending
                    }
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
                    "No task selected",
                    systemImage: "sidebar.right",
                    description: Text("Pick a task from the list to see its detail.")
                )
            }
        }
    }
}

struct SidebarView: View {
    @Bindable var session: SessionModel

    var body: some View {
        List(selection: $session.selectedProjectID) {
            Section("Projects") {
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
                Section("Documents") {
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
                ContentUnavailableView("No documents", systemImage: "doc.text")
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
