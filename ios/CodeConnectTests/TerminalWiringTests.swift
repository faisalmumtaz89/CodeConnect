import SwiftTerm
import SwiftUI
import UIKit
import XCTest

@testable import CodeConnect

/// What the terminal tab *renders*, measured on a real screen.
///
/// The carrier's own rules are covered next door in `TerminalCarrierTests`. This
/// file covers the wiring between those rules and the pixels, which is the half
/// that stays green when the view is put back the way it was: a carrier that
/// knows whose terminal it is holding buys nothing if the screen renders a phase
/// it never asked for, and a byte count that cannot saturate buys nothing if the
/// clock on screen is following a different number.
///
/// **Read off the render, not off the accessibility tree.** Measured here: a
/// hosted SwiftUI view in a process with no assistive technology attached
/// publishes no accessibility elements at all — every node in the hierarchy
/// answers `accessibilityElementCount() == 0` — so the labels and traits
/// `CCKeyCap` and `SnapshotFrame` declare are unreachable from a unit test. The
/// screen itself is not, and the screen is what these claims are about.
@MainActor
final class TerminalWiringTests: XCTestCase {

    // MARK: - The modifier the cap has to follow

    /// SwiftTerm owns `controlModifier` and clears it the moment it has applied
    /// it to one character, posting `terminalViewControlModifierReset` as it
    /// does. The cap is re-read from the view at that moment and never written
    /// from a guess about what it holds.
    ///
    /// Without that observer the cap stays lit after `^C`, and the `d` typed
    /// next for `^D` reaches the agent's pane as a literal `d` underneath a key
    /// still claiming Control is held — the wrong-character failure the row
    /// exists to prevent, and one that leaves no trace on this side.
    ///
    /// The two references are the same row with the modifier *pinned* rather
    /// than followed, so what the live row is compared against is a rendering of
    /// the answer rather than a threshold. Held is asserted first: a row that
    /// never rendered would satisfy the cleared assertion on its own.
    func testTheControlCapGoesDarkWhenSwiftTermClearsTheModifier() async throws {
        let scene = try windowScene()
        let size = CGSize(width: 393, height: 80)
        // A real emulator, because the behaviour under test is SwiftTerm's: the
        // notification comes from its own `didSet`, on the true-to-false edge.
        let emulator = SwiftTerm.TerminalView(frame: CGRect(x: 0, y: 0, width: 320, height: 200))
        emulator.controlModifier = true

        func row(_ isControlActive: @escaping () -> Bool) -> TerminalKeyRow {
            TerminalKeyRow(
                onEscape: {}, onTab: {}, onControl: {}, onInterrupt: {}, onArrow: { _ in },
                isControlActive: isControlActive)
        }
        let held = Host(row { true }, scene: scene, size: size)
        let cleared = Host(row { false }, scene: scene, size: size)
        let live = Host(
            row { [weak emulator] in emulator?.controlModifier ?? false },
            scene: scene, size: size)
        defer { live.dismantle(); cleared.dismantle(); held.dismantle() }
        await settle()

        let lit = pixels(of: held.view, size: size)
        let dark = pixels(of: cleared.view, size: size)
        // The latch is the whole difference between the two references — one
        // 40pt cap filled `accent` against one filled `surfaceOverlay`. If they
        // rendered alike, nothing below could mean anything.
        XCTAssertGreaterThan(
            differingPixels(lit, dark), 1_000,
            "the held and cleared references render alike, so the row draws no latch")

        XCTAssertEqual(
            differingPixels(pixels(of: live.view, size: size), lit), 0,
            "a row over a held modifier must render exactly as one that is always held")

        // The edge SwiftTerm posts on, and the only thing this test does to the
        // row: everything after is the row reading the modifier back.
        emulator.controlModifier = false
        await settle()

        XCTAssertEqual(
            differingPixels(pixels(of: live.view, size: size), dark), 0,
            "the cap is still lit after SwiftTerm cleared the modifier")
    }

    // MARK: - The clock that cannot freeze

