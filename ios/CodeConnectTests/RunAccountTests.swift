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

    /// **Where a move stands** (`switching`), listing by listing across the daemon's recorded switches: waiting while
    /// the turn ends, moved once the process has ended, gone once the next message starts it; failed while a refused
    /// start stands.
    func testEachListingSaysWhereTheMoveStands() throws {
        func states(_ resource: String) throws -> [RunSwitching?] {
            let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: resource, withExtension: "json"))
            let frames = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).arrayValue)
            return try frames.compactMap { $0["sessions"]?[0] }.map { try decoded($0).switching }
        }
        XCTAssertEqual(try states("switch-claude-wire"), [.waiting, .waiting, .moved, .moved, nil])
        XCTAssertEqual(try states("switch-failed-then-recovered-claude-wire"), [.moved, .failed, .failed, nil])
    }
}
