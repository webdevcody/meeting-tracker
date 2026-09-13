import AVFoundation
import Foundation
import Speech

/// One on-device SpeechAnalyzer + SpeechTranscriber per audio source.
final class TrackTranscriber {
    let source: Source
    let transcriber: SpeechTranscriber
    let analyzerFormat: AVAudioFormat
    private let analyzer: SpeechAnalyzer
    private let converter = BufferConverter()
    private let conversionErrors = RateLimitedLog()
    private let input: AsyncStream<AnalyzerInput>
    private let builder: AsyncStream<AnalyzerInput>.Continuation
    private var resultsTask: Task<Void, Error>?
    private var feedTask: Task<Void, Never>?
    private var inputDropped = 0

    static func makeTranscriber(locale: Locale, fast: Bool) -> SpeechTranscriber {
        SpeechTranscriber(locale: locale,
                          transcriptionOptions: [],
                          reportingOptions: fast ? [.fastResults] : [],
                          attributeOptions: [])
    }

    /// Downloads the speech model for the transcriber's locale if it isn't installed.
    static func ensureAssets(for transcriber: SpeechTranscriber,
                             onProgress: @escaping @Sendable (Double) -> Void) async throws {
        guard let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) else {
            return
        }
        let progress = request.progress
        let poller = Task {
            while !Task.isCancelled {
                onProgress(progress.fractionCompleted)
                try? await Task.sleep(for: .milliseconds(250))
            }
        }
        defer { poller.cancel() }
        try await request.downloadAndInstall()
    }

    init(source: Source, transcriber: SpeechTranscriber, analyzerFormat: AVAudioFormat) {
        self.source = source
        self.transcriber = transcriber
        self.analyzerFormat = analyzerFormat
        self.analyzer = SpeechAnalyzer(modules: [transcriber])
        // Bounded for the same reason as Recorder's streams: if the analyzer stalls, drop
        // audio rather than accumulate it for hours.
        (input, builder) = AsyncStream.makeStream(of: AnalyzerInput.self,
                                                  bufferingPolicy: .bufferingNewest(Recorder.maxQueuedBuffers))
    }

    func start(session: Session, audio: AsyncStream<AVAudioPCMBuffer>) async throws {
        try await analyzer.prepareToAnalyze(in: analyzerFormat)

        let source = self.source
        let transcriber = self.transcriber
        resultsTask = Task {
            do {
                for try await result in transcriber.results where result.isFinal {
                    let text = String(result.text.characters).trimmingCharacters(in: .whitespacesAndNewlines)
                    // Skip empty / punctuation-only results (e.g. a lone ".").
                    guard text.rangeOfCharacter(from: .alphanumerics) != nil else { continue }
                    await session.add(Segment(source: source,
                                              text: text,
                                              start: result.range.start.seconds,
                                              end: result.range.end.seconds))
                }
            } catch is CancellationError {
                // `cancel()` — a discard: the stream was ended on purpose, nothing to say.
                throw CancellationError()
            } catch {
                // Surface immediately: audio keeps recording, but this track's transcript stops.
                log("⚠ \(source.label) transcriber stopped: \(error) — audio is still being recorded")
                throw error
            }
        }

        try await analyzer.start(inputSequence: input)

        feedTask = Task { [self] in
            for await buffer in audio {
                do {
                    let converted = try converter.convert(buffer, to: analyzerFormat)
                    guard converted.frameLength > 0 else { continue }
                    if case .dropped = builder.yield(AnalyzerInput(buffer: converted)) {
                        inputDropped += 1
                        if inputDropped == 1 || inputDropped % 1000 == 0 {
                            log("⚠ \(source.label) speech analyzer is falling behind; dropped \(inputDropped) buffer(s) so far")
                        }
                    }
                } catch {
                    conversionErrors.log("⚠ \(source.label) conversion failed: \(error)")
                }
            }
            builder.finish()
        }
    }

    /// Waits for the audio stream to drain, then finalizes any pending recognition.
    func finish() async {
        await feedTask?.value
        builder.finish()
        do {
            try await analyzer.finalizeAndFinishThroughEndOfInput()
        } catch {
            log("⚠ \(source.label) transcriber finalize failed: \(error)")
        }
        do {
            _ = try await resultsTask?.value
        } catch {
            // already logged when it happened
        }
    }

    func cancel() async {
        feedTask?.cancel()
        builder.finish()
        await analyzer.cancelAndFinishNow()
        resultsTask?.cancel()
    }
}
