import Foundation
import Testing

@testable import JaumKit

struct FrameGridTests {
    private func cell(_ x: UInt16, _ y: UInt16, _ sym: String) -> WireCell {
        WireCell(x: x, y: y, sym: sym)
    }

    @Test func startsEmpty() {
        let grid = FrameGrid()
        #expect(grid.isEmpty)
        #expect(grid.cell(x: 0, y: 0) == nil)
        #expect(grid.textRows().isEmpty)
    }

    @Test func fullFrameSetsSizeAndCells() {
        var grid = FrameGrid()
        grid.apply(.frameFull(cols: 3, rows: 2, cells: [cell(0, 0, "a"), cell(2, 1, "b")]))
        #expect(!grid.isEmpty)
        #expect(grid.cols == 3)
        #expect(grid.rows == 2)
        #expect(grid.cell(x: 0, y: 0)?.sym == "a")
        #expect(grid.cell(x: 1, y: 0) == nil)
        #expect(grid.textRows() == ["a  ", "  b"])
    }

    @Test func diffMergesIntoExistingFrame() {
        var grid = FrameGrid()
        grid.apply(.frameFull(cols: 2, rows: 1, cells: [cell(0, 0, "a"), cell(1, 0, "b")]))
        grid.apply(.frameDiff([cell(1, 0, "c")]))
        #expect(grid.textRows() == ["ac"])
    }

    @Test func fullFrameResetsPreviousCells() {
        var grid = FrameGrid()
        grid.apply(.frameFull(cols: 2, rows: 1, cells: [cell(0, 0, "a"), cell(1, 0, "b")]))
        grid.apply(.frameFull(cols: 2, rows: 1, cells: [cell(0, 0, "x")]))
        #expect(grid.textRows() == ["x "])
    }

    @Test func outOfBoundsCellsAreIgnored() {
        var grid = FrameGrid()
        grid.apply(.frameFull(cols: 2, rows: 1, cells: [cell(5, 5, "z")]))
        grid.apply(.frameDiff([cell(2, 0, "z"), cell(0, 1, "z")]))
        #expect(grid.textRows() == ["  "])
    }

    @Test func lookupOutsideBoundsIsNil() {
        var grid = FrameGrid()
        grid.apply(.frameFull(cols: 2, rows: 1, cells: []))
        #expect(grid.cell(x: 2, y: 0) == nil)
        #expect(grid.cell(x: 0, y: 1) == nil)
    }

    @Test func controlMessagesDoNotTouchTheGrid() {
        var grid = FrameGrid()
        grid.apply(.frameFull(cols: 1, rows: 1, cells: [cell(0, 0, "a")]))
        grid.apply(.detach)
        grid.apply(.runEditor(path: "/tmp/x"))
        #expect(grid.textRows() == ["a"])
    }
}
