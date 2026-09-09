import Foundation
import Observation

/// **One Codex session's two mutations: stop the turn, and say something.**
///
/// A separate object rather than more fields on `AppModel`, because these are
/// the only two things in the app that are *idempotent by client-minted id*, and
/// that rule needs somewhere it can be stated once and tested without a socket.
///
/// The rule, and why it matters more here than anywhere else: the daemon's
/// ledger treats a retry under the **same** `request_id` carrying the **same**
/// material as a replay — it reports what the first attempt did and does not act
/// again — and a retry under the same id carrying *different* material as a
/// conflict, which it refuses by name. So an id must be stable exactly as long
/// as the words are, and must change the moment they do. Get it backwards in
/// either direction and the phone either says something twice or cannot say it
/// at all.
@MainActor
@Observable
final class CodexControls {

    /// What a mutation is doing right now.
    enum Activity<Result: Sendable & Hashable>: Sendable, Hashable {
        case idle
        /// Sent, no answer yet. The control shows this rather than spinning
        /// silently, so a slow Mac is legible rather than looking dead.
        case inFlight
        /// The daemon answered. Shown until the reader moves on.
        case settled(Result)
        /// The frame never left, and the phone knows why. Distinct from a
        /// refusal: a refusal is the daemon's word, and this is the app's.
        case notSent(String)
        /// **The frame left and no answer ever came back.**
        ///
        /// Its own case, and not `notSent`, because the two are opposite
        /// promises: `notSent` says the words certainly did not reach Codex, and
        /// this says nobody knows. `DaemonConnection.request` can throw *after*
        /// `send` succeeded — a request timeout, a socket loss, `failAllWaiters`
        /// on a reconnect — and mapping all of those to "nothing was sent" is
        /// the app inventing a guarantee it does not have.
        ///
        /// It carries the same consequence as the wire's own `indeterminate`:
        /// an unchanged retry is refused, because saying it twice is the one
        /// outcome nobody can undo.
        case sentNoAnswer(String)
    }

    /// What this session is filed under, so the durable ledger can key by run.
    private let sessionKey: String
    /// **Spent material that outlives the process.** In-memory sets answer for
    /// this launch; this answers for every launch after it. See
    /// `CodexSpentLedger` — quitting the app is not evidence about what the Mac
    /// did with a frame it never acknowledged.
    private let ledger: CodexSpentLedger?

    init(sessionKey: String = "", ledger: CodexSpentLedger? = nil) {
        self.sessionKey = sessionKey
        self.ledger = ledger
    }

    private(set) var stop: Activity<InterruptResult> = .idle
    private(set) var compose: Activity<ComposeResult> = .idle

    /// Until when Stop is greyed after a link-state refusal.
    ///
    /// **Bounded, never permanent.** Four of the daemon's eleven refusal
    /// sentences end in *"try again shortly"* or *"try again in a few seconds"*,
    /// so a control that stayed dead would be contradicting the sentence printed
    /// directly above it. Ten seconds is long enough that a reader does not
    /// hammer a link that is reconnecting, and short enough that the daemon's
    /// own advice stays true.
    private(set) var stopGreyedUntil: Date?

    static let refusalCooldown: TimeInterval = 10

    // MARK: Idempotent request ids

    /// The id currently open for a piece of material, and the material it is
    /// open for. One of each, because only one mutation of each kind can be in
    /// flight per session — `DaemonConnection.request` enforces that too.
    private var openStop: (id: String, material: String)?
    private var openCompose: (id: String, material: String)?

    /// **Material this phone must never send again.**
    ///
    /// The wire's `indeterminate` says the mutation was written and its outcome
    /// is unknown, and the daemon says in its own sentence that *it will not be
    /// sent again*. The phone's `sentNoAnswer` says the same thing about a frame
    /// whose reply was lost. In both cases a second attempt on the same material
    /// would be a real second mutation — the same words said twice to Codex, or
    /// a second abort aimed at a turn nobody can account for — and no id scheme
    /// can make that safe, because the daemon has no record to replay.
    ///
    /// So the material is retained, and the *send path* refuses it. Retiring the
    /// id (which is what the code did before) had exactly the opposite effect:
    /// the next attempt minted a fresh id and looked, to the daemon, like a
    /// brand-new thing to say.
    private var spentComposeMaterial: Set<String> = []
    private var spentStopMaterial: Set<String> = []

