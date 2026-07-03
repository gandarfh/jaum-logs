import JaumKit
import SwiftUI

@main
struct JaumApp: App {
    @State private var session = SessionModel(backend: PreviewBackend())
    @State private var terminal = TerminalModel(transport: UnixSocketTransport())

    var body: some Scene {
        WindowGroup {
            RootView(session: session, terminal: terminal)
                .background(WindowSharingGuard())
                .task {
                    session.start()
                }
        }
    }
}
