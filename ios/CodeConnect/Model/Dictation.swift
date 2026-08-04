// `@preconcurrency` for one call: `AVAudioConverter.convert(to:error:withInputFrom:)`
// takes an input block that AVFoundation invokes *synchronously*, on this thread,
// during that call — it is a pull, not a callback. The header predates Sendable
// annotation, so the block imports as `@Sendable` and the compiler reads a
// same-thread closure as a concurrent one: it flags the `AVAudioPCMBuffer` it is
// handed and the local `served` flag it sets, neither of which is ever seen by a
// second thread. Suppressed at the import rather than by restructuring code that
// is already correct.
@preconcurrency import AVFoundation
import Foundation
import Speech

// =============================================================================
//  Dictation — live speech-to-text for the compose bar.
// =============================================================================
//
//  All recognition is Apple's own and none of it leaves the phone by this
//  app's hand: `SpeechAnalyzer`/`SpeechTranscriber` on iOS 26 (fully
//  on-device), `SFSpeechRecognizer` before that, pinned to on-device
//  recognition wherever the locale supports it. No third-party engine, no API
//  key, no audio upload of ours.
//
//  The controller's whole job is to turn a hot microphone into two strings:
//  `finalizedText`, which the recognizer has committed, and `volatileText`,
//  its current hypothesis. The compose bar renders the hypothesis in
//  `textTertiary` and lets it solidify — uncertainty shown as state, never
//  hidden. **Dictation only ever inserts text.** Sending stays the compose
//  bar's separate, deliberate act.

/// Why dictation could not start, in words the compose bar can show verbatim.
struct DictationError: Error {
    let reason: String
    /// True when the fix lives in Settings (a permission), so the note above
    /// the composer can carry the link rather than a dead explanation.
    let needsSettings: Bool
}

@MainActor
@Observable
final class DictationController {
    enum Phase: Equatable {
        case idle
        /// Between the tap and the first live buffer: permission prompts and
        /// — on a locale's first use — Apple's model download happen here.
        /// Its own state, because during a download the old `idle` looked
        /// inert, invited more taps, and each tap raced another engine up.
        case starting
        case recording
        /// Rendered as a visible line above the composer — the same rule as a
        /// blocked send: a control that cannot act says why, on screen.
        case failed(reason: String, needsSettings: Bool)
    }

    private(set) var phase: Phase = .idle
    /// Text the recognizer has committed. Rendered at full strength.
    private(set) var finalizedText = ""
    /// The recognizer's current hypothesis. Rendered `textTertiary`, replaced
    /// as it solidifies.
    private(set) var volatileText = ""

    var isRecording: Bool { phase == .recording }
    var isStarting: Bool { phase == .starting }

    /// Bumped by every teardown; a start that resumes from its awaits into a
    /// generation it does not own was cancelled mid-flight and must build
    /// nothing on the rubble.
    private var generation = 0

