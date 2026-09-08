import CryptoKit
import Foundation

/// **The two mutation hashes, mirrored from `mac/protocol/src/hash.rs`.**
///
/// Each preimage is a domain tag followed by its fields, every field prefixed
/// with its own length:
///
/// ```
/// material = "<tag>"
/// for field in fields:
///     material += "\n" + decimal(byteLength(field)) + ":" + field
/// digest   = lowercase_hex(SHA256(utf8(material)))
/// ```
///
/// **`byteLength` is `utf8.count`, never `count`.** Rust's `str::len()` is a
/// UTF-8 byte count and Swift's `String.count` is grapheme clusters; the two
/// agree on every ASCII string and disagree the first time somebody sends an
/// accent, a CJK character or an emoji. A `.count` implementation therefore
/// passes every ASCII test and fails silently in production, with the daemon
/// reporting `stale payload_hash` for a message the phone believed it had
/// hashed correctly. `CodexHashTests` pins the two vectors that tell them apart.
///
/// The length prefix is what makes the fields unambiguous: without it
/// `("cc-1\na", "b")` and `("cc-1", "a\nb")` would share a preimage, and one
/// request id could be made to name a different mutation.
enum CodexHash {

    /// Identity of one stop: which session's which turn.
    ///
    /// `sessionRef` is **exactly the string the message's `session_id` field
    /// carries**. The daemon recomputes with the client's own spelling, so the
    /// phone contracts itself to `session_uid` always (decision D4) rather than
    /// relying on a normalisation the hash check does not perform.
    static func interrupt(sessionRef: String, turnID: String) -> String {
        digest(tag: "codeconnect.interrupt.v1", fields: [sessionRef, turnID])
    }

    /// Identity of one message: which session is told which words.
    ///
    /// **The turn is deliberately absent.** The phone cannot know whether its
    /// words will start a turn or steer one — that is decided at the Mac, at the
    /// instant the daemon writes — so putting the turn in would turn the honest
    /// retry of an unacknowledged send, which is the whole reason the hash
    /// exists, into a conflict whenever the session had moved.
    static func compose(sessionRef: String, text: String) -> String {
        digest(tag: "codeconnect.compose.v1", fields: [sessionRef, text])
    }

    private static func digest(tag: String, fields: [String]) -> String {
        var material = tag
        for field in fields {
            material += "\n\(field.utf8.count):\(field)"
        }
        return SHA256.hash(data: Data(material.utf8))
            .map { String(format: "%02x", $0) }
            .joined()
    }
}

// MARK: - Reading a Codex approval card

/// Typed reads over a Codex card's `tool_input`.
///
/// A Codex card reuses `ApprovalCard` unchanged — the Mac builds it that way on
/// purpose, so the phone's existing `SHA-256(display_text) == payload_hash` gate
/// holds — but everything *inside* `tool_input` is Codex's own shape. These
/// accessors are where that shape is read, once, so the card view and the option
/// rows cannot disagree about what a card offers.
///
/// Nothing here throws and nothing force-unwraps: a reshaped field means
/// "absent", which the card renders as absent rather than guessing.
enum CodexCard {

    /// One option exactly as the card offered it.
    ///
    /// `id` is **opaque and verbatim** — the daemon validates it against its own
    /// stored option set and reconstructs the wire decision from that option's
    /// own payload, never from anything the phone supplies. The phone must never
    /// synthesise one, shorten one, or infer one from a label.
    struct Option: Sendable, Hashable, Identifiable {
        let id: String
        let label: String
    }

    /// One file the card proposes to change.
    struct Change: Sendable, Hashable, Identifiable {
        let path: String
        /// `add` / `update` / `delete` / `rename`, as the app-server named it.
        let kind: String
        /// Where a rename would move it, when the kind is a rename.
        let movePath: String?
        /// The unified diff for this file, exactly as captured.
        let diff: String

        var id: String { path }
    }

    /// The files a card could not carry, because the patch exceeded the 32-file
    /// ceiling. **Constructed shape** — no capture in the corpus exercises it
    /// (contract §4.6) — so it is rendered as a count and a digest and never as
    /// a list of paths the phone does not have.
    struct ChangesOmitted: Sendable, Hashable {
        let count: Int
        let sha256: String
    }

    /// The card's option table. Empty when the card is not a Codex card, which
    /// is how a Claude card falls through to Allow/Deny unchanged.
    static func options(in toolInput: JSONValue?) -> [Option] {
        guard let rows = toolInput?["options"]?.arrayValue else { return [] }
        return rows.compactMap { row in
            guard let id = row["id"]?.stringValue, !id.isEmpty else { return nil }
            // A label the daemon did not supply is not invented: the id is
            // opaque but at least it is the daemon's own word.
            return Option(id: id, label: row["label"]?.stringValue ?? id)
        }
    }

    /// Why the agent is asking, when it said. Present on 0.153 command cards,
    /// absent on every measured file-change card.
    static func reason(in toolInput: JSONValue?) -> String? {
        guard let reason = toolInput?["reason"]?.stringValue, !reason.isEmpty else { return nil }
        return reason
    }

