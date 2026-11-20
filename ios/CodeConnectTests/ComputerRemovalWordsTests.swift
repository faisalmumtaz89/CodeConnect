import CodeConnectKit
import XCTest

/// The iPhone asks to remove a computer in the Kit's words, the Mac's own, and
/// they read as the iPhone's did when it held them itself.
final class ComputerRemovalWordsTests: XCTestCase {
    func testTheIPhoneAsksInTheWordsItAlwaysUsed() {
        XCTAssertEqual(ComputerRemoval.question, "Remove this computer?")
        XCTAssertEqual(ComputerRemoval.action, "Remove")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: "Studio", on: .iPhone),
            "This iPhone forgets Studio. To use it again, show a new code on it and scan it (codeconnect.sh/support shows how).")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: nil, on: .iPhone),
            "This iPhone forgets this computer. To use it again, show a new code on it and scan it (codeconnect.sh/support shows how).")
    }
}
