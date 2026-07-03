import JaumKit
import SwiftUI

enum SidebarItem: String, Hashable, CaseIterable, Identifiable {
    case board
    case chat
    case terminal

    var id: String { rawValue }

    var title: String {
        switch self {
        case .board: "Board"
        case .chat: "Chat da sessão"
        case .terminal: "Terminal"
        }
    }

    var systemImage: String {
        switch self {
        case .board: "square.grid.3x1.below.line.grid.1x2"
        case .chat: "bubble.left.and.bubble.right"
        case .terminal: "terminal"
        }
    }
}

struct RootView: View {
    @Bindable var session: SessionModel
    @Bindable var terminal: TerminalModel
    @State private var sidebarSelection: SidebarItem? = .board

    var body: some View {
        NavigationSplitView {
            SidebarView(session: session, selection: $sidebarSelection)
        } content: {
            switch sidebarSelection ?? .board {
            case .board:
                BoardView(session: session)
            case .chat:
                ChatView(session: session)
            case .terminal:
                TerminalMirrorView(terminal: terminal)
            }
        } detail: {
            if let task = session.selectedTask {
                TaskDetailView(task: task)
            } else {
                ContentUnavailableView(
                    "Nenhuma task selecionada",
                    systemImage: "sidebar.right",
                    description: Text("Escolha uma task no board para ver os detalhes.")
                )
            }
        }
        .navigationTitle("Jaum")
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

struct SidebarView: View {
    @Bindable var session: SessionModel
    @Binding var selection: SidebarItem?

    var body: some View {
        List(selection: $selection) {
            Section("Projeto") {
                ForEach(SidebarItem.allCases) { item in
                    Label(item.title, systemImage: item.systemImage)
                        .tag(item)
                }
            }
            Section("Andamento") {
                ForEach(session.columns) { column in
                    HStack {
                        Text(column.status.displayName)
                        Spacer()
                        Text("\(column.tasks.count)")
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                }
            }
        }
        .safeAreaInset(edge: .bottom) {
            ConnectionBadge(state: session.connection)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
        }
        .navigationSplitViewColumnWidth(min: 180, ideal: 220)
    }
}

struct ConnectionBadge: View {
    let state: ConnectionState

    var body: some View {
        Label(state.displayName, systemImage: "circle.fill")
            .font(.footnote)
            .foregroundStyle(color)
    }

    private var color: Color {
        switch state {
        case .connected: .green
        case .connecting: .orange
        case .disconnected: .gray
        }
    }
}