    /// Everything heard so far, volatile tail included. The tail is *meant* to
    /// be taken: the words land in an editable field for review, so a
    /// half-finalized hypothesis is exactly as correctable as a committed one.
    var transcript: String {
        Self.join(finalizedText, volatileText)
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Everything one recognition run owns — engine, tap relay, recognizer
    /// machinery — built *off* the controller and installed only if the
    /// start that built it is still the current one. A stale start disposes
    /// what it built and touches nothing shared: the race this closes had a
    /// cancelled start's failure overwriting a newer start's state, and its
    /// cleanup bulldozing whichever session currently owned the controller.
    @MainActor
    final class Session {
        let engine: AVAudioEngine
        fileprivate let relay: AudioTapRelay
        let isAnalyzer: Bool
        var legacyRecognizer: SFSpeechRecognizer?
        var legacyTask: SFSpeechRecognitionTask?
        /// Finishes the iOS 26 analyzer stream, when this is that kind.
        var finishAnalyzer: (() -> Void)?
        var resultsTask: Task<Void, Never>?
        /// Whether this session holds one claim on the shared audio session
        /// — set by the builder that claimed, consumed by `dispose`.
        fileprivate var ownsAudioClaim = false
        private var disposed = false
        #if DEBUG
            private(set) var disposeCount = 0
        #endif

        fileprivate init(engine: AVAudioEngine, relay: AudioTapRelay, isAnalyzer: Bool) {
            self.engine = engine
            self.relay = relay
            self.isAnalyzer = isAnalyzer
        }

        /// Idempotent, and the only teardown there is: whoever holds the
        /// session — the controller, or the stale start that built it —
        /// calls this exactly where it stands. Releases exactly the claims
        /// it owns: the audio session is shared, and deactivating it
        /// outright would silence whoever holds it now.
        func dispose() {
            guard !disposed else { return }
            disposed = true
            #if DEBUG
                disposeCount += 1
            #endif
            finishAnalyzer?()
            finishAnalyzer = nil
            resultsTask?.cancel()
            resultsTask = nil
            // `cancel`, not `finish`: a final result arriving afterwards
            // would mutate text the user is now editing.
            legacyTask?.cancel()
            legacyTask = nil
            legacyRecognizer = nil
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
            if ownsAudioClaim {
                ownsAudioClaim = false
                DictationController.releaseAudioSession()
            }
        }
    }

    private var session: Session?
    #if DEBUG
        /// Test seams: replace the real engine builders (which need a live
        /// audio stack and permission prompts no test host has) with a
        /// controlled backend, and observe what is currently installed.
        var sessionBackendForTesting: (@MainActor () async throws -> Session)?
        var installedSessionForTesting: Session? { session }

        /// A minimal session for the race tests: a never-started engine and
        /// a relay that goes nowhere. It claims the shared audio session the
        /// way a real build does, so claim accounting is observable.
        static func makeStubSessionForTesting() -> Session {
            let stub = Session(
                engine: AVAudioEngine(),
                relay: AudioTapRelay(onBuffer: { _ in }),
                isAnalyzer: false)
            stub.ownsAudioClaim = (try? claimAudioSession()) != nil
            return stub
        }

        /// The live claim count, for the reverse-order supersession test.
        static var audioClaimsForTesting: Int { audioClaims }
    #endif

    // MARK: Lifecycle

    func start() async {
        guard phase != .recording, phase != .starting else { return }
        phase = .starting
        let owned = generation
        finalizedText = ""
        volatileText = ""

        // Built off to the side, installed only if this start still owns the
        // controller. A stale build — cancelled mid-download, superseded by
        // a newer tap — disposes what it made and *says nothing*: its
        // failure is not news about the current state, and publishing it
        // was the race.
        let built: Session
        do {
            #if DEBUG
                if let backend = sessionBackendForTesting {
                    built = try await backend()
                } else {
                    built = try await buildSession()
                }
            #else
                built = try await buildSession()
            #endif
        } catch {
            guard generation == owned, phase == .starting else { return }
            if let error = error as? DictationError {
                phase = .failed(reason: error.reason, needsSettings: error.needsSettings)
            } else {
                phase = .failed(
                    reason: "The microphone could not start: \(error.localizedDescription)",
                    needsSettings: false)
            }
            return
        }
        guard generation == owned, phase == .starting else {
            built.dispose()
            return
        }
        session = built
        phase = .recording
    }

    /// The real builder: the analyzer engine where the OS and its catalogue
    /// allow, the legacy engine otherwise.
    private func buildSession() async throws -> Session {
        // Permission is the builder's first act, not the controller's: the
        // injected test backend replaces the *whole* startup, prompts
        // included — a test host has no microphone to ask about.
        guard await AVAudioApplication.requestRecordPermission() else {
            throw DictationError(
                reason: "Microphone access is off, so there is nothing to transcribe.",
                needsSettings: true)
        }
        if #available(iOS 26.0, *) {
            return try await buildAnalyzerSession()
        }
        return try await buildLegacySession()
    }

    /// Ends the session and returns what was heard. The caller stages it in
    /// the field; nothing here sends anything anywhere.
    func stop() -> String {
        let heard = transcript
        teardown()
        phase = .idle
        return heard
    }

    /// Ends the session and discards the transcript.
    func cancel() {
        teardown()
        phase = .idle
    }

    /// A failure note should not outlive the user's next act. Typing is that
    /// act as much as retrying the mic is.
    func clearFailure() {
        if case .failed = phase { phase = .idle }
    }

