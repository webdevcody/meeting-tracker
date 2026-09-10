import Foundation

/// `--json` mode: newline-delimited JSON events on stdout for a parent process (the `meet`
/// TUI), with every human-readable line moved to stderr. One event per line, one line per
/// event, written unbuffered so a consumer sees a segment the moment it is final.
///
/// Events (`"event"` key): `config`, `started`, `status`, `segment`, `paused`, `finished`.
enum Events {
    nonisolated(unsafe) private(set) static var enabled = false
    private static let lock = NSLock()

    static func enable() { enabled = true }

    static func emit(_ event: String, _ fields: [String: Any] = [:]) {
        guard enabled else { return }
        var object = fields
        object["event"] = event
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])
        else { return }
        lock.lock()
        defer { lock.unlock() }
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data("\n".utf8))
    }
}

/// A line meant for a person. Goes to stdout normally; to stderr in `--json` mode so
/// stdout stays pure NDJSON.
func note(_ s: String) {
    if Events.enabled {
        FileHandle.standardError.write(Data((s + "\n").utf8))
    } else {
        print(s)
        fflush(stdout)
    }
}