    /// **Turns this phone has watched end**, learned from a mutation result
    /// rather than from an event.
    ///
    /// `interrupt` answers `aborted` or `duplicate` long before the
    /// `turn_complete` that will eventually say the same thing on the event
    /// stream, and between the two the turn is still "running" everywhere the
    /// phone looks. Stop stayed offered, and a second tap sent a second
    /// interrupt at a turn that was already gone.
    private(set) var retiredTurns: Set<String> = []

    /// **Turns this phone has watched begin**, likewise.
    ///
    /// `compose` answers `started` / `steered` / `duplicate` carrying the turn
    /// its words landed in — and the contract names that as a source of turn
    /// identity in its own right. Without it, a compose that starts a turn hides
    /// Stop until the first item event happens to arrive.
    private(set) var observedTurns: [String] = []

    /// Whether this exact material has already been put on the wire with no
    /// account of what became of it.
    func composeIsSpent(material: String) -> Bool {
        spentComposeMaterial.contains(material)
            || ledger?.isSpent(.compose, session: sessionKey, material: material) == true
    }

    func stopIsSpent(material: String) -> Bool {
        spentStopMaterial.contains(material)
            || ledger?.isSpent(.stop, session: sessionKey, material: material) == true
    }

    /// One place that spends a stop, so memory and disk can never disagree.
    private func spendStop(_ material: String) {
        spentStopMaterial.insert(material)
        ledger?.markSpent(.stop, session: sessionKey, material: material)
    }

    private func spendCompose(_ material: String) {
        spentComposeMaterial.insert(material)
        ledger?.markSpent(.compose, session: sessionKey, material: material)
    }

    /// The `request_id` to send a stop under.
    ///
    /// Stable while the **turn** is the same, because that is what the identity
    /// of a stop is: `interrupt_hash(session, turn)`. Aiming at a different turn
    /// under the same id is exactly what the daemon refuses — *"this request id
    /// was used to stop a different turn"* — so a new turn mints a new id.
    func stopRequestID(material: String, mint: () -> String = { "stop-" + UUID().uuidString })
        -> String
    {
        if let open = openStop, open.material == material { return open.id }
        let id = mint()
        openStop = (id, material)
        return id
    }

    /// The `request_id` to send a message under. Stable while the **words** are,
    /// for the same reason: `compose_hash(session, text)` is the identity, and a
    /// retry of an unacknowledged send is the whole thing the hash exists for.
    func composeRequestID(material: String, mint: () -> String = { "say-" + UUID().uuidString })
        -> String
    {
        if let open = openCompose, open.material == material { return open.id }
        let id = mint()
        openCompose = (id, material)
        return id
    }

    // MARK: Recording what happened

    /// **Starting one mutation clears the other's outcome.**
    ///
    /// The composer has one note slot and the stop result was drawn first, so a
    /// settled Stop outranked every later compose result: a stop refusal
    /// followed by a successful message went on showing the stop refusal. There
    /// is one slot because there is one question — *what happened when I just
    /// did that* — and the answer is about the thing that was just done.
    func beginStop() {
        stop = .inFlight
        compose = .idle
    }

    func beginCompose() {
        compose = .inFlight
        stop = .idle
        stopGreyedUntil = nil
    }

    /// The daemon answered a stop.
    ///
    /// A **link-state** refusal greys the control for a bounded window; every
    /// other refusal does not, because the reader can act on those immediately —
    /// a stale hash, an empty turn or a reused id are all things a fresh attempt
    /// fixes, and greying would just make the fix take ten seconds longer.
    ///
    /// **`indeterminate` never greys and never retries.** The stop was issued
    /// and the daemon did not live to see what it did; offering a resend would
    /// risk aborting a turn nobody meant to touch.
    func settleStop(_ result: InterruptResult, material: String, turnID: String, now: Date = Date())
    {
        stop = .settled(result)
        if case .rejected(let reason) = result, Self.isLinkStateRefusal(reason) {
            stopGreyedUntil = now.addingTimeInterval(Self.refusalCooldown)
        }
        switch result {
        case .aborted, .duplicate:
            // **The turn is gone, now** — not when `turn_complete` catches up.
            // Until this line existed the phone went on offering Stop for a turn
            // it had just watched end, and a second tap aborted it again.
            retiredTurns.insert(turnID)
            openStop = nil
        case .indeterminate:
            // Issued, outcome unknown, and the daemon's own sentence says it
            // will not be sent again. Neither will this phone — in this launch
            // or any later one.
            spendStop(material)
            openStop = nil
        case .rejected:
            // Nothing was actuated, so the material is untouched and an
            // unedited retry is still the same first attempt.
            break
        case .unknown:
            // **A word this build cannot read licenses nothing — including a
            // refusal.** Spending the material made the next tap answer "this
            // was already sent and what became of it is not known", which is an
            // actuation claim in the app's own voice about a status it cannot
            // interpret. The whole point of the arm is to claim neither way, so
            // the material stays unspent and the next tap is allowed.
            //
            // **And the open id is retained**, which is the other half of the
            // same thought. Clearing it let the retry mint a fresh id, and a
            // fresh id is a brand-new mutation to the Mac: its replay ledger
            // cannot match it against the first, so if the unreadable status
            // meant the interrupt *landed*, the second tap aborts a second
            // time. Keeping the id hands the decision to the one side that has
            // the facts — same id, same material, so the daemon replays its
            // first answer or refuses the duplicate.
            break
        }
    }