    /// The transcript is capped, so past the cap its count is a constant while
    /// output is still arriving. A clock driven from that count stops at the
    /// second the buffer filled, and an hour later the strip and the snapshot
    /// stamp both state that a dead terminal was last live then — the one lie
    /// the strip exists to prevent.
    ///
    /// Two screens, identical in every respect except what reaches them: one
    /// keeps receiving output after the cap, one does not. The screen that kept
    /// receiving must name a later second than the screen that stopped. Driven
    /// from `transcript.count` they name the same one, because that count stops
    /// moving at exactly the point this test starts measuring.
    ///
    /// Compared above the pane: that band carries the strip's `last output` line
    /// and the snapshot frame's stamp, and leaves out the panes themselves —
    /// which are deliberately not fed alike, that being the premise.
    func testTheFreshnessStampAdvancesAfterTheTranscriptHasStoppedGrowing() async throws {
        let scene = try windowScene()
        let size = CGSize(width: 393, height: 852)
        let live = StubbedDaemon()
        let stopped = StubbedDaemon()
        try await live.attach(to: "A")
        try await stopped.attach(to: "A")

        let liveHost = Host(tab(over: live), scene: scene, size: size)
        let stoppedHost = Host(tab(over: stopped), scene: scene, size: size)
        defer { stoppedHost.dismantle(); liveHost.dismantle() }
        await settle()

        // Both screens stamp whole seconds, and this pair of stamps is taken
        // microseconds apart. Mid-second, so they cannot land either side of a
        // tick and name different seconds for no reason at all.
        let intoSecond = Date().timeIntervalSince1970.truncatingRemainder(dividingBy: 1)
        try await Task.sleep(for: .seconds(1.5 - intoSecond))

        // A chunk bound's worth at a time — larger frames are a protocol fault
        // the carrier now ends the terminal for — and enough of them to pass
        // the 256 KiB cap: from the first trim on, every further chunk leaves
        // the transcript at exactly the trimmed size.
        let chunk = [UInt8](repeating: UInt8(ascii: "a"), count: Wire.Terminal.maxChunkBytes)
        for _ in 0..<20 {
            try live.deliverOutput(chunk)
            try stopped.deliverOutput(chunk)
        }
        await settle()
        let filled = live.model.terminal.transcript.count
        let counted = live.model.terminal.totalOutputBytes

        // Past the carrier's own throttle, which is one sample a second — the
        // clock shows whole seconds, and stamping per chunk would invalidate
        // every observer of the carrier to redraw the same string.
        await settle(for: 1.4)
        for _ in 0..<12 { try live.deliverOutput(chunk) }
        await settle()

        // The premise, stated where it is measured: across the second batch the
        // transcript's count is a constant and the received count is not.
        XCTAssertEqual(
            live.model.terminal.transcript.count, filled,
            "the transcript is not capped, so this test measures nothing")
        XCTAssertGreaterThan(live.model.terminal.totalOutputBytes, counted)

        try live.close(code: "session_exited", reason: "the session ended")
        try stopped.close(code: "session_exited", reason: "the session ended")
        await settle()

        // Both screens reached the snapshot state. Without this the comparison
        // below could be satisfied by two screens that never drew a terminal.
        let livePane = try XCTUnwrap(panes(in: liveHost.view).first, "no pane on the live screen")
        let stoppedPane = try XCTUnwrap(
            panes(in: stoppedHost.view).first, "no pane on the stopped screen")
        let band = min(
            livePane.convert(livePane.bounds, to: liveHost.view).minY,
            stoppedPane.convert(stoppedPane.bounds, to: stoppedHost.view).minY)

        XCTAssertGreaterThan(
            differingPixels(
                pixels(of: liveHost.view, size: size, above: band),
                pixels(of: stoppedHost.view, size: size, above: band)),
            0,
            "both screens name the same second, so the clock stopped when the buffer filled")
    }

    // MARK: - Whose terminal is on screen

    /// One carrier serves the whole app, because the terminal has to survive the
    /// tab being rebuilt. A tab that renders the carrier's phase unasked draws
    /// another run's live pane under this run's label and types into it.
    ///
    /// Both standings are measured, because they fail through different guards.
    /// While the other run's terminal is open this run is `heldByAnotherRun`;
    /// once it has closed the carrier holds nothing and this run is `free` — and
    /// it is that second standing which still has a snapshot to draw.
    ///
    /// The screen for the run that *does* own the terminal is the control: it
    /// draws a pane in both standings, so the absence measured for the other run
    /// is an absence rather than a screen that failed to render.
    func testATabForAnotherRunRendersNoTerminalWhileTheCarrierHoldsThisOne() async throws {
        let scene = try windowScene()
        let size = CGSize(width: 393, height: 852)
        let daemon = StubbedDaemon()
        try await daemon.attach(to: "A")
        try daemon.deliverOutput(Array("the agent's own pane\r\n".utf8))

        let owner = Host(tab(over: daemon, run: "A"), scene: scene, size: size)
        let other = Host(tab(over: daemon, run: "B"), scene: scene, size: size)
        defer { other.dismantle(); owner.dismantle() }
        await settle()

        XCTAssertEqual(
            panes(in: owner.view).count, 1,
            "the run holding the terminal must draw one")
        XCTAssertEqual(
            panes(in: other.view).count, 0,
            "a live terminal for another run is on screen under this run's label")

        // A terminal that has ended holds nothing, so this run is no longer held
        // off — but the snapshot the other run left behind is still not this
        // run's to draw.
        try daemon.close(code: "session_exited", reason: "the session ended")
        await settle()

        XCTAssertEqual(
            panes(in: owner.view).count, 1,
            "the run that owned the terminal must keep its snapshot")
        XCTAssertEqual(
            panes(in: other.view).count, 0,
            "another run's snapshot is on screen under this run's label")
    }

    // MARK: - The pane the scrollback lives in

