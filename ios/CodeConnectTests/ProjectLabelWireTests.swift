import XCTest

@testable import CodeConnect

/// The minor-11 wire: a run named by the project it is working in, and where a
/// tapped notification goes.
final class ProjectLabelWireTests: XCTestCase {

    private func summary(_ json: String) throws -> SessionSummary {
        try JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
    }

    private let base = """
        "session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",\
        "tmux_session":"cc-1","cwd":"/srv/dev/code/Aion",\
        "lifecycle":"live","link":"attached","last_seq":7,\
        "created_at":"2026-08-07T10:00:00.000Z","updated_at":"2026-08-07T10:00:00.000Z"
        """

    func testTheProjectLabelIsTakenFromTheDaemonRatherThanDerivedHere() throws {
        let decoded = try summary("{\(base),\"project_label\":\"Aion\"}")
        XCTAssertEqual(decoded.projectLabel, "Aion")
    }

    /// The daemon's label wins even where this app could derive a different
    /// one: one resolver, on the Mac. This pins the decode that `RunLabel`
    /// builds every visible name on.
    func testADaemonLabelWinsOverAnythingDerivableFromTheWorkingDirectory() throws {
        let decoded = try summary("{\(base),\"project_label\":\"Renamed\"}")
        XCTAssertEqual(decoded.projectLabel, "Renamed")
        XCTAssertEqual(decoded.cwd, "/srv/dev/code/Aion")
    }

    /// An older daemon sends no label. Empty is not a bug to paper over with the
    /// tmux name — it is this app saying nobody has told it.
    func testAnOlderDaemonYieldsAnEmptyLabelRatherThanAFallback() throws {
        let decoded = try summary("{\(base)}")
        XCTAssertEqual(decoded.projectLabel, "")
        XCTAssertEqual(decoded.sessionID, "cc-1", "the handle is still there for attach")
    }
}