    /// The frame left and the answer never arrived. Same consequence as
    /// `indeterminate`, for the same reason.
    func stopSentNoAnswer(_ reason: String, material: String) {
        stop = .sentNoAnswer(reason)
        spendStop(material)
        openStop = nil
    }

    func composeSentNoAnswer(_ reason: String, material: String) {
        compose = .sentNoAnswer(reason)
        spendCompose(material)
        openCompose = nil
    }

    func settleCompose(_ result: ComposeResult, material: String) {
        compose = .settled(result)
        switch result {
        case .started(let turn), .steered(let turn), .duplicate(let turn, _):
            // The words landed. The turn they landed in is a fact the phone now
            // holds, and Stop should offer it without waiting for an event.
            observeTurn(turn)
            openCompose = nil
        case .indeterminate:
            // **Written, outcome unknown, never said again.** Retiring the id
            // alone made the next attempt look brand new to the daemon; the
            // material is what has to be retained — and across launches.
            spendCompose(material)
            openCompose = nil
        case .rejected:
            // Nothing was sent, so an unedited retry is still the first attempt.
            break
        case .unknown:
            // Same rule as the stop, both halves: unreadable is not "issued",
            // and the open id survives so an unchanged retry is the same ask.
            // See `settleStop`.
            break
        }
    }

    private func observeTurn(_ turn: String) {
        guard !turn.isEmpty, !retiredTurns.contains(turn) else { return }
        observedTurns.removeAll { $0 == turn }
        observedTurns.append(turn)
    }

    /// The frame never left this phone. Recorded in the app's own words, never
    /// dressed as something the daemon said.
    /// **A refusal is an outcome too.**
    ///
    /// These are the mutations that never reached `beginStop`/`beginCompose` —
    /// refused by the link state, an empty draft, spent material — so they used
    /// to write their sentence while the *other* mutation's older banner stayed
    /// on screen above it. One slot, one question, and the answer is always
    /// about the thing that was just attempted.
    func stopNotSent(_ reason: String) {
        stop = .notSent(reason)
        compose = .idle
    }

    func composeNotSent(_ reason: String) {
        compose = .notSent(reason)
        stop = .idle
        // **The grey survives**, unlike in `beginCompose`. That one clears it
        // because a compose which actually left proves the link came back; this
        // one is a compose that never left, which proves nothing — and the
        // daemon's "try again shortly" is still the last word on the link.
    }

    /// Whether the bounded grey is still in force.
    func isStopGreyed(now: Date) -> Bool {
        guard let stopGreyedUntil else { return false }
        return now < stopGreyedUntil
    }

    /// **Which refusals are worth waiting out** — the daemon's own answer.
    ///
    /// This matched three English phrases and was wrong the day the shared
    /// fixture landed: 8 of the daemon's 20 link-state refusals, twelve missed.
    /// The classification now comes from `fixtures/codex/refusal-sentences.json`,
    /// which the daemon emits from the one place each sentence is written and
    /// keeps byte-identical to its build — so a reword changes the file, the
    /// file changes the app, and `CodexRefusalClassifierTests` fails if the two
    /// ever disagree.
    ///
    /// That last clause is only true because the test reads the **source**
    /// fixture out of the repo and compares this app's bundled copy to it byte
    /// for byte. It used to compare the app's copy to the test bundle's copy —
    /// two derivatives of one `cp` — and both sat two rows behind the daemon,
    /// so `compose_start_in_flight` left this composer live against a link that
    /// was mid-first-turn with every test green.
    ///
    /// An unmatched sentence does **not** grey: a control the reader could act
    /// on now, held dead for ten seconds, is the worse of the two errors.
    static func isLinkStateRefusal(_ reason: String) -> Bool {
        CodexRefusals.greys(reason)
    }
}
