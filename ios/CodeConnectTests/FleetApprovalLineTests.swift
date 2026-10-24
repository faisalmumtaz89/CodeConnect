import CryptoKit
import SwiftUI
import XCTest

@testable import CodeConnect

/// The fleet's line for a run held on an approval carries the first line of
/// its command, drawn and spoken, as the chat's approval row, the Deck bar and
/// the fleet's line for a tool call do.
///
/// It carried the whole command: a blocked row spoke every line of a
/// multi-line one — 43% of the owner's Bash commands have more than one. The
/// row also says how long the run has waited, as a duration, where a run not
/// waiting says when it last acted, as an age.
@MainActor
final class FleetApprovalLineTests: XCTestCase {

    private func frame(_ value: JSONValue) throws -> ServerMessage {
        try XCTUnwrap(
            try? JSONDecoder().decode(ServerMessage.self, from: JSONEncoder().encode(value)))
    }

    /// A live, attached run, and minor 22's held card on it asking about
    /// `command`, with the text and hash the daemon computes for it — or, not
    /// `blocked`, a prompt typed into it at the same moment instead.
    private func model(
        command: String = "git status\r\ngit push --force origin main", blocked: Bool = true,
        acted: Bool = true
    ) throws -> AppModel {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("fleet-line-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }

        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-22-wire", withExtension: "json"))
        let wire = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
        var request = try XCTUnwrap(wire["held_request"]?.objectValue)
        var payload = try XCTUnwrap(request["payload"]?.objectValue)
        var card = try XCTUnwrap(payload["card"]?.objectValue)
        let input = JSONValue.object(["command": .string(command)])
        let text = "Bash\n\(input.canonicalJSONString)"
        card["tool_input"] = input
        card["display_text"] = .string(text)
        card["payload_hash"] = .string(
            SHA256.hash(data: Data(text.utf8)).map { String(format: "%02x", $0) }.joined())
        payload["card"] = .object(card)
        request["payload"] = .object(payload)

        model.connection.injectForTesting(
            try frame(
                .object([
                    "type": .string("sessions"),
                    "sessions": .array([
                        .object([
                            "session_uid": .string(WireRecording.uid), "session_id": .string("cc-1"),
                            "tmux_session": .string("cc-1"), "cwd": .string("/tmp/work"),
                            "project_label": .string("work"), "lifecycle": .string("live"),
                            "link": .string("attached"), "last_seq": .int(2),
                            "created_at": .string("2026-10-08T00:00:00Z"),
                            "updated_at": .string("2026-10-08T00:00:00Z"),
                            "blocked_on": .array(blocked ? [.string("toolu_allowed")] : []),
                        ])
                    ]),
                ])))
        if !acted { return model }
        if !blocked {
            request["kind"] = .string("user_message")
            request["source"] = .string("transcript")
            request["payload"] = .object(["message": .object(["content": .string("step 1")])])
        }
        model.connection.injectForTesting(
            try frame(.object(["type": .string("event"), "event": .object(request)])))
        return model
    }

    private func blockedRow(_ model: AppModel) async throws -> FleetRow {
        for _ in 0..<100 {
            // Blocked from the summary at once; its card reaches the row when
            // the session has rebuilt its timeline.
            if let row = model.fleet.first, row.status == .blocked, row.activity != nil { return row }
            try await Task.sleep(for: .milliseconds(20))
        }
        return try XCTUnwrap(nil, "no blocked row with its card: \(model.fleet)")
    }

    /// The row's own sentence and its tool-and-argument pair both stop at the
    /// command's first line.
    func testTheFleetRowCarriesTheFirstLineOfABlockedCommand() async throws {
        let row = try await blockedRow(try model())
        XCTAssertEqual(row.subtitle, "Bash git status")
        XCTAssertEqual(row.activity?.argument, "git status")
    }

    // MARK: On the screen

    private typealias SetAccessibilityEnabled = @convention(c) (Int32) -> Void

    private func setAccessibility(_ enabled: Bool) throws {
        let library = try XCTUnwrap(dlopen("/usr/lib/libAccessibility.dylib", RTLD_NOW))
        let symbol = try XCTUnwrap(dlsym(library, "_AXSApplicationAccessibilitySetEnabled"))
        unsafeBitCast(symbol, to: SetAccessibilityEnabled.self)(enabled ? 1 : 0)
    }

    private func labels(in object: NSObject) -> [String] {
        if object.accessibilityElementsHidden { return [] }
        if object.isAccessibilityElement { return [object.accessibilityLabel ?? ""] }
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
        return children.flatMap { labels(in: $0) }
    }

    /// What VoiceOver says on the run's row, drawn on an iPhone 17 Pro's
    /// screen.
    private func spokenRow(
        command: String = "ls", blocked: Bool = true, acted: Bool = true
    ) async throws -> String {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let model = try model(command: command, blocked: blocked, acted: acted)
        if blocked {
            _ = try await blockedRow(model)
        } else {
            try await Task.sleep(for: .milliseconds(500))
        }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = UIHostingController(rootView: FleetView().environment(model))
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(2))
        let found = labels(in: window)
        window.isHidden = true
        return try XCTUnwrap(found.first { $0.hasPrefix("work, ") }, "no row: \(found)")
    }

