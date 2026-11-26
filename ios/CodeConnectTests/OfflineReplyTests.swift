import XCTest

@testable import CodeConnectKit

/// The reply line of a run whose computer is offline names that computer and says when a reply can go: once it is
/// back. Only the reply line: a control that is not a reply (model, mode) keeps the link's own sentence. An offline link
/// with a cause of its own (a name that is not resolving) keeps that cause, which says more.
@MainActor
final class OfflineReplyTests: XCTestCase {
    private let key = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    private func model(linkState: String) -> AppModel {
        UserDefaults.standard.set(linkState, forKey: "CC_LINK_STATE")
        let model = AppModel(
            cache: EventCache(),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(canApproveReliably: true, sendText: true, extra: [:]), minor: 39, machineName: "lumen5090")
        return model
    }

    override func tearDown() {
        UserDefaults.standard.removeObject(forKey: "CC_LINK_STATE")
        super.tearDown()
    }

    func testTheReplyLineNamesTheOfflineComputerAndWaitsForIt() {
        let model = model(linkState: "unreachable")
        XCTAssertEqual(model.linkHealth.level, .offline)
        XCTAssertEqual(
            model.standingNote(for: key, draft: "hello")?.text, "lumen5090 is offline. You can reply once it’s back.")
        XCTAssertEqual(model.sendBlockedReason(for: key, draft: "hello"), model.linkHealth.disabledReason)
    }

    func testAnOfflineLinkWithItsOwnCauseKeepsIt() {
        let model = model(linkState: "tailnet")
        XCTAssertEqual(model.linkHealth.level, .offline)
        XCTAssertEqual(model.standingNote(for: key, draft: "hello")?.text, model.linkHealth.disabledReason)
    }
}
