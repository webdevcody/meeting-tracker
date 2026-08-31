import Accelerate
import AVFoundation
import Foundation

/// Sums the per-source tracks into one mono AAC file.
///
/// The tracks share a timeline: every writer is opened before any capture source starts,
/// and the pause gate skips the same wall-clock stretch on all of them, so frame N of each
/// file is (to within a buffer) the same instant. Shorter tracks are zero-padded and the sum
/// is hard-clipped to ±1 — speech from two sources rarely peaks at the same moment, and the
/// alternative (scaling everything down) would make quiet passages quieter for no gain.
enum AudioMixer {
    /// Frames per pass: 10 s at 48 kHz, ~2 MB per buffer.
    static let chunkFrames: AVAudioFrameCount = 48_000 * 10

    /// Writes `output` from `inputs`. With a single input the file is moved, not re-encoded.
    static func merge(_ inputs: [URL], into output: URL) throws {
        guard !inputs.isEmpty else { throw MTError.message("nothing to merge") }
        let fm = FileManager.default
        if fm.fileExists(atPath: output.path) { try fm.removeItem(at: output) }
        if inputs.count == 1 {
            try fm.moveItem(at: inputs[0], to: output)
            return
        }
        try mix(inputs, into: output)
        for url in inputs { try? fm.removeItem(at: url) }
    }

    static func mix(_ inputs: [URL], into output: URL) throws {
        let files = try inputs.map { try AVAudioFile(forReading: $0) }
        guard let first = files.first else { throw MTError.message("nothing to mix") }
        let format = first.processingFormat
        for f in files.dropFirst() where f.processingFormat != format {
            throw MTError.message("cannot mix \(f.url.lastPathComponent): format \(f.processingFormat) ≠ \(format)")
        }
        guard format.commonFormat == .pcmFormatFloat32, !format.isInterleaved, format.channelCount == 1 else {
            throw MTError.message("cannot mix: unexpected track format \(format)")
        }

        let out = try AVAudioFile(forWriting: output, settings: TrackWriter.settings,
                                  commonFormat: .pcmFormatFloat32, interleaved: false)
        defer { out.close() }
        guard let acc = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: chunkFrames),
              let tmp = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: chunkFrames),
              let accData = acc.floatChannelData?[0], let tmpData = tmp.floatChannelData?[0] else {
            throw MTError.message("cannot allocate mix buffers")
        }

        var lower: Float = -1, upper: Float = 1
        while true {
            var frames = 0
            vDSP_vclr(accData, 1, vDSP_Length(chunkFrames))
            for f in files {
                let remaining = f.length - f.framePosition
                guard remaining > 0 else { continue }
                tmp.frameLength = 0
                try f.read(into: tmp, frameCount: min(chunkFrames, AVAudioFrameCount(remaining)))
                let n = Int(tmp.frameLength)
                guard n > 0 else { continue }
                vDSP_vadd(accData, 1, tmpData, 1, accData, 1, vDSP_Length(n))
                frames = max(frames, n)
            }
            if frames == 0 { break }
            vDSP_vclip(accData, 1, &lower, &upper, accData, 1, vDSP_Length(frames))
            acc.frameLength = AVAudioFrameCount(frames)
            try out.write(from: acc)
        }
    }
}
