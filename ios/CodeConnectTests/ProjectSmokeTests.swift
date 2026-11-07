import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// Proves the unit-test target is wired to the app target and that the two
/// pinned SPM dependencies link. Everything else in this bundle tests behaviour;
/// this one tests the build.
final class ProjectSmokeTests: XCTestCase {
    func testAppModuleIsVisible() {
        XCTAssertEqual(Wire.protocolVersion, 1)
    }
}
