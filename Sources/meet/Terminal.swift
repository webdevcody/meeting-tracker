import Darwin
import Foundation

enum Terminal {
    nonisolated(unsafe) private static var original: termios?
    nonisolated(unsafe) private static var signalSources: [DispatchSourceSignal] = []
    private static let signalQueue = DispatchQueue(label: "meet.signals")

    static var stdinIsTTY: Bool { isatty(0) == 1 }
    static var stdoutIsTTY: Bool { isatty(1) == 1 }

    static func enableRaw() {
        guard stdinIsTTY, original == nil else { return }
        var t = termios()
        guard tcgetattr(0, &t) == 0 else { return }
        original = t
        t.c_lflag &= ~tcflag_t(ECHO | ICANON)
        withUnsafeMutablePointer(to: &t.c_cc) { p in
            p.withMemoryRebound(to: cc_t.self, capacity: Int(NCCS)) { cc in
                cc[Int(VMIN)] = 1
                cc[Int(VTIME)] = 0
            }
        }
        tcsetattr(0, TCSANOW, &t)
        atexit { Terminal.restore() }
    }

    static func restore() {
        guard var o = original else { return }
        tcsetattr(0, TCSANOW, &o)
        original = nil
    }

    /// Bytes read from stdin on a dedicated blocking thread.
    static func keyStream() -> AsyncStream<UInt8> {
        AsyncStream { continuation in
            let thread = Thread {
                var b: UInt8 = 0
                while read(0, &b, 1) == 1 {
                    continuation.yield(b)
                }
                continuation.finish()
            }
            thread.name = "meet.keys"
            thread.start()
        }
    }

    /// Routes SIGINT/SIGTERM/SIGHUP into `handler` instead of killing the process, so a
    /// closed terminal window or dropped SSH session still ends the meeting cleanly
    /// (files finalized, transcript written, hooks run).
    static func onStopSignals(_ handler: @escaping @Sendable (Int32) -> Void) {
        for sig in [SIGINT, SIGTERM, SIGHUP] {
            signal(sig, SIG_IGN)
            let src = DispatchSource.makeSignalSource(signal: sig, queue: signalQueue)
            src.setEventHandler { handler(sig) }
            src.resume()
            signalSources.append(src)
        }
    }
}