    /// The emulator holds the scrollback; the carrier's transcript is capped at
    /// a quarter of a megabyte, and a `cargo build` puts megabytes through a
    /// pane. So a pane rebuilt at the moment the session ends comes back
    /// holding a fraction of what the reader was looking at — and that moment
    /// is exactly when its buffer became the only copy.
    ///
    /// Written as two arms of the tab's switch, a live pane and a snapshot pane
    /// are two different views to SwiftUI, which dismantles one to build the
    /// other. Identity is therefore what is asserted, not presence: a pane is
    /// on screen either way, which is why the count next door cannot see this.
    ///
    /// The marker is pushed out of the transcript *before* the session ends,
    /// and that is the premise — asserted where it is measured. A marker a
    /// rebuilt pane could replay would prove nothing about which pane is on
    /// screen.
    func testTheEmulatorSurvivesTheSessionEnding() async throws {
        let scene = try windowScene()
        let size = CGSize(width: 393, height: 852)
        let daemon = StubbedDaemon()
        try await daemon.attach(to: "A")

        let host = Host(tab(over: daemon), scene: scene, size: size)
        defer { host.dismantle() }
        await settle()
        let live = try XCTUnwrap(panes(in: host.view).first, "no pane while the terminal is live")

        let marker = "cc-scrollback-marker"
        try daemon.deliverOutput(Array("\(marker)\r\n".utf8))
        // A carriage return and one character, over and over. This has to
        // overflow the transcript's cap without overflowing the emulator's
        // scrollback, and a quarter of a megabyte of *lines* would scroll the
        // marker away as surely as a rebuilt pane would. Overwriting the first
        // column costs no lines at all.
        // Sized to the chunk bound, because a larger frame is now a protocol
        // fault the carrier ends the terminal for.
        var chunk: [UInt8] = []
        chunk.reserveCapacity(Wire.Terminal.maxChunkBytes)
        for _ in 0..<(Wire.Terminal.maxChunkBytes / 2) { chunk.append(contentsOf: "\rx".utf8) }
        for _ in 0..<20 { try daemon.deliverOutput(chunk) }
        await settle()

        // The premise, stated where it is measured: what the pane is holding is
        // no longer anywhere else, so a pane built after this point could not
        // replay it from the carrier.
        XCTAssertFalse(
            String(decoding: daemon.model.terminal.transcript(forRun: "A"), as: UTF8.self)
                .contains(marker),
            "the transcript still holds the marker, so this test measures nothing")
        XCTAssertTrue(text(of: live).contains(marker), "the marker never reached the emulator")

        try daemon.close(code: "session_exited", reason: "the session ended")
        await settle()

        let snapshot = try XCTUnwrap(panes(in: host.view).first, "no pane after the session ended")
        XCTAssertEqual(panes(in: host.view).count, 1, "the tab left a second pane on screen")
        XCTAssertTrue(
            live === snapshot,
            "the session ending rebuilt the emulator, so everything above the transcript's cap is gone")
        XCTAssertTrue(
            text(of: snapshot).contains(marker),
            "the pane no longer holds what it was showing when the session ended")
    }

    // MARK: - The picker is not a demolition

    /// Flipping to the timeline and back must not cost the reader the pane.
    ///
    /// This is the same defect as the one above, one level up and worse: it
    /// fires on ordinary navigation while the session is still running, not
    /// only when it ends. `SessionDetailView` chose between its two surfaces
    /// with a `switch`, which puts them in one structural position — so a
    /// glance at the timeline dismantled the emulator, and what came back was
    /// whatever the carrier's 224 KiB still held.
    ///
    /// Proven the same way: one `UIView` instance, and a marker that exists
    /// nowhere but inside it.
    func testTheEmulatorSurvivesATripToTheTimelineAndBack() async throws {
        let scene = try windowScene()
        let size = CGSize(width: 393, height: 852)
        let daemon = StubbedDaemon()
        try await daemon.attach(to: "A")

        let surface = SurfaceBox(.terminal)
        let host = Host(
            SurfaceHarness(model: daemon.model, box: surface), scene: scene, size: size)
        defer { host.dismantle() }
        await settle()

        let live = try XCTUnwrap(panes(in: host.view).first, "the Terminal surface drew no pane")

        let marker = "cc-picker-marker"
        try daemon.deliverOutput(Array("\(marker)\r\n".utf8))
        // Overflows the transcript's cap without scrolling the marker out of
        // the emulator — see the session-ending test for why it overwrites one
        // column rather than emitting lines.
        var chunk: [UInt8] = []
        chunk.reserveCapacity(Wire.Terminal.maxChunkBytes)
        for _ in 0..<(Wire.Terminal.maxChunkBytes / 2) { chunk.append(contentsOf: "\rx".utf8) }
        for _ in 0..<20 { try daemon.deliverOutput(chunk) }
        await settle()

        XCTAssertFalse(
            String(decoding: daemon.model.terminal.transcript(forRun: "A"), as: UTF8.self)
                .contains(marker),
            "the transcript still holds the marker, so this test measures nothing")
        XCTAssertTrue(text(of: live).contains(marker), "the marker never reached the emulator")

        surface.value = .timeline
        await settle()
        // Kept, and closed for input: a pane nobody is looking at must not be
        // holding the keyboard.
        let hidden = try XCTUnwrap(panes(in: host.view).first as? TerminalPaneView)
        XCTAssertFalse(
            hidden.canBecomeFirstResponder, "the hidden pane can still take the keyboard")
        // And it is genuinely hidden. Without this the two assertions around it
        // would also hold for a screen that simply never moved.
        var invisible = false
        var node: UIView? = hidden
        while let current = node, !invisible {
            invisible = current.layer.opacity == 0 || current.isHidden
            node = current.superview
        }
        XCTAssertTrue(invisible, "the terminal is still drawn over the timeline")

        surface.value = .terminal
        await settle()

        let returned = try XCTUnwrap(panes(in: host.view).first, "no pane after coming back")
        XCTAssertEqual(panes(in: host.view).count, 1, "the screen left a second pane behind")
        XCTAssertTrue(
            live === returned,
            "a look at the timeline rebuilt the emulator, so the scrollback above the cap is gone")
        XCTAssertTrue(
            text(of: returned).contains(marker),
            "the pane no longer holds what it was showing before the picker moved")
        XCTAssertTrue(
            returned.canBecomeFirstResponder,
            "the pane came back closed for input")
    }

    // MARK: - A rebuilt pane does not pretend to be whole

