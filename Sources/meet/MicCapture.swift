import AVFoundation
import Foundation

/// Captures the default input device via AVAudioEngine.
final class MicCapture {
    private let engine = AVAudioEngine()
    private var observer: NSObjectProtocol?
    /// Receives each buffer with the host time of its first frame.
    private var onBuffer: ((AVAudioPCMBuffer, UInt64) -> Void)?
    private(set) var format: AVAudioFormat?
    private let lock = NSLock()
    private var running = false

    static func requestPermission() async -> Bool {
        await AVCaptureDevice.requestAccess(for: .audio)
    }

    func start(echoCancellation: Bool, onBuffer: @escaping (AVAudioPCMBuffer, UInt64) -> Void) throws {
        self.onBuffer = onBuffer
        let input = engine.inputNode
        if echoCancellation {
            do {
                try input.setVoiceProcessingEnabled(true)
            } catch {
                log("⚠ echo cancellation unavailable: \(error.localizedDescription)")
            }
        }
        try installAndStart()
        observer = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: engine, queue: nil
        ) { [weak self] _ in
            self?.handleConfigurationChange()
        }
    }

    private func installAndStart() throws {
        let input = engine.inputNode
        let fmt = input.outputFormat(forBus: 0)
        guard fmt.sampleRate > 0, fmt.channelCount > 0 else {
            throw MTError.message("no microphone input available")
        }
        format = fmt
        input.removeTap(onBus: 0)
        input.installTap(onBus: 0, bufferSize: 4096, format: fmt) { [weak self] buffer, when in
            guard let self, let copy = buffer.deepCopy() else { return }
            self.onBuffer?(copy, when.isHostTimeValid ? when.hostTime : mach_absolute_time())
        }
        engine.prepare()
        try engine.start()
        lock.withLock { running = true }
    }

    private func handleConfigurationChange() {
        guard lock.withLock({ running }) else { return }
        do {
            try installAndStart()
            log("ℹ microphone configuration changed; restarted input (\(Int(format?.sampleRate ?? 0)) Hz)")
        } catch {
            log("⚠ microphone restart failed: \(error)")
        }
    }

    /// Tears the engine down and brings it back up on the current default input device.
    func restart() throws {
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        try installAndStart()
    }

    func stop() {
        lock.withLock { running = false }
        if let observer { NotificationCenter.default.removeObserver(observer) }
        observer = nil
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
    }
}
