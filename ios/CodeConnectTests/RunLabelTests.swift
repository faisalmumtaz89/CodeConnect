import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// What a run is called, and when the app admits it cannot tell two apart.
final class RunLabelTests: XCTestCase {

    private func summary(
        uid: String, project: String, created: String = "2026-08-07T09:05:00.000Z"
    ) throws -> SessionSummary {
        let json = """
            {"session_uid":"\(uid)","session_id":"cc-1",\
            "cwd":"/srv/dev/code/checkout-7","lifecycle":"live","link":"attached","last_seq":7,\
            "created_at":"\(created)","updated_at":"2026-08-07T10:00:00.000Z",\
            "project_label":"\(project)"}
            """
        return try JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
    }

    private func labels(_ summaries: [SessionSummary]) -> [String: RunLabel] {
        RunLabel.labels(for: summaries)
    }

    /// **The daemon's label, not one derived here.** The working directory says
    /// `checkout-7` and the daemon says `Aion`, so a label that came from `cwd`
    /// would be visibly wrong rather than accidentally right.
    func testARunIsCalledByTheProjectTheDaemonNamedAndNotByItsDirectory() throws {
        let one = try summary(uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "Aion")
        XCTAssertEqual(one.cwd, "/srv/dev/code/checkout-7", "the premise: they differ")
        XCTAssertEqual(labels([one])[one.sessionKey], RunLabel(project: "Aion", qualifier: nil))
    }

    /// The fallbacks all reach for something that is not a project. Saying so is
    /// the only honest answer.
    func testARunWithNoProjectSaysSoRatherThanReachingForTheWorkingDirectory() throws {
        let one = try summary(uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "")
        let label = try XCTUnwrap(labels([one])[one.sessionKey])
        XCTAssertEqual(label, .unknown)
        XCTAssertEqual(label.inline, "Unknown project")
        XCTAssertFalse(label.inline.contains("checkout-7"), "not the cwd, which does name one")
        XCTAssertFalse(label.inline.contains("cc-1"), "and not the tmux counter")
    }

    /// **Runs in one project are told apart by when they started**, on runs CodeConnect started (the
    /// daemon's summary names no tmux session), in all three shapes. One run needs no qualifier. Two that
    /// started in different minutes are each qualified with the time it started — the rendered times
    /// themselves, so a qualifier built from anything else, a uid tail or an index, fails here. Two that
    /// started in the same minute get neither: a qualifier that repeats looks like it tells them apart and
    /// does not, and two rows that read alike are the honest picture of two runs started together.
    func testRunsInOneProjectAreToldApartByWhenTheyStarted() throws {
        let early = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "Aion", created: "2026-08-07T09:05:00.000Z")
        let later = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS", project: "Aion", created: "2026-08-07T11:47:00.000Z")
        let sameMinute = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQT", project: "Aion", created: "2026-08-07T09:05:59.000Z")
        let started = { (run: SessionSummary) -> String in
            "started " + ISO8601.parse(run.createdAt)!.formatted(.dateTime.hour().minute())
        }

        XCTAssertEqual(labels([early])[early.sessionKey], RunLabel(project: "Aion", qualifier: nil), "one run")

        let two = labels([early, later])
        XCTAssertEqual(two[early.sessionKey], RunLabel(project: "Aion", qualifier: started(early)), "\(two)")
        XCTAssertEqual(two[later.sessionKey], RunLabel(project: "Aion", qualifier: started(later)), "\(two)")

        let tied = labels([early, sameMinute])
        XCTAssertNil(try XCTUnwrap(tied[early.sessionKey]).qualifier, "\(tied)")
        XCTAssertNil(try XCTUnwrap(tied[sameMinute.sessionKey]).qualifier, "\(tied)")
    }

    func testATimestampThatDoesNotParseWithholdsTheQualifierRatherThanGuessing() throws {
        let good = try summary(uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "Aion")
        let broken = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS", project: "Aion", created: "not a date")
        let all = labels([good, broken])
        XCTAssertNil(try XCTUnwrap(all[good.sessionKey]).qualifier)
        XCTAssertNil(try XCTUnwrap(all[broken.sessionKey]).qualifier)
    }

    /// Runs in different projects are already told apart by the project.
    func testRunsInDifferentProjectsAreNotGivenQualifiersTheyDoNotNeed() throws {
        let aion = try summary(uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "Aion")
        let other = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS", project: "Ledger",
            created: "2026-08-07T11:47:00.000Z")
        let all = labels([aion, other])
        XCTAssertNil(try XCTUnwrap(all[aion.sessionKey]).qualifier)
        XCTAssertNil(try XCTUnwrap(all[other.sessionKey]).qualifier)
    }

    /// **A daemon that names nothing still has to produce a usable list.** Every
    /// row would otherwise read `Unknown project`, which is the pile of
    /// identical rows this replaced rather than an improvement on it. A start
    /// time is not a name, so it invents nothing.
    func testUnnamedRunsAreStillToldApartWhenTheyHonestlyCanBe() throws {
        let one = try summary(uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "")
        let two = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS", project: "", created: "2026-08-07T11:47:00.000Z")
        let all = labels([one, two])
        let first = try XCTUnwrap(all[one.sessionKey])
        XCTAssertEqual(first.project, "Unknown project", "and still not a cwd or a counter")
        XCTAssertNotNil(first.qualifier)
        XCTAssertNotEqual(first, try XCTUnwrap(all[two.sessionKey]))
    }

    /// The same guard as everywhere else: when the start time cannot tell them
    /// apart, they share a label rather than wear one that pretends.
    func testUnnamedRunsStartedInTheSameMinuteShareOneLabel() throws {
        let one = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "", created: "2026-08-07T09:05:01.000Z")
        let two = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS", project: "", created: "2026-08-07T09:05:44.000Z")
        let all = labels([one, two])
        XCTAssertEqual(all[one.sessionKey], .unknown)
        XCTAssertEqual(all[two.sessionKey], .unknown)
    }

    /// **A directory really can be called `Unknown project`.** The daemon does
    /// not forbid it, so a run named that and a run named nothing draw the same
    /// words — and must be told apart like any other collision, rather than
    /// falling into separate groups that each think they are alone.
    func testARunActuallyNamedUnknownProjectCollidesWithAnUnnamedOne() throws {
        let unnamed = try summary(uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", project: "")
        let literal = try summary(
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS", project: "Unknown project",
            created: "2026-08-07T11:47:00.000Z")
        let all = labels([unnamed, literal])
        let one = try XCTUnwrap(all[unnamed.sessionKey])
        let two = try XCTUnwrap(all[literal.sessionKey])
        XCTAssertEqual(one.project, two.project, "the premise: they read the same")
        XCTAssertNotEqual(one, two, "so they cannot also be labelled the same")
    }

    /// The separator is punctuation for the eye; a screen reader gets a pause.
    func testTheSpokenLabelDropsTheSeparator() {
        let plain = RunLabel(project: "Aion", qualifier: nil)
        XCTAssertEqual(plain.inline, "Aion")
        XCTAssertEqual(plain.spoken, "Aion")

        let qualified = RunLabel(project: "Aion", qualifier: "started 09:05")
        XCTAssertEqual(qualified.inline, "Aion · started 09:05")
        XCTAssertEqual(qualified.spoken, "Aion, started 09:05")
    }
}
