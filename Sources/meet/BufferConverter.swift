import AVFoundation

/// Converts PCM buffers between formats (sample rate / channel / layout) using a
/// cached AVAudioConverter. Not thread-safe; use one per serial consumer.
final class BufferConverter {
    private var converter: AVAudioConverter?

    func convert(_ buffer: AVAudioPCMBuffer, to format: AVAudioFormat) throws -> AVAudioPCMBuffer {
        let input = buffer.format
        if input == format { return buffer }

        if let c = converter, c.inputFormat == input, c.outputFormat == format {
            // reuse
        } else {
            guard let c = AVAudioConverter(from: input, to: format) else {
                throw MTError.message("cannot convert \(input) -> \(format)")
            }
            c.primeMethod = .none
            converter = c
        }
        guard let converter else { throw MTError.message("no converter") }

        let ratio = format.sampleRate / input.sampleRate
        let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up)) + 16
        guard let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: max(capacity, 1)) else {
            throw MTError.message("cannot allocate conversion buffer")
        }

        var error: NSError?
        var consumed = false
        let status = converter.convert(to: out, error: &error) { _, outStatus in
            if consumed {
                outStatus.pointee = .noDataNow
                return nil
            }
            consumed = true
            outStatus.pointee = .haveData
            return buffer
        }
        if status == .error {
            throw error ?? MTError.message("audio conversion failed")
        }
        return out
    }
}

extension AVAudioPCMBuffer {
    /// Deep-copies a buffer whose memory is only valid during an audio callback.
    func deepCopy() -> AVAudioPCMBuffer? {
        guard let copy = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: max(frameLength, 1)) else { return nil }
        copy.frameLength = frameLength
        let src = UnsafeMutableAudioBufferListPointer(mutableAudioBufferList)
        let dst = UnsafeMutableAudioBufferListPointer(copy.mutableAudioBufferList)
        for i in 0..<min(src.count, dst.count) {
            guard let s = src[i].mData, let d = dst[i].mData else { continue }
            memcpy(d, s, Int(min(src[i].mDataByteSize, dst[i].mDataByteSize)))
        }
        return copy
    }

    /// True if every sample is exactly zero (Float32 formats only; otherwise false).
    var isSilent: Bool {
        guard let ch = floatChannelData, frameLength > 0 else { return false }
        let n = Int(frameLength) * (format.isInterleaved ? Int(format.channelCount) : 1)
        let channels = format.isInterleaved ? 1 : Int(format.channelCount)
        for c in 0..<channels {
            let p = ch[c]
            for i in 0..<n where p[i] != 0 { return false }
        }
        return true
    }
}

extension AVAudioPCMBuffer {
    /// A zero-filled buffer of `frames` frames.
    static func silence(format: AVAudioFormat, frames: AVAudioFrameCount) -> AVAudioPCMBuffer? {
        guard frames > 0, let buf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else { return nil }
        buf.frameLength = frames
        let list = UnsafeMutableAudioBufferListPointer(buf.mutableAudioBufferList)
        for b in list {
            if let data = b.mData { memset(data, 0, Int(b.mDataByteSize)) }
        }
        return buf
    }

    /// The buffer without its first `n` frames; nil if nothing would remain.
    func dropping(firstFrames n: AVAudioFrameCount) -> AVAudioPCMBuffer? {
        guard n < frameLength else { return nil }
        if n == 0 { return self }
        let remaining = frameLength - n
        guard let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: remaining) else { return nil }
        out.frameLength = remaining
        let bytesPerFrame = Int(format.streamDescription.pointee.mBytesPerFrame)
        let src = UnsafeMutableAudioBufferListPointer(mutableAudioBufferList)
        let dst = UnsafeMutableAudioBufferListPointer(out.mutableAudioBufferList)
        for i in 0..<min(src.count, dst.count) {
            guard let s = src[i].mData, let d = dst[i].mData else { continue }
            memcpy(d, s + Int(n) * bytesPerFrame, Int(remaining) * bytesPerFrame)
        }
        return out
    }
}