    /// The working directory a command would run in.
    static func cwd(in toolInput: JSONValue?) -> String? {
        guard let cwd = toolInput?["cwd"]?.stringValue, !cwd.isEmpty else { return nil }
        return cwd
    }

    /// The inline patch. Up to 32 files, 16 KiB per diff, 128 KiB shared — the
    /// Mac's own ceilings, so a card is bounded by construction.
    static func changes(in toolInput: JSONValue?) -> [Change] {
        guard let rows = toolInput?["changes"]?.arrayValue else { return [] }
        return rows.compactMap { row in
            guard let path = row["path"]?.stringValue else { return nil }
            return Change(
                path: path,
                kind: row["kind"]?["type"]?.stringValue ?? "update",
                movePath: row["kind"]?["move_path"]?.stringValue,
                diff: row["diff"]?.stringValue ?? "")
        }
    }

    /// One change's diff, through the app's own unified-diff parser.
    ///
    /// A Codex change carries a **bare hunk** — `@@ -1 +1 @@\n-hello\n+goodbye\n`
    /// — with no `diff --git` header, because the app-server is describing one
    /// file and not a commit. `UnifiedDiff.parse` is written against git's
    /// output, so the header is synthesised here rather than a second parser
    /// being written: the folding, the word-level highlight, the line numbering
    /// and every AX5 wrapping decision in `CCDiffPrimitives` are then the same
    /// code that draws the diff sheet, proven by the same tests.
    ///
    /// The synthesised header is never shown. `CCDiffFileHeader` is drawn from
    /// `Change.path` and `Change.kind`, which are what the wire actually said.
    static func parsedDiff(for change: Change) -> UnifiedDiff.FileDiff? {
        guard !change.diff.isEmpty else { return nil }
        let path = change.path
        let header = "diff --git a/\(path) b/\(path)\n--- a/\(path)\n+++ b/\(path)\n"
        return UnifiedDiff.parse(header + change.diff).files.first
    }

    // MARK: How much of the patch the card draws

    /// **The most diff rows a decision card will ever build.**
    ///
    /// The card is not the diff sheet. Its job is to be read before a decision,
    /// and at the ceiling — 32 files sharing 128 KiB — four files alone are
    /// thousands of rows, which put the option list five screens below the fold
    /// in `codex-card-ceiling--L`. `DiffView` solved the same problem for the
    /// sheet with `hunkRowBudget`; this is the card's, and it is deliberately
    /// smaller because the card must stay answerable.
    ///
    /// 160 rows is a little over two screens of diff at reading size, which is
    /// as much as anyone reads before they decide they need the Mac.
    static let diffRowBudget = 160

    /// How the budget is spent across the files a card is showing.
    ///
    /// In order, greedily: the first files are drawn whole until the budget runs
    /// out, and what is left over is counted rather than dropped silently. A
    /// per-file share was the other option and it is worse — it truncates a
    /// two-line rename to make room for a file nobody scrolled to.
    struct DiffPlan: Sendable, Equatable {
        /// Rows allowed for each shown change, positionally.
        var allowances: [Int] = []
        /// Rows this card is not drawing, across every shown file.
        var heldBack: Int = 0
    }

    static func diffPlan(for changes: [Change], budget: Int = diffRowBudget) -> DiffPlan {
        var plan = DiffPlan()
        var left = budget
        for change in changes {
            let rows = parsedDiff(for: change)?.hunks.reduce(0) { $0 + $1.lines.count } ?? 0
            let allowed = min(rows, left)
            plan.allowances.append(allowed)
            plan.heldBack += rows - allowed
            left -= allowed
        }
        return plan
    }

    static func changesOmitted(in toolInput: JSONValue?) -> ChangesOmitted? {
        guard let block = toolInput?["changes_omitted"],
            let count = block["count"]?.intValue, count > 0
        else { return nil }
        return ChangesOmitted(count: Int(count), sha256: block["sha256"]?.stringValue ?? "")
    }

}

// MARK: - What the composer may send

/// One draft message and everything the phone can decide about it **before**
/// anything is sent.
///
/// A value rather than a method on the view, so the byte rule is testable
/// without a viewport — and so the ceiling is enforced in one place instead of
/// being re-derived by the counter, the send button and the send path.
struct ComposeDraft: Sendable, Hashable {
    var text: String

    /// The measurement the Mac makes: `text.len()` in Rust is UTF-8 bytes.
    var byteCount: Int { text.utf8.count }

    /// **The daemon's rule, exactly.** `state.rs` rejects `text.is_empty()` and
    /// nothing else; this trimmed first, so a message of newlines — which has
    /// bytes the Mac would have accepted — was refused by the phone in the
    /// daemon's name. If a stricter client policy is ever wanted it has to be
    /// argued for and labelled as the app's own, not smuggled in as the wire's.
    private var isEffectivelyEmpty: Bool { text.isEmpty }

