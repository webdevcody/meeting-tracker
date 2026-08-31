import Foundation

/// Where a track's audio comes from. `mic` is whatever this Mac's input device hears (the
/// recording user and anyone else in the room); `system` is whatever this Mac plays
/// (remote call participants, videos, …).
enum Source: String, Codable, Sendable, CaseIterable {
    case mic, system

    var label: String {
        switch self {
        case .mic: return "Microphone"
        case .system: return "System"
        }
    }
}

struct Segment: Codable, Sendable {
    var source: Source
    var text: String
    var start: Double
    var end: Double
}

/// Single owner of session state and the only writer to stdout.
actor Session {
    private(set) var paused = false
    private(set) var stopped = false
    private(set) var segments: [Segment] = []
    private var counts: [Source: Int] = [:]
    private var startedAt: Date?
    private var stoppedAt: Date?
    private var pausedAccum: TimeInterval = 0
    private var pauseStart: Date?
    private var stopWaiters: [CheckedContinuation<Void, Never>] = []
    private var statusText: String?
    private let showStatus: Bool
    private let sources: [Source]

    init(sources: [Source]) {
        self.showStatus = Terminal.stdoutIsTTY
        self.sources = sources
    }

    var elapsed: TimeInterval {
        guard let startedAt else { return 0 }
        let now = stoppedAt ?? Date()
        var e = now.timeIntervalSince(startedAt) - pausedAccum
        if let pauseStart { e -= now.timeIntervalSince(pauseStart) }
        return max(0, e)
    }

    var pausedSeconds: TimeInterval { pausedAccum }
    var segmentCount: Int { segments.count }

    func start() {
        startedAt = Date()
        draw()
    }

    func togglePause() -> Bool {
        if paused {
            if let ps = pauseStart { pausedAccum += Date().timeIntervalSince(ps) }
            pauseStart = nil
            paused = false
        } else {
            pauseStart = Date()
            paused = true
        }
        draw()
        return paused
    }

    func requestStop() {
        guard !stopped else { return }
        let now = Date()
        if paused, let ps = pauseStart {
            pausedAccum += now.timeIntervalSince(ps)
            pauseStart = nil
        }
        stoppedAt = now
        stopped = true
        clearStatus()
        for w in stopWaiters { w.resume() }
        stopWaiters.removeAll()
    }

    func waitUntilStopped() async {
        if stopped { return }
        await withCheckedContinuation { stopWaiters.append($0) }
    }

    func add(_ segment: Segment) {
        segments.append(segment)
        counts[segment.source, default: 0] += 1
        print("\r\u{1B}[2K\(segment.source.label): \(segment.text)")
        draw()
    }

    /// Temporary status shown instead of the recording line (e.g. model download).
    func setStatus(_ text: String?) {
        statusText = text
        draw()
    }

    func draw() {
        guard showStatus, !stopped else { return }
        let line: String
        if let statusText {
            line = statusText
        } else {
            let state = paused ? "⏸ PAUSED" : "● REC"
            let tracks = sources.map { "\($0.rawValue) \(counts[$0, default: 0])" }.joined(separator: " · ")
            line = "\(state) \(Self.format(elapsed)) │ \(tracks) │ \(Diagnostics.residentMB()) MB │ [space] pause  [q] stop"
        }
        print("\r\u{1B}[2K\(line)", terminator: "")
        fflush(stdout)
    }

    func clearStatus() {
        guard showStatus else { return }
        print("\r\u{1B}[2K", terminator: "")
        fflush(stdout)
    }

    static func format(_ t: TimeInterval) -> String {
        let s = Int(t)
        return String(format: "%02d:%02d:%02d", s / 3600, (s % 3600) / 60, s % 60)
    }
}
