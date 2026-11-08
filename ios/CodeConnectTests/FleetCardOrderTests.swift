import SwiftUI
import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The home screen's cards stay where they are. The owner: "those cards keep
/// moving up and down and because of that sometimes I will click on something
/// and something else will open because the card has moved."
///
/// A status or an event changes what a card says, never where it is. A project
/// or a run that arrives or leaves while the list is in sight waits, named by
/// the header's pill, until the owner lets it in: then a new project goes on
/// top and the rest close up in order. Nothing above the cards grows or shrinks
/// with the fleet: the first decision, and the banner of a fleet read off the
/// phone going live, move no card.
@MainActor
final class FleetCardOrderTests: XCTestCase {
    private let alpha = "01K8N0AAAAAAAAAAAAAAAAAAAA"
    private let bravo = "01K8N0BBBBBBBBBBBBBBBBBBBB"
    private let charlie = "01K8N0CCCCCCCCCCCCCCCCCCCC"
    private let delta = "01K8N0DDDDDDDDDDDDDDDDDDDD"
    private let alphaTwo = "01K8N0EEEEEEEEEEEEEEEEEEEE"
    /// A project whose name fills an iPhone SE's card.
    private let longProject = "platform-reconciliation-service"
    private var folders: [String] { ["alpha", "bravo", "charlie", "delta", longProject] }

    /// A run in `/tmp/<project>`, started at `created` and last touched at
    /// `updated` (hh:mm), held on a card when `waiting`, live unless `ended`,
    /// as the computer sends it.
    private func runJSON(
        _ uid: String, _ project: String, updated: String, created: String = "09:00", waiting: Bool = false,
        ended: Bool = false
    ) -> String {
        """
        {"session_uid":"\(uid)","session_id":"cc-1","tmux_session":"cc-1",
         "cwd":"/tmp/\(project)","project_label":"\(project)","lifecycle":"\(ended ? "exited" : "live")",
         "link":"attached","last_seq":0,"created_at":"2026-11-08T\(created):00Z",
         "updated_at":"2026-11-08T\(updated):00Z",
         "blocked_on":\(waiting ? "[\"toolu_x\"]" : "[]")}
        """
    }

