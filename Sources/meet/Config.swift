import Foundation

struct HooksConfig: Codable, Sendable {
    /// Shell commands run (sequentially, via `/bin/sh -c`) after the transcript is written.
    var onDone: [String] = []
    /// Extra environment variables exported to every hook. Values support `{{placeholders}}`
    /// (see `Placeholders`). Applied last, so they can override the built-in MT_* variables.
    var env: [String: String] = [:]

    init(onDone: [String] = [], env: [String: String] = [:]) {
        self.onDone = onDone
        self.env = env
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        onDone = try c.decodeIfPresent([String].self, forKey: .onDone) ?? []
        env = try c.decodeIfPresent([String: String].self, forKey: .env) ?? [:]
    }
}

/// Settings for the bundled `hooks/summarize-transcript.sh` hook (or any hook that reads the
/// `MT_SUMMARY_*` variables). Everything here is optional; unset keys fall back to the hook's
/// built-in defaults.
struct SummaryConfig: Codable, Sendable {
    /// Where summaries are written. Default: `<outputDir>/summaries`. → `MT_SUMMARY_DIR`
    var dir: String?
    /// The instruction given to Claude (the user prompt). Either a string or an array of
    /// lines (joined with newlines). → `MT_SUMMARY_PROMPT`
    var prompt: String?
    /// Read the prompt from this file instead (relative paths resolve against the config file).
    var promptFile: String?
    /// Text passed to `claude --append-system-prompt`. String or array of lines.
    /// → `MT_SUMMARY_SYSTEM_PROMPT`
    var systemPrompt: String?
    var systemPromptFile: String?
    /// Claude model, e.g. "claude-sonnet-5". → `MT_SUMMARY_MODEL`
    var model: String?
    /// Path to the `claude` binary if it is not in PATH. → `MT_CLAUDE_BIN`
    var claudePath: String?

    enum CodingKeys: String, CodingKey {
        case dir, prompt, promptFile, systemPrompt, systemPromptFile, model, claudePath
    }

    /// Defaults written by `meet init`. Keep in sync with the fallbacks in
    /// hooks/summarize-transcript.sh (used when the config leaves these unset).
    static let defaultPrompt = "Summarize this meeting transcript."
    static let defaultSystemPrompt = """
        You are running headless as a meet hook; do not ask questions.
        Read the transcript in the prompt and write ONE Markdown file into the summary directory using the Write tool.
        File name: <meeting date>_<descriptive-kebab-case-title>.md (e.g. 2026-08-28_billing-migration-release-plan.md) — the title must describe what the meeting was actually about, 3-7 words, lowercase, hyphens only, no other punctuation.
        File contents: a '# ' heading with the descriptive title in Title Case, a line with the date and duration, then sections: '## Summary' (2-5 sentences), '## Key Points' (bullets), '## Decisions' (bullets, or 'None recorded'), '## Action Items' (bullets with owner if stated, or 'None recorded').
        Speakers are labeled by audio source: Microphone (this Mac's microphone — the recording user and anyone else in the room) and System (audio this Mac played — remote call participants, videos); either may contain several people. Do not invent facts that are not in the transcript.
        When done, print only the absolute path of the file you wrote.
        """

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        dir = try c.decodeIfPresent(String.self, forKey: .dir)
        prompt = try Self.decodeText(c, .prompt)
        promptFile = try c.decodeIfPresent(String.self, forKey: .promptFile)
        systemPrompt = try Self.decodeText(c, .systemPrompt)
        systemPromptFile = try c.decodeIfPresent(String.self, forKey: .systemPromptFile)
        model = try c.decodeIfPresent(String.self, forKey: .model)
        claudePath = try c.decodeIfPresent(String.self, forKey: .claudePath)
    }

    /// Accepts `"text"` or `["line", "line"]` (joined with "\n").
    private static func decodeText(_ c: KeyedDecodingContainer<CodingKeys>, _ key: CodingKeys) throws -> String? {
        if let s = try? c.decodeIfPresent(String.self, forKey: key) { return s }
        if let lines = try c.decodeIfPresent([String].self, forKey: key) { return lines.joined(separator: "\n") }
        return nil
    }

    /// Resolved prompt texts, read from files where requested. Inline text wins over a file.
    struct Resolved: Sendable {
        var prompt: String?
        var systemPrompt: String?
    }

    func resolvePrompts() throws -> Resolved {
        var r = Resolved(prompt: prompt, systemPrompt: systemPrompt)
        if r.prompt == nil, let f = promptFile { r.prompt = try Self.readText(f, what: "summary.promptFile") }
        if r.systemPrompt == nil, let f = systemPromptFile { r.systemPrompt = try Self.readText(f, what: "summary.systemPromptFile") }
        return r
    }

    private static func readText(_ path: String, what: String) throws -> String {
        do {
            return try String(contentsOfFile: path, encoding: .utf8)
                .trimmingCharacters(in: .newlines)
        } catch {
            throw MTError.message("could not read \(what) \(path): \(error.localizedDescription)")
        }
    }
}

struct Config: Codable, Sendable {
    var outputDir: String = "~/Meetings"
    var locale: String = "en-US"
    var echoCancellation: Bool = false
    var fast: Bool = false
    var summary: SummaryConfig = SummaryConfig()
    var hooks: HooksConfig = HooksConfig()

