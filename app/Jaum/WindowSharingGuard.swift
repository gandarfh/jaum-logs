import AppKit
import SwiftUI

/// Marks the hosting window as non-shareable (`sharingType = .none`), so it
/// stays invisible to screen capture and screen sharing (Meet, Zoom, etc).
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
            window?.sharingType = .none
        }
    }
}
