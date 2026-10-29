import CryptoKit
import SwiftUI
import XCTest

@testable import CodeConnect

/// The fleet's line for a run held on an approval carries the first line of
/// its command, drawn and spoken, as the chat's approval row and the fleet's
/// line for a tool call do.
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

    /// A live, attached run in `cwd`, and minor 22's held card on it asking
    /// about `command`, with the text and hash the daemon computes for it — or,
    /// not `blocked`, a prompt typed into it at the same moment instead.
    private func model(
        command: String = "git status\r\ngit push --force origin main", blocked: Bool = true,
        acted: Bool = true, cwd: String = "/tmp/work"
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
                            "tmux_session": .string("cc-1"), "cwd": .string(cwd),
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
        elements(in: object).map { $0.accessibilityLabel ?? "" }
    }

    private func elements(in object: NSObject) -> [NSObject] {
        if object.accessibilityElementsHidden { return [] }
        if object.isAccessibilityElement { return [object] }
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
        return children.flatMap { elements(in: $0) }
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
        window.rootViewController = UIHostingController(
            rootView: FleetView().environment(model).environment(Machines(holding: model)))
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(2))
        let found = labels(in: window)
        window.isHidden = true
        // The row, not its card's header, which also starts with the project.
        return try XCTUnwrap(
            found.first { $0.hasPrefix("work, Claude Code session, ") }, "no row: \(found)")
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

    /// The count under the title and the Deck bar say how many wait for you in
    /// one sentence, and the bar says at which risk, as the card itself rates it.
    func testTheDeckBarSaysHowManyWaitAndAtWhichRisk() async throws {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let model = try model()
        _ = try await blockedRow(model)
        let risk = try XCTUnwrap(model.deck.first).assessment(profile: model.daemonProfile).effective
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = UIHostingController(
            rootView: FleetView().environment(model).environment(Machines(holding: model)))
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(2))
        let found = elements(in: window)
        let names = found.map { $0.accessibilityLabel ?? "" }
        let bar = found.first {
            $0.accessibilityLabel == "1 waiting for you" && !($0.accessibilityValue ?? "").isEmpty
        }
        window.isHidden = true
        XCTAssertEqual(
            names.filter { $0 == "1 waiting for you" }.count, 2, names.joined(separator: " | "))
        XCTAssertTrue(
            names.contains { $0.caseInsensitiveCompare("Waiting for you") == .orderedSame },
            names.joined(separator: " | "))
        XCTAssertEqual(
            bar?.accessibilityValue, "1 \(risk.label.lowercased()) risk",
            names.joined(separator: " | "))
    }

    /// The screen is titled Projects, and a project's card is headed by its
    /// folder, as a shell prints it, and the computer the daemon says it is.
    func testAProjectCardIsHeadedByItsFolderAndItsComputer() async throws {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let model = try model(cwd: "/Users/jordan/work")
        _ = try await blockedRow(model)
        // The computer's name arrives with a handshake, and a handshake holds
        // the screen as it is until the Mac answers a catch-up this test never
        // answers, so it comes once the row is drawn — as on a redial.
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-25-wire", withExtension: "json"))
        let wire = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
        model.connection.injectForTesting(try frame(try XCTUnwrap(wire["ccd_hello_ack"])))
        XCTAssertEqual(model.machineName, "Jordan’s Mac Studio")
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = UIHostingController(
            rootView: FleetView().environment(model).environment(Machines(holding: model)))
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(2))
        let found = elements(in: window)
        let spoken = found.map { $0.accessibilityLabel ?? "" }.joined(separator: " | ")
        let headings = found.filter { $0.accessibilityTraits.contains(.header) }
            .map { $0.accessibilityLabel ?? "" }
        window.isHidden = true
        XCTAssertTrue(headings.contains("Projects"), spoken)
        XCTAssertTrue(
            headings.contains { $0.contains("~/work") && $0.contains("Jordan’s Mac Studio") }, spoken)
    }

    /// A live run in `/tmp/x`.
    private func liveRun(_ uid: String) throws -> SessionSummary {
        try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/tmp/x","lifecycle":"live","link":"attached","last_seq":0,
                 "created_at":"2026-10-28T10:00:00Z","updated_at":"2026-10-28T10:00:00Z",
                 "blocked_on":[]}
                """.utf8))
    }

    /// The same folder on two computers is two projects, each card naming its
    /// own computer and that computer's link — and a link that drops while the
    /// screen is up changes that card alone.
    func testEachCardNamesItsOwnComputerAndItsLink() async throws {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let pairing = PairingStore(ephemeral: true)
        for host in ["studio.example.net", "laptop.example.net"] {
            pairing.save(
                DaemonEndpoint(host: host, port: 8787, credential: .token(host), useTLS: false))
        }
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("fleet-machines-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let machines = Machines(
            pairing: pairing, cache: EventCache(root: root),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
        for model in machines.models {
            model.connection.simulateConnectedForTesting()
            model.connection.simulateCapabilitiesForTesting(
                Capabilities(extra: ["session_uid": .bool(true)]), minor: 7)
        }
        let (studio, laptop) = (machines.models[0], machines.models[1])
        studio.adoptSummariesForTesting([
            try liveRun("01K8N0AAAAAAAAAAAAAAAAAAAA"), try liveRun("01K8N0CCCCCCCCCCCCCCCCCCCC"),
        ])
        laptop.adoptSummariesForTesting([try liveRun("01K8N0BBBBBBBBBBBBBBBBBBBB")])
        XCTAssertEqual(machines.fleet.count, 3)
        try await Task.sleep(for: .milliseconds(500))

        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
        window.rootViewController = UIHostingController(
            rootView: FleetView().environment(machines.linkModel).environment(machines))
        window.isHidden = false
        defer { window.isHidden = true }
        func screen() async throws -> (cards: [String], spoken: String) {
            window.rootViewController?.view.layoutIfNeeded()
            try await Task.sleep(for: .seconds(2))
            let found = elements(in: window)
            let cards = found.filter {
                $0.accessibilityTraits.contains(.header) && ($0.accessibilityLabel ?? "").contains("/tmp/x")
            }.map { $0.accessibilityLabel ?? "" }
            return (cards, found.map { $0.accessibilityLabel ?? "" }.joined(separator: " | "))
        }

        let before = try await screen()
        XCTAssertEqual(before.cards.count, 2, before.spoken)
        XCTAssertTrue(before.cards.contains { $0.contains("On studio.example.net, connected") }, before.spoken)
        XCTAssertTrue(before.cards.contains { $0.contains("On laptop.example.net, connected") }, before.spoken)
        XCTAssertTrue(before.spoken.contains("3 agents on 2 computers"), before.spoken)

        laptop.connection.setPhaseForTesting(.waiting(until: Date(), reason: "down"))
        let after = try await screen()
        XCTAssertTrue(after.cards.contains { $0.contains("On laptop.example.net, Offline") }, after.spoken)
        XCTAssertTrue(after.cards.contains { $0.contains("On studio.example.net, connected") }, after.spoken)
    }

    /// A live fleet with no runs is one instruction: how to start an agent,
    /// with no header naming the same empty state in other words above it, and
    /// the instruction's title is the screen's heading. Settings, top right, is
    /// the word it shows.
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
        window.rootViewController = UIHostingController(
            rootView: FleetView().environment(model).environment(Machines(holding: model)))
        window.isHidden = false
        window.rootViewController?.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(2))
        let found = elements(in: window)
        let names = found.map { $0.accessibilityLabel ?? "" }
        let spoken = names.joined(separator: " | ")
        let headings = found.filter { $0.accessibilityTraits.contains(.header) }
            .map { $0.accessibilityLabel ?? "" }
        window.isHidden = true
        XCTAssertEqual(headings, ["Start an agent on your computer"], spoken)
        XCTAssertTrue(spoken.contains("codeconnect claude"), spoken)
        XCTAssertTrue(spoken.contains("codeconnect codex"), spoken)
        XCTAssertFalse(spoken.contains("Nothing is waiting for you"), spoken)
        XCTAssertFalse(spoken.contains("No agents running"), spoken)
        XCTAssertFalse(spoken.contains("No agents registered"), spoken)
        XCTAssertTrue(names.contains("Settings"), spoken)
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
