import XCTest

@testable import CodeConnect

/// The two redundancy rules that cannot be proven by looking at a screen.
///
/// The other three cuts — the numbered options, the wait clock, the band count —
/// are visible in a rendered card and are asserted there. These two are decisions
/// about *content*, and the fixtures happen not to contain the duplicate, so the
/// rule itself is tested rather than one example of it.
final class RedundancyTests: XCTestCase {

    // MARK: A description that only repeats the command is not a description

    func testARestatementOfTheCommandIsSuppressed() {
        // The exact case from the owner's screenshot: a card showing
        // `echo SECONDCARD`, and one line below it, `Echo SECONDCARD`.
        XCTAssertFalse(
            DecisionCardView.saysSomethingNew("Echo SECONDCARD", beyond: "echo SECONDCARD"),
            "sentence-cased restatement must be recognised as the command again")
    }

    func testPunctuationAndSpacingCannotSmuggleADuplicatePast() {
        XCTAssertFalse(
            DecisionCardView.saysSomethingNew("Echo 'SECONDCARD'.", beyond: "echo SECONDCARD"))
        XCTAssertFalse(
            DecisionCardView.saysSomethingNew("  echo   SECONDCARD  ", beyond: "echo SECONDCARD"))
    }

    func testADescriptionThatAddsAReasonSurvives() {
        // The whole point of keeping the field: a goal, a destination or a
        // consequence is the thing a reader cannot get from the command itself.
        XCTAssertTrue(
            DecisionCardView.saysSomethingNew(
                "Print the marker so the test can see the run reached this point",
                beyond: "echo SECONDCARD"))
        XCTAssertTrue(
            DecisionCardView.saysSomethingNew(
                "Force-push, discarding remote commits", beyond: "git push --force origin main"))
    }

    func testAnEmptyCommandNeverSwallowsADescription() {
        // Degenerate input must fail toward showing the reader more, not less.
        XCTAssertTrue(DecisionCardView.saysSomethingNew("Reads the README", beyond: ""))
    }
}
