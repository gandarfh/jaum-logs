import Foundation
import Testing

@testable import JaumKit

struct ReviewIndicatorTests {
    private let now = Date(timeIntervalSince1970: 1_000_000)

    @Test func idleHasNoIndicator() {
        #expect(ReviewIndicator.make(for: .idle, now: now) == nil)
    }

    @Test func runningShowsTheSpinner() {
        let indicator = ReviewIndicator.make(for: .running, now: now)
        #expect(indicator?.glyph == .running)
        #expect(indicator?.label == "Reviewing")
    }

    @Test func rereviewPendingAndFailedGlyphs() {
        #expect(ReviewIndicator.make(for: .rereviewPending, now: now)?.glyph == .rereviewPending)
        #expect(ReviewIndicator.make(for: .rereviewFailed, now: now)?.glyph == .rereviewFailed)
    }

    @Test func reviewedWithFindingsCarriesShaAndCount() {
        let verdict = ReviewVerdict(
            reviewedSHA: "9534bae", findings: 2, reviewedAt: now.addingTimeInterval(-300))
        let indicator = ReviewIndicator.make(for: .reviewed(verdict), now: now)
        #expect(indicator?.glyph == .reviewed(hasFindings: true))
        #expect(indicator?.label.contains("2 findings") == true)
        #expect(indicator?.label.contains("9534bae") == true)
        #expect(indicator?.label.contains("5min ago") == true)
    }

    @Test func reviewedWithoutFindingsIsSingular() {
        let verdict = ReviewVerdict(
            reviewedSHA: "abc1234", findings: 0, reviewedAt: now)
        let indicator = ReviewIndicator.make(for: .reviewed(verdict), now: now)
        #expect(indicator?.glyph == .reviewed(hasFindings: false))
        #expect(indicator?.label.contains("no findings") == true)

        let single = ReviewVerdict(reviewedSHA: "abc1234", findings: 1, reviewedAt: now)
        #expect(
            ReviewIndicator.make(for: .reviewed(single), now: now)?.label.contains("1 finding")
                == true)
    }

    @Test func relativeTimeBuckets() {
        func label(_ secondsAgo: TimeInterval) -> String {
            ReviewIndicator.relativeTime(from: now.addingTimeInterval(-secondsAgo), to: now)
        }
        #expect(label(-10) == "just now")
        #expect(label(10) == "just now")
        #expect(label(30) == "just now")
        #expect(label(90) == "1min ago")
        #expect(label(60 * 59) == "59min ago")
        #expect(label(60 * 60) == "1h ago")
        #expect(label(60 * 60 * 23) == "23h ago")
        #expect(label(60 * 60 * 24) == "1d ago")
        #expect(label(60 * 60 * 24 * 7) == "7d ago")
    }

    @Test func relativeTimePastAWeekIsAbsolute() {
        let long = ReviewIndicator.relativeTime(
            from: now.addingTimeInterval(-60 * 60 * 24 * 40), to: now)
        #expect(!long.hasSuffix("ago"))
        #expect(!long.isEmpty)
    }
}
