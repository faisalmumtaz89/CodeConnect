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

    /// The Mac's folder question names its folder (screen 35); the rest of its words are the iPhone's.
    func testTheTrustQuestionNamesItsFolder() {
        XCTAssertEqual(
            FolderWords.question("Claude-Code-Setup"), "Do you trust the authors of the files in Claude-Code-Setup?")
        XCTAssertEqual(FolderWords.trust, "Yes, I trust the authors")
        XCTAssertEqual(FolderWords.distrust, "No, I don’t trust the authors")
        XCTAssertEqual(FolderWords.footnote, "Asked once for this folder. You can change it in Settings › Folders.")
    }

    func testAnyOtherFolderIsAsItIs() {
        XCTAssertEqual(StartProject.shown("/srv/models"), "/srv/models")
        XCTAssertEqual(StartProject.shown("/"), "/")
        XCTAssertEqual(StartProject.shown("/home"), "/home")
    }
}
