import AVFoundation

/// Writes mono AAC (.m4a) at 48 kHz, converting from whatever the source delivers.
final class TrackWriter {
    static let sampleRate: Double = 48_000
    /// The PCM format tracks are encoded from (and silence is generated in).
    static let pcmFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: sampleRate,
                                         channels: 1, interleaved: false)!
    /// Shared by every .m4a this tool writes (per-source tracks and the merged file).
    static let settings: [String: Any] = [
        AVFormatIDKey: kAudioFormatMPEG4AAC,
        AVSampleRateKey: sampleRate,
        AVNumberOfChannelsKey: 1,
        AVEncoderBitRateKey: 64_000,
    ]

    let url: URL
    private let file: AVAudioFile
    private let converter = BufferConverter()
    private(set) var framesWritten: Int64 = 0
    private var closed = false

    init(url: URL) throws {
        self.url = url
        file = try AVAudioFile(forWriting: url, settings: Self.settings,
                               commonFormat: .pcmFormatFloat32, interleaved: false)
    }

    func write(_ buffer: AVAudioPCMBuffer) throws {
        guard !closed else { return }
        let out = try converter.convert(buffer, to: file.processingFormat)
        guard out.frameLength > 0 else { return }
        try file.write(from: out)
        framesWritten += Int64(out.frameLength)
    }

    var seconds: Double { Double(framesWritten) / file.processingFormat.sampleRate }

    func close() {
        guard !closed else { return }
        closed = true
        file.close()
    }
}
