import CodeConnectKit
import XCTest

/// Browsing a computer's folders reads in the Kit's words, as the iPhone showed them, and moves up and in by the
/// Kit's paths, the same in both apps.
final class StartBrowseWordsTests: XCTestCase {
    func testTheBrowserReadsAsTheIPhoneShowedIt() {
        XCTAssertEqual(StartWords.browseFolders, "Browse folders")
        XCTAssertEqual(StartWords.showHiddenFolders, "Show hidden folders")
        XCTAssertEqual(StartWords.newFolderAction, "New folder…")
        XCTAssertEqual(StartWords.useThisFolder, "Use this folder")
        XCTAssertEqual(StartWords.newFolder, "New folder")
        XCTAssertEqual(StartWords.newFolderIn("/Users/dev/clients"), "In /Users/dev/clients")
        XCTAssertEqual(StartWords.folderName, "Name")
        XCTAssertEqual(StartWords.couldNotMake("app-5"), "Couldn’t make “app-5”")
        XCTAssertEqual(StartWords.firstFolders, "The first 2,000 folders, in name order.")
        XCTAssertEqual(StartWords.noFolders(showingHidden: true), "No folders in here.")
        XCTAssertEqual(StartWords.noFolders(showingHidden: false), "No folders in here, hidden ones aside.")
    }

    func testWhileListingItSaysWhichFolder() {
        XCTAssertEqual(StartWords.readingFolder("/Users/dev/clients", on: "macbook-air"), "Reading /Users/dev/clients…")
        XCTAssertEqual(StartWords.readingFolder(nil, on: "macbook-air"), "Reading macbook-air’s home folder…")
    }

    func testTheMacsButtonsReadInTitleStyle() {
        XCTAssertEqual(StartWords.newFolderAction.titleStyle, "New Folder…")
        XCTAssertEqual(StartWords.useThisFolder.titleStyle, "Use This Folder")
    }

    func testUpAndInStayInsideTheRoot() {
        XCTAssertEqual(StartContext.parentFolder("/Users/dev/clients"), "/Users/dev")
        XCTAssertEqual(StartContext.parentFolder("/Users"), "/")
        XCTAssertEqual(StartContext.parentFolder("/"), "/")
        XCTAssertEqual(StartContext.childFolder("/Users/dev", "clients"), "/Users/dev/clients")
        XCTAssertEqual(StartContext.childFolder("/", "Users"), "/Users")
    }
}
