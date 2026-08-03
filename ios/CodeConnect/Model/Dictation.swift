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
    /// Smoothed input level, 0…1, for the meter. Proof the mic hears you.
    private(set) var level: Float = 0
    private(set) var startedAt: Date?

    var isRecording: Bool { phase == .recording }

    /// Everything heard so far, volatile tail included. The tail is *meant* to
    /// be taken: the words land in an editable field for review, so a
    /// half-finalized hypothesis is exactly as correctable as a committed one.
    var transcript: String {
        Self.join(finalizedText, volatileText)
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var engine: AVAudioEngine?
    private var relay: AudioTapRelay?
    private var legacyRecognizer: SFSpeechRecognizer?
    private var legacyTask: SFSpeechRecognitionTask?
    /// `AnalyzerSession` on iOS 26. Typed `Any` so the stored property does
    /// not need the availability its class has.
    private var analyzerSession: Any?
    private var resultsTask: Task<Void, Never>?

    // MARK: Lifecycle

    func start() async {
        guard !isRecording else { return }
        finalizedText = ""
        volatileText = ""
        level = 0

        guard await AVAudioApplication.requestRecordPermission() else {
            phase = .failed(
                reason: "Microphone access is off, so there is nothing to transcribe.",
                needsSettings: true)
            return
        }

        do {
            if #available(iOS 26.0, *) {
                try await startAnalyzer()
            } else {
                try await startLegacy()
            }
            startedAt = Date()
            phase = .recording
        } catch let error as DictationError {
            teardown()
            phase = .failed(reason: error.reason, needsSettings: error.needsSettings)
        } catch {
            teardown()
            phase = .failed(
                reason: "The microphone could not start: \(error.localizedDescription)",
                needsSettings: false)
        }
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

    // MARK: iOS 26 — SpeechAnalyzer

    @available(iOS 26.0, *)
    private func startAnalyzer() async throws {
        let locale = Locale.current
        let supported = await SpeechTranscriber.supportedLocales
        guard
            supported.contains(where: {
                $0.identifier(.bcp47) == locale.identifier(.bcp47)
            })
        else {
            throw DictationError(
                reason: "On-device dictation does not support this language yet.",
                needsSettings: false)
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

        let converter = BufferConverter(to: format)
        let relay = AudioTapRelay(
            onLevel: { [weak self] value in
                Task { @MainActor in self?.absorb(level: value) }
            },
            onBuffer: { buffer in
                if let converted = converter.convert(buffer) {
                    continuation.yield(AnalyzerInput(buffer: converted))
                }
            })
        self.relay = relay
        engine = try Self.makeEngine(relay: relay)

        resultsTask = Task { [weak self] in
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

        analyzerSession = AnalyzerSession(analyzer: analyzer, continuation: continuation)
    }

    /// The iOS 26 machinery that `stop`/`cancel` must reach. Its own class so
    /// the controller can store it without carrying the availability.
    @available(iOS 26.0, *)
    private final class AnalyzerSession {
        let analyzer: SpeechAnalyzer
        let continuation: AsyncStream<AnalyzerInput>.Continuation

        init(analyzer: SpeechAnalyzer, continuation: AsyncStream<AnalyzerInput>.Continuation) {
            self.analyzer = analyzer
            self.continuation = continuation
        }

        func finish() {
            continuation.finish()
            let analyzer = self.analyzer
            Task { try? await analyzer.finalizeAndFinishThroughEndOfInput() }
        }
    }

    // MARK: iOS 17–25 — SFSpeechRecognizer

    private func startLegacy() async throws {
        let status = await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
        }
        guard status == .authorized else {
            throw DictationError(
                reason: "Speech recognition access is off, so dictation cannot run.",
                needsSettings: true)
        }
        guard let recognizer = SFSpeechRecognizer(locale: .current) ?? SFSpeechRecognizer(),
            recognizer.isAvailable
        else {
            throw DictationError(
                reason: "Speech recognition is not available for this language right now.",
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

        let relay = AudioTapRelay(
            onLevel: { [weak self] value in
                Task { @MainActor in self?.absorb(level: value) }
            },
            onBuffer: { request.append($0) })
        self.relay = relay
        engine = try Self.makeEngine(relay: relay)

        legacyRecognizer = recognizer
        legacyTask = recognizer.recognitionTask(with: request) { [weak self] result, _ in
            // Only Sendable values cross to the main actor: the result object
            // stays on whatever queue Speech called us on.
            guard let result else { return }
            let text = result.bestTranscription.formattedString
            let isFinal = result.isFinal
            Task { @MainActor in self?.absorb(text: text, isFinal: isFinal) }
        }
    }

    // MARK: Shared plumbing

    private static func makeEngine(relay: AudioTapRelay) throws -> AVAudioEngine {
        let session = AVAudioSession.sharedInstance()
        try session.setCategory(.record, mode: .measurement, options: [.duckOthers])
        try session.setActive(true, options: [])

        let engine = AVAudioEngine()
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        input.installTap(onBus: 0, bufferSize: 2048, format: format) { buffer, _ in
            relay.handle(buffer)
        }
        engine.prepare()
        try engine.start()
        return engine
    }

    private func teardown() {
        if #available(iOS 26.0, *), let session = analyzerSession as? AnalyzerSession {
            session.finish()
        }
        analyzerSession = nil
        resultsTask?.cancel()
        resultsTask = nil
        // `cancel`, not `finish`: the transcript was already taken (or
        // deliberately discarded) — a final result arriving afterwards would
        // mutate text the user is now editing.
        legacyTask?.cancel()
        legacyTask = nil
        legacyRecognizer = nil
        relay = nil
        engine?.inputNode.removeTap(onBus: 0)
        engine?.stop()
        engine = nil
        try? AVAudioSession.sharedInstance().setActive(
            false, options: [.notifyOthersOnDeactivation])
        finalizedText = ""
        volatileText = ""
        level = 0
        startedAt = nil
    }

    private func absorb(level newValue: Float) {
        guard isRecording else { return }
        // Smoothed, or the meter strobes with every buffer.
        level = level * 0.55 + newValue * 0.45
    }

    private func absorb(text: String, isFinal: Bool) {
        guard isRecording else { return }
        if isFinal {
            // The new engine finalizes in increments; the legacy one restates
            // the whole utterance. Joining handles both: a restatement arrives
            // exactly once, at the end, with nothing finalized before it.
            if #available(iOS 26.0, *), analyzerSession != nil {
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
/// calls `handle` on its own realtime thread; the two closures forward work
/// out — buffers to whichever recognizer is live, levels to the main actor.
///
/// `@unchecked Sendable` is load-bearing and narrow: the closures are set once
/// before the engine starts and never mutated while it runs.
private final class AudioTapRelay: @unchecked Sendable {
    private let onLevel: @Sendable (Float) -> Void
    private let onBuffer: (AVAudioPCMBuffer) -> Void

    init(
        onLevel: @escaping @Sendable (Float) -> Void,
        onBuffer: @escaping (AVAudioPCMBuffer) -> Void
    ) {
        self.onLevel = onLevel
        self.onBuffer = onBuffer
    }

    func handle(_ buffer: AVAudioPCMBuffer) {
        onBuffer(buffer)
        onLevel(Self.rms(of: buffer))
    }

    /// Root-mean-square of the buffer, scaled into 0…1 for the meter.
    private static func rms(of buffer: AVAudioPCMBuffer) -> Float {
        guard let data = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return 0 }
        var sum: Float = 0
        for i in 0..<Int(buffer.frameLength) {
            sum += data[i] * data[i]
        }
        let value = (sum / Float(buffer.frameLength)).squareRoot()
        // Speech at a phone's arm length sits around 0.01–0.1 RMS; the ×8
        // puts conversation mid-meter instead of pinning it to the floor.
        return min(1, value * 8)
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
