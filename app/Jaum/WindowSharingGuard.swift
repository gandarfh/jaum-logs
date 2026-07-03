import AppKit
import SwiftUI

/// Applies the validated invisibility setup to the hosting window:
/// `sharingType = .none` plus the window level and collection behavior that
/// keep it out of legacy screen capture (Meet shared from Chrome).
/// Known limit: native ScreenCaptureKit capture (macOS 15+) ignores this.
struct WindowSharingGuard: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = ProtectedView()
        view.applyProtection()
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        (nsView as? ProtectedView)?.applyProtection()
    }

    private final class ProtectedView: NSView {
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            applyProtection()
        }

        func applyProtection() {
            guard let window else { return }
            window.sharingType = .none
            window.level = NSWindow.Level(
                rawValue: Int(CGWindowLevelForKey(.assistiveTechHighWindow)))
            window.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle]
        }
    }
}
