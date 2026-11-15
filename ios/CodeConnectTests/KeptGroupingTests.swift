import XCTest

@testable import CodeConnectKit

/// Owner decision 193: the Mac keeps the fleet list's Computer/Project grouping between launches, in its own
/// preferences (`FleetGroup.Kind.kept(in:)` and `keep(in:)`). Each test has a defaults store of its own, removed after
/// it, never the standard one.
final class KeptGroupingTests: XCTestCase {
    private var suite = ""
    private var defaults: UserDefaults!

    override func setUp() {
        suite = "cc.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)!
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
    }

    /// Nothing kept: the list opens by computer.
    func testNothingKeptOpensByComputer() {
        XCTAssertEqual(FleetGroup.Kind.kept(in: defaults), .computer)
    }

    /// A choice is written under its key when made, and the next launch, a store opened afresh on the same
    /// preferences, reads it back; so does a change back.
    func testAChoiceIsKeptForTheNextLaunch() {
        FleetGroup.Kind.project.keep(in: defaults)
        XCTAssertEqual(defaults.string(forKey: "codeconnect.fleet.grouping"), "project", "written when chosen")
        XCTAssertEqual(FleetGroup.Kind.kept(in: UserDefaults(suiteName: suite)!), .project, "read at the next launch")

        FleetGroup.Kind.computer.keep(in: defaults)
        XCTAssertEqual(defaults.string(forKey: "codeconnect.fleet.grouping"), "computer", "written when changed back")
        XCTAssertEqual(FleetGroup.Kind.kept(in: UserDefaults(suiteName: suite)!), .computer, "read back")
    }
}
