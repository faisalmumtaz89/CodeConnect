import XCTest

@testable import CodeConnectKit

/// A run's folder as a shell prints it: a home folder as `~`, and every other folder whole, `/Users/Shared` among them,
/// which is a Mac's folder for every user and no one's home.
final class ShellPathTests: XCTestCase {
    func testAHomeFolderIsATilde() {
        XCTAssertEqual(FleetGroup.shellPath("/Users/sam/app"), "~/app")
        XCTAssertEqual(FleetGroup.shellPath("/home/sam/app"), "~/app")
        XCTAssertEqual(FleetGroup.shellPath("/Users/sam"), "~")
    }

    func testTheSharedFolderIsNoOnesHome() {
        XCTAssertEqual(FleetGroup.shellPath("/Users/Shared/app"), "/Users/Shared/app")
        XCTAssertEqual(FleetGroup.shellPath("/Users/Shared"), "/Users/Shared")
    }

    func testAnyOtherFolderIsWhole() {
        XCTAssertEqual(FleetGroup.shellPath("/srv/app"), "/srv/app")
        XCTAssertEqual(FleetGroup.shellPath("/Users"), "/Users")
    }
}
