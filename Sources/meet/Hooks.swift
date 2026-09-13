import Foundation

enum Hooks {
    /// Runs each command sequentially via `/bin/sh -c`, with MT_* env vars set and the
    /// transcript text on stdin. Failures are logged and the next hook still runs. In
    /// `--json` mode each hook is bracketed by a `hook` event (`phase` `start` / `end`,
    /// with its 1-based `index`, the `count`, the `command`, and at the end its exit
    /// `status` and the `secs` it took), so the TUI can show the hook that is running —
    /// the bundled one calls Claude to summarize the transcript — and how it ended.
    static func run(_ commands: [String], env extra: [String: String], cwd: URL, stdin text: String) async {
        Terminal.restore()
        for (i, command) in commands.enumerated() {
            note("→ hook[\(i + 1)]: \(command)")
            Events.emit("hook", ["index": i + 1, "count": commands.count, "command": command, "phase": "start"])
            let started = Date()
            let status = await runOne(command, env: extra, cwd: cwd, stdin: text)
            if status != 0 {
                log("⚠ hook[\(i + 1)] exited \(status)")
            }
            Events.emit("hook", ["index": i + 1, "count": commands.count, "command": command, "phase": "end",
                                 "status": Int(status), "secs": Date().timeIntervalSince(started)])
        }
    }

    private static func runOne(_ command: String, env extra: [String: String], cwd: URL, stdin text: String) async -> Int32 {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", command]
        process.environment = ProcessInfo.processInfo.environment.merging(extra) { $1 }
        process.currentDirectoryURL = cwd
        let pipe = Pipe()
        process.standardInput = pipe
        // stdout/stderr inherit -> hook output streams to the terminal. In --json mode
        // stdout belongs to the event stream, so the hook's output joins ours on stderr.
        if Events.enabled { process.standardOutput = FileHandle.standardError }

        return await withCheckedContinuation { continuation in
            process.terminationHandler = { p in
                continuation.resume(returning: p.terminationStatus)
            }
            do {
                try process.run()
            } catch {
                log("⚠ could not start hook: \(error)")
                continuation.resume(returning: 127)
                return
            }
            DispatchQueue.global().async {
                let handle = pipe.fileHandleForWriting
                try? handle.write(contentsOf: Data(text.utf8))
                try? handle.close()
            }
        }
    }
}
