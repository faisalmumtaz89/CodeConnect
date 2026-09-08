import Foundation

/// **The daemon's own refusal sentences, as data.**
///
/// The wire carries no refusal code — `InterruptResult.rejected` and
/// `ComposeResult.rejected` are bare human sentences — so the phone has to
/// decide from the text whether a refusal is one to wait out. It did that by
/// matching three English phrases, and the shared fixture proved that wrong the
/// day it landed: the daemon writes **18** link-state refusals and those three
/// clauses caught 8. Ten sentences that say the link is coming back would have
/// left the control live against a link that was reconnecting.
///
/// Guessing at prose is the whole defect, so this stops guessing. The daemon
/// emits every sentence it can send, with its own category, from the one place
/// each is written (`mac/ccd/src/codex_refusals.rs`); its gate test keeps the
/// file byte-identical to the build. The phone matches against those rows.
///
/// **Tokens are wildcards, anchored in order.** A row's text is a template —
/// `{uid}`, `{ref}`, `{n}` and the rest are filled at the Mac — so a row matches
/// a sentence when the template's literal segments appear in it, in order, with
/// anything between them. Nothing a token could contain can make a sentence
/// match a row it does not belong to, because the literal segments around it
/// still have to line up.
enum CodexRefusals {

    /// What the daemon says this refusal is about.
    enum Category: String, Sendable, Hashable {
        /// A fact about the control link right now — the same ask a moment later
        /// may well be written.
        case linkState = "link_state"
        /// A local store or lookup failure *before* any write. Nothing reached
        /// Codex, and the same ask may succeed on retry.
        case transientLocal = "transient_local"
        /// A settled fact: the ask was wrong, the id is spent, or the outcome is
        /// already recorded.
        case permanent
        /// The app-server or the broker refused the write.
        case wireCode = "wire_code"
        /// A category a newer daemon names and this build does not. **Never
        /// greys**: a control the reader could act on now, held dead for ten
        /// seconds, is the worse of the two errors.
        case unknown

        init(wire raw: String) {
            self = Category(rawValue: raw) ?? .unknown
        }

        /// Whether a refusal in this category is worth waiting out.
        ///
        /// Both greying categories say the same thing in different words: *this
        /// did not happen, and the same ask may work shortly*. `permanent` and
        /// `wire_code` say the opposite, and several of them add "it will not be
        /// sent again" — a bounded grey followed by a live control would be an
        /// invitation to do exactly what that sentence forbids.
        var greys: Bool {
            switch self {
            case .linkState, .transientLocal: return true
            case .permanent, .wireCode, .unknown: return false
            }
        }
    }

    struct Sentence: Sendable, Hashable {
        let id: String
        let category: Category
        /// The template's literal segments, in order, with the tokens removed.
        /// Matching is "these appear in this order"; the tokens are the gaps.
        let segments: [String]
        /// Whether the template **begins** with a token (`{uid} is a Claude
        /// session…`). Four of the daemon's rows do, and anchoring their first
        /// literal segment to the start of the sentence would never match — the
        /// uid is in front of it.
        let startsWithToken: Bool
        /// And whether it **ends** with one. Without this the tail was never
        /// anchored, so a sentence that quoted a row and then carried on still
        /// matched it — greying a control for ten seconds on a refusal that was
        /// not a link-state refusal at all.
        let endsWithToken: Bool
    }

    /// Every sentence the daemon can send, loaded once.
    ///
    /// An empty catalogue is not a crash: it means no refusal greys, which is
    /// the safe direction and exactly what an older build would have done.
    static let all: [Sentence] = load()

    /// Whether this refusal should grey its control for the bounded window.
    ///
    /// Lowercased on both sides: the daemon writes these sentences and the phone
    /// shows them verbatim, but a match must not turn on a capital letter.
    static func greys(_ reason: String) -> Bool {
        guard let match = match(reason) else { return false }
        return match.category.greys
    }

    /// The row this sentence came from, or nil when none matches.
    ///
    /// The **most specific** match wins: two rows can share a prefix, and the
    /// one with more literal text pinned down is the one that is really being
    /// read. Without that a short generic row could claim a longer sibling's
    /// sentence and hand back the wrong category.
    static func match(_ reason: String) -> Sentence? {
        let lowered = reason.lowercased()
        return
            all
            .filter { matches(lowered, $0) }
            .max { lhs, rhs in weight(lhs) < weight(rhs) }
    }

    private static func weight(_ sentence: Sentence) -> Int {
        sentence.segments.reduce(0) { $0 + $1.count }
    }

    /// The literal segments, in order, with anything in the gaps.
    private static func matches(_ text: String, _ sentence: Sentence) -> Bool {
        guard !sentence.segments.isEmpty else { return false }
        var cursor = text.startIndex
        for (index, segment) in sentence.segments.enumerated() {
            guard let found = text.range(of: segment, range: cursor..<text.endIndex) else {
                return false
            }
            // A template whose first character is literal must match from the
            // start, or a row could claim a longer sentence that merely quotes
            // it. A template that opens with a token cannot be anchored that
            // way — the filled value is in front of its first literal — so it
            // is matched on order alone.
            if index == 0, !sentence.startsWithToken, found.lowerBound != text.startIndex {
                return false
            }
            // The same rule at the other end: a template whose last character
            // is literal describes a sentence that ENDS there. Anything after
            // it is a different sentence that happens to quote this one.
            if index == sentence.segments.count - 1, !sentence.endsWithToken,
                found.upperBound != text.endIndex
            {
                return false
            }
            cursor = found.upperBound
        }
        return true
    }

    // MARK: Loading

    private struct Wire: Decodable {
        struct Row: Decodable {
            let id: String
            let category: String
            let text: String
        }
        let sentences: [Row]
        let tokens: [String: String]
    }

    private static func load() -> [Sentence] {
        guard let url = Bundle.main.url(forResource: "refusal-sentences", withExtension: "json"),
            let data = try? Data(contentsOf: url),
            let wire = try? JSONDecoder().decode(Wire.self, from: data)
        else { return [] }
        return parse(wire)
    }

    /// Split each template on its tokens. Internal so a test can drive it over
    /// the repo's own copy of the file rather than the bundled one.
    static func parse(fileContents data: Data) -> [Sentence] {
        guard let wire = try? JSONDecoder().decode(Wire.self, from: data) else { return [] }
        return parse(wire)
    }

    private static func parse(_ wire: Wire) -> [Sentence] {
        let tokens = wire.tokens.keys.sorted { $0.count > $1.count }
        return wire.sentences.map { row in
            var segments = [row.text.lowercased()]
            for token in tokens {
                segments = segments.flatMap {
                    $0.components(separatedBy: token.lowercased())
                }
            }
            // A leading empty segment is how "the template starts with a token"
            // shows up in the split, and it is the fact the anchor needs.
            let startsWithToken = segments.first?.isEmpty == true
            // A trailing empty segment is the mirror fact: the template's last
            // character was a token, so its tail cannot be anchored.
            let endsWithToken = segments.last?.isEmpty == true
            return Sentence(
                id: row.id,
                category: Category(wire: row.category),
                segments: segments.filter { !$0.isEmpty },
                startsWithToken: startsWithToken,
                endsWithToken: endsWithToken)
        }
    }
}
