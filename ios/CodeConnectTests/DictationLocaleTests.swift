import Speech
import XCTest

@testable import CodeConnect

/// The dictation locale ladder — the fix for the mic refusing before it ever
/// listened.
///
/// The shipped bug: recognition demanded an exact BCP-47 match between
/// `Locale.current` and the engine's supported list. Engines publish fixed
/// language-region pairs; real devices fuse UI language with residence
/// region — an English speaker in Riyadh is `en-SA`, which no list contains
/// — so the tap failed instantly with a message blaming a language Apple
/// supports fine.
@MainActor
final class DictationLocaleTests: XCTestCase {

    private func resolve(
        supported: [String], current: String, preferred: [String] = []
    ) -> String? {
        DictationController.resolveRecognitionLocale(
            supported: supported.map(Locale.init(identifier:)),
            current: Locale(identifier: current),
            preferred: preferred.map(Locale.init(identifier:))
        )?.identifier(.bcp47)
    }

    private let applesEnglishes = ["en-US", "en-GB", "en-IN", "en-AU", "ar-SA", "fr-FR"]

    /// The reported defect, exactly: English UI, Saudi region.
    func testAnEnglishSpeakerInAnUnlistedRegionGetsEnglish() {
        XCTAssertEqual(
            resolve(supported: applesEnglishes, current: "en_SA"),
            "en-US",
            "the language's own likely region carries an unlisted pairing home")
    }

    func testAnExactMatchAlwaysWins() {
        XCTAssertEqual(resolve(supported: applesEnglishes, current: "en_GB"), "en-GB")
        XCTAssertEqual(resolve(supported: applesEnglishes, current: "ar_SA"), "ar-SA")
    }

    /// A candidate's own region outranks the language's likely one. The
    /// extension keeps the exact-match branch out of play — without it this
    /// asserted the wrong rung of the ladder entirely.
    func testTheCandidatesRegionOutranksTheLikelyRegion() {
        XCTAssertEqual(
            resolve(
                supported: ["en-US", "en-IN"],
                current: "hi_IN",
                preferred: ["en-IN-u-hc-h23"]),
            "en-IN")
    }

    /// The current locale leads; the preferred list is the fallback, in the
    /// user's own order.
    func testPreferredLanguagesRescueAnUnsupportedCurrentLanguage() {
        XCTAssertEqual(
            resolve(
                supported: ["en-US", "fr-FR"],
                current: "fa_IR",
                preferred: ["fa-IR", "fr-FR", "en-US"]),
            "fr-FR")
    }

    /// Script is part of the language: Traditional Chinese must never
    /// silently become Simplified.
    func testScriptIsNeverCrossed() {
        XCTAssertEqual(
            resolve(supported: ["zh-CN", "zh-TW"], current: "zh-Hant-US"),
            "zh-TW")
        XCTAssertEqual(
            resolve(supported: ["zh-CN", "zh-TW"], current: "zh-Hans-US"),
            "zh-CN")
    }

    /// The likely region is computed for the language *with its script*:
    /// bare `zh` maximizes to Hans-CN, which would strand a Traditional
    /// speaker on the alphabetical fallback (HK) instead of Hant's own
    /// likely home (TW).
    func testTheLikelyRegionKeepsTheScript() {
        XCTAssertEqual(
            resolve(supported: ["zh-HK", "zh-TW"], current: "zh-Hant-US"),
            "zh-TW",
            "two Hant locales: the script's likely region decides, not the alphabet")
    }

    /// Locale extensions (hour-cycle, calendar) ride along on real devices;
    /// they must not defeat the match.
    func testLocaleExtensionsDoNotDefeatTheMatch() {
        XCTAssertEqual(
            resolve(supported: applesEnglishes, current: "en-US-u-hc-h23"),
            "en-US")
    }

    /// No language overlap is the one honest refusal.
    func testAGenuinelyUnshippedLanguageResolvesToNothing() {
        XCTAssertNil(resolve(supported: ["ja-JP"], current: "en_SA", preferred: ["en-SA"]))
    }

    /// With no likely-region entry either, the choice is still deterministic.
    func testTheTieBreakIsDeterministic() {
        XCTAssertEqual(
            resolve(supported: ["en-GB", "en-AU"], current: "en_SA"),
            "en-AU",
            "sorted first when neither the candidate's nor the likely region is listed")
    }

    /// Against Apple's real analyzer list on this OS. On a device the list
    /// is Apple's shipped languages and the reported shape — an English
    /// speaker with a Saudi region — must resolve. On the simulator the
    /// list is *empty* (measured), which is exactly why the analyzer path
    /// falls back to the legacy engine rather than blaming the language;
    /// the legacy catalogue must then carry the same shape.
    func testTheReportedDeviceShapeResolvesAgainstApplesRealLists() async throws {
        guard #available(iOS 26.0, *) else { return }
        let analyzer = await SpeechTranscriber.supportedLocales
        if !analyzer.isEmpty {
            let resolved = DictationController.resolveRecognitionLocale(
                supported: analyzer,
                current: Locale(identifier: "en_SA"),
                preferred: [Locale(identifier: "en-SA")])
            XCTAssertEqual(resolved?.language.languageCode?.identifier, "en")
        }
        // The fallback engine's real catalogue — present on simulator too.
        let legacy = Array(SFSpeechRecognizer.supportedLocales())
        XCTAssertFalse(legacy.isEmpty, "the legacy engine always has a catalogue")
        let resolved = DictationController.resolveRecognitionLocale(
            supported: legacy,
            current: Locale(identifier: "en_SA"),
            preferred: [Locale(identifier: "en-SA")])
        XCTAssertEqual(
            resolved?.language.languageCode?.identifier, "en",
            "en_SA must find an English in the legacy catalogue")
    }
}
