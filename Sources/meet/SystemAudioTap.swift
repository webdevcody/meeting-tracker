import AVFoundation
import CoreAudio
import Foundation

/// Captures everything routed to the default system output device via a Core Audio
/// process tap (macOS 14.2+). Requires the "System Audio Recording Only" TCC grant.
///
/// The aggregate device is bound to the output device that was current at start, so the
/// tap watches for default-output-device changes (headphones, AirPods, sleep/wake) and
/// rebuilds itself. `Recorder` additionally calls `restart` if buffers stop arriving.
final class SystemAudioTap {
    private var tapID = AudioObjectID(kAudioObjectUnknown)
    private var aggregateID = AudioObjectID(kAudioObjectUnknown)
    private var procID: AudioDeviceIOProcID?
    private let ioQueue = DispatchQueue(label: "meet.system-tap")
    /// Serializes open/close/restart. Core Audio property listeners are delivered here too.
    private let controlQueue = DispatchQueue(label: "meet.system-tap.control")
    private(set) var format: AVAudioFormat?
    private var running = false
    private var wantRunning = false
    /// Receives each buffer with the host time of its first frame.
    private var onBuffer: ((AVAudioPCMBuffer, UInt64) -> Void)?
    private var restartPending = false
    private var deviceListener: AudioObjectPropertyListenerBlock?
    private(set) var restartCount = 0

    private static let watchedSelectors: [AudioObjectPropertySelector] = [
        kAudioHardwarePropertyDefaultSystemOutputDevice,
        kAudioHardwarePropertyDefaultOutputDevice,
    ]

    func start(onBuffer: @escaping (AVAudioPCMBuffer, UInt64) -> Void) throws {
        self.onBuffer = onBuffer
        try controlQueue.sync {
            wantRunning = true
            try open()
        }
        installDeviceListener()
    }

    func stop() {
        removeDeviceListener()
        controlQueue.sync {
            wantRunning = false
            close()
        }
    }

    /// Tears down and rebuilds the tap against the *current* default output device.
    /// Safe to call from any thread; no-op after `stop()`.
    func restart(reason: String) {
        controlQueue.async { [self] in
            guard wantRunning else { return }
            close()
            do {
                try open()
                restartCount += 1
                log("ℹ system audio tap restarted (\(reason))")
            } catch {
                log("⚠ system audio tap restart failed (\(reason)): \(error) — will retry")
            }
        }
    }

    deinit { stop() }

    // MARK: - open / close (controlQueue only)

    private func open() throws {
        let desc = CATapDescription(monoGlobalTapButExcludeProcesses: [])
        desc.uuid = UUID()
        desc.muteBehavior = .unmuted
        desc.isPrivate = true
        desc.name = "meet"

        var tap = AudioObjectID(kAudioObjectUnknown)
        var err = AudioHardwareCreateProcessTap(desc, &tap)
        guard err == noErr else { throw MTError.coreAudio("AudioHardwareCreateProcessTap", err) }
        tapID = tap

        var asbd = AudioStreamBasicDescription()
        var addr = AudioObjectPropertyAddress(mSelector: kAudioTapPropertyFormat,
                                              mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        err = AudioObjectGetPropertyData(tapID, &addr, 0, nil, &size, &asbd)
        guard err == noErr else { close(); throw MTError.coreAudio("kAudioTapPropertyFormat", err) }
        guard let fmt = AVAudioFormat(streamDescription: &asbd) else {
            close(); throw MTError.message("unsupported tap format")
        }
        format = fmt

        let outputUID: String
        do {
            outputUID = try Self.defaultSystemOutputDeviceUID()
        } catch {
            close(); throw error
        }

        let aggDesc: [String: Any] = [
            kAudioAggregateDeviceNameKey: "meet-tap",
            kAudioAggregateDeviceUIDKey: UUID().uuidString,
            kAudioAggregateDeviceMainSubDeviceKey: outputUID,
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceIsStackedKey: false,
            kAudioAggregateDeviceTapAutoStartKey: true,
            kAudioAggregateDeviceSubDeviceListKey: [[kAudioSubDeviceUIDKey: outputUID]],
            kAudioAggregateDeviceTapListKey: [[
                kAudioSubTapUIDKey: desc.uuid.uuidString,
                kAudioSubTapDriftCompensationKey: true,
            ]],
        ]
        var agg = AudioObjectID(kAudioObjectUnknown)
        err = AudioHardwareCreateAggregateDevice(aggDesc as CFDictionary, &agg)
        guard err == noErr else { close(); throw MTError.coreAudio("AudioHardwareCreateAggregateDevice", err) }
        aggregateID = agg

        let deliver = onBuffer
        err = AudioDeviceCreateIOProcIDWithBlock(&procID, aggregateID, ioQueue) { inNow, inInputData, inInputTime, _, _ in
            guard let wrapped = AVAudioPCMBuffer(pcmFormat: fmt, bufferListNoCopy: inInputData, deallocator: nil),
                  wrapped.frameLength > 0,
                  let copy = wrapped.deepCopy() else { return }
            let hostTime: UInt64
            if inInputTime.pointee.mFlags.contains(.hostTimeValid) {
                hostTime = inInputTime.pointee.mHostTime
            } else if inNow.pointee.mFlags.contains(.hostTimeValid) {
                hostTime = inNow.pointee.mHostTime
            } else {
                hostTime = mach_absolute_time()
            }
            deliver?(copy, hostTime)
        }
        guard err == noErr else { close(); throw MTError.coreAudio("AudioDeviceCreateIOProcIDWithBlock", err) }

        err = AudioDeviceStart(aggregateID, procID)
        guard err == noErr else { close(); throw MTError.coreAudio("AudioDeviceStart", err) }
        running = true
    }

    private func close() {
        if running, let procID { AudioDeviceStop(aggregateID, procID) }
        running = false
        if let procID {
            AudioDeviceDestroyIOProcID(aggregateID, procID)
            self.procID = nil
        }
        if aggregateID != kAudioObjectUnknown {
            AudioHardwareDestroyAggregateDevice(aggregateID)
            aggregateID = AudioObjectID(kAudioObjectUnknown)
        }
        if tapID != kAudioObjectUnknown {
            AudioHardwareDestroyProcessTap(tapID)
            tapID = AudioObjectID(kAudioObjectUnknown)
        }
    }

    // MARK: - default output device listener

    private func installDeviceListener() {
        guard deviceListener == nil else { return }
        let listener: AudioObjectPropertyListenerBlock = { [weak self] _, _ in
            self?.scheduleRestart(reason: "default output device changed")
        }
        deviceListener = listener
        for selector in Self.watchedSelectors {
            var addr = AudioObjectPropertyAddress(mSelector: selector,
                                                  mScope: kAudioObjectPropertyScopeGlobal,
                                                  mElement: kAudioObjectPropertyElementMain)
            AudioObjectAddPropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &addr, controlQueue, listener)
        }
    }

