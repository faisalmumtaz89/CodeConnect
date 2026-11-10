import SwiftUI
import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The rule that keeps the home screen still under a finger: a card's height
/// depends only on how many runs it holds, never on what they are doing or
/// what this iPhone may do about them. A card is its header, one line whatever
/// its computer's link, and its runs, so the rule holds exactly when a run is
/// as tall in every state the fleet can put it in.
///
/// Every state, crossed: each status (waiting at each risk, on one card and on
/// two), each capability, what the run last did (a tool and its argument, a
/// sentence, nothing), off the disk or live, and for Codex the Stop control
/// absent, offered, in flight, cooling down and settled with each outcome; on
/// an iPhone SE's card at the default size
/// and at the largest reading size. A run's agent is not a state — a run is
/// Claude Code or Codex for life — so each agent has a height of its own.
@MainActor
final class FleetRowHeightTests: XCTestCase {

    /// The width of a card's content on an iPhone SE: the screen less the
    /// card's margins.
    private let width: CGFloat = 375 - 32

    private func summary(_ agent: String, ended: Bool, waiting: Int) throws -> SessionSummary {
        try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"01K8N0AAAAAAAAAAAAAAAAAAAA","session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/tmp/alpha","project_label":"alpha","agent":"\(agent)",
                 "lifecycle":"\(ended ? "exited" : "live")","link":"attached","last_seq":0,
                 "created_at":"2026-11-08T09:00:00Z","updated_at":"2026-11-08T10:00:00Z",
                 "blocked_on":\((0..<waiting).map { "\"toolu_\($0)\"" })}
                """.utf8))
    }

    private struct State {
        let status: FleetStatus
        let risk: RiskClass?
        let cards: Int
    }

    private var states: [State] {
        var all = [State]()
        for risk in [RiskClass.low, .medium, .high] {
            for cards in [1, 2] { all.append(State(status: .blocked, risk: risk, cards: cards)) }
        }
        for status in [FleetStatus.failed, .doneUnreviewed, .running, .idle, .ended] {
            all.append(State(status: status, risk: nil, cards: 0))
        }
        return all
    }

    private let capabilities: [CapabilityBadge] = [
        .control, .observe(reason: "This session has ended."), .unknown(reason: "Not connected."),
    ]

    private let activities: [(FleetActivity?, String)] = [
        (FleetActivity(tool: "Bash", argument: "git push --force origin main"), "Bash git push --force origin main"),
        (nil, "Finished the change and ran the tests."),
        (nil, ""),
    ]

    /// A Codex run's Stop, as a row can draw it: absent, offered, in flight,
    /// cooling down after a refusal, or settled with each kind of outcome.
    /// The outcome comes after the owner's tap, but a late answer and the
    /// cooldown's end come with no touch at all.
    private struct CodexStop: CustomStringConvertible {
        let name: String
        let offered: Bool
        let blockedReason: String?
        let activity: CodexControls.Activity<InterruptResult>
        var description: String { name }
    }

    /// The daemon's own sentences, from its catalogue
    /// (`fixtures/codex/refusal-sentences.json`): its link-state refusal, which
    /// greys Stop for ten seconds, and its longest stop sentence.
    private let linkRefusal =
        "the computer has lost its control link to the Codex session and is reconnecting, so nothing was sent; "
        + "try again shortly, or stop the turn at the computer"
    private let longestStopSentence =
        "the link's connection to Codex ended while this request to stop the turn was in flight, and the computer "
        + "could not record that either, so it cannot say what became of it. Check the computer."
    /// What the fleet draws for a greyed Stop (`FleetView.stopBlockedReason`).
    private let coolingDown = "Try again in a moment."

    private var codexStops: [CodexStop] {
        [
            CodexStop(name: "absent", offered: false, blockedReason: nil, activity: .idle),
            CodexStop(name: "offered", offered: true, blockedReason: nil, activity: .idle),
            CodexStop(name: "stopping", offered: true, blockedReason: nil, activity: .inFlight),
            CodexStop(name: "cooling down", offered: true, blockedReason: coolingDown, activity: .idle),
            CodexStop(name: "stopped", offered: false, blockedReason: nil, activity: .settled(.aborted(turnID: "t-1"))),
            CodexStop(
                name: "refused", offered: true, blockedReason: nil,
                activity: .settled(.rejected(reason: longestStopSentence))),
            CodexStop(
                name: "refused, cooling down", offered: true, blockedReason: coolingDown,
                activity: .settled(.rejected(reason: linkRefusal))),
            CodexStop(
                name: "already stopped", offered: false, blockedReason: nil,
                activity: .settled(.duplicate(turnID: "t-1"))),
            CodexStop(
                name: "outcome unknown", offered: false, blockedReason: nil,
                activity: .settled(.indeterminate(reason: longestStopSentence))),
            CodexStop(
                name: "unclear", offered: true, blockedReason: nil, activity: .settled(.unknown(status: "paused"))),
            CodexStop(
                name: "not sent", offered: true, blockedReason: nil,
                activity: .notSent(CodexLinkState.offline.blockedReason(for: .stop) ?? "")),
            CodexStop(
                name: "no answer", offered: false, blockedReason: nil,
                activity: .sentNoAnswer(AppModel.sentNoAnswerSentence)),
        ]
    }

    /// How tall the run's line is, drawn as a project card draws it.
    private func height(_ row: FleetRow, blocked: BlockedCard?, stop: CodexStop, type: DynamicTypeSize) -> CGFloat {
        // A card's runs answer to `control`, so a run shows its capability when
        // it is settled and cannot act: the card's own rule (`showsCapability`).
        let content = FleetRowCell.Content(
            row: row, waitingSince: Date(timeIntervalSinceNow: -64), blocked: blocked,
            showsCapability: row.capability.isSettled && !row.capability.canAct,
            separator: true, titledByAgent: true, removalOffered: false, selection: nil,
            stopOffered: stop.offered, stopBlockedReason: stop.blockedReason, stop: stop.activity)
        let view = FleetRowCell(
            content: content, remove: { nil }, confirm: { _ in }, coordinator: CCSwipeCloseCoordinator(),
            onOpenDiff: {}, onMarkReviewed: {}, onStop: {}, onTap: {}
        )
        .environment(\.dynamicTypeSize, type)
        let host = UIHostingController(rootView: view)
        return host.sizeThatFits(in: CGSize(width: width, height: .greatestFiniteMagnitude)).height
    }

    func testARunIsAsTallInEveryStateItCanBeIn() throws {
        for type in [DynamicTypeSize.large, .xxxLarge] {
            for agent in ["claude", "codex"] {
                let stops = agent == "codex" ? codexStops : [codexStops[0]]
                var heights: [String: CGFloat] = [:]
                for state in states {
                    let summary = try summary(agent, ended: state.status == .ended, waiting: state.cards)
                    for capability in capabilities {
                        for (activity, subtitle) in activities {
                            for cached in [false, true] {
                                for stop in stops {
                                    let row = FleetRow(
                                        summary: summary, status: state.status, subtitle: subtitle,
                                        activity: activity, label: RunLabel(project: "alpha", qualifier: nil),
                                        capability: capability, blockedCount: state.cards,
                                        lastEventAt: Date(timeIntervalSinceNow: -64),
                                        cachedAt: cached ? Date(timeIntervalSinceNow: -360) : nil,
                                        machineID: nil, machineName: nil)
                                    let blocked = state.risk.map {
                                        BlockedCard(
                                            activity: FleetActivity(tool: "Bash", argument: "git push --force origin main"),
                                            risk: $0, truncation: .head)
                                    }
                                    let name =
                                        "\(state.status)\(state.risk.map { " \($0)" } ?? "") ×\(state.cards), "
                                        + "\(capability), \(subtitle.isEmpty && activity == nil ? "no line" : activity == nil ? "sentence" : "tool"), "
                                        + "\(cached ? "cached" : "live"), stop \(stop)"
                                    heights[name] = height(row, blocked: blocked, stop: stop, type: type)
                                }
                            }
                        }
                    }
                }
                // Every height the run took, how many states took it, and the
                // first of them by name.
                let byHeight = Dictionary(grouping: heights, by: { ($0.value * 3).rounded() / 3 })
                let report = byHeight.keys.sorted().map { height in
                    let names = byHeight[height]!.map(\.key).sorted()
                    return "\(height) pt ×\(names.count), e.g. \(names[0])"
                }
                XCTAssertEqual(byHeight.count, 1, "\(agent) at \(type): \(report.joined(separator: " | "))")
            }
        }
    }

    /// A run that stops running keeps its title where it was: in a card, a run
    /// with no dot holds the dot's column. Running and failed draw the same
    /// title and the same line under it, so the two pictures are the same right
    /// of the dot's column and left of the clock.
    func testARunsTitleStaysWhereItWasWhenItStopsRunning() throws {
        let summary = try summary("claude", ended: false, waiting: 0)
        let pictures = try [FleetStatus.running, .failed].map { status in
            try picture(
                FleetRow(
                    summary: summary, status: status, subtitle: "Finished the change and ran the tests.",
                    activity: nil, label: RunLabel(project: "alpha", qualifier: nil), capability: .control,
                    blockedCount: 0, lastEventAt: Date(timeIntervalSinceNow: -64), cachedAt: nil,
                    machineID: nil, machineName: nil))
        }
        let reds = Set(stride(from: 0, to: pictures[0].count, by: 4).map { pictures[0][$0] })
        XCTAssertGreaterThan(reds.count, 2, "no title was drawn")
        XCTAssertEqual(pictures[0].count, pictures[1].count)
        XCTAssertTrue(pictures[0] == pictures[1], "the failed run's title is not where the running run's is")
    }

    /// The run as a card draws it, at one pixel a point, from the dot's column
    /// to the middle of the card: its RGBA bytes.
    private func picture(_ row: FleetRow) throws -> [UInt8] {
        let content = FleetRowCell.Content(
            row: row, waitingSince: Date(timeIntervalSinceNow: -64), blocked: nil, showsCapability: false,
            separator: true, titledByAgent: true, removalOffered: false, selection: nil,
            stopOffered: false, stopBlockedReason: nil, stop: .idle)
        let renderer = ImageRenderer(
            content: FleetRowCell(
                content: content, remove: { nil }, confirm: { _ in }, coordinator: CCSwipeCloseCoordinator(),
                onOpenDiff: {}, onMarkReviewed: {}, onStop: {}, onTap: {}
            )
            .environment(\.dynamicTypeSize, .large)
            .frame(width: width))
        renderer.scale = 1
        let image = try XCTUnwrap(renderer.cgImage)
        let left = Int(CC.space.md + CC.size.dot)
        let crop = try XCTUnwrap(
            image.cropping(to: CGRect(x: left, y: 0, width: Int(width / 2) - left, height: image.height)))
        var bytes = [UInt8](repeating: 0, count: crop.width * crop.height * 4)
        bytes.withUnsafeMutableBytes { raw in
            CGContext(
                data: raw.baseAddress, width: crop.width, height: crop.height, bitsPerComponent: 8,
                bytesPerRow: crop.width * 4, space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            )?.draw(crop, in: CGRect(x: 0, y: 0, width: crop.width, height: crop.height))
        }
        return bytes
    }
}
