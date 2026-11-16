import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Minor 40 (decisions 214/215): a mode the menu marks `confirm` is set only with the person's
/// confirmation.** The daemon marks the dangerous mode `confirm`, refuses a bare `set_mode` of it, and
/// applies one carrying `confirmed`. Proven against ccd's own bytes (minor-40-wire.json): the app's
/// decode of the menu is what ccd wrote, and its encode of the confirmed send is what ccd reads.
final class Minor40ConfirmTests: XCTestCase {

    private let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-40-wire", withExtension: "json"))
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func encoded(_ message: ClientMessage) throws -> JSONValue {
        try JSONDecoder().decode(JSONValue.self, from: JSONEncoder().encode(message))
    }

    func testTheMenuMarksOnlyTheConfirmMode() throws {
        let modes = try XCTUnwrap(try wire()["menus_modes"]?.decoded([MenuMode].self))
        XCTAssertEqual(modes.map(\.value), ["default", "acceptEdits", "plan", "bypassPermissions"])
        XCTAssertEqual(modes.map(\.confirm), [false, false, false, true], "only Bypass permissions is marked")
    }

    func testSetModeCarriesConfirmedOnlyWhenConfirmed() throws {
        // A bare request omits `confirmed`, which the daemon refuses for a mode that needs it.
        XCTAssertEqual(
            try encoded(
                .setMode(session: uid, requestID: "mode-1", mode: "bypassPermissions", confirmed: false)),
            try XCTUnwrap(try wire()["set_mode_bare_request"]))
        // The person's confirmation carries `confirmed: true`, which the daemon applies.
        XCTAssertEqual(
            try encoded(
                .setMode(session: uid, requestID: "mode-2", mode: "bypassPermissions", confirmed: true)),
            try XCTUnwrap(try wire()["set_mode_confirmed_request"]))
    }
}
