import ArgumentParser
import Foundation

/// `meet init` — write a fully populated config file.
struct Init: ParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "init",
        abstract: "Create a meet.json in the current directory (or the global config with --global).",
        discussion: """
        Per-project setup: run `meet init` inside a project, edit the prompt and paths in
        ./meet.json, then run `meet` from that directory (or any subdirectory) — the nearest
        meet.json up the tree is used. Meetings are stored in ./meetings by default,
        relative to the config file.

        --global writes ~/.config/meet/config.json instead, the fallback used when
        no project config is found (default storage ~/Meetings). Existing files are not
        overwritten unless --force is passed. The prompt and system prompt are written inline
        so you can edit them in place; --prompt-files puts them in Markdown files next to the
        config instead (summary.promptFile / summary.systemPromptFile).
        """
    )

    @Argument(help: "Where to write the config (default: ./\(Config.localFileName)).")
    var path: String?

    @Flag(help: "Write the global config (\(Config.defaultPath)) instead of a project one.")
    var global = false

    @Flag(help: "Overwrite an existing file.")
    var force = false

    @Flag(name: .customLong("prompt-files"), help: "Write the prompt and system prompt as summary-prompt.md / summary-system-prompt.md next to the config.")
    var promptFiles = false

    @Option(name: .customLong("out-dir"), help: "outputDir value (default: ./meetings for a project config, ~/Meetings for --global).")
    var outDir: String?

    @Option(name: .customLong("summary-dir"), help: "summary.dir value (default <out-dir>/summaries).")
    var summaryDir: String?

    @Option(help: "locale value (default en-US).")
    var locale: String = "en-US"

    @Option(help: "summary.model value (default: omitted, Claude's default model).")
    var model: String?

    @Option(help: "Path to the summary hook script (default: auto-detected hooks/summarize-transcript.sh next to this binary's checkout).")
    var hook: String?

    @Flag(name: .customLong("no-hook"), help: "Leave hooks.onDone empty.")
    var noHook = false

    func run() throws {
        let fm = FileManager.default
        let cwd = URL(fileURLWithPath: fm.currentDirectoryPath)

        // Destination
        let target: String
        if let path {
            target = resolvePath(path, base: cwd)
        } else if global {
            target = expandTilde(Config.defaultPath)
        } else {
            target = cwd.appendingPathComponent(Config.localFileName).path
        }
        let outDir = self.outDir ?? (global ? "~/Meetings" : "meetings")
        if fm.fileExists(atPath: target), !force {
            throw MTError.message("\(target) already exists (use --force to overwrite)")
        }
        let targetDir = URL(fileURLWithPath: target).deletingLastPathComponent()
        try fm.createDirectory(at: targetDir, withIntermediateDirectories: true)

        // Hook
        var hookPath: String? = nil
        var hookNote: String? = nil
        if !noHook {
            if let hook {
                hookPath = resolvePath(hook, base: cwd)
                if !fm.fileExists(atPath: hookPath!) { hookNote = "⚠ hook not found at \(hookPath!)" }
            } else if let found = Self.locateBundledHook() {
                hookPath = found
            } else {
                hookNote = "⚠ could not find hooks/summarize-transcript.sh near this binary; hooks.onDone left empty. Pass --hook <path> or edit the file."
            }
        }

        // Prompts
        var promptEntry: String
        var systemPromptEntry: String
        var written: [String] = [target]
        if promptFiles {
            let p = targetDir.appendingPathComponent("summary-prompt.md")
            let sp = targetDir.appendingPathComponent("summary-system-prompt.md")
            for (url, text) in [(p, SummaryConfig.defaultPrompt), (sp, SummaryConfig.defaultSystemPrompt)] {
                if fm.fileExists(atPath: url.path), !force {
                    throw MTError.message("\(url.path) already exists (use --force to overwrite)")
                }
                try (text + "\n").write(to: url, atomically: true, encoding: .utf8)
                written.append(url.path)
            }
            promptEntry = "\"promptFile\": \(json("summary-prompt.md"))"
            systemPromptEntry = "\"systemPromptFile\": \(json("summary-system-prompt.md"))"
        } else {
            promptEntry = "\"prompt\": \(json(SummaryConfig.defaultPrompt))"
            let lines = SummaryConfig.defaultSystemPrompt
                .components(separatedBy: "\n")
                .map { "      \(json($0))" }
                .joined(separator: ",\n")
            systemPromptEntry = "\"systemPrompt\": [\n\(lines)\n    ]"
        }

        let modelEntry = model.map { "    \"model\": \(json($0)),\n" } ?? ""
        let summaryDirValue = summaryDir ?? (outDir.hasSuffix("/") ? outDir + "summaries" : outDir + "/summaries")
        let hooksValue = hookPath.map { "[\n      \(json($0))\n    ]" } ?? "[]"

        let text = """
        {
          "outputDir": \(json(outDir)),
          "locale": \(json(locale)),
          "echoCancellation": false,
          "fast": false,

          "summary": {
            "dir": \(json(summaryDirValue)),
        \(modelEntry)    \(promptEntry),
            \(systemPromptEntry)
          },

          "hooks": {
            "onDone": \(hooksValue),
            "env": {}
          }
        }

        """

        // Make sure what we wrote parses with our own loader before claiming success.
        do {
            _ = try JSONDecoder().decode(Config.self, from: Data(text.utf8))
        } catch {
            throw MTError.message("internal error: generated config does not parse: \(error)")
        }
        try text.write(toFile: target, atomically: true, encoding: .utf8)

        for w in written { print("wrote \(w)") }
        if let hookNote { log(hookNote) }
        print("")
        print("Edit summary.prompt / summary.systemPrompt to change what Claude writes; outputDir / summary.dir")
        print("are relative to the config file. Check the result with:")
        print("  meet record --show-config" + (path != nil && !global ? " --config \(target)" : ""))
        if !global, path == nil {
            print("Then run `meet` from this directory (or any subdirectory) to record.")
            if fm.fileExists(atPath: cwd.appendingPathComponent(".git").path) {
                print("Tip: add `\(outDir)/` to .gitignore — it will hold audio files.")
            }
        }
    }

    /// Finds `hooks/summarize-transcript.sh` by walking up from the (symlink-resolved)
    /// executable, which lives at `<checkout>/.build/<triple>/release/meet`.
    static func locateBundledHook() -> String? {
        let fm = FileManager.default
        guard let exe = Bundle.main.executableURL?.resolvingSymlinksInPath() else { return nil }
        var dir = exe.deletingLastPathComponent()
        for _ in 0..<6 {
            let candidate = dir.appendingPathComponent("hooks/summarize-transcript.sh").path
            if fm.fileExists(atPath: candidate) { return candidate }
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        return nil
    }

    private func json(_ s: String) -> String {
        let enc = JSONEncoder()
        enc.outputFormatting = [.withoutEscapingSlashes]
        return String(decoding: (try? enc.encode(s)) ?? Data("\"\"".utf8), as: UTF8.self)
    }
}
