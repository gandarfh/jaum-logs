import Foundation

/// Presentation of a task's review state on the board, kept out of the views
/// so the glyph, label and time formatting are unit-testable.
public struct ReviewIndicator: Hashable, Sendable {
    public enum Glyph: Hashable, Sendable {
        case running
        case rereviewPending
        case rereviewFailed
        case reviewed(hasFindings: Bool)
    }

    public var glyph: Glyph
    public var label: String

    public init(glyph: Glyph, label: String) {
        self.glyph = glyph
        self.label = label
    }

    /// Builds the indicator for a state, or nil when there is nothing to show
    /// (idle). `now` is injected so tests are deterministic.
    public static func make(for state: ReviewState, now: Date) -> ReviewIndicator? {
        switch state {
        case .idle:
            return nil
        case .running:
            return ReviewIndicator(glyph: .running, label: "Reviewing")
        case .rereviewPending:
            return ReviewIndicator(glyph: .rereviewPending, label: "Re-review pending")
        case .rereviewFailed:
            return ReviewIndicator(glyph: .rereviewFailed, label: "CI failing")
        case .reviewed(let verdict):
            let findings =
                verdict.findings == 0
                ? "no findings"
                : "\(verdict.findings) finding\(verdict.findings == 1 ? "" : "s")"
            let when = relativeTime(from: verdict.reviewedAt, to: now)
            let label = "\(findings) \u{00B7} \(verdict.reviewedSHA) \u{00B7} \(when)"
            return ReviewIndicator(glyph: .reviewed(hasFindings: verdict.findings > 0), label: label)
        }
    }

    /// Compact relative time ("just now", "5min ago", "3h ago", "2d ago");
    /// past a week it falls back to an absolute local date. Mirrors the Rust
    /// board formatting.
    public static func relativeTime(from date: Date, to now: Date) -> String {
        let seconds = now.timeIntervalSince(date)
        if seconds < 0 { return "just now" }
        let minutes = Int(seconds / 60)
        if minutes < 1 { return "just now" }
        if minutes < 60 { return "\(minutes)min ago" }
        let hours = minutes / 60
        if hours < 24 { return "\(hours)h ago" }
        let days = hours / 24
        if days <= 7 { return "\(days)d ago" }
        let formatter = DateFormatter()
        formatter.dateStyle = .medium
        formatter.timeStyle = .none
        return formatter.string(from: date)
    }
}
