import Foundation

/// On-disk last-known state, so a cold open shows something real immediately —
/// with its age attached, never presented as live.
///
/// Plain files rather than SwiftData: the cache is an append-only tail of an
/// already-durable log on the Mac. It has no queries, no relations and no
/// migrations worth the name, and a corrupt cache must be cheap to discard.
actor EventCache {
    struct CachedEvents: Sendable {
        var events: [Event]
        var cachedAt: Date

        var lastSeq: UInt64 { events.last?.seq ?? 0 }
    }

    struct CachedFleet: Sendable {
        var sessions: [SessionSummary]
        var cachedAt: Date
    }

    /// What the keys in this cache *mean*.
    ///
    /// The two are not interchangeable and a file written under one is not
    /// readable under the other: `cc-1` names whichever run holds the name now,
    /// while a uid names one run forever. Recording which regime wrote the cache
    /// is what lets a mode change be detected and discarded rather than
    /// producing a timeline stitched out of two agents' work.
    enum Keying: String, Codable, Sendable {
        /// Pre-uid daemons (protocol minor < 2): the tmux name is the identity.
        case name
        /// Protocol minor 2 and up: files are keyed by `session_uid`.
        case uid
    }

    /// Bounds a single session file. The daemon keeps the whole log; this is a
    /// window, and the window is always the newest events.
    private static let maxEventsPerSession = 3000
    /// Bumped to 2 when `Keying` was introduced. A version-1 file is name-keyed
    /// by construction and is simply not read.
    private static let formatVersion = 2

    private let root: URL

    init(root: URL? = nil) {
        if let root {
            self.root = root
        } else {
            let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)
                .first ?? URL(fileURLWithPath: NSTemporaryDirectory())
            self.root = base.appendingPathComponent("CodeConnect/cache", isDirectory: true)
        }
        try? FileManager.default.createDirectory(at: self.root, withIntermediateDirectories: true)
    }

    // MARK: - Files

    private struct EventEnvelope: Codable {
        var version: Int
        /// The key this file is filed under — a uid or a tmux name, per
        /// `keying`. Kept under its old name so the field means the same thing
        /// it did in version 1.
        var sessionID: String
        var keying: Keying
        var cachedAt: Date
        var events: [Event]
    }

    /// Just enough of any generation of the event file to say *what it was
    /// about*. Used only when sweeping the cache, so that history discarded by
    /// a keying change can be reported per session rather than vanishing.
    private struct EventEnvelopeHeader: Decodable {
        var sessionID: String
        var events: [Event]
    }

    private struct FleetEnvelope: Codable {
        var version: Int
        var keying: Keying
        var cachedAt: Date
        var sessions: [SessionSummary]
    }

    /// The regime the files on disk were written under.
    private struct KeyingMarker: Codable {
        var version: Int
        var keying: Keying
    }

    /// Keys are uids or tmux names today, but a cache path must never be able to
    /// escape its directory on a future key scheme.
    private func url(forSession key: String) -> URL {
        let safe = key.unicodeScalars.map { scalar -> String in
            CharacterSet.alphanumerics.contains(scalar) || scalar == "-" || scalar == "_"
                ? String(scalar) : String(format: "%%%02X", scalar.value)
        }.joined()
        return root.appendingPathComponent("session-\(safe).json")
    }

    private var fleetURL: URL { root.appendingPathComponent("fleet.json") }
    private var keyingURL: URL { root.appendingPathComponent("keying.json") }

    // MARK: - Keying

    /// The regime the files on disk were written under, or `nil` when nothing
    /// current says — an install that predates the marker, or one whose cache
    /// has been cleared. Memoised because every read and write consults it and
    /// it changes at most once per launch.
    private var declaredKeying: Keying??

    private var storedKeying: Keying? {
        if let declaredKeying { return declaredKeying }
        let value: Keying?
        if let data = try? Data(contentsOf: keyingURL),
            let marker = try? JSONDecoder().decode(KeyingMarker.self, from: data),
            marker.version == Self.formatVersion
        {
            value = marker.keying
        } else {
            value = nil
        }
        declaredKeying = .some(value)
        return value
    }

    /// What to tag a file being written now. An un-adopted cache is name-keyed:
    /// that is what every build predating session uids wrote, and what this one
    /// writes when it has no daemon to ask.
    private var effectiveKeying: Keying { storedKeying ?? .name }

    /// Switch the cache to `keying`, discarding everything filed under the other
    /// regime, and report which keys were dropped.
    ///
    /// Dropping is the honest move rather than a clever remap. A name-keyed file
    /// holds the events of *whichever* runs held that name — possibly several,
    /// spliced — and there is nothing in it that says which parts belong to the
    /// run the daemon is now offering under that uid. A cold re-sync from the
    /// daemon costs one backfill and is the only version of that history anyone
    /// can vouch for.
    ///
    /// Returns the dropped keys so the app can say so on the sessions it
    /// affected, instead of silently presenting a shorter timeline as if that
    /// were all there ever was.
    /// A cache with no current marker is swept as well as re-tagged: it was
    /// written by a build whose files this one cannot read anyway, and leaving
    /// them would keep unreadable files on disk forever.
    @discardableResult
    func adopt(keying: Keying) -> Set<String> {
        guard storedKeying != .some(keying) else { return [] }
        var dropped: Set<String> = []
        let files =
            (try? FileManager.default.contentsOfDirectory(
                at: root, includingPropertiesForKeys: nil)) ?? []
        for file in files where file.lastPathComponent.hasPrefix("session-") {
            if let data = try? Data(contentsOf: file),
                let header = try? JSONDecoder().decode(EventEnvelopeHeader.self, from: data),
                !header.events.isEmpty
            {
                dropped.insert(header.sessionID)
            }
            try? FileManager.default.removeItem(at: file)
        }
        // The fleet list is keyed the same way and is re-sent on every connect,
        // so there is nothing to preserve and a stale one would seed the wrong
        // states before the live list lands.
        try? FileManager.default.removeItem(at: fleetURL)
        write(KeyingMarker(version: Self.formatVersion, keying: keying), to: keyingURL)
        declaredKeying = .some(keying)
        return dropped
    }

    // MARK: - Reads

    func loadEvents(key: String) -> CachedEvents? {
        guard let data = try? Data(contentsOf: url(forSession: key)),
            let envelope = try? JSONDecoder().decode(EventEnvelope.self, from: data),
            envelope.version == Self.formatVersion,
            envelope.keying == effectiveKeying
        else { return nil }
        return CachedEvents(events: envelope.events, cachedAt: envelope.cachedAt)
    }

    func loadFleet() -> CachedFleet? {
        guard let data = try? Data(contentsOf: fleetURL),
            let envelope = try? JSONDecoder().decode(FleetEnvelope.self, from: data),
            envelope.version == Self.formatVersion,
            envelope.keying == effectiveKeying
        else { return nil }
        return CachedFleet(sessions: envelope.sessions, cachedAt: envelope.cachedAt)
    }

    // MARK: - Writes

    func saveEvents(_ events: [Event], key: String) {
        let window = Array(events.suffix(Self.maxEventsPerSession))
        write(
            EventEnvelope(
                version: Self.formatVersion, sessionID: key, keying: effectiveKeying,
                cachedAt: Date(), events: window),
            to: url(forSession: key))
    }

    func saveFleet(_ sessions: [SessionSummary]) {
        write(
            FleetEnvelope(
                version: Self.formatVersion, keying: effectiveKeying, cachedAt: Date(),
                sessions: sessions),
            to: fleetURL)
    }

    func clearEvents(key: String) {
        try? FileManager.default.removeItem(at: url(forSession: key))
    }

    func clearAll() {
        try? FileManager.default.removeItem(at: root)
        try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        // The marker went with the directory, so the next `adopt` has to be able
        // to see that and re-declare rather than trusting a stale memo.
        declaredKeying = nil
    }

    private func write<T: Encodable>(_ value: T, to url: URL) {
        guard let data = try? JSONEncoder().encode(value) else { return }
        try? data.write(to: url, options: .atomic)
    }
}

