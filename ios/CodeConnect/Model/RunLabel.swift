import Foundation

/// **What to call a run, everywhere it is named.**
///
/// One rule, in one place. Two rules produce two names for one run — a screen
/// that derives its own from `cwd` trims differently from the next screen, and a
/// notification cannot derive anything at all, because the phone may not be
/// running when it is composed. A reader who sees three names has to work out
/// that they are one agent.
///
/// The project is the daemon's, not this app's. `SessionSummary.projectLabel` is
/// resolved on the Mac and is the only authority; deriving a second one here is
/// how the names come apart.
struct RunLabel: Sendable, Hashable {
    /// The project the run is working in, or the honest admission that nobody
    /// has said. Never a working directory, a tmux counter, or a uid.
    let project: String
    /// Shown only when it actually tells two runs apart — see `labels(for:)`.
    let qualifier: String?

    /// The whole label on one line, for the places that have exactly one.
    var inline: String {
        guard let qualifier else { return project }
        return "\(project) · \(qualifier)"
    }

    /// What a screen reader hears: the same words, without the separator, which
    /// is punctuation for the eye and a stumble for the ear.
    var spoken: String {
        guard let qualifier else { return project }
        return "\(project), \(qualifier)"
    }
}

extension RunLabel {
    /// When the daemon has not named a project.
    ///
    /// **Said plainly rather than papered over.** The fallbacks all reach for
    /// something that is not a project — the tmux counter is reused by the next
    /// run, the uid is an identifier — and a name that misleads is worse than
    /// one that admits it does not know.
    static let unknown = RunLabel(project: "Unknown project", qualifier: nil)

    /// Labels for a whole fleet, keyed by `sessionKey`.
    ///
    /// **The qualifier is earned, not assumed.** Two runs in one project are
    /// ordinary — a second agent on the same checkout — and a bare repetition
    /// is confusing, so a start time is offered to tell them apart. It is
    /// withheld unless it is *true and useful*:
    ///
    ///   * **True**: only a hosted run has a start CodeConnect witnessed. An
    ///     adopted run's `created_at` is when the daemon first saw a hook from
    ///     a conversation that may have been running for an hour, and calling
    ///     that "started" would be a fact invented to fill a gap.
    ///   * **Useful**: two runs started in the same minute render the same
    ///     qualifier, and a discriminator that does not discriminate is worse
    ///     than none, because it looks like it does.
    ///
    /// When it cannot be both, the runs share a label. That is honest: they are
    /// two agents in one project, and nothing this app knows separates them.
    static func labels(for summaries: [SessionSummary]) -> [String: RunLabel] {
        var labels: [String: RunLabel] = [:]
        // **Grouped by what is rendered, not by what arrived.** A directory may
        // legitimately be called `Unknown project`, and grouping on the raw
        // field would put that run and an unnamed one in separate groups that
        // then draw the same words with nothing to tell them apart.
        //
        // **Unnamed runs collide too.** A daemon below minor 11 names no
        // project for any run, so every row reads `Unknown project` and the
        // list stops being a list. A start time is not a name, so offering one
        // here invents nothing that could be mistaken for one.
        let grouped = Dictionary(grouping: summaries) { summary in
            summary.projectLabel.isEmpty ? unknown.project : summary.projectLabel
        }
        for (project, group) in grouped {
            let plain = RunLabel(project: project, qualifier: nil)
            guard group.count > 1, let starts = distinctStarts(in: group) else {
                for summary in group { labels[summary.sessionKey] = plain }
                continue
            }
            for summary in group {
                labels[summary.sessionKey] = RunLabel(
                    project: project, qualifier: "started \(starts[summary.sessionKey] ?? "")")
            }
        }
        return labels
    }

    /// The rendered start time of every run in the group, or nil if they cannot
    /// all be told apart by one.
    private static func distinctStarts(in group: [SessionSummary]) -> [String: String]? {
        var rendered: [String: String] = [:]
        for summary in group {
            // Hosted only: `tmuxSession` empty means adopted, and the timestamp
            // is a first sighting rather than a start.
            guard !summary.tmuxSession.isEmpty,
                let started = ISO8601.parse(summary.createdAt)
            else { return nil }
            rendered[summary.sessionKey] = started.formatted(.dateTime.hour().minute())
        }
        guard Set(rendered.values).count == group.count else { return nil }
        return rendered
    }
}
