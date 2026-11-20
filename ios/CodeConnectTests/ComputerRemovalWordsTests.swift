import CodeConnectKit
import XCTest

/// Both apps ask to remove a computer in the Kit's words, which say both ways it comes back: from Add a computer
/// while another computer on the same Tailscale account lists it, otherwise by a new code.
final class ComputerRemovalWordsTests: XCTestCase {
    func testRemovingSaysBothWaysTheComputerComesBack() {
        XCTAssertEqual(ComputerRemoval.question, "Remove this computer?")
        XCTAssertEqual(ComputerRemoval.action, "Remove")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: "Studio", on: .iPhone),
            "This iPhone forgets Studio. To use it again, open Add a computer, which lists it while this iPhone is "
                + "connected to another computer on your Tailscale account, or show a new code on it and scan it "
                + "(codeconnect.sh/support shows how).")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: nil, on: .iPhone),
            "This iPhone forgets this computer. To use it again, open Add a computer, which lists it while this "
                + "iPhone is connected to another computer on your Tailscale account, or show a new code on it and "
                + "scan it (codeconnect.sh/support shows how).")
        XCTAssertEqual(
            ComputerRemoval.sentence(name: "Studio", on: .mac),
            "This Mac forgets Studio. To use it again, open Add a Computer, which lists it while this Mac is "
                + "connected to another computer on your Tailscale account, or show a new code on it and enter it "
                + "here (codeconnect.sh/support shows how).")
    }
}
