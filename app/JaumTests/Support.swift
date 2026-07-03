import AppKit
import JaumKit
import SwiftUI
import Testing

@testable import Jaum

/// Hosts a SwiftUI view in an offscreen window and pumps the run loop once,
/// forcing every body in the tree to evaluate (what the coverage gate needs).
@MainActor
@discardableResult
func renderInWindow(_ view: some View, size: CGSize = CGSize(width: 900, height: 600)) -> NSWindow {
    let window = NSWindow(
        contentRect: NSRect(origin: .zero, size: size),
        styleMask: [.titled, .closable, .resizable],
        backing: .buffered,
        defer: false
    )
    window.contentViewController = NSHostingController(rootView: view)
    window.orderBack(nil)
    window.contentView?.layoutSubtreeIfNeeded()
    RunLoop.main.run(until: Date().addingTimeInterval(0.05))
    window.orderOut(nil)
    return window
}

/// Polls a condition on the main actor until it holds or the timeout hits.
func waitUntil(
    timeout: TimeInterval = 2,
    _ condition: @MainActor @escaping () -> Bool
) async -> Bool {
    let deadline = ContinuousClock.now.advanced(by: .seconds(timeout))
    while ContinuousClock.now < deadline {
        if await MainActor.run(body: condition) { return true }
        try? await Task.sleep(for: .milliseconds(10))
    }
    return await MainActor.run(body: condition)
}

/// A session model fed by the scripted preview backend, ready for rendering.
@MainActor
func startedSession() async -> SessionModel {
    let model = SessionModel(backend: PreviewBackend())
    model.start()
    _ = await waitUntil { !model.tasks.isEmpty && model.pendingPermission != nil }
    return model
}

@MainActor
func sampleTask(_ session: SessionModel, _ id: String) -> TaskItem {
    session.tasks.first { $0.id == id }!
}