    /// **A pane seeded from a trimmed buffer says what is missing, and the
    /// replayed bytes cannot take that back.**
    ///
    /// The navigation rebuild — out to the fleet and back — is the one path
    /// where a genuinely fresh emulator is handed the carrier's capped buffer:
    /// the whole detail screen goes with the stack, and the terminal is still
    /// attached, so no repaint arrives to replace what is replayed. Everything
    /// above the cap is gone, and drawn without a word the tail reads as the
    /// whole of what the session printed.
    ///
    /// **The tail is the adversary, and it is the whole reason these are three
    /// tests rather than one.** The notice used to be bytes fed to the emulator
    /// ahead of the replay, and the replay is arbitrary terminal control: this
    /// tail erases the screen it was written on. The test that passed for the
    /// in-band marker replayed `\rx` over and over — a tail that cannot erase
    /// or scroll anything — so it proved the marker had been *injected* and
    /// never that it could be *read*.
    ///
    /// A *reattach* is a different path and needs none of this: the confirmed
    /// attach empties the buffer, and the daemon opens every attachment by
    /// painting the pane's whole current screen. See the repaint test below.
    func testARebuiltPaneSaysWhatIsMissingWhenTheTailErasesTheScreen() async throws {
        let pair = try await trimmedPair(tail: Array("\u{1b}[2Jerased\r\n".utf8))
        defer { pair.dismantle() }

        try assertNoticeIsShown(
            pair, "an `ESC[2J` in the replayed tail wiped the pane's account of itself")
    }

    /// The same claim under a tail that switches to the alternate screen: the
    /// pane the reader is looking at is not the pane the notice was written on,
    /// and nothing brings it back. `smcup` is what every full-screen program
    /// sends on the way in — vim, less, `tmux` inside `tmux` — so this is the
    /// ordinary case and not a contrived one.
    func testARebuiltPaneSaysWhatIsMissingWhenTheTailTakesTheAlternateScreen() async throws {
        let pair = try await trimmedPair(tail: Array("\u{1b}[?1049halternate\r\n".utf8))
        defer { pair.dismantle() }

        try assertNoticeIsShown(
            pair, "the alternate screen hid the pane's account of itself")
    }

    /// And under a tail that simply prints. SwiftTerm keeps a bounded
    /// scrollback, so enough lines push anything written above them out of the
    /// emulator entirely — no control sequence required, and no way to tell
    /// afterwards that there was ever anything there.
    func testARebuiltPaneSaysWhatIsMissingWhenTheTailScrollsPastTheScrollback() async throws {
        // Ten times SwiftTerm's default retained region, in the bytes it takes
        // to say so.
        let tail = Array(String(repeating: "\r\n", count: 5_000).utf8)
        let pair = try await trimmedPair(tail: tail)
        defer { pair.dismantle() }

        try assertNoticeIsShown(
            pair, "the replayed tail scrolled the pane's account of itself away")
    }

    /// **Showing the notice must not cost the emulator the scrollback it is
    /// describing.** The notice is view state written *after* the pane has been
    /// seeded, so it appears by changing what the tab draws above the emulator.
    /// Were it an arm of a conditional rather than an optional sibling, SwiftUI
    /// would answer that change by building a second pane — and the replayed
    /// bytes, which by then are the only copy of themselves, would go with the
    /// first one.
    func testTheNoticeDoesNotCostTheEmulatorWhatItIsDescribing() async throws {
        let sentinel = "cc-tail-sentinel"
        let pair = try await trimmedPair(tail: Array("\(sentinel)\r\n".utf8))
        defer { pair.dismantle() }

        // The premise: the notice really is up on the rebuilt screen, so what
        // follows is measured across the change that draws it.
        XCTAssertGreaterThan(
            try noticeHeight(pair), 0,
            "no notice on the rebuilt screen, so this test measures nothing")

        // **Object identity, not content.** A pane SwiftUI rebuilt here would
        // be seeded from the same transcript and hold the same bytes — so
        // reading the screen back cannot tell the two apart, and only the
        // emulator's own identity can. The distinction is not academic: the
        // transcript is the *last* 224 KiB of a session, and a pane that is
        // thrown away and re-seeded once is one that can be thrown away again
        // later, when its scrollback is the only copy of itself.
        let seeded = try XCTUnwrap(pair.paneAtSeed, "the rebuild drew no pane to seed")
        let pane = try XCTUnwrap(panes(in: pair.rebuilt.view).first, "the notice drew no pane")
        XCTAssertTrue(
            seeded === pane,
            "the notice appearing rebuilt the emulator underneath it")
        XCTAssertEqual(panes(in: pair.rebuilt.view).count, 1, "the screen left a second pane behind")
        XCTAssertTrue(
            text(of: pane).contains(sentinel), "the pane is not holding what was replayed into it")
    }

    /// And a rebuild from a buffer that lost nothing claims no gap — nor does
    /// the screen that was there all along and holds every byte in its own
    /// scrollback. A notice on every pane is a notice nobody reads, and this
    /// one would be false.
    func testAPaneRebuiltFromAWholeBufferClaimsNoGap() async throws {
        let scene = try windowScene()
        let daemon = StubbedDaemon()
        try await daemon.attach(to: "A")

        let survivor = Host(tab(over: daemon), scene: scene, size: Self.screen)
        await settle()
        try daemon.deliverOutput(Array("a short session\r\n".utf8))
        await settle()
        XCTAssertFalse(
            daemon.model.terminal.transcriptIsTruncated,
            "the buffer was trimmed, so this test measures nothing")

        let rebuilt = Host(tab(over: daemon), scene: scene, size: Self.screen)
        let pair = HostPair(daemon: daemon, survivor: survivor, rebuilt: rebuilt)
        defer { pair.dismantle() }
        await settle()

        let pane = try XCTUnwrap(panes(in: rebuilt.view).first, "the rebuild drew no pane")
        XCTAssertTrue(
            text(of: pane).contains("a short session"), "the rebuild replayed nothing at all")
        XCTAssertEqual(
            try noticeHeight(pair), 0,
            "the screen claims a gap above a buffer that lost nothing")
    }

