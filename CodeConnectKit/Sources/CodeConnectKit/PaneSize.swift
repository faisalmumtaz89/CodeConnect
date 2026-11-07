import Foundation

/// A pane's size in cells, as the daemon states it.
public struct PaneSize: Equatable {
    public let cols: Int
    public let rows: Int

    public init(cols: Int, rows: Int) {
        self.cols = cols
        self.rows = rows
    }
}