    /// The blocked row speaks the command's first line and nothing after it.
    func testTheDrawnBlockedRowSpeaksTheFirstLineOfItsCommand() async throws {
        let row = try await spokenRow(command: "git status\r\ngit push --force origin main")
        XCTAssertTrue(row.contains(", Bash, git status, waiting "), row.debugDescription)
        XCTAssertFalse(row.contains("git push"), row.debugDescription)
    }

    /// A first line with nothing on it is spoken as no argument, as the chat's
    /// approval row speaks it, rather than as an empty clause (`Bash, ,`).
    func testABlockedCommandWithABlankFirstLineIsSpokenAsTheToolAlone() async throws {
        let row = try await spokenRow(command: "\ngit push --force origin main")
        XCTAssertTrue(row.contains(", Bash, waiting "), row.debugDescription)
    }

    /// The Deck bar names the tool when the command's first line is blank,
    /// rather than drawing and speaking an empty command (`work ·  · waiting`).
    func testTheDeckBarNamesTheToolForABlankFirstLine() async throws {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let model = try model(command: "\ngit push --force origin main")
        _ = try await blockedRow(model)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = UIHostingController(rootView: FleetView().environment(model))
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(2))
        let preview = try XCTUnwrap(
            values(in: window).first { $0.hasPrefix("work · ") }, "no Deck bar")
        window.isHidden = true
        XCTAssertTrue(preview.hasPrefix("work · Bash · waiting "), preview)
    }

    private func values(in object: NSObject) -> [String] {
        if object.accessibilityElementsHidden { return [] }
        if object.isAccessibilityElement { return [object.accessibilityValue ?? ""] }
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
        return children.flatMap { values(in: $0) }
    }

    /// A live fleet with no runs says so once: the header does not name the
    /// same empty state in other words ("No agents registered") above it.
    func testAnEmptyLiveFleetSaysItOnce() async throws {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("fleet-empty-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(
            try frame(.object(["type": .string("sessions"), "sessions": .array([])])))
        try await Task.sleep(for: .milliseconds(500))
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = UIHostingController(rootView: FleetView().environment(model))
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(2))
        let spoken = labels(in: window).joined(separator: " | ")
        window.isHidden = true
        XCTAssertTrue(spoken.contains("No agents running"), spoken)
        XCTAssertFalse(spoken.contains("No agents registered"), spoken)
    }

    /// How long the run has waited is a duration — "waiting 370 hours" — and
    /// never an age.
    func testTheBlockedRowSpeaksHowLongItHasWaited() async throws {
        let row = try await spokenRow()
        XCTAssertNotNil(row.range(of: #", waiting \d+ hours(,|$)"#, options: .regularExpression), row)
        XCTAssertFalse(row.contains(" ago"), row)
    }

    /// A run with nothing to show yet speaks no empty clause where its
    /// activity would be (`Idle, , control`).
    func testARowWithNoActivitySpeaksNoEmptyClause() async throws {
        let row = try await spokenRow(blocked: false, acted: false)
        XCTAssertFalse(row.components(separatedBy: ", ").contains(""), row.debugDescription)
    }

    /// When a run that is not waiting last did something is an age.
    func testARowNotWaitingSpeaksWhenItLastActed() async throws {
        let row = try await spokenRow(blocked: false)
        XCTAssertNotNil(
            row.range(of: #", last activity \d+ hours ago(,|$)"#, options: .regularExpression), row)
    }
}