    // MARK: Locale resolution

    /// The locale recognition should run in, chosen from what the engine
    /// supports and what this user actually speaks — never by demanding an
    /// exact match on `Locale.current`.
    ///
    /// Exact matching was the shipped bug: engines publish a fixed set of
    /// language-region pairs (`en-US`, `en-GB`, `ar-SA`, …), while a real
    /// device fuses UI language with residence region — an English speaker
    /// in Riyadh is `en-SA`, a pair no list will ever contain — so the mic
    /// refused before listening, blaming a "language" Apple supports fine.
    ///
    /// The ladder, per candidate (current locale first, then the user's
    /// ordered language list): exact BCP-47; else same language *and
    /// script* (so Traditional Chinese never silently becomes Simplified),
    /// preferring the candidate's own region, then the language's likely
    /// region (`en` maximizes to `en-Latn-US`, so plain English lands on
    /// `en-US`), then the sorted first for determinism. `nil` means the
    /// user's language genuinely is not shipped — the one honest time to
    /// say so.
    static func resolveRecognitionLocale(
        supported: [Locale], current: Locale, preferred: [Locale]
    ) -> Locale? {
        let tag = { (locale: Locale) in locale.identifier(.bcp47).lowercased() }
        let languageScript = { (locale: Locale) -> (String, String) in
            // `maximalIdentifier` fills in likely subtags (en → en-Latn-US),
            // giving every locale a comparable script even when unstated.
            let maximal = Locale.Language(identifier: locale.language.maximalIdentifier)
            return (
                maximal.languageCode?.identifier.lowercased() ?? "",
                maximal.script?.identifier.lowercased() ?? ""
            )
        }
        for candidate in [current] + preferred {
            if let exact = supported.first(where: { tag($0) == tag(candidate) }) {
                return exact
            }
            let wanted = languageScript(candidate)
            guard !wanted.0.isEmpty else { continue }
            let speakers = supported.filter { languageScript($0) == wanted }
            guard !speakers.isEmpty else { continue }
            if let region = candidate.region,
                let sameRegion = speakers.first(where: { $0.region == region })
            {
                return sameRegion
            }
            // The likely region of the candidate's language *with its
            // script*: bare `zh` maximizes to Hans-CN and would send a
            // Traditional-script speaker to the wrong likely home; zh-Hant
            // maximizes to TW.
            let maximal = Locale.Language(identifier: candidate.language.maximalIdentifier)
            let likely = Locale.Language(
                identifier: Locale.Language(
                    languageCode: maximal.languageCode, script: maximal.script, region: nil
                ).maximalIdentifier
            ).region
            if let likely, let home = speakers.first(where: { $0.region == likely }) {
                return home
            }
            return speakers.min { tag($0) < tag($1) }
        }
        return nil
    }

    /// The refusal for a language no engine ships, named in the user's own
    /// terms — the old copy blamed "this language" while refusing English.
    private static func unsupportedLanguage(current: Locale) -> DictationError {
        let code = current.language.languageCode?.identifier
        let name = code.flatMap { current.localizedString(forLanguageCode: $0) }
        return DictationError(
            reason: "Dictation does not support \(name ?? "this language") yet.",
            needsSettings: false)
    }

    // MARK: iOS 26 — SpeechAnalyzer