/// Non-secret, non-critical per-session UI state. Losing it costs one "new"
/// badge, so `UserDefaults` is the right amount of machinery.
///
/// Keyed by `SessionSummary.sessionKey`, which on a uid-minting daemon means one
/// entry per agent *run* rather than one per tmux name — so unlike every other
/// map in the app this one has to be bounded, or a long-lived install would grow
/// it forever.
enum ReviewMarks {
    private static let key = "codeconnect.reviewedSeq"
    /// Roughly a year of heavy use at a handful of runs a day. Small enough that
    /// the whole map stays a few kilobytes.
    static let maxEntries = 200

    static func reviewedSeq(for sessionKey: String) -> UInt64 {
        let map = UserDefaults.standard.dictionary(forKey: key) as? [String: NSNumber]
        return map?[sessionKey]?.uint64Value ?? 0
    }

    static func markReviewed(sessionKey: String, seq: UInt64) {
        var map = UserDefaults.standard.dictionary(forKey: key) as? [String: NSNumber] ?? [:]
        // Monotonic: opening an older cached view must not un-review newer work.
        if let existing = map[sessionKey]?.uint64Value, existing >= seq { return }
        map[sessionKey] = NSNumber(value: seq)
        UserDefaults.standard.set(pruned(map), forKey: key)
    }

    /// Keep the newest `maxEntries`.
    ///
    /// A ULID sorts by the millisecond it was minted, so plain string order *is*
    /// creation order for uids, and dropping the lowest drops the oldest runs.
    /// Legacy tmux names (`cc-1`) sort above every uid — the alphabet starts at
    /// `0` — so they survive a prune, which is right: there are only ever a
    /// handful of them and each one is reused rather than accumulating.
    static func pruned(_ map: [String: NSNumber]) -> [String: NSNumber] {
        guard map.count > maxEntries else { return map }
        let keep = Set(map.keys.sorted().suffix(maxEntries))
        return map.filter { keep.contains($0.key) }
    }
}