    /// **And it goes when the pane is repainted.** A confirmed attach empties
    /// the transcript, because the daemon opens every attachment by capturing
    /// the pane's whole current screen and sending it as one repaint. From that
    /// frame on the emulator is showing an authoritative screen, and a notice
    /// that outlived it would be describing a buffer that no longer exists.
    ///
    /// Driven down the path the app really takes there: the socket drops, the
    /// handshake lands, and the tab reattaches on its own. Nothing in the view
    /// watches for the clearing — an attach is rendered as the connecting card,
    /// the card is not the pane, and the emulator built on the far side of it
    /// reports its own seeding from a whole buffer. This is the test that says
    /// that chain holds end to end rather than in the one place it is written.
    func testTheNoticeGoesWhenAConfirmedAttachRepaintsThePane() async throws {
        let pair = try await trimmedPair(tail: Array("still here\r\n".utf8))
        defer { pair.dismantle() }
        XCTAssertGreaterThan(
            try noticeHeight(pair), 0,
            "no notice to clear, so this test measures nothing")

        pair.daemon.dropSocket()
        await settle()
        pair.daemon.reconnect()
        await settle()
        await pair.daemon.model.terminal.settleForTesting()
        await settle()
        XCTAssertTrue(
            pair.daemon.model.terminal.phase.isAttached,
            "the terminal never reattached, so nothing repainted the pane")
        XCTAssertFalse(
            pair.daemon.model.terminal.transcriptIsTruncated,
            "the confirmed attach left a trimmed transcript behind")

        XCTAssertEqual(
            try noticeHeight(pair), 0,
            "the notice outlived the repaint that made the pane whole again")
    }

    /// What the notice *says*, which is the half a listener gets. `CCGapMarker`
    /// speaks its own label, so this one string is both the rule's caption and
    /// the sentence VoiceOver reads — and a rule with no sentence behind it is
    /// decoration in the one channel that cannot see it.
    ///
    /// Asserted on the string rather than on the accessibility tree because
    /// there is no accessibility tree to read: a hosted SwiftUI view in a
    /// process with no assistive technology attached publishes no elements at
    /// all. See this file's own note.
    func testTheNoticeSaysWhatIsMissingRatherThanNamingALimit() {
        let notice = TerminalTabView.trimmedBufferNotice.lowercased()
        XCTAssertTrue(
            notice.contains("output"),
            "the notice never says what is missing: \(TerminalTabView.trimmedBufferNotice)")
        XCTAssertTrue(
            notice.contains("earlier") || notice.contains("before"),
            "the notice never says the missing output came first: "
                + TerminalTabView.trimmedBufferNotice)
        XCTAssertFalse(
            notice.contains("truncat") || notice.contains("buffer"),
            "the notice names this app's limit instead of the reader's loss: "
                + TerminalTabView.trimmedBufferNotice)
    }

    // MARK: - Two screens over one carrier

    /// A screen that was built before the buffer lost its head, and one built
    /// after it — over the same carrier, so everything either of them draws
    /// above the emulator is identical except the notice under test.
    ///
    /// The survivor is not scaffolding. It is the other half of the claim: the
    /// pane that lived through the trim holds every byte in its own scrollback
    /// and must say nothing, at the same moment as the pane that was handed the
    /// remains and must.
    @MainActor
    private struct HostPair {
        let daemon: StubbedDaemon
        let survivor: Host
        let rebuilt: Host
        /// The emulator the rebuilt screen had *before* the notice went up —
        /// taken at construction, which is a runloop turn earlier than the
        /// state that draws the notice is written. See
        /// `testTheNoticeDoesNotCostTheEmulatorWhatItIsDescribing`.
        var paneAtSeed: SwiftTerm.TerminalView?

        func dismantle() {
            rebuilt.dismantle()
            survivor.dismantle()
        }
    }

    /// The 393×852 screen every claim in this file is measured on.
    private static let screen = CGSize(width: 393, height: 852)

    /// Two screens over one carrier whose buffer has lost its head, the second
    /// of them seeded from what is left of it.
    ///
    /// `tail` is the last thing the daemon sends, so it is the last thing the
    /// rebuilt emulator replays — which makes it the adversary each of these
    /// tests is really about.
    private func trimmedPair(tail: [UInt8]) async throws -> HostPair {
        let scene = try windowScene()
        let daemon = StubbedDaemon()
        try await daemon.attach(to: "A")

        let survivor = Host(tab(over: daemon), scene: scene, size: Self.screen)
        await settle()

        // Past the 256 KiB cap, in frames the chunk bound allows — overwriting
        // one column rather than emitting lines, so the bulk itself neither
        // scrolls nor clears and the tail is the only thing that can.
        var chunk: [UInt8] = []
        chunk.reserveCapacity(Wire.Terminal.maxChunkBytes)
        for _ in 0..<(Wire.Terminal.maxChunkBytes / 2) { chunk.append(contentsOf: "\rx".utf8) }
        for _ in 0..<20 { try daemon.deliverOutput(chunk) }
        try daemon.deliverOutput(tail)
        await settle()
        XCTAssertTrue(
            daemon.model.terminal.transcriptIsTruncated,
            "nothing was trimmed, so this test measures nothing")

        let rebuilt = Host(tab(over: daemon), scene: scene, size: Self.screen)
        // Before the runloop turns: `Host` lays out on construction, so the
        // emulator exists and has been seeded, while the notice — written off
        // the update pass, as SwiftUI requires — has not been drawn yet.
        let seedPane = panes(in: rebuilt.view).first
        await settle()
        return HostPair(
            daemon: daemon, survivor: survivor, rebuilt: rebuilt, paneAtSeed: seedPane)
    }

