import Foundation

struct MeetingOutput {
    let meetingDir: URL
    let transcriptMD: URL
    let transcriptJSON: URL
    let metaJSON: URL
    let transcriptText: String
}

enum Output {
    static let audioFileName = "audio.m4a"

    struct TranscriptJSON: Codable {
        var startedAt: String
        var durationSecs: Double
        var segments: [Segment]
    }

    struct Meta: Codable {
        var startedAt: String
        var endedAt: String
        var durationSecs: Double
        var pausedSecs: Double
        var locale: String
        var sources: [String]
        var audio: String
        var echoCancellation: Bool
        var fast: Bool
    }

    static func write(meetingDir: URL,
                      startedAt: Date,
                      endedAt: Date,
                      durationSecs: Double,
                      pausedSecs: Double,
                      locale: String,
                      sources: [Source],
                      echoCancellation: Bool,
                      fast: Bool,
                      segments rawSegments: [Segment]) throws -> MeetingOutput {
        // Stable sort by start; ties keep insertion order (mic before system when equal
        // is enforced by sorting on (start, source)).
        let sorted = rawSegments.enumerated().sorted { a, b in
            if a.element.start != b.element.start { return a.element.start < b.element.start }
            if a.element.source != b.element.source { return a.element.source == .mic }
            return a.offset < b.offset
        }.map(\.element)

        // Coalesce consecutive same-source segments into paragraphs.
        var paragraphs: [(Source, String)] = []
        for s in sorted {
            if let last = paragraphs.last, last.0 == s.source {
                paragraphs[paragraphs.count - 1].1 += " " + s.text
            } else {
                paragraphs.append((s.source, s.text))
            }
        }

        let title = Self.titleFormatter.string(from: startedAt)
        var md = "# Meeting \(title)\n"
        md += "Duration: \(humanDuration(durationSecs))\n\n"
        if paragraphs.isEmpty {
            md += "_(no speech detected)_\n"
        } else {
            for (source, text) in paragraphs {
                md += "**\(source.label):** \(text)\n\n"
            }
        }

        let iso = ISO8601DateFormatter()
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]

        let mdURL = meetingDir.appendingPathComponent("transcript.md")
        let jsonURL = meetingDir.appendingPathComponent("transcript.json")
        let metaURL = meetingDir.appendingPathComponent("meta.json")

        try md.write(to: mdURL, atomically: true, encoding: .utf8)
        try encoder.encode(TranscriptJSON(startedAt: iso.string(from: startedAt),
                                          durationSecs: durationSecs,
                                          segments: sorted)).write(to: jsonURL)
        try encoder.encode(Meta(startedAt: iso.string(from: startedAt),
                                endedAt: iso.string(from: endedAt),
                                durationSecs: durationSecs,
                                pausedSecs: pausedSecs,
                                locale: locale,
                                sources: sources.map(\.rawValue),
                                audio: audioFileName,
                                echoCancellation: echoCancellation,
                                fast: fast)).write(to: metaURL)

        return MeetingOutput(meetingDir: meetingDir, transcriptMD: mdURL,
                             transcriptJSON: jsonURL, metaJSON: metaURL, transcriptText: md)
    }

    static let dirFormatter: DateFormatter = {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd_HH-mm"
        return f
    }()

    static let titleFormatter: DateFormatter = {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd HH:mm"
        return f
    }()

    static func humanDuration(_ secs: Double) -> String {
        let s = Int(secs.rounded())
        if s >= 3600 { return "\(s / 3600)h \((s % 3600) / 60)m \(s % 60)s" }
        if s >= 60 { return "\(s / 60)m \(s % 60)s" }
        return "\(s)s"
    }
}
