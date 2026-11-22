import XCTest

/// The runner's half of `FixtureSignal`: a directory of its own, named to the app at launch, and a file written
/// into it for each held fixture the test releases. The app waits on the file, so the held thing happens when
/// the test says, not when a clock does.
struct FixtureSignals {
    let directory = (NSTemporaryDirectory() as NSString)
        .appendingPathComponent("cc-fixture-signals-\(UUID().uuidString)")

    /// Names the directory to `app`, emptied of an earlier launch's signals; call before launching.
    func arm(_ app: XCUIApplication) {
        try? FileManager.default.removeItem(atPath: directory)
        try? FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
        app.launchEnvironment["CC_FIXTURE_SIGNALS"] = directory
    }

    /// Releases the hold named `name` and returns once the app has taken it; a hold the app never reaches fails the
    /// test here, by name, instead of leaving it to time out on whatever it waits for next.
    func send(_ name: String, file: StaticString = #filePath, line: UInt = #line) {
        let path = (directory as NSString).appendingPathComponent(name)
        FileManager.default.createFile(atPath: path, contents: nil)
        let taken = XCTNSPredicateExpectation(
            predicate: NSPredicate { _, _ in FileManager.default.fileExists(atPath: path + ".taken") }, object: nil)
        if XCTWaiter.wait(for: [taken], timeout: 15) != .completed {
            XCTFail("the app never took the fixture signal \"\(name)\"", file: file, line: line)
        }
    }
}