    /// Path the config was loaded from (nil = built-in defaults). Not serialized.
    var configPath: String? = nil

    enum CodingKeys: String, CodingKey {
        case outputDir, locale, echoCancellation, fast, summary, hooks
    }

    static let envVar = "MEET_CONFIG"
    static let localFileName = "meet.json"
    static let defaultPath = "~/.config/meet/config.json"
    /// Pre-rename location, still honoured as a last resort.
    static let legacyDefaultPath = "~/.config/meeting-tracker/config.json"

    /// Candidate locations, first match wins (after `--config` and `$MEET_CONFIG`):
    /// `meet.json` in the current directory or any parent (nearest wins, like `.git`), then
    /// the global `~/.config/meet/config.json` (or the pre-rename
    /// `~/.config/meeting-tracker/config.json`).
    static var searchPaths: [String] {
        var paths: [String] = []
        var dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        while true {
            paths.append(dir.appendingPathComponent(localFileName).path)
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        paths.append(expandTilde(defaultPath))
        paths.append(expandTilde(legacyDefaultPath))
        return paths
    }

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        outputDir = try c.decodeIfPresent(String.self, forKey: .outputDir) ?? "~/Meetings"
        locale = try c.decodeIfPresent(String.self, forKey: .locale) ?? "en-US"
        echoCancellation = try c.decodeIfPresent(Bool.self, forKey: .echoCancellation) ?? false
        fast = try c.decodeIfPresent(Bool.self, forKey: .fast) ?? false
        summary = try c.decodeIfPresent(SummaryConfig.self, forKey: .summary) ?? SummaryConfig()
        hooks = try c.decodeIfPresent(HooksConfig.self, forKey: .hooks) ?? HooksConfig()
    }

    /// Loads the config file. Resolution order:
    /// 1. `explicitPath` (`--config`) — must exist
    /// 2. `$MEET_CONFIG` — must exist
    /// 3. `meet.json` in the current directory or the nearest parent
    /// 4. `~/.config/meet/config.json`, then the legacy `~/.config/meeting-tracker/config.json`
    /// If none exist, built-in defaults are used.
    static func load(from explicitPath: String?) throws -> Config {
        let fm = FileManager.default
        var path: String?
        if let explicitPath {
            path = expandTilde(explicitPath)
            guard fm.fileExists(atPath: path!) else { throw MTError.message("config file not found: \(path!)") }
        } else if let fromEnv = ProcessInfo.processInfo.environment[envVar], !fromEnv.isEmpty {
            path = expandTilde(fromEnv)
            guard fm.fileExists(atPath: path!) else { throw MTError.message("config file not found ($\(envVar)): \(path!)") }
        } else {
            path = searchPaths.first { fm.fileExists(atPath: $0) }
        }
        guard let path else { return Config() }

        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        var cfg: Config
        do {
            cfg = try JSONDecoder().decode(Config.self, from: data)
        } catch {
            throw MTError.message("could not parse \(path): \(error)")
        }
        cfg.configPath = path
        cfg.resolveRelativePaths(against: URL(fileURLWithPath: path).deletingLastPathComponent())
        return cfg
    }

    /// Relative paths in the config are relative to the config file's directory, so a config
    /// can ship alongside its prompt files.
    private mutating func resolveRelativePaths(against base: URL) {
        outputDir = resolvePath(outputDir, base: base)
        summary.dir = summary.dir.map { resolvePath($0, base: base) }
        summary.promptFile = summary.promptFile.map { resolvePath($0, base: base) }
        summary.systemPromptFile = summary.systemPromptFile.map { resolvePath($0, base: base) }
        summary.claudePath = summary.claudePath.map { $0.contains("/") ? resolvePath($0, base: base) : $0 }
    }

    /// Absolute path of the summary directory (`summary.dir` or `<outputDir>/summaries`).
    var summaryDir: String {
        if let d = summary.dir { return expandTilde(d) }
        return expandTilde(outputDir) + "/summaries"
    }

    /// Effective configuration as pretty-printed JSON (for `--show-config`).
    func prettyJSON() throws -> String {
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return String(decoding: try enc.encode(self), as: UTF8.self)
    }
}

func expandTilde(_ s: String) -> String {
    NSString(string: s).expandingTildeInPath
}

/// `~`-expands, and makes relative paths absolute against `base`.
func resolvePath(_ s: String, base: URL) -> String {
    let expanded = expandTilde(s)
    if expanded.hasPrefix("/") { return expanded }
    return base.appendingPathComponent(expanded).standardizedFileURL.path
}

/// `{{name}}` substitution for prompts and hook env values.
enum Placeholders {
    static func expand(_ template: String, with vars: [String: String]) -> String {
        var out = template
        for (k, v) in vars { out = out.replacingOccurrences(of: "{{\(k)}}", with: v) }
        return out
    }
}

enum MTError: Error, CustomStringConvertible {
    case message(String)
    case coreAudio(String, OSStatus)

    var description: String {
        switch self {
        case .message(let m): return m
        case .coreAudio(let fn, let status): return "\(fn) failed (OSStatus \(status))"
        }
    }
}

func log(_ s: String) {
    FileHandle.standardError.write(Data(("\r\u{1B}[2K" + s + "\n").utf8))
}