    /// The height the rebuilt screen gave the notice: how much further down its
    /// emulator starts than the survivor's.
    ///
    /// **Geometry, because the notice is out of band by design.** It is not in
    /// the emulator any more, so no reading of the emulator can find it; and it
    /// is not in the accessibility tree a unit test can reach. What it is, is a
    /// row the tab lays out above the pane and nothing else ever occupies — so
    /// two screens over one carrier differ there by exactly its height.
    private func noticeHeight(_ pair: HostPair) throws -> CGFloat {
        try paneTop(in: pair.rebuilt) - paneTop(in: pair.survivor)
    }

    /// Where the emulator starts, in its screen's own coordinates.
    private func paneTop(in host: Host) throws -> CGFloat {
        let pane = try XCTUnwrap(panes(in: host.view).first, "the screen drew no pane")
        return pane.convert(pane.bounds, to: host.view).minY
    }

    /// The notice is up on the rebuilt screen, is not up on the survivor, and
    /// the room it was given is not empty space.
    private func assertNoticeIsShown(
        _ pair: HostPair, _ message: String, file: StaticString = #filePath, line: UInt = #line
    ) throws {
        let survivorTop = try paneTop(in: pair.survivor)
        let rebuiltTop = try paneTop(in: pair.rebuilt)
        guard rebuiltTop - survivorTop >= CC.size.hitTarget else {
            return XCTFail(
                "\(message) — the rebuilt screen drew nothing above its pane",
                file: file, line: line)
        }
        // And what is in that room is the marker, drawn in the one colour this
        // product paints a missing-data rule: a band that merely *exists* would
        // be satisfied by a spacer.
        XCTAssertGreaterThan(
            warningInk(in: pair.rebuilt, band: survivorTop..<rebuiltTop), 100,
            "\(message) — the room above the pane is empty",
            file: file, line: line)
        XCTAssertEqual(
            warningInk(in: pair.survivor, band: survivorTop..<rebuiltTop), 0,
            "the pane that lived through the trim claims a gap it did not suffer",
            file: file, line: line)
    }

    /// Pixels of `CC.color.warning` in a horizontal band of `host`'s render.
    ///
    /// The band is measured in points from the top of the screen and the render
    /// is at 2×, which is the only conversion here. `CC.color.warning` is the
    /// marker's ink and nothing else in this band draws in it — the band is
    /// below the strip and above the emulator, which paints its own absolute
    /// background.
    private func warningInk(in host: Host, band: Range<CGFloat>) -> Int {
        let scale = 2
        let width = Int(Self.screen.width) * scale
        let ink = UIColor(CC.color.warning).resolvedColor(with: host.view.traitCollection)
        var red: CGFloat = 0
        var green: CGFloat = 0
        var blue: CGFloat = 0
        var alpha: CGFloat = 0
        ink.getRed(&red, green: &green, blue: &blue, alpha: &alpha)
        let target = [red, green, blue].map { UInt8(clamping: Int(($0 * 255).rounded())) }

        let pixels = self.pixels(of: host.view, size: Self.screen)
        var count = 0
        for row in Int(band.lowerBound) * scale..<Int(band.upperBound) * scale {
            for column in 0..<width {
                let index = (row * width + column) * 4
                guard index + 2 < pixels.count else { continue }
                // A tolerance, because the label is antialiased text: its stems
                // reach the colour, its edges land short of it.
                let near = (0..<3).allSatisfy { channel in
                    abs(Int(pixels[index + channel]) - Int(target[channel])) <= 12
                }
                if near { count += 1 }
            }
        }
        return count
    }
    // MARK: - A snapshot is not a keyboard

    /// Every route from a keystroke to the wire ends at `TerminalCarrier.send`,
    /// which drops what it is handed for a terminal that is not attached. The
    /// pane outlives the session on purpose — it holds the scrollback — so
    /// without a refusal the reader taps the snapshot, is given a keyboard and
    /// an esc/tab/ctrl/^C row, types `ls`, and is told nothing at all while the
    /// strip says `Not live`.
    ///
    /// Measured on the pane rather than on the key row, because that is where
    /// the answer is: the row is an `inputAccessoryView`, so it is shown with
    /// the keyboard or not at all, and the keyboard is shown to a first
    /// responder or not at all.
    ///
    /// The live pane is asserted first. A pane that was never a keyboard would
    /// satisfy every assertion below on its own.
    func testASnapshotRefusesTheKeyboardItCannotSendFrom() async throws {
        let scene = try windowScene()
        let size = CGSize(width: 393, height: 852)
        let daemon = StubbedDaemon()
        try await daemon.attach(to: "A")
        try daemon.deliverOutput(Array("the agent's own pane\r\n".utf8))

        let host = Host(tab(over: daemon), scene: scene, size: size)
        defer { host.dismantle() }
        await settle()

        let live = try XCTUnwrap(panes(in: host.view).first, "no pane while the terminal is live")
        XCTAssertTrue(live.canBecomeFirstResponder, "a live pane must be able to take the keyboard")
        XCTAssertTrue(live.becomeFirstResponder(), "a live pane must take the keyboard")

        try daemon.close(code: "session_exited", reason: "the session ended")
        await settle()

        let snapshot = try XCTUnwrap(panes(in: host.view).first, "no pane after the session ended")
        XCTAssertFalse(
            snapshot.isFirstResponder,
            "the keyboard is still up over a terminal that has ended")
        XCTAssertFalse(
            snapshot.canBecomeFirstResponder,
            "a tap on the snapshot still raises a keyboard whose keystrokes are dropped")
        XCTAssertFalse(snapshot.becomeFirstResponder(), "the snapshot still takes the keyboard")
    }

