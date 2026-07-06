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
/// The default is generous because CI runners take seconds to launch the
/// host app and evaluate the first render.
func waitUntil(
    timeout: TimeInterval = 15,
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
    let ready = await waitUntil { !model.tasks.isEmpty && model.pendingPermission != nil }
    precondition(ready, "preview backend did not deliver the scripted session in time")
    return model
}

@MainActor
func sampleTask(_ session: SessionModel, _ id: String) -> TaskItem {
    guard let task = session.tasks.first(where: { $0.id == id }) else {
        preconditionFailure("scripted task \(id) missing from the preview session")
    }
    return task
}

/// Transport that connects to nowhere and accepts every send, for exercising
/// flows (editor round-trip) that need a working connection but no daemon.
final class NullTransport: WireTransport, Sendable {
    func connect() async throws -> AsyncThrowingStream<Data, any Error> {
        AsyncThrowingStream { _ in }
    }

    func send(_ data: Data) async throws {}

    func close() async {}
}