    @available(iOS 26.0, *)
    private func buildAnalyzerSession() async throws -> Session {
        let supported = await SpeechTranscriber.supportedLocales
        guard !supported.isEmpty else {
            // Measured: the simulator ships `SpeechAnalyzer` with an empty
            // locale catalogue, so *every* start died here blaming the
            // user's language. An engine with no languages at all is not
            // that — it is an engine that cannot serve anyone; the legacy
            // recognizer keeps its own catalogue even there.
            return try await buildLegacySession()
        }
        guard
            let locale = Self.resolveRecognitionLocale(
                supported: supported,
                current: .current,
                preferred: Locale.preferredLanguages.map(Locale.init))
        else {
            // The analyzer not shipping this language is not the last word —
            // the legacy engine carries its own, larger catalogue, and its
            // resolver issues the honest refusal if it cannot serve either.
            return try await buildLegacySession()
        }

        let transcriber = SpeechTranscriber(
            locale: locale,
            transcriptionOptions: [],
            reportingOptions: [.volatileResults],
            attributeOptions: [])

        // First use per locale downloads the model. Subsequent starts are
        // instant; the await here is honest about the once.
        if let installation = try await AssetInventory.assetInstallationRequest(
            supporting: [transcriber])
        {
            try await installation.downloadAndInstall()
        }

        let analyzer = SpeechAnalyzer(modules: [transcriber])
        guard
            let format = await SpeechAnalyzer.bestAvailableAudioFormat(
                compatibleWith: [transcriber])
        else {
            throw DictationError(
                reason: "Transcription could not agree on an audio format.",
                needsSettings: false)
        }

        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
        try await analyzer.start(inputSequence: stream)
        let finishAnalyzer = {
            continuation.finish()
            Task { try? await analyzer.finalizeAndFinishThroughEndOfInput() }
            return
        }

        let converter = BufferConverter(to: format)
        let relay = AudioTapRelay(onBuffer: { buffer in
            if let converted = converter.convert(buffer) {
                continuation.yield(AnalyzerInput(buffer: converted))
            }
        })
        let engine: AVAudioEngine
        do {
            engine = try Self.makeEngine(relay: relay)
        } catch {
            // The analyzer is already running; a builder that throws must
            // not leave its own partial machinery humming.
            finishAnalyzer()
            throw error
        }

        let built = Session(engine: engine, relay: relay, isAnalyzer: true)
        built.ownsAudioClaim = true
        built.finishAnalyzer = finishAnalyzer
        built.resultsTask = Task { [weak self] in
            do {
                for try await result in transcriber.results {
                    let text = String(result.text.characters)
                    let isFinal = result.isFinal
                    self?.absorb(text: text, isFinal: isFinal)
                }
            } catch {
                // The stream ending on stop/cancel arrives here too; a live
                // failure surfaces as a stalled meter and an empty transcript,
                // both visible, so there is nothing further to report.
            }
        }
        return built
    }

    // MARK: iOS 17–25 — SFSpeechRecognizer

    private func buildLegacySession() async throws -> Session {
        let status = await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
        }
        guard status == .authorized else {
            throw DictationError(
                reason: "Speech recognition access is off, so dictation cannot run.",
                needsSettings: true)
        }
        guard
            let locale = Self.resolveRecognitionLocale(
                supported: Array(SFSpeechRecognizer.supportedLocales()),
                current: .current,
                preferred: Locale.preferredLanguages.map(Locale.init))
        else {
            throw Self.unsupportedLanguage(current: .current)
        }
        guard let recognizer = SFSpeechRecognizer(locale: locale), recognizer.isAvailable
        else {
            throw DictationError(
                reason: "Speech recognition is not available right now.",
                needsSettings: false)
        }

        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        // On-device wherever the locale supports it — the audio then never
        // leaves the phone on this path either. Where it does not, Apple's
        // server does the work; that trade is the OS's, not ours to hide.
        if recognizer.supportsOnDeviceRecognition {
            request.requiresOnDeviceRecognition = true
        }

        let relay = AudioTapRelay(onBuffer: { request.append($0) })
        let built = Session(
            engine: try Self.makeEngine(relay: relay), relay: relay, isAnalyzer: false)
        built.ownsAudioClaim = true