    // MARK: - Coming back from a dropped socket

    /// iOS tears sockets down in the background, and the drop ends the terminal
    /// as it happens — while the link is already back to dialling. So neither
    /// the tab appearing nor the app waking can reopen it: both run at a moment
    /// when an attach is refused for want of a connection. The handshake
    /// landing is the first moment that works, and the tab has to be watching
    /// for it.
    ///
    /// The premise is asserted in the middle: the terminal really is ended, and
    /// the link really is not connected, at the point where the two foreground
    /// paths would have run.
    func testTheTerminalReattachesWhenTheConnectionComesBack() async throws {
        let scene = try windowScene()
        let size = CGSize(width: 393, height: 852)
        let daemon = StubbedDaemon()
        try await daemon.attach(to: "A")
        try daemon.deliverOutput(Array("the agent's own pane\r\n".utf8))

        let host = Host(tab(over: daemon), scene: scene, size: size)
        defer { host.dismantle() }
        await settle()
        XCTAssertEqual(panes(in: host.view).count, 1, "no live pane to drop")

        daemon.dropSocket()
        await settle()
        XCTAssertFalse(
            daemon.model.terminal.phase.isAttached, "the drop left the terminal attached")
        XCTAssertFalse(
            daemon.model.connection.phase.isConnected,
            "the link is still connected, so this test measures nothing")

        daemon.reconnect()
        await settle()
        await daemon.model.terminal.settleForTesting()
        await settle()

        XCTAssertTrue(
            daemon.model.terminal.phase.isAttached,
            "the terminal did not reattach when the connection came back")
        XCTAssertEqual(
            panes(in: host.view).count, 1, "the terminal reattached but the tab draws no pane")
    }

    // MARK: - Hosting

    private func tab(over daemon: StubbedDaemon, run: String = "A") -> some View {
        TerminalTabView(
            sessionUID: run, tmuxName: "cc-\(run.lowercased())", unhosted: false, runLabel: run
        )
        .environment(daemon.model)
    }

    private func windowScene() throws -> UIWindowScene {
        try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first,
            "no window scene: these tests have to actually render to mean anything")
    }

    /// Runs the updates the last change scheduled, then hands the runloop back.
    ///
    /// The flush is not optional. A window in a test process is never on a
    /// display, so nothing drives SwiftUI's update pass on its own: measured
    /// without it, eight separate arrivals of output produced exactly one body
    /// evaluation — at the end — which made two screens agree about a clock
    /// neither of them had actually followed.
    private func settle(for seconds: TimeInterval = 0.25) async {
        CATransaction.flush()
        try? await Task.sleep(for: .seconds(seconds))
        CATransaction.flush()
    }

    // MARK: - Reading the screen

    /// Every SwiftTerm pane in the hierarchy. The emulator is a `UIView`, so it
    /// is on screen or it is not — there is no third answer to read.
    private func panes(in view: UIView) -> [SwiftTerm.TerminalView] {
        var found: [SwiftTerm.TerminalView] = []
        func walk(_ view: UIView) {
            if let pane = view as? SwiftTerm.TerminalView { found.append(pane) }
            for subview in view.subviews { walk(subview) }
        }
        walk(view)
        return found
    }

    /// What a pane is holding, scrollback included — which is the thing the
    /// carrier's capped transcript is not a copy of.
    private func text(of pane: SwiftTerm.TerminalView) -> String {
        String(decoding: pane.getTerminal().getBufferAsData(), as: UTF8.self)
    }

    /// What `view` actually draws, as premultiplied RGBA at 2×.
    ///
    /// `above` crops to the chrome over the terminal pane, for the claims that
    /// are about what the screen *says* rather than about what the emulator was
    /// fed.
    private func pixels(of view: UIView, size: CGSize, above cut: CGFloat? = nil) -> [UInt8] {
        let scale: CGFloat = 2
        let width = Int(size.width * scale)
        let height = Int((cut ?? size.height) * scale)
        var data = [UInt8](repeating: 0, count: width * height * 4)
        data.withUnsafeMutableBytes { raw in
            guard
                let context = CGContext(
                    data: raw.baseAddress, width: width, height: height, bitsPerComponent: 8,
                    bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            else { return XCTFail("no bitmap to render into") }
            // **Flipped, because a raw bitmap context is not a view.** Its
            // origin is bottom-left, and `CALayer.render(in:)` draws in
            // whatever system it is handed — so without this the image comes
            // out upside down and `above:` crops the *bottom* of the screen
            // while claiming to crop the chrome. It did, until the truncation
            // notice was measured against it: the notice laid out 44pt under
            // the strip was found 646pt further down the bitmap.
            context.translateBy(x: 0, y: CGFloat(height))
            context.scaleBy(x: scale, y: -scale)
            view.layer.render(in: context)
        }
        return data
    }

    private func differingPixels(_ lhs: [UInt8], _ rhs: [UInt8]) -> Int {
        guard lhs.count == rhs.count else {
            XCTFail("two renders of different sizes cannot be compared")
            return -1
        }
        var count = 0
        for index in stride(from: 0, to: lhs.count, by: 4)
        where lhs[index] != rhs[index] || lhs[index + 1] != rhs[index + 1]
            || lhs[index + 2] != rhs[index + 2] {
            count += 1
        }
        return count
    }
}

