import AppKit
import JaumKit
import SwiftUI
import Testing

@testable import Jaum

@MainActor
struct ComponentsTests {
    @Test func statusDotRendersEveryStatus() {
        for status in TaskStatus.allCases {
            renderInWindow(StatusDot(status: status), size: CGSize(width: 40, height: 40))
        }
    }

    @Test func connectionPillRendersAllStates() {
        renderInWindow(
            ConnectionPill(state: .connected, averageLatency: 24, samples: [24, 22, 31, 18]),
            size: CGSize(width: 260, height: 40)
        )
        renderInWindow(
            ConnectionPill(state: .connecting, averageLatency: nil, samples: []),
            size: CGSize(width: 260, height: 40)
        )
        renderInWindow(
            ConnectionPill(state: .disconnected, averageLatency: nil, samples: []),
            size: CGSize(width: 260, height: 40)
        )
    }

    @Test func sparklineHandlesEmptyAndSamples() {
        renderInWindow(Sparkline(samples: []), size: CGSize(width: 46, height: 14))
        renderInWindow(Sparkline(samples: [10]), size: CGSize(width: 46, height: 14))
        renderInWindow(
            Sparkline(samples: [24, 22, 31, 18, 26, 21]), size: CGSize(width: 46, height: 14))
        renderInWindow(Sparkline(samples: [7, 7, 7]), size: CGSize(width: 46, height: 14))
    }

    @Test func chipRendersWithAndWithoutIcon() {
        renderInWindow(Chip(text: "worktree", systemImage: "arrow.triangle.branch"))
        renderInWindow(Chip(text: "3 constraints"))
    }

    @Test func buttonStylesRender() {
        renderInWindow(
            HStack {
                Button("Play") {}.buttonStyle(PrimaryButtonStyle())
                Button("Review") {}.buttonStyle(GhostButtonStyle())
            }
        )
    }

    @Test func markdownTextRendersInlineMarkdown() {
        renderInWindow(MarkdownText("Texto com **negrito** e `codigo`"))
        renderInWindow(MarkdownText(""))
    }

    @Test func windowSharingGuardProtectsTheWindow() {
        let window = renderInWindow(Text("conteudo").background(WindowSharingGuard()))
        #expect(window.sharingType == .none)
        #expect(window.level == .normal)
    }
}
