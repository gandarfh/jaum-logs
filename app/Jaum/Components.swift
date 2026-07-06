import JaumKit
import SwiftUI

/// Status told by shape and weight, never by hue (approved visual language):
/// WIP filled, Review dimmed, Pronto outlined, Backlog faint, Merged thin
/// outline.
struct StatusDot: View {
    let status: TaskStatus
    var size: CGFloat = 9

    var body: some View {
        Group {
            switch status {
            case .wip:
                Circle().fill(Color.primary)
            case .review:
                Circle().fill(Color.primary.opacity(0.55))
            case .ready:
                Circle().strokeBorder(Color.secondary, lineWidth: 1.6)
            case .backlog:
                Circle().fill(Color.secondary.opacity(0.35))
            case .merged:
                Circle().strokeBorder(Color.secondary.opacity(0.5), lineWidth: 1.2)
            }
        }
        .frame(width: size, height: size)
        .accessibilityLabel(status.displayName)
    }
}

/// Global connection element: state plus average response time with a mini
/// sparkline, present at the top of every screen.
struct ConnectionPill: View {
    let state: ConnectionState
    let averageLatency: Double?
    let samples: [Double]

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(state == .connected ? Color.primary : Color.secondary.opacity(0.4))
                .frame(width: 7, height: 7)
            Text(state.displayName)
                .font(.caption.weight(.semibold))
            if let averageLatency {
                Divider().frame(height: 12)
                Image(systemName: "bolt")
                    .font(.caption2)
                Text("\(Int(averageLatency.rounded()))ms")
                    .font(.caption)
                    .monospacedDigit()
                Sparkline(samples: samples)
                    .frame(width: 46, height: 14)
                    .opacity(0.7)
            }
        }
        .foregroundStyle(state == .connected ? .primary : .secondary)
        .padding(.horizontal, 11)
        .padding(.vertical, 4)
        .background(Capsule().strokeBorder(.separator))
    }
}

struct Sparkline: View {
    let samples: [Double]

    var body: some View {
        Canvas { context, size in
            guard samples.count > 1 else { return }
            let maxValue = max(samples.max() ?? 1, 1)
            let minValue = samples.min() ?? 0
            let range = max(maxValue - minValue, 1)
            let stepX = size.width / CGFloat(samples.count - 1)
            var path = Path()
            for (index, sample) in samples.enumerated() {
                let x = CGFloat(index) * stepX
                let normalized = (sample - minValue) / range
                let y = size.height - CGFloat(normalized) * (size.height - 3) - 1.5
                if index == 0 {
                    path.move(to: CGPoint(x: x, y: y))
                } else {
                    path.addLine(to: CGPoint(x: x, y: y))
                }
            }
            context.stroke(path, with: .color(.primary), lineWidth: 1.3)
        }
    }
}

/// Pill-shaped metadata chip (worktree, findings, PRs, constraints).
struct Chip: View {
    let text: String
    var systemImage: String?

    var body: some View {
        HStack(spacing: 4) {
            if let systemImage {
                Image(systemName: systemImage)
                    .font(.system(size: 9))
            }
            Text(text)
                .font(.system(size: 10.5, weight: .semibold))
        }
        .foregroundStyle(.secondary)
        .padding(.horizontal, 7)
        .padding(.vertical, 2)
        .background(Capsule().strokeBorder(.separator))
    }
}

/// Primary action: filled with the foreground color, monochrome hierarchy
/// without hue (mock's white Play button).
struct PrimaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.caption.weight(.semibold))
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .foregroundStyle(Color(nsColor: .windowBackgroundColor))
            .background(Color.primary, in: RoundedRectangle(cornerRadius: 8))
            .opacity(configuration.isPressed ? 0.75 : 1)
    }
}

/// Ghost action: text plus hairline border.
struct GhostButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.caption.weight(.semibold))
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .foregroundStyle(.secondary)
            .background(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
            .opacity(configuration.isPressed ? 0.75 : 1)
    }
}

/// Inline markdown (bold, code, links) with whitespace preserved; falls back
/// to plain text when parsing fails.
struct MarkdownText: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        if let attributed = try? AttributedString(
            markdown: text,
            options: AttributedString.MarkdownParsingOptions(
                interpretedSyntax: .inlineOnlyPreservingWhitespace
            )
        ) {
            Text(attributed)
        } else {
            Text(text)
        }
    }
}
