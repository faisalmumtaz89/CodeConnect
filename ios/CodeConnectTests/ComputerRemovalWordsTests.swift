import CodeConnectKit
import XCTest

/// Both apps ask to remove a computer in the Kit's words, which name it and where it comes back: the place holding both
/// routes, the Tailscale list and a code (Add Computer on a Mac, Settings on the iPhone).
final class ComputerRemovalWordsTests: XCTestCase {
    func testRemovingNamesTheComputerAndWhereItComesBack() {
        let phone = ComputerRemoval.ask(name: "Studio", system: "macOS", on: .iPhone)
        XCTAssertEqual(phone.title, "Remove Studio?")
        XCTAssertEqual(phone.items.map(\.mark), ["desktopcomputer"])
        XCTAssertEqual(phone.items.map(\.words), ["This iPhone forgets Studio"])
        XCTAssertEqual(phone.note, "You can add it again from Settings.")
        XCTAssertEqual([phone.cancel, phone.confirm], ["Cancel", "Remove"])
        XCTAssertEqual(phone.message, "This iPhone forgets Studio. You can add it again from Settings.")

        let unnamed = ComputerRemoval.ask(name: nil, system: nil, on: .iPhone)
        XCTAssertEqual(unnamed.title, "Remove this computer?")
        XCTAssertEqual(unnamed.message, "This iPhone forgets this computer. You can add it again from Settings.")

        let mac = ComputerRemoval.ask(name: "lumen5090", system: "Linux", on: .mac)
        XCTAssertEqual(mac.items.map(\.mark), ["server.rack"])
        XCTAssertEqual(mac.items.map(\.words), ["This Mac forgets lumen5090"])
        XCTAssertEqual(mac.note, "You can add it again with Add Computer.")
    }
}
