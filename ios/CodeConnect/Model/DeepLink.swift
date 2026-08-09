import Foundation

/// Where a notification should land.
///
/// A push is a doorbell. An approval lands *past* the fleet, in the Deck — the
/// list of what is waiting; every other kind lands on the fleet itself. It aims
/// at a list rather than at one decision because
/// its payload names no session and no decision: one that pointed at a single card
/// would have to keep being right about it after it was answered, superseded or
/// already run, and would have to carry an identifier through Apple to do it.
///
/// URL forms, all under the `codeconnect` scheme:
///
///   * `codeconnect://deck` — the cross-fleet queue.
///   * `codeconnect://deck/<request-id>` — the queue, starting on one card.
///   * `codeconnect://session/<session-id>` — one session's timeline.
///   * `codeconnect://session/<session-id>?request=<request-id>` — that
///     session's timeline with the decision card already open.
///   * `codeconnect://session/<session-id>/diff` — straight to the diff.
enum DeepLink: Sendable, Hashable {
    /// The fleet, with whatever was on top of it cleared.
    ///
    /// No URL form — nothing types this — and it exists because a tapped
    /// notification about something undecidable has to land somewhere
    /// *deterministic*. Doing nothing only resembles the fleet on a cold launch;
    /// a warm app would stay on whatever screen it was left on.
    case fleet
    case deck(requestID: String?)
    case session(sessionID: String, requestID: String?)
    case diff(sessionID: String)

    static let scheme = "codeconnect"

    init?(url: URL) {
        guard url.scheme?.lowercased() == Self.scheme else { return nil }
        // `codeconnect://deck/x` parses as host "deck", path "/x".
        let segments =
            [url.host].compactMap { $0 }
            + url.pathComponents.filter { $0 != "/" }
        guard let first = segments.first?.lowercased() else { return nil }
        let query = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let requestID = query.first { $0.name == "request" }?.value

        switch first {
        case "deck":
            self = .deck(requestID: requestID ?? segments.dropFirst().first)
        case "session":
            guard let sessionID = segments.dropFirst().first, !sessionID.isEmpty else {
                return nil
            }
            if segments.dropFirst(2).first?.lowercased() == "diff" {
                self = .diff(sessionID: sessionID)
            } else {
                self = .session(sessionID: sessionID, requestID: requestID)
            }
        default:
            return nil
        }
    }

    var url: URL? {
        switch self {
        case .fleet:
            return nil
        case .deck(let requestID):
            guard let requestID else { return URL(string: "\(Self.scheme)://deck") }
            return URL(string: "\(Self.scheme)://deck?request=\(escaped(requestID))")
        case .session(let sessionID, let requestID):
            let base = "\(Self.scheme)://session/\(escaped(sessionID))"
            guard let requestID else { return URL(string: base) }
            return URL(string: "\(base)?request=\(escaped(requestID))")
        case .diff(let sessionID):
            return URL(string: "\(Self.scheme)://session/\(escaped(sessionID))/diff")
        }
    }

    private func escaped(_ value: String) -> String {
        value.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? value
    }
}
