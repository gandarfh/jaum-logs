import Foundation

/// Client-side mirror of the daemon's screen buffer. The daemon owns all
/// state and streams cell deltas; this grid just applies them in order.
public struct FrameGrid: Hashable, Sendable {
    public private(set) var cols: UInt16 = 0
    public private(set) var rows: UInt16 = 0
    private var cells: [WireCell?] = []

    public init() {}

    public var isEmpty: Bool { cols == 0 || rows == 0 }

    public mutating func apply(_ message: ServerMessage) {
        switch message {
        case .frameFull(let cols, let rows, let newCells):
            self.cols = cols
            self.rows = rows
            cells = Array(repeating: nil, count: Int(cols) * Int(rows))
            merge(newCells)
        case .frameDiff(let changed):
            merge(changed)
        case .detach, .runEditor:
            break
        }
    }

    public func cell(x: UInt16, y: UInt16) -> WireCell? {
        guard x < cols, y < rows else { return nil }
        return cells[Int(y) * Int(cols) + Int(x)]
    }

    /// Plain-text rows, blank-filled where no cell was received. Useful for
    /// tests and accessibility descriptions.
    public func textRows() -> [String] {
        (0..<rows).map { y in
            var row = ""
            for x in 0..<cols {
                row += cell(x: x, y: y)?.sym ?? " "
            }
            return row
        }
    }

    private mutating func merge(_ changed: [WireCell]) {
        for cell in changed where cell.x < cols && cell.y < rows {
            cells[Int(cell.y) * Int(cols) + Int(cell.x)] = cell
        }
    }
}