    private func run(
        _ uid: String, _ project: String, updated: String, created: String = "09:00", waiting: Bool = false,
        ended: Bool = false
    ) throws -> SessionSummary {
        try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                runJSON(uid, project, updated: updated, created: created, waiting: waiting, ended: ended).utf8))
    }

    /// One connected computer, drawn on an iPhone 17 Pro's screen, or as
    /// `width` × `height` at `type`.
    private func fleet(
        width: CGFloat = 402, height: CGFloat = 874, type: DynamicTypeSize = .large
    ) throws -> (AppModel, UIWindow) {
        let (model, window) = try drawn(
            EventCache(root: temporaryRoot()), width: width, height: height, type: type)
        model.connection.simulateConnectedForTesting()
        return (model, window)
    }

    private func temporaryRoot() -> URL {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("fleet-order-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        return root
    }

    /// A computer's model over `cache`, drawn on an iPhone 17 Pro's screen or
    /// as `width` × `height` at `type`. The computer can answer every card, as a
    /// real one does (`can_approve_reliably`), so a run shows no observe badge
    /// until something takes control away.
    private func drawn(
        _ cache: EventCache, width: CGFloat = 402, height: CGFloat = 874, type: DynamicTypeSize = .large
    ) throws -> (AppModel, UIWindow) {
        let model = AppModel(cache: cache)
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(canApproveReliably: true, extra: ["session_uid": .bool(true)]), minor: 7)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: width, height: height))
        window.rootViewController = UIHostingController(
            rootView: FleetView().environment(\.dynamicTypeSize, type).environment(model)
                .environment(Machines(holding: model)))
        window.isHidden = false
        addTeardownBlock { @MainActor in window.isHidden = true }
        return (model, window)
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

    /// The cards' projects top to bottom, each project's run row on screen, and
    /// everything the screen says.
    private func screen(
        _ window: UIWindow
    ) async throws -> (cards: [String], rows: [String: CGRect], spoken: String) {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        window.rootViewController?.view.layoutIfNeeded()
        try await Task.sleep(for: .seconds(2))
        let found = elements(in: window)
        let cards = found.filter { $0.accessibilityTraits.contains(.header) }
            .sorted { $0.accessibilityFrame.minY < $1.accessibilityFrame.minY }
            .compactMap { card in folders.first { (card.accessibilityLabel ?? "").contains("/tmp/\($0)") } }
        var rows: [String: CGRect] = [:]
        for element in found where !element.accessibilityTraits.contains(.header) {
            let label = element.accessibilityLabel ?? ""
            if let project = folders.first(where: { label.hasPrefix("\($0), ") }) {
                rows[project] = element.accessibilityFrame
            }
        }
        return (cards, rows, found.map { $0.accessibilityLabel ?? "" }.joined(separator: " | "))
    }

    private func assertNoRunMoved(
        _ before: [String: CGRect], _ after: [String: CGRect], _ context: String = "",
        file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertFalse(before.isEmpty, "\(context)no run drawn", file: file, line: line)
        for (project, frame) in before {
            XCTAssertEqual(
                after[project]?.minY ?? .infinity, frame.minY, accuracy: 0.5,
                "\(context)\(project) moved: \(frame) → \(after[project].map { "\($0)" } ?? "gone")",
                file: file, line: line)
        }
    }

    private typealias SetAccessibilityEnabled = @convention(c) (Int32) -> Void

    private func setAccessibility(_ enabled: Bool) throws {
        let library = try XCTUnwrap(dlopen("/usr/lib/libAccessibility.dylib", RTLD_NOW))
        let symbol = try XCTUnwrap(dlsym(library, "_AXSApplicationAccessibilitySetEnabled"))
        unsafeBitCast(symbol, to: SetAccessibilityEnabled.self)(enabled ? 1 : 0)
    }

    /// A run that starts waiting, and another that acts after everyone else,
    /// both keep their cards where they were.
    func testAStatusOrAnEventNeverMovesACard() async throws {
        let (model, window) = try fleet()
        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
            try run(charlie, "charlie", updated: "10:01"),
        ])
        let before = try await screen(window)
        XCTAssertEqual(before.cards, ["alpha", "bravo", "charlie"])

        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:09"),
            try run(charlie, "charlie", updated: "10:05", waiting: true),
        ])
        let after = try await screen(window)
        XCTAssertEqual(after.cards, before.cards)
    }

    /// The point a finger is on holds the same run before and after the fleet
    /// changes, so a tap opens what it was aimed at.
    func testTheRunUnderAFingerIsStillThereAfterTheFleetChanges() async throws {
        let (model, window) = try fleet()
        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
            try run(charlie, "charlie", updated: "10:01"),
        ])
        let before = try await screen(window)
        let aim = try XCTUnwrap(before.rows["charlie"], "\(before.rows)")
        let finger = CGPoint(x: aim.midX, y: aim.midY)

        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:09"),
            try run(charlie, "charlie", updated: "10:05", waiting: true),
        ])
        let after = try await screen(window)
        let under = after.rows.first { $0.value.contains(finger) }?.key
        XCTAssertEqual(under, "charlie", "\(after.rows)")
    }

    /// Taps the header's pill that says `words`, as VoiceOver would.
    private func letIn(_ window: UIWindow, _ words: String) throws {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let pill = try XCTUnwrap(
            elements(in: window).first { $0.accessibilityLabel == words }, "no pill saying \(words)")
        XCTAssertTrue(pill.accessibilityActivate())
    }

    /// A project that arrives while the list is in sight waits, named by the
    /// header's pill, and moves no card; let in, it goes on top.
    func testANewProjectWaitsForTheOwnerThenArrivesOnTop() async throws {
        let (model, window) = try fleet()
        let three = [
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
            try run(charlie, "charlie", updated: "10:01"),
        ]
        model.adoptSummariesForTesting(three)
        let before = try await screen(window)
        XCTAssertEqual(before.cards, ["alpha", "bravo", "charlie"])

        model.adoptSummariesForTesting(three + [try run(delta, "delta", updated: "09:30")])
        let held = try await screen(window)
        XCTAssertEqual(held.cards, before.cards)
        assertNoRunMoved(before.rows, held.rows)
        XCTAssertTrue(held.spoken.contains("1 new project"), held.spoken)

        try letIn(window, "1 new project")
        let arrived = try await screen(window)
        XCTAssertEqual(arrived.cards, ["delta", "alpha", "bravo", "charlie"])
        XCTAssertFalse(arrived.spoken.contains("new project"), arrived.spoken)
    }

    /// A project whose last run has gone keeps its card, and every card its
    /// place, until the owner lets it go; then the rest close up in order.
    func testAProjectThatEndsStaysUntilTheOwnerLetsItGo() async throws {
        let (model, window) = try fleet()
        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
            try run(charlie, "charlie", updated: "10:01"),
        ])
        let before = try await screen(window)
        XCTAssertEqual(before.cards, ["alpha", "bravo", "charlie"])

        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(charlie, "charlie", updated: "10:01"),
        ])
        let held = try await screen(window)
        XCTAssertEqual(held.cards, before.cards)
        assertNoRunMoved(before.rows, held.rows)
        XCTAssertTrue(held.spoken.contains("1 ended"), held.spoken)

        try letIn(window, "1 ended")
        let left = try await screen(window)
        XCTAssertEqual(left.cards, ["alpha", "charlie"])
    }

    /// The first decision to arrive moves no card: the header above the cards is
    /// as tall with a count waiting as without one.
    func testTheFirstDecisionToArriveMovesNoCard() async throws {
        let (model, window) = try fleet()
        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
        ])
        let before = try await screen(window)
        XCTAssertFalse(before.spoken.contains("waiting for you"), before.spoken)

        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"),
            try run(bravo, "bravo", updated: "10:02", waiting: true),
        ])
        let after = try await screen(window)
        XCTAssertTrue(after.spoken.contains("1 waiting for you"), after.spoken)
        assertNoRunMoved(before.rows, after.rows)
    }

    /// A launch shows the fleet saved on the phone, with a banner that says how
    /// old it is, then hears from the computer: the banner goes and no card moves.
    func testAFleetReadOffThePhoneGoingLiveMovesNoCard() async throws {
        let cache = EventCache(root: temporaryRoot())
        let saved = [try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02")]
        await cache.saveFleet(saved)
        let (model, window) = try drawn(cache)
        await model.loadCachedFleetForTesting()
        // Active, as at launch, so the model's clock runs and the banner is
        // earned once the launch's grace has passed.
        model.scenePhaseChanged(to: .active)
        try await Task.sleep(for: .seconds(FleetFreshness.launchGrace + 1))
        let before = try await screen(window)
        XCTAssertTrue(before.spoken.localizedCaseInsensitiveContains("last known state"), before.spoken)

        model.connection.simulateConnectedForTesting()
        let live = [runJSON(alpha, "alpha", updated: "10:03"), runJSON(bravo, "bravo", updated: "10:02")]
        let frame = #"{"type":"sessions","sessions":["# + live.joined(separator: ",") + "]}"
        model.connection.injectForTesting(try JSONDecoder().decode(ServerMessage.self, from: Data(frame.utf8)))
        let after = try await screen(window)
        XCTAssertFalse(after.spoken.localizedCaseInsensitiveContains("last known state"), after.spoken)
        assertNoRunMoved(before.rows, after.rows)
    }

    /// The screen as a picture, kept with the test's results. Taken on the
    /// app's screen: outside a scene the window is never on one, and its
    /// layers kept the screen as it was first drawn. Being put there moves
    /// what the window measures afterwards, so a test photographs last.
    private func photograph(_ window: UIWindow, _ name: String) throws {
        window.windowScene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        defer { window.windowScene = nil }
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    /// A run that ends inside a card the list holds keeps its line: on a
    /// computer that can answer every card, ending is what turns a run observe
    /// only, and its badge must not add a line that moves the cards below.
    func testARunEndingInsideAHeldCardMovesNoCard() async throws {
        let (model, window) = try fleet()
        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
            try run(charlie, "charlie", updated: "10:01"),
        ])
        let before = try await screen(window)
        XCTAssertFalse(before.spoken.contains("observe only"), before.spoken)

        model.adoptSummariesForTesting([
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02", ended: true),
            try run(charlie, "charlie", updated: "10:01"),
        ])
        let after = try await screen(window)
        XCTAssertTrue(after.spoken.contains("Ended"), after.spoken)
        XCTAssertEqual(after.cards, before.cards)
        assertNoRunMoved(before.rows, after.rows)
        try photograph(window, "held-ended-run--L")
    }

    /// The header is as tall whatever it says: the pill empty, naming each kind
    /// of change alone and all three together, with nothing waiting and with a
    /// count waiting, on an iPhone SE at the default size and at the largest
    /// reading size. Measured where it matters, at the first card's top.
    func testTheHeaderIsAsTallWhateverItSays() async throws {
        let changes: [(String, Bool, Bool, Bool)] = [
            // (pill, a new project, a new agent, a run gone)
            ("nothing held", false, false, false), ("1 new project", true, false, false),
            ("1 new agent", false, true, false), ("1 ended", false, false, true),
            ("1 new project · 1 new agent · 1 ended", true, true, true),
        ]
        for (type, size) in [(DynamicTypeSize.large, "L"), (.xxxLarge, "xxxL")] {
            var tops: [String: CGFloat] = [:]
            for waiting in [false, true] {
                for (pill, project, agent, gone) in changes {
                    let (model, window) = try fleet(width: 375, height: 667, type: type)
                    let base = [
                        try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
                        try run(charlie, "charlie", updated: "10:01", waiting: waiting),
                    ]
                    model.adoptSummariesForTesting(base)
                    _ = try await screen(window)
                    var next = base.filter { !gone || $0.cwd != "/tmp/bravo" }
                    if agent { next.append(try run(alphaTwo, "alpha", updated: "10:04")) }
                    if project { next.append(try run(delta, "delta", updated: "09:30")) }
                    model.adoptSummariesForTesting(next)
                    let after = try await screen(window)
                    if pill != "nothing held" {
                        XCTAssertTrue(after.spoken.contains(pill), "\(pill) at \(size): \(after.spoken)")
                    }
                    // The top card's first run: a waiting run puts charlie's
                    // card first on first sight, so the top is not alpha's.
                    tops["\(pill)\(waiting ? ", 1 waiting" : "")"] = after.rows.values.map(\.minY).min()
                    if waiting == false, project, agent, gone, type == .large {
                        try photograph(window, "pill-three-kinds-SE--L")
                    }
                }
            }
            XCTAssertEqual(tops.count, 2 * changes.count, "\(tops)")
            XCTAssertEqual(
                tops.values.max() ?? 0, tops.values.min() ?? 1, accuracy: 0.5,
                "the first card's top at \(size): \(tops.sorted { $0.key < $1.key })")
        }
    }

    /// A run held back from a card adds no line to the runs the card shows: runs
    /// in one folder are told apart by when they started only among the runs
    /// their card holds.
    func testARunHeldBackFromACardAddsNoLineToIt() async throws {
        let (model, window) = try fleet()
        let base = [
            try run(alpha, "alpha", updated: "10:03"), try run(bravo, "bravo", updated: "10:02"),
            try run(charlie, "charlie", updated: "10:01"),
        ]
        model.adoptSummariesForTesting(base)
        let before = try await screen(window)

        model.adoptSummariesForTesting(base + [try run(alphaTwo, "alpha", updated: "10:04", created: "09:30")])
        let after = try await screen(window)
        XCTAssertTrue(after.spoken.contains("1 new agent"), after.spoken)
        XCTAssertEqual(after.cards, before.cards)
        assertNoRunMoved(before.rows, after.rows)
    }

    /// A card's header is as tall whatever its computer's link says: the word
    /// beside the computer's name moves no card, for short and long project
    /// names and computer names on an iPhone SE. Each state in a window of its
    /// own, photographed after it is measured: the photograph puts the window
    /// on the app's screen, which moves what is measured after it.
    func testACardsHeaderIsAsTallWhateverItsLinkSays() async throws {
        let phases: [(String, DaemonConnection.Phase?)] = [
            ("connected", nil), ("Connecting", .connecting),
            ("Offline", .waiting(until: Date(timeIntervalSinceNow: 60), reason: "The computer did not answer.")),
            ("Not paired", .failed(reason: "This iPhone is not paired with it.")),
        ]
        for computer in ["Mac mini", "Design Studio MacBook Pro"] {
            for project in ["bravo", longProject] {
                for (word, phase) in phases {
                    let (model, window) = try fleet(width: 375, height: 667)
                    model.connection.simulateCapabilitiesForTesting(
                        Capabilities(canApproveReliably: true, extra: ["session_uid": .bool(true)]), minor: 7,
                        machineName: computer)
                    model.adoptSummariesForTesting([
                        try run(alpha, project, updated: "10:03"), try run(charlie, "charlie", updated: "10:02"),
                    ])
                    let live = try await screen(window)
                    if let phase { model.connection.setPhaseForTesting(phase) }
                    let after = try await screen(window)
                    XCTAssertTrue(after.spoken.contains("On \(computer), \(word)"), after.spoken)
                    assertNoRunMoved(live.rows, after.rows, "\(computer), \(project), \(word): ")
                    if computer != "Mac mini", project == longProject {
                        try photograph(window, "card-header-\(word.lowercased().replacingOccurrences(of: " ", with: "-"))-SE--L")
                    }
                }
            }
        }
    }
}
