import XCTest

@testable import CodeConnectKit

/// A box that sends to an agent takes Return as every message box does: Return sends, ⇧Return and ⌥Return add a line.
final class ReturnAddsLineTests: XCTestCase {
    private func addsLine(shift: Bool = false, option: Bool = false, command: Bool = false, control: Bool = false)
        -> Bool
    {
        ComposeProse.returnAddsLine(shift: shift, option: option, command: command, control: control)
    }

    func testReturnAloneSends() {
        XCTAssertFalse(addsLine())
    }

    func testShiftReturnAddsALine() {
        XCTAssertTrue(addsLine(shift: true))
    }

    func testOptionReturnAddsALine() {
        XCTAssertTrue(addsLine(option: true))
        XCTAssertTrue(addsLine(shift: true, option: true))
    }

    func testCommandOrControlReturnIsTheBoxsOwn() {
        XCTAssertFalse(addsLine(command: true))
        XCTAssertFalse(addsLine(control: true))
        XCTAssertFalse(addsLine(shift: true, command: true))
    }
}