        built.legacyRecognizer = recognizer
        built.legacyTask = recognizer.recognitionTask(with: request) { [weak self] result, _ in
            // Only Sendable values cross to the main actor: the result object
            // stays on whatever queue Speech called us on.
            guard let result else { return }
            let text = result.bestTranscription.formattedString
            let isFinal = result.isFinal
            Task { @MainActor in self?.absorb(text: text, isFinal: isFinal) }
        }
        return built
    }

    // MARK: Shared plumbing

    /// The audio session is process-wide: whoever deactivates it silences
    /// every holder, so activation is a counted *claim* and only the last
    /// release deactivates. Without this, a stale build disposing after a
    /// successor had installed cut the successor's live microphone.
    private static var audioClaims = 0

    private static func claimAudioSession() throws {
        let shared = AVAudioSession.sharedInstance()
        try shared.setCategory(.record, mode: .measurement, options: [.duckOthers])
        try shared.setActive(true, options: [])
        audioClaims += 1
    }

    private static func releaseAudioSession() {
        audioClaims = max(0, audioClaims - 1)
        guard audioClaims == 0 else { return }
        try? AVAudioSession.sharedInstance().setActive(
            false, options: [.notifyOthersOnDeactivation])
    }

    /// Failure-transactional: a throw from any step rolls back everything
    /// this call did — tap, engine, and the audio claim — so a failed build
    /// leaves no session active and nothing ducked, with no `Session` object
    /// needed to carry the cleanup.
    private static func makeEngine(relay: AudioTapRelay) throws -> AVAudioEngine {
        try claimAudioSession()
        let engine = AVAudioEngine()
        do {
            let input = engine.inputNode
            let format = input.outputFormat(forBus: 0)
            input.installTap(onBus: 0, bufferSize: 2048, format: format) { buffer, _ in
                relay.handle(buffer)
            }
            engine.prepare()
            try engine.start()
        } catch {
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
            releaseAudioSession()
            throw error
        }
        return engine
    }

    private func teardown() {
        // The bump is what orphans any start still in flight: it resumes,
        // sees a generation it does not own, and disposes its own build.
        generation += 1
        session?.dispose()
        session = nil
        finalizedText = ""
        volatileText = ""
    }

    private func absorb(text: String, isFinal: Bool) {
        guard isRecording else { return }
        if isFinal {
            // The new engine finalizes in increments; the legacy one restates
            // the whole utterance. Joining handles both: a restatement arrives
            // exactly once, at the end, with nothing finalized before it.
            if session?.isAnalyzer == true {
                finalizedText = Self.join(finalizedText, text)
            } else {
                finalizedText = text
            }
            volatileText = ""
        } else {
            volatileText = text
        }
    }

    private static func join(_ a: String, _ b: String) -> String {
        guard !a.isEmpty else { return b }
        guard !b.isEmpty else { return a }
        if a.hasSuffix(" ") || b.hasPrefix(" ") { return a + b }
        return a + " " + b
    }
}

// MARK: - The audio thread's world

/// Everything the tap callback touches, and nothing it must not. Core Audio
/// calls `handle` on its own realtime thread; the closure forwards buffers
/// to whichever recognizer is live.
///
/// `@unchecked Sendable` is load-bearing and narrow: the closure is set once
/// before the engine starts and never mutated while it runs.
private final class AudioTapRelay: @unchecked Sendable {
    private let onBuffer: (AVAudioPCMBuffer) -> Void

    init(onBuffer: @escaping (AVAudioPCMBuffer) -> Void) {
        self.onBuffer = onBuffer
    }

    func handle(_ buffer: AVAudioPCMBuffer) {
        onBuffer(buffer)
    }
}

/// Resamples tap buffers into the analyzer's preferred format. Stateful — a
/// resampler carries filter history between calls — so it lives for the whole
/// session and is only ever touched from the tap thread.
private final class BufferConverter: @unchecked Sendable {
    private let format: AVAudioFormat
    private var converter: AVAudioConverter?

    init(to format: AVAudioFormat) {
        self.format = format
    }

    func convert(_ buffer: AVAudioPCMBuffer) -> AVAudioPCMBuffer? {
        guard buffer.format != format else { return buffer }
        if converter == nil || converter?.inputFormat != buffer.format {
            converter = AVAudioConverter(from: buffer.format, to: format)
        }
        guard let converter else { return nil }

        let ratio = format.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up)) + 16
        guard let output = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else {
            return nil
        }

        var error: NSError?
        // `nonisolated(unsafe)` because the block below is `@Sendable` by import
        // and this flag is not: it is written and read only inside that block,
        // which AVFoundation runs synchronously on this thread before `convert`
        // returns. There is no second thread to race with, and a lock here would
        // be machinery guarding nothing.
        nonisolated(unsafe) var served = false
        converter.convert(to: output, error: &error) { _, status in
            if served {
                status.pointee = .noDataNow
                return nil
            }
            served = true
            status.pointee = .haveData
            return buffer
        }
        return error == nil ? output : nil
    }
}
