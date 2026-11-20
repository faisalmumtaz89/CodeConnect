import CodeConnectKit
import XCTest

/// Both apps ask to remove a computer in the Kit's words, which name where it comes back: the place holding both
/// routes, the Tailscale list and a code (Add a Computer on a Mac, Settings on the iPhone).
final class ComputerRemovalWordsTests: XCTestCase {
    func testRemovingSaysWhereTheComputerComesBack() {
        XCTAssertEqual(ComputerRemoval.question, "Remove this computer?")
        XCTAssertEqual(ComputerRemoval.action, "Remove")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: "Studio", on: .iPhone),
            "This iPhone forgets Studio. You can add it again from Settings.")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: nil, on: .iPhone),
            "This iPhone forgets this computer. You can add it again from Settings.")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: "Studio", on: .mac),
            "This Mac forgets Studio. You can add it again from Add a Computer.")
    }
}