// MARK: - A hosted screen

/// A view in a real, visible window, which is what makes SwiftUI run its
/// lifecycle at all: `onAppear`, `onReceive` and `onChange` belong to a view
/// that is being rendered, and a hosting controller left off screen is not.
///
/// A window off the scene rather than a bare frame: the lifecycle these tests
/// measure is the app's, and the app's runs in a scene.
@MainActor
private final class Host {
    let view: UIView
    private let window: UIWindow
    private let previousKey: UIWindow?

    init<Root: View>(_ root: Root, scene: UIWindowScene, size: CGSize) {
        let controller = UIHostingController(rootView: root)
        view = controller.view
        window = UIWindow(windowScene: scene)
        window.frame = CGRect(origin: .zero, size: size)
        previousKey = scene.windows.first { $0.isKeyWindow }
        window.rootViewController = controller
        window.makeKeyAndVisible()
        view.layoutIfNeeded()
    }

    func dismantle() {
        window.isHidden = true
        window.rootViewController = nil
        previousKey?.makeKeyAndVisible()
    }
}

/// A surface selection a test owns, so the picker can be thrown from outside
/// the screen that normally owns it.
@MainActor
@Observable
private final class SurfaceBox {
    var value: SessionDetailView.Surface
    init(_ value: SessionDetailView.Surface) { self.value = value }
}

/// Wraps the screen so the box is read where SwiftUI is watching.
///
/// The read has to happen in a `body`, plainly. Handing the screen a
/// `Binding` whose getter reads the box does not do it: the getter runs during
/// the screen's body, but observation registers what a body touches directly,
/// so the box was written, nothing invalidated, and the screen went on showing
/// its first render — measured, with every ancestor of the pane still at
/// opacity 1 after a switch to the timeline. A test that cannot move the view
/// it is asserting about proves nothing.
private struct SurfaceHarness: View {
    let model: AppModel
    let box: SurfaceBox

    var body: some View {
        let current = box.value
        SessionDetailView(
            route: SessionRoute(key: "A"),
            surfaceForTesting: Binding(get: { current }, set: { box.value = $0 })
        )
        .environment(model)
    }
}

// MARK: - A Mac that is only the far end of the wire

/// A real `AppModel` with the socket taken out and nothing else.
///
/// `AppModel` builds the real `DaemonConnection` and the real `TerminalCarrier`
/// over it, and the view reads the carrier the model owns — so what these tests
/// drive is the chain the app runs. Only the write is intercepted, by the
/// connection's own debug seam, which is what makes `.attached` reachable at
/// all: the carrier gets there by sending `terminal_attach` and being answered
/// for the id it chose, and that id is private and unguessable by design.
@MainActor
private final class StubbedDaemon {
    let model = AppModel()
    /// The id the carrier chose. Every later frame has to carry it, or the
    /// carrier is right to ignore the frame.
    private(set) var attachmentID: String?

    init() {
        // Paired, because that is the only kind of app that can be connected.
        // The tab's own connect path stops at "pair with your Mac first"
        // without one, so a screen built over an unpaired model could never
        // reach the terminal it is for.
        if let endpoint = DaemonEndpoint.parse(address: "mac.ts.net:9000", token: "device-token") {
            model.pairing.saveEphemeral(endpoint)
        } else {
            XCTFail("the stub's own address does not parse")
        }
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["terminal_pty": .bool(true)]), minor: 0)
        model.connection.sendStub = { [weak self] message in
            guard case .terminalAttach(let id, _, _, _, _) = message else { return }
            self?.attachmentID = id
            // Answered where the write happens, as the daemon answers it: the
            // carrier is waiting in `.attaching` for this id, and nothing else
            // moves it on.
            self?.model.connection.onTerminal?(
                .terminalAttached(
                    attachmentID: id, inputCredit: Wire.Terminal.initialOutputCredit,
                    maxChunkBytes: nil, maxOutstandingCredit: nil))
        }
    }

    func attach(to run: String) async throws {
        model.terminal.attach(sessionUID: run, cols: 80, rows: 24)
        // Awaited rather than slept for: the outbound chain runs when the
        // scheduler gets to it, so any fixed sleep is a bet a loaded machine
        // eventually loses.
        await model.terminal.settleForTesting()
        XCTAssertTrue(model.terminal.phase.isAttached, "the carrier never attached")
    }

    func deliverOutput(_ bytes: [UInt8]) throws {
        let id = try XCTUnwrap(attachmentID, "the carrier sent no attach")
        model.connection.onTerminal?(
            .terminalOutput(attachmentID: id, base64: Data(bytes).base64EncodedString()))
    }

    /// The daemon closing the terminal, which is what puts the tab into the
    /// snapshot state. `session_exited` on purpose: a close a retry cannot
    /// change is one the tab will not quietly reattach after.
    func close(code: String, reason: String) throws {
        let id = try XCTUnwrap(attachmentID, "the carrier sent no attach")
        model.connection.onTerminal?(
            .terminalClosed(attachmentID: id, code: code, reason: reason))
    }

    /// The socket going away, in the order `DaemonConnection.close` does it:
    /// the link falls back to dialling first, and only then is the terminal
    /// told. That order is the whole difficulty — the terminal ends while
    /// there is no connection to reopen it on, so nothing the tab can do at
    /// that moment works.
    func dropSocket() {
        model.connection.simulatePhaseForTesting(.connecting)
        model.connection.onDisconnected?()
    }

    /// The handshake landing again, which is the first moment a terminal the
    /// drop ended can be reopened.
    func reconnect() {
        model.connection.simulateConnectedForTesting()
    }
}
