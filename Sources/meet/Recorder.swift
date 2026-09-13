import AVFoundation
import Foundation
import Synchronization

/// Owns the audio sources and per-source file writers, applies the pause gate and the
/// per-source mute gate, keeps every track locked to one shared recording clock, and fans
/// each source's buffers out to an AsyncStream for the transcriber.
///
/// Mute: a muted source keeps capturing, but every buffer it delivers is replaced with
/// silence of the same length before it reaches the file and the transcriber — the track
/// stays in sync with the others and the transcriber hears nothing, so a muted stretch is
/// silence in the audio and absent from the transcript.
///
/// Alignment: each capture callback comes with the host-clock time of its first frame. A
/// track's expected position is that time on the recording clock (pauses excluded); when a
/// buffer lands later than where the file currently ends, the gap is filled with silence
/// first (system audio while the output device is idle, a tap restart on device change, a
/// Bluetooth mic re-linking); when it lands earlier the overlap is trimmed. Idle sources are
/// padded on a timer too, so the files never fall more than a couple of seconds behind, and
/// at stop every track is padded to the same final length. The silence goes to the
/// transcriber as well, so transcript timestamps share the timeline.
final class Recorder {
    struct TrackPipeline {
        let writer: TrackWriter
        let stream: AsyncStream<AVAudioPCMBuffer>
        let continuation: AsyncStream<AVAudioPCMBuffer>.Continuation
        let writeErrors = RateLimitedLog()
        var dropped = 0
        /// Seconds of silence inserted where the source delivered nothing.
        var paddedSeconds: Double = 0
        /// Seconds discarded because the source ran ahead of the shared timeline.
        var trimmedSeconds: Double = 0
        /// Host time (seconds) at which the last real buffer ended; 0 until the first one.
        var lastEnd: Double = 0
    }

    /// Recording time on the host (mach) clock, shared by every source. Touched from audio
    /// threads under a lock.
    struct Clock {
        /// Host time the recording (and the session timer) started; nil until `startClock()`.
        var start: Double?
        var paused = false
        var pausedAccum: Double = 0
        var pauseStart: Double = 0

        /// Seconds of recording (pauses excluded) at host time `t`; nil while paused or
        /// before the clock has started. Negative for audio captured before the start.
        func position(at t: Double) -> Double? {
            guard let start, !paused else { return nil }
            return t - start - pausedAccum
        }

        /// Where the recording ends if it is stopped at host time `t`.
        func finalPosition(at t: Double) -> Double {
            guard let start else { return 0 }
            return (paused ? pauseStart : t) - start - pausedAccum
        }

        mutating func setPaused(_ p: Bool, at t: Double) {
            guard p != paused else { return }
            if p { pauseStart = t } else { pausedAccum += t - pauseStart }
            paused = p
        }
    }

    /// Max buffers queued per source for the transcriber. If the analyzer falls behind real
    /// time the oldest audio is dropped (a transcript gap, logged) instead of growing memory
    /// without bound for the rest of a multi-hour session. Worst case ≈ 2048 × 16 KiB = 32 MiB.
    static let maxQueuedBuffers = 2048

    /// A source that has delivered nothing for this long while it should be running gets
    /// torn down and rebuilt (sleep/wake, device removed, HAL hiccup).
    static let staleAfter: TimeInterval = 30
    static let restartCooldown: TimeInterval = 30

    /// A source that has delivered nothing for this long is padded with silence up to
    /// (now − idleAfter) on every health tick; the remainder is filled exactly when its next
    /// real buffer arrives. Must exceed a buffer's duration plus delivery latency so a late
    /// buffer never lands on top of padding (a 4096-frame HFP buffer is 256 ms).
    static let idleAfter: Double = 1.0

    /// A real buffer is realigned only when it lands more than this far from where the
    /// track currently ends. Anything smaller is delivery jitter or device-clock skew and is
    /// left alone rather than turned into audible clicks; misalignment is therefore bounded
    /// by this value and never accumulates.
    static let alignTolerance: Double = 0.25

    /// Silence is written in chunks of at most this many frames (1 s).
    static let silenceChunkFrames: AVAudioFrameCount = 48_000

    let meetingDir: URL
    /// Which sources are captured, in display order. Never empty.
    let sources: [Source]
    let echoCancellation: Bool
    private(set) var pipelines: [Source: TrackPipeline] = [:]

    private let clock = Mutex(Clock())
    private let queue = DispatchQueue(label: "meet.recorder")
    private var systemTap: SystemAudioTap?
    private var mic: MicCapture?
    private var stopped = false

    /// Sources whose audio is being replaced with silence (touched from audio threads).
    private let muted = Mutex<Set<Source>>([])

    // liveness: last time each source delivered a buffer (touched from audio threads)
    private let lastBuffer = Mutex<[Source: Date]>([:])
    private var lastRestartAttempt: [Source: Date] = [:]

    // silence watchdog for the system tap
    private var tapFramesSeen: Int64 = 0
    private var tapNonSilentSeen = false
    private var tapWarned = false

