import XCTest

@testable import CodeConnect

/// The dictation start/cancel race, driven through the real controller with
/// a continuation-controlled backend — the seam that exists because a
/// first-use model download opens a seconds-wide window in which the user
/// can cancel or retap, and a stale start used to publish its failure over
/// the new state and bulldoze whichever session had since been installed.
@MainActor
final class DictationRaceTests: XCTestCase {

    private typealias SessionContinuation =
        CheckedContinuation<DictationController.Session, any Error>

    /// Hands each suspended backend's continuation to the test, entirely on
    /// the main actor — an AsyncStream iterator here trips Swift 6
    /// sendability, and the gate needs none of it.
    @MainActor
    private final class BackendGate {
        private var pending: [SessionContinuation] = []
        private var waiters: [CheckedContinuation<SessionContinuation, Never>] = []

        func arrived(_ backend: SessionContinuation) {
            if waiters.isEmpty {
                pending.append(backend)
            } else {
                waiters.removeFirst().resume(returning: backend)
            }
        }

        func next() async -> SessionContinuation {
            if !pending.isEmpty { return pending.removeFirst() }
            return await withCheckedContinuation { waiters.append($0) }
        }
    }

    /// A controller whose startup suspends until the test resumes it —
    /// success or failure, at the moment of the test's choosing.
    private func suspendedController() -> (DictationController, BackendGate) {
        let controller = DictationController()
        let gate = BackendGate()
        controller.sessionBackendForTesting = {
            try await withCheckedThrowingContinuation { continuation in
                gate.arrived(continuation)
            }
        }
        return (controller, gate)
    }

    /// A cancelled start's *failure* is not news about the current state.
    func testAStaleFailureIsSilent() async {
        let (controller, backends) = suspendedController()
        let running = Task { await controller.start() }
        let backend = await backends.next()
        XCTAssertTrue(controller.isStarting)

        controller.cancel()
        XCTAssertEqual(controller.phase, .idle)
        backend.resume(
            throwing: DictationError(reason: "stale boom", needsSettings: false))
        await running.value
        XCTAssertEqual(
            controller.phase, .idle,
            "a failure from a cancelled start must not overwrite idle")
    }

    /// A cancelled start's *success* disposes what it built and installs
    /// nothing.
    func testAStaleSuccessIsDisposedNotInstalled() async {
        let (controller, backends) = suspendedController()
        let running = Task { await controller.start() }
        let backend = await backends.next()

        controller.cancel()
        let stale = DictationController.makeStubSessionForTesting()
        backend.resume(returning: stale)
        await running.value

        XCTAssertEqual(stale.disposeCount, 1, "the stale build cleans itself up")
        XCTAssertNil(controller.installedSessionForTesting)
        XCTAssertEqual(controller.phase, .idle)
    }

    /// The un-cancelled path still publishes honestly.
    func testAFreshFailureStillPublishes() async {
        let (controller, backends) = suspendedController()
        let running = Task { await controller.start() }
        let backend = await backends.next()

        backend.resume(
            throwing: DictationError(reason: "no engine", needsSettings: false))
        await running.value
        XCTAssertEqual(
            controller.phase, .failed(reason: "no engine", needsSettings: false))
    }

    /// Cancel, retap: the first start's late success must not disturb the
    /// second's — each build answers only to its own generation.
    func testANewerStartIsUntouchedByItsPredecessor() async {
        let (controller, backends) = suspendedController()
        let first = Task { await controller.start() }
        let firstBackend = await backends.next()

        controller.cancel()
        let second = Task { await controller.start() }
        let secondBackend = await backends.next()

        // The predecessor lands late, successfully — and into a generation
        // it does not own.
        let staleSession = DictationController.makeStubSessionForTesting()
        firstBackend.resume(returning: staleSession)
        await first.value
        XCTAssertEqual(staleSession.disposeCount, 1)
        XCTAssertTrue(controller.isStarting, "the successor is still starting")

        let liveSession = DictationController.makeStubSessionForTesting()
        secondBackend.resume(returning: liveSession)
        await second.value
        XCTAssertTrue(controller.isRecording)
        XCTAssertEqual(liveSession.disposeCount, 0, "the live session stands")
        XCTAssertTrue(controller.installedSessionForTesting === liveSession)

        controller.cancel()
        XCTAssertEqual(liveSession.disposeCount, 1, "teardown reaches the installed session")
    }

    /// The order codex's forward test could not force: the successor
    /// installs FIRST, and only then does the predecessor's stale success
    /// land and dispose. The shared audio session is claim-counted, so the
    /// stale disposal releases exactly its own claim — the successor's
    /// microphone keeps its.
    func testAPredecessorsLateDisposalCannotSilenceTheSuccessor() async {
        let (controller, backends) = suspendedController()
        let base = DictationController.audioClaimsForTesting

        let first = Task { await controller.start() }
        let firstBackend = await backends.next()
        controller.cancel()
        let second = Task { await controller.start() }
        let secondBackend = await backends.next()

        // Successor completes first and is recording on its own claim.
        let live = DictationController.makeStubSessionForTesting()
        secondBackend.resume(returning: live)
        await second.value
        XCTAssertTrue(controller.isRecording)
        XCTAssertEqual(DictationController.audioClaimsForTesting, base + 1)

        // Now the predecessor's success lands, late and stale.
        let stale = DictationController.makeStubSessionForTesting()
        firstBackend.resume(returning: stale)
        await first.value
        XCTAssertEqual(stale.disposeCount, 1)
        XCTAssertTrue(controller.isRecording, "the successor keeps recording")
        XCTAssertTrue(controller.installedSessionForTesting === live)
        XCTAssertEqual(
            DictationController.audioClaimsForTesting, base + 1,
            "the stale disposal released its own claim and no one else's")

        controller.cancel()
        XCTAssertEqual(
            DictationController.audioClaimsForTesting, base,
            "the last release is the one that lets the session go quiet")
        XCTAssertEqual(live.disposeCount, 1)
    }

    /// Dispose is idempotent in effect: a second call releases nothing more.
    func testDisposeReleasesItsClaimExactlyOnce() {
        let base = DictationController.audioClaimsForTesting
        let session = DictationController.makeStubSessionForTesting()
        XCTAssertEqual(DictationController.audioClaimsForTesting, base + 1)
        session.dispose()
        session.dispose()
        XCTAssertEqual(DictationController.audioClaimsForTesting, base)
    }

    /// A second tap during startup is a no-op, not a second engine.
    func testADoubleTapWhileStartingIsIgnored() async {
        let (controller, backends) = suspendedController()
        let first = Task { await controller.start() }
        let backend = await backends.next()

        await Task { await controller.start() }.value
        XCTAssertTrue(controller.isStarting, "the second tap changed nothing")

        backend.resume(returning: DictationController.makeStubSessionForTesting())
        await first.value
        XCTAssertTrue(controller.isRecording, "exactly one backend ran")
        controller.cancel()
    }
}
