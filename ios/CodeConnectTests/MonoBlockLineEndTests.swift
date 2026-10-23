import SwiftUI
import XCTest

@testable import CodeConnect

/// A block of output draws one line per line, whatever ends them.
///
/// `CCMonoBlock` drew a run per `\n`, and a Windows-style line end left its
/// `\r` on the run before it — which `Text` draws as a second, empty line
/// (`"a\r"` in mono: 37pt high, against 17pt for `"a"`). Output written with
/// `\r\n` — 499 Bash results in the owner's transcripts — drew a blank line
/// under every line.
@MainActor
final class MonoBlockLineEndTests: XCTestCase {

    /// The block's height, laid out in a window at an iPhone 17 Pro's width,
    /// so it has measured its own column before it is read.
    private func height(_ text: String) -> CGFloat {
        let controller = UIHostingController(rootView: CCMonoBlock(text).padding(16))
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = controller
        window.isHidden = false
        controller.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))
        defer { window.isHidden = true }
        return controller.sizeThatFits(in: CGSize(width: 402, height: 10_000)).height
    }

    /// Output ending its lines `\r\n`, or in a lone `\r`, is as tall as its
    /// twin ending them `\n`.
    func testCRLFOutputDrawsAsManyLinesAsItsLFTwin() {
        let lines = ["total 24", "drwxr-xr-x  5 dev  staff  160 Oct 23 .", "-rw-r--r--  1 dev  staff   42 README.md"]
        let lf = height(lines.joined(separator: "\n"))
        for end in ["\r\n", "\r"] {
            XCTAssertEqual(
                height(lines.joined(separator: end)), lf,
                end.unicodeScalars.map { $0.escaped(asASCII: true) }.joined())
        }
    }

    /// A vertical tab is not a line break on screen — `Text` draws through
    /// it — so it starts no line of its own in a block either.
    func testAVerticalTabStartsNoLine() {
        XCTAssertEqual(height("printf 'a\u{0B}b'\necho done"), height("printf 'ab'\necho done"))
    }
}
