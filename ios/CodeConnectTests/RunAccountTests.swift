import XCTest

@testable import CodeConnectKit

/// **The account a run's process runs on** (`SessionSummary.account`, minor 48): read as the daemon names it, Default
/// told by its null id and never by its label, and absent read as not known.
///
/// From the daemon's own `codex-settings-wire.json` (regenerated at minor 48: every run CodeConnect started names its
/// account) and `restart-link-state-wire.json` (frozen, from a daemon before the field).
final class RunAccountTests: XCTestCase {

    private func wire(_ name: String) throws -> [String: JSONValue] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func decoded(_ value: JSONValue) throws -> SessionSummary {
        try JSONDecoder().decode(SessionSummary.self, from: Data(value.canonicalJSONString.utf8))
    }

    func testARunCodeConnectStartedNamesItsAccountAndDefaultIsTheNullID() throws {
        let run = try decoded(try XCTUnwrap(try wire("codex-settings-wire")["session_codex_live"]))
        let account = try XCTUnwrap(run.account, "a run CodeConnect started names its account")
        XCTAssertNil(account.id)
        XCTAssertEqual(account.label, "Default")
        XCTAssertTrue(account.isDefault)
    }

    /// An account a person named "Default" is not Default: only the null id is.
    func testDefaultIsToldByTheIDNotTheLabel() throws {
        var run = try XCTUnwrap(try wire("codex-settings-wire")["session_codex_live"]?.objectValue)
        run["account"] = .object(["id": .string("01M8299ZGSRFFX29Z8KF3Q5Q2S"), "label": .string("Default")])
        let account = try XCTUnwrap(try decoded(.object(run)).account)
        XCTAssertFalse(account.isDefault)
        XCTAssertEqual(account.label, "Default")
    }

    func testAnOlderDaemonsRunNamesNoAccount() throws {
        let sessions = try XCTUnwrap(try wire("restart-link-state-wire")["before_restart"]?["sessions"]?.arrayValue)
        XCTAssertFalse(sessions.isEmpty)
        for value in sessions { XCTAssertNil(try decoded(value).account, "absent is not known, never Default") }
    }
}