    init(meetingDir: URL, sources: [Source], echoCancellation: Bool) {
        precondition(!sources.isEmpty, "Recorder needs at least one source")
        self.meetingDir = meetingDir
        self.sources = sources
        self.echoCancellation = echoCancellation
    }

    var useMic: Bool { sources.contains(.mic) }
    var useSystem: Bool { sources.contains(.system) }

    func audioURL(for source: Source) -> URL {
        meetingDir.appendingPathComponent("\(source.rawValue).m4a")
    }

    func stream(for source: Source) -> AsyncStream<AVAudioPCMBuffer>? {
        pipelines[source]?.stream
    }

    func setPaused(_ p: Bool) {
        let now = Self.hostNow()
        clock.withLock { $0.setPaused(p, at: now) }
    }

    /// Mutes (or unmutes) one source: from the next buffer on, its audio is written and
    /// transcribed as silence. Takes effect at once, from any thread.
    func setMuted(_ source: Source, _ m: Bool) {
        muted.withLock { if m { $0.insert(source) } else { $0.remove(source) } }
    }

    func isMuted(_ source: Source) -> Bool {
        muted.withLock { $0.contains(source) }
    }

    /// Starts the shared recording clock. Call at the same moment as `Session.start()` so
    /// track position 0 is the session's 0:00; audio captured before this is discarded.
    func startClock() {
        let now = Self.hostNow()
        clock.withLock { $0.start = now }
    }

    func start() throws {
        for source in sources {
            let writer = try TrackWriter(url: audioURL(for: source))
            let (stream, continuation) = AsyncStream.makeStream(of: AVAudioPCMBuffer.self,
                                                                bufferingPolicy: .bufferingNewest(Self.maxQueuedBuffers))
            pipelines[source] = TrackPipeline(writer: writer, stream: stream, continuation: continuation)
        }

        let now = Date()
        lastBuffer.withLock { for s in sources { $0[s] = now } }

        if useSystem {
            let tap = SystemAudioTap()
            try tap.start { [weak self] buffer, hostTime in
                self?.handle(buffer, hostTime: hostTime, for: .system)
            }
            systemTap = tap
        }

        if useMic {
            let m = MicCapture()
            try m.start(echoCancellation: echoCancellation) { [weak self] buffer, hostTime in
                self?.handle(buffer, hostTime: hostTime, for: .mic)
            }
            mic = m
        }
    }

    // MARK: - timeline

    static func hostNow() -> Double {
        AVAudioTime.seconds(forHostTime: mach_absolute_time())
    }

    /// Track frame index (48 kHz) for a recording position in seconds.
    static func frame(at position: Double) -> Int64 {
        Int64((position * TrackWriter.sampleRate).rounded())
    }

    private static let toleranceFrames = Int64(alignTolerance * TrackWriter.sampleRate)

    private func handle(_ buffer: AVAudioPCMBuffer, hostTime: UInt64, for source: Source) {
        lastBuffer.withLock { $0[source] = Date() }
        let t = AVAudioTime.seconds(forHostTime: hostTime)
        guard let position = clock.withLock({ $0.position(at: t) }) else { return }
        let expected = Self.frame(at: position)
        let duration = Double(buffer.frameLength) / buffer.format.sampleRate
        let isMuted = muted.withLock { $0.contains(source) }
        queue.async { [self] in
            guard !stopped, var p = pipelines[source] else { return }
            defer { pipelines[source] = p }
            // The watchdog sees the real buffer: it is about whether the tap delivers
            // anything at all, which a mute must not hide.
            if source == .system { watchdog(buffer) }
            p.lastEnd = t + duration

            var buffer = buffer
            if isMuted {
                guard let silence = AVAudioPCMBuffer.silence(format: buffer.format, frames: buffer.frameLength) else { return }
                buffer = silence
            }
            if position < 0 {
                // Captured (partly) before the clock started: keep only the part after 0:00.
                guard let rest = buffer.dropping(firstFrames: AVAudioFrameCount(-position * buffer.format.sampleRate)) else { return }
                buffer = rest
            }
            let delta = max(expected, 0) - p.writer.framesWritten
            if delta > Self.toleranceFrames {
                pad(&p, frames: delta, source: source)
            } else if delta < -Self.toleranceFrames {
                let overlap = AVAudioFrameCount(Double(-delta) * buffer.format.sampleRate / TrackWriter.sampleRate)
                p.trimmedSeconds += Double(min(overlap, buffer.frameLength)) / buffer.format.sampleRate
                guard let rest = buffer.dropping(firstFrames: overlap) else { return }
                buffer = rest
            }
            deliver(buffer, to: &p, source: source)
        }
    }

    /// Writes the buffer to the track file and hands it to the transcriber. Queue only.
    private func deliver(_ buffer: AVAudioPCMBuffer, to p: inout TrackPipeline, source: Source) {
        do {
            try p.writer.write(buffer)
        } catch {
            p.writeErrors.log("⚠ write failed for \(source.rawValue).m4a: \(error)")
        }
        if case .dropped = p.continuation.yield(buffer) {
            p.dropped += 1
            if p.dropped == 1 || p.dropped % 1000 == 0 {
                log("⚠ \(source.label) transcription is falling behind; dropped \(p.dropped) audio buffer(s) so far (audio file is unaffected)")
            }
        }
    }