    private func removeDeviceListener() {
        guard let listener = deviceListener else { return }
        deviceListener = nil
        for selector in Self.watchedSelectors {
            var addr = AudioObjectPropertyAddress(mSelector: selector,
                                                  mScope: kAudioObjectPropertyScopeGlobal,
                                                  mElement: kAudioObjectPropertyElementMain)
            AudioObjectRemovePropertyListenerBlock(AudioObjectID(kAudioObjectSystemObject), &addr, controlQueue, listener)
        }
    }

    /// Both watched properties usually fire together; coalesce into one restart after the
    /// new device has settled.
    private func scheduleRestart(reason: String) {
        // Called on controlQueue.
        guard wantRunning, !restartPending else { return }
        restartPending = true
        controlQueue.asyncAfter(deadline: .now() + 1.0) { [self] in
            restartPending = false
            restart(reason: reason)
        }
    }

    // MARK: - Core Audio helpers

    static func defaultSystemOutputDeviceUID() throws -> String {
        var deviceID = AudioObjectID(kAudioObjectUnknown)
        var size = UInt32(MemoryLayout<AudioObjectID>.size)
        var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultSystemOutputDevice,
                                              mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        var err = AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &deviceID)
        guard err == noErr, deviceID != kAudioObjectUnknown else {
            throw MTError.coreAudio("kAudioHardwarePropertyDefaultSystemOutputDevice", err)
        }

        var uid: CFString = "" as CFString
        size = UInt32(MemoryLayout<CFString>.size)
        addr.mSelector = kAudioDevicePropertyDeviceUID
        err = withUnsafeMutablePointer(to: &uid) { p in
            AudioObjectGetPropertyData(deviceID, &addr, 0, nil, &size, p)
        }
        guard err == noErr else { throw MTError.coreAudio("kAudioDevicePropertyDeviceUID", err) }
        return uid as String
    }

    /// Whether the default output device currently has any client running IO on it.
    static func outputDeviceIsRunningSomewhere() -> Bool {
        var deviceID = AudioObjectID(kAudioObjectUnknown)
        var size = UInt32(MemoryLayout<AudioObjectID>.size)
        var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultSystemOutputDevice,
                                              mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &deviceID) == noErr else {
            return false
        }
        var running: UInt32 = 0
        size = UInt32(MemoryLayout<UInt32>.size)
        addr.mSelector = kAudioDevicePropertyDeviceIsRunningSomewhere
        guard AudioObjectGetPropertyData(deviceID, &addr, 0, nil, &size, &running) == noErr else { return false }
        return running != 0
    }
}
