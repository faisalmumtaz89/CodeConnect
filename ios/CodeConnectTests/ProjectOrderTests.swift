import XCTest

@testable import CodeConnectKit

/// The owner's order of the projects: kept, new ones on top, gone ones
/// skipped and remembered, and changed by a move alone.
final class ProjectOrderTests: XCTestCase {
    private let a = ProjectKey(machineID: "studio", folder: "/Users/j/a")
    private let b = ProjectKey(machineID: "studio", folder: "/Users/j/b")
    private let c = ProjectKey(machineID: "laptop", folder: "/Users/j/a")
    private let d = ProjectKey(machineID: nil, folder: "/tmp/d")

    func testTheSavedOrderIsKeptWhateverOrderTheFleetArrivesIn() {
        XCTAssertEqual(ProjectOrder.arrange([c, a, b], saved: [a, b, c]), [a, b, c])
    }

    func testANewProjectGoesOnTopAndAGoneOneIsSkipped() {
        XCTAssertEqual(ProjectOrder.arrange([b, d, c], saved: [a, b, c]), [d, b, c])
    }

    func testRecordingKeepsANewProjectWhereItAppeared() {
        let saved = ProjectOrder.recording([d, a, b], into: [a, b])
        XCTAssertEqual(saved, [d, a, b])
        XCTAssertEqual(ProjectOrder.recording([d, a, b], into: saved), saved)
        XCTAssertEqual(ProjectOrder.arrange([a, b, d], saved: saved), [d, a, b])
    }

    /// A project that left keeps its place for when it comes back.
    func testAProjectThatComesBackReturnsToItsPlace() {
        let saved = [a, b, c]
        XCTAssertEqual(ProjectOrder.arrange([a, c], saved: saved), [a, c])
        XCTAssertEqual(ProjectOrder.arrange([a, b, c], saved: saved), [a, b, c])
    }

    func testMovingUpPutsTheCardBeforeItsTarget() {
        XCTAssertEqual(ProjectOrder.moving(c, to: a, shown: [a, b, c], saved: [a, b, c]), [c, a, b])
    }

    func testMovingDownPutsTheCardAfterItsTarget() {
        XCTAssertEqual(ProjectOrder.moving(a, to: b, shown: [a, b, c], saved: [a, b, c]), [b, a, c])
    }

    /// A project not on screen keeps its place through a move of the others.
    func testAMoveLeavesAProjectNotOnScreenWhereItWas() {
        let moved = ProjectOrder.moving(c, to: a, shown: [a, c], saved: [a, b, c])
        XCTAssertEqual(moved, [c, a, b])
        XCTAssertEqual(ProjectOrder.arrange([a, b, c], saved: moved), [c, a, b])
    }

    func testAMoveOntoItselfOrAnUnknownCardChangesNothing() {
        XCTAssertEqual(ProjectOrder.moving(a, to: a, shown: [a, b], saved: [a, b]), [a, b])
        XCTAssertEqual(ProjectOrder.moving(a, to: d, shown: [a, b], saved: [a, b]), [a, b])
    }

    func testAKeySurvivesBeingStored() {
        for key in [a, c, d, ProjectKey(machineID: "m", folder: "/with\u{1F}separator")] {
            XCTAssertEqual(ProjectKey(stored: key.stored), key)
        }
    }
}
