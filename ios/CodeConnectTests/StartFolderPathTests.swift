import XCTest

@testable import CodeConnectKit

/// **A folder as a shell prints it**, one rule for a recent folder and for the folder the browser shows (screen 33's
/// "~/models"): a home folder, a Mac's or a Linux computer's, as "~".
final class StartFolderPathTests: XCTestCase {
    func testAHomeFolderIsTilde() {
        XCTAssertEqual(StartProject.shown("/home/fm/models"), "~/models")
        XCTAssertEqual(StartProject.shown("/Users/jordan/code/query-parser"), "~/code/query-parser")
        XCTAssertEqual(StartProject.shown("/home/fm"), "~")
    }

    func testAnyOtherFolderIsAsItIs() {
        XCTAssertEqual(StartProject.shown("/srv/models"), "/srv/models")
        XCTAssertEqual(StartProject.shown("/"), "/")
        XCTAssertEqual(StartProject.shown("/home"), "/home")
    }
}
