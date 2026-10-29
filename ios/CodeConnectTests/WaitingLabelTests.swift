import SwiftUI
import XCTest

@testable import CodeConnect

/// How long something has waited is spoken as a duration — "waiting 1
/// minute" — and when something last happened as an age — "last activity 1
/// minute ago".
///
/// Every waiting label built its number with `Format.spokenAge`, which ends in
/// "ago", so VoiceOver said "waiting 1 minute ago": on the chat's approval and
/// question rows, the fleet's blocked row and the wait clock.
@MainActor
final class WaitingLabelTests: XCTestCase {

    func testADurationIsSpokenWithoutAgo() {
        for (seconds, spoken) in [
            (0.0, "0 seconds"), (1, "1 second"), (59, "59 seconds"), (72, "1 minute"),
            (119, "1 minute"), (120, "2 minutes"), (3600, "1 hour"), (7200, "2 hours"),
        ] {
            XCTAssertEqual(Format.spokenDuration(seconds), spoken)
            XCTAssertEqual(Format.spokenAge(seconds), "\(spoken) ago")
        }
    }

    // MARK: On the screen

    private typealias SetAccessibilityEnabled = @convention(c) (Int32) -> Void

    private func setAccessibility(_ enabled: Bool) throws {
        let library = try XCTUnwrap(dlopen("/usr/lib/libAccessibility.dylib", RTLD_NOW))
        let symbol = try XCTUnwrap(dlsym(library, "_AXSApplicationAccessibilitySetEnabled"))
        unsafeBitCast(symbol, to: SetAccessibilityEnabled.self)(enabled ? 1 : 0)
    }

    private func stops(in object: NSObject) -> [(label: String, value: String)] {
        if object.accessibilityElementsHidden { return [] }
        if object.isAccessibilityElement {
            return [(object.accessibilityLabel ?? "", object.accessibilityValue ?? "")]
        }
        let count = object.accessibilityElementCount()
        let children: [NSObject]
        if let elements = object.accessibilityElements as? [NSObject], !elements.isEmpty {
            children = elements
        } else if count > 0, count != NSNotFound {
            children = (0..<count).compactMap { object.accessibilityElement(at: $0) as? NSObject }
        } else if let view = object as? UIView {
            children = view.subviews.filter { !$0.isHidden }
        } else {
            children = []
        }
        return children.flatMap { stops(in: $0) }
    }

    /// What VoiceOver stops on in `view`, drawn on an iPhone 17 Pro's screen.
    private func spoken(_ view: some View) throws -> [String] {
        try stopsOnScreen(view).map(\.label)
    }

    /// The values of those stops, where VoiceOver reads a control's preview.
    private func spokenValues(_ view: some View) throws -> [String] {
        try stopsOnScreen(view).map(\.value)
    }

    private func stopsOnScreen(_ view: some View) throws -> [(label: String, value: String)] {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = UIHostingController(rootView: view)
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(1))
        defer { window.isHidden = true }
        return stops(in: window)
    }

    // MARK: The daemon's cards, asked 72 seconds ago

    private func approval(_ wire: String, _ key: String) throws -> ApprovalItem {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: wire, withExtension: "json"))
        let root = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
        var request = try XCTUnwrap(root[key]?.objectValue)
        let stamp = ISO8601DateFormatter()
        stamp.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        request["ts"] = .string(stamp.string(from: Date().addingTimeInterval(-72)))
        let event = try XCTUnwrap(JSONValue.object(request).decoded(Event.self))
        var built: ApprovalItem?
        for item in TimelineBuilder.build([event]) {
            if case .approval(let approval) = item.content { built = approval }
        }
        return try XCTUnwrap(built, "no approval row was built")
    }

    func testAPendingApprovalRowSpeaksHowLongItHasWaited() throws {
        let row = ApprovalRow(approval: try approval("minor-22-wire", "held_request"), risk: .medium, onOpen: {})
        let found = try spoken(row)
        XCTAssertEqual(found.count, 1, "\(found)")
        XCTAssertTrue(found.first?.hasSuffix(", waiting 1 minute") == true, "\(found)")
    }

    func testAPendingQuestionRowSpeaksHowLongItHasWaited() throws {
        let approval = try approval("minor-21-wire", "held_request")
        let questions = try XCTUnwrap(QuestionCard(card: approval.card))
        let found = try spoken(
            QuestionRow(
                approval: approval, questions: questions, risk: .low, answersQuestions: true,
                linkReason: nil, onOpen: {}))
        XCTAssertTrue(found.contains { $0.hasSuffix(". waiting 1 minute.") }, "\(found)")
    }

    func testTheWaitClockSpeaksHowLongItHasWaited() throws {
        let now = Date()
        for prefix in ["waiting", nil] {
            let found = try spoken(
                CCWaitClock(since: now.addingTimeInterval(-72), now: now, prefix: prefix))
            XCTAssertEqual(found, ["waiting 1 minute"])
        }
    }

    /// The Deck bar's spoken preview ends with the wait, as a duration.
    func testTheDeckBarSpeaksHowLongItHasWaited() throws {
        let now = Date()
        let found = try spokenValues(
            DeckAccessoryBar(
                count: 1, topPlace: "work", topCommand: "git status",
                waitingSince: now.addingTimeInterval(-72), now: now, open: {}))
        XCTAssertTrue(found.contains("work · git status · waiting 1 minute"), "\(found)")
    }

    /// A tool call whose argument starts with a blank line is spoken as the
    /// tool alone, as the fleet row speaks it, rather than as an empty clause.
    func testAToolCallWithABlankFirstLineSpeaksNoEmptyClause() throws {
        let found = try spoken(
            ToolRow(
                tool: ToolItem(
                    toolUseID: nil, name: "Bash", argument: "\ngit push --force origin main", status: .succeeded,
                    durationMS: nil, input: nil, output: nil, isTruncated: false)))
        let row = try XCTUnwrap(found.first { $0.hasPrefix("Bash tool call") }, "\(found)")
        XCTAssertFalse(row.components(separatedBy: ", ").contains(""), row)
    }

    /// The design gallery's blocked fleet row is the shipped label, so it says
    /// what the shipped row says.
    func testTheGallerysBlockedRowSpeaksTheShippedWait() throws {
        let found = try spoken(CCGallery(page: .rows))
        let row = try XCTUnwrap(found.first { $0.contains(", Blocked, risk HIGH, ") }, "\(found)")
        XCTAssertTrue(row.hasSuffix(", waiting 4 minutes, control"), row)
    }
}