    /// Why this draft cannot be sent, in the phone's own words, or nil when it
    /// can. Both refusals are ones the daemon would also make — the point of
    /// making them here is that a round trip to be told so is a round trip
    /// nobody needed, and the oversize one names the number the reader can act on.
    var blockedReason: String? {
        if isEffectivelyEmpty { return "There is nothing to say yet." }
        if byteCount > Wire.maxComposeBytes {
            return "This message is \(byteCount) bytes. The ceiling is \(Wire.maxComposeBytes)."
        }
        return nil
    }

    /// The counter, shown only as the ceiling comes into view.
    ///
    /// A counter on every message is noise on a composer whose ordinary message
    /// is a sentence; a counter that appears only once you are already over is a
    /// scolding. It arrives with enough room left to do something about it.
    var counterText: String? {
        guard byteCount >= Self.counterThreshold else { return nil }
        return "\(Self.number(byteCount)) / \(Self.number(Wire.maxComposeBytes)) bytes"
    }

    /// Whether the counter is reporting a draft that is already too long.
    var isOverCeiling: Bool { byteCount > Wire.maxComposeBytes }

    /// Roughly 90% of the ceiling: far enough in that it is not noise, early
    /// enough that a sentence can still be cut.
    private static let counterThreshold = 7_400

    private static func number(_ value: Int) -> String {
        value.formatted(.number.grouping(.automatic))
    }
}

// MARK: - Which turn is running

/// **Where the phone's `turn_id` comes from, and why it has to be derived.**
///
/// `interrupt` requires a `turn_id` and the approval card gives the phone none:
/// `raise_codex_approval` builds its event with `with_source_event_id` only, and
/// `threadId`/`turnId`/`itemId` are ruled *identity, not content* and kept out
/// of `tool_input` deliberately. Worse, **`turn/started` maps to no event at
/// all** — there is no "a turn began" fact on this wire. A turn's existence is
/// carried by its items and its terminal.
///
/// So the running turn is the newest `turn_id` seen on this session's event
/// envelopes with no `turn_complete` carrying the *same* id after it. When there
/// is none, Stop is hidden — the one honest hide in the phase, because it is a
/// fact the phone genuinely holds rather than a guess about the Mac.
///
/// **D2 landed, and this stayed.** Minor 19 puts `turn_id` on the approval
/// event's own envelope, which deletes the inference for every card a current
/// daemon raises. The derivation is kept as the fallback for cards from a daemon
/// that predates the field — and it needs no branch to prefer the envelope,
/// because an approval event's `turn_id` arrives as `Event.turnID` exactly like
/// every other event's, and the newest one wins by construction.
enum CodexTurnTracker {

    /// The turn this session is running, or nil.
    ///
    /// Reads the **envelope**, never a payload: `turn_id` is an envelope field
    /// on every item event the adapter emits, and a payload's copy of it (if a
    /// future one carried one) would be a second source of truth for one fact.
    ///
    /// Deliberately agnostic about the agent. Who is allowed to act on a running
    /// turn is decided at the call site by `SessionSummary.agent`, because that
    /// is where the answer can be wrong in a way a reader would notice.
    static func runningTurn(
        in events: [Event], observed: [String] = [], retired: Set<String> = []
    ) -> String? {
        var candidate: String?
        // **Which turns the events say are over**, kept by id rather than
        // collapsed into "nothing is running". The two are not the same fact,
        // and treating them as one is what let an ended turn come back below.
        var ended: Set<String> = []
        for event in events {
            guard let turn = event.turnID, !turn.isEmpty else { continue }
            if event.isTurnComplete {
                ended.insert(turn)
                // Only its own terminal ends a turn. Another turn's says
                // nothing about this one, and treating it as an end would hide
                // Stop on a session that is very much still working.
                if candidate == turn { candidate = nil }
                continue
            }
            // A turn named after its own terminal is a straggler, not a
            // restart: `turn_complete` is the last word on that id.
            if ended.contains(turn) { continue }
            candidate = turn
        }

        // **What a mutation told us, which no event may have said yet.**
        //
        // A compose answers `started` / `steered` / `duplicate` carrying the
        // turn its words landed in, and the first item event for that turn can
        // be seconds behind — so Stop was hidden on a turn the phone had just
        // been handed the id of.
        //
        // It is consulted only for turns **no terminal names**. The old
        // condition was `candidate == nil`, which is also exactly what a
        // matching `turn_complete` produces — so compose→`Started{T1}`→
        // `turn_complete(T1)` resurrected T1 and offered Stop on a turn the Mac
        // had finished. A late `Steered{T1}` after the terminal did the same.
        if candidate == nil,
            let latest = observed.last(where: { !retired.contains($0) && !ended.contains($0) })
        {
            candidate = latest
        }

        // **And what a mutation told us is over.** `interrupt` answers
        // `aborted` / `duplicate` well before the `turn_complete` that will
        // eventually agree; between the two the turn was still "running"
        // everywhere the phone looked, Stop stayed offered, and a second tap
        // sent a second abort at a turn that was already gone.
        if let turn = candidate, retired.contains(turn) { return nil }
        return candidate
    }
}