    /// Appends `frames` frames of silence to the track. Queue only.
    private func pad(_ p: inout TrackPipeline, frames: Int64, source: Source) {
        var remaining = frames
        while remaining > 0 {
            let n = AVAudioFrameCount(min(remaining, Int64(Self.silenceChunkFrames)))
            guard let silence = AVAudioPCMBuffer.silence(format: TrackWriter.pcmFormat, frames: n) else { return }
            deliver(silence, to: &p, source: source)
            remaining -= Int64(n)
        }
        p.paddedSeconds += Double(frames) / TrackWriter.sampleRate
    }

    /// Pads every source that has been quiet for `idleAfter` up to (now − idleAfter).
    private func padIdleSources() {
        let cutoff = Self.hostNow() - Self.idleAfter
        guard let position = clock.withLock({ $0.position(at: cutoff) }) else { return }
        let target = Self.frame(at: position)
        guard target > 0 else { return }
        queue.async { [self] in
            guard !stopped else { return }
            for source in sources {
                guard var p = pipelines[source], p.lastEnd <= cutoff else { continue }
                let delta = target - p.writer.framesWritten
                if delta > 0 {
                    pad(&p, frames: delta, source: source)
                    pipelines[source] = p
                }
            }
        }
    }

    private func watchdog(_ buffer: AVAudioPCMBuffer) {
        guard !tapNonSilentSeen, !tapWarned else { return }
        if !buffer.isSilent {
            tapNonSilentSeen = true
            return
        }
        tapFramesSeen += Int64(buffer.frameLength)
        let seconds = Double(tapFramesSeen) / buffer.format.sampleRate
        if seconds >= 10, SystemAudioTap.outputDeviceIsRunningSomewhere() {
            tapWarned = true
            log("⚠ no system audio received in 10 s while audio is playing — grant \"System Audio Recording Only\" to your terminal in System Settings › Privacy & Security › Screen & System Audio Recording, then re-run.")
        }
    }

    /// Periodic housekeeping (call ~once a second from the main loop): pads idle sources,
    /// and restarts a source that has gone quiet for `staleAfter` seconds, at most once per
    /// `restartCooldown`.
    ///
    /// The system tap legitimately delivers nothing while the output device is idle (Core
    /// Audio does not run IO on e.g. a Bluetooth headset with no audio playing), so it is
    /// only restarted when the device reports it is running and we still see no buffers.
    func checkHealth(now: Date = Date()) {
        guard !stopped else { return }
        padIdleSources()
        let last = lastBuffer.withLock { $0 }
        for source in sources {
            guard let t = last[source], now.timeIntervalSince(t) >= Self.staleAfter else { continue }
            if let attempt = lastRestartAttempt[source], now.timeIntervalSince(attempt) < Self.restartCooldown { continue }
            if source == .system, !SystemAudioTap.outputDeviceIsRunningSomewhere() { continue }
            lastRestartAttempt[source] = now
            let quiet = Int(now.timeIntervalSince(t))
            switch source {
            case .system:
                systemTap?.restart(reason: "no system audio buffers for \(quiet) s while audio is playing")
            case .mic:
                do {
                    try mic?.restart()
                    log("ℹ microphone restarted (no buffers for \(quiet) s)")
                } catch {
                    log("⚠ microphone restart failed (no buffers for \(quiet) s): \(error) — will retry")
                }
            }
        }
    }

    /// Stops capture, pads every track to the same final length, flushes and closes files,
    /// and finishes the transcriber streams.
    func stop() {
        systemTap?.stop()
        systemTap = nil
        mic?.stop()
        mic = nil
        let hostNow = Self.hostNow()
        let target = Self.frame(at: clock.withLock { $0.finalPosition(at: hostNow) })
        queue.sync {
            stopped = true
            for source in sources {
                guard var p = pipelines[source] else { continue }
                let delta = target - p.writer.framesWritten
                if delta > 0 { pad(&p, frames: delta, source: source) }
                p.writer.close()
                p.continuation.finish()
                pipelines[source] = p
            }
        }
    }

    func seconds(for source: Source) -> Double {
        pipelines[source]?.writer.seconds ?? 0
    }

    func paddedSeconds(for source: Source) -> Double {
        pipelines[source]?.paddedSeconds ?? 0
    }

    /// Diagnostics for verbose periodic logging.
    var healthSummary: String {
        var parts = sources.map { source -> String in
            let p = pipelines[source]
            return "\(source.rawValue) dropped \(p?.dropped ?? 0) padded \(Output.humanDuration(p?.paddedSeconds ?? 0)) trimmed \(Output.humanDuration(p?.trimmedSeconds ?? 0))"
        }
        if let systemTap { parts.append("tap restarts \(systemTap.restartCount)") }
        return parts.joined(separator: ", ")
    }
}
