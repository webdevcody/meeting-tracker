import ArgumentParser
import AVFoundation
import Foundation
import Speech

@main
struct MeetingTracker: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "meet-rec",
        abstract: "Record microphone and/or system audio, transcribe on-device, then run hooks.",
        discussion: """
        This is the recording engine behind `meet`: the `meet` TUI spawns `meet-rec record --json`
        and turns the transcript into action items. Running meet-rec directly with no subcommand
        records a meeting (same as `meet-rec record`). Use `meet-rec init` to create a config file.
        """,
        subcommands: [Record.self, Init.self],
        defaultSubcommand: Record.self
    )
}

struct Record: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "record",
        abstract: "Record a meeting (default when no subcommand is given).",
        discussion: """
        Keys while recording: [space] pause/resume, [q]/[enter]/ctrl-c stop.

        Config file (first found wins): --config, $MEET_CONFIG, meet.json in the current
        directory or nearest parent, ~/.config/meet/config.json. CLI flags override the
        file. Run with --show-config to print the effective configuration, or `meet init`
        to create a per-project one.
        """
    )

    @Option(name: .customLong("out-dir"), help: "Directory to store meetings in (config: outputDir; default ~/Meetings).")
    var outDir: String?

    @Option(name: .customLong("summary-dir"), help: "Directory the summary hook writes to (config: summary.dir; default <out-dir>/summaries).")
    var summaryDir: String?

    @Option(name: .customLong("summary-prompt-file"), help: "File containing the Claude prompt used by the summary hook (config: summary.prompt / summary.promptFile).")
    var summaryPromptFile: String?

    @Option(name: .customLong("summary-system-prompt-file"), help: "File containing the system prompt for the summary hook (config: summary.systemPrompt / summary.systemPromptFile).")
    var summarySystemPromptFile: String?

    @Option(name: .customLong("summary-model"), help: "Claude model for the summary hook (config: summary.model).")
    var summaryModel: String?

    @Option(help: "Speech locale, e.g. en-US (config: locale).")
    var locale: String?

    @Option(help: "Path to the config JSON file.")
    var config: String?

    @Option(help: "Stop automatically after N seconds of recording (useful for testing).")
    var duration: Int?

    @Flag(name: .customLong("show-config"), help: "Print the effective configuration as JSON and exit.")
    var showConfig = false

    @Flag(name: .customLong("no-hooks"), help: "Do not run onDone hooks.")
    var noHooks = false

    @Flag(name: .customLong("no-mic"), help: "Record system audio only (no microphone).")
    var noMic = false

    @Flag(name: .customLong("no-system"), help: "Record the microphone only (no system audio).")
    var noSystem = false

    @Flag(name: .customLong("keep-tracks"), help: "Keep the per-source mic.m4a / system.m4a next to the merged audio.m4a.")
    var keepTracks = false

    @Flag(help: "Enable echo cancellation on the microphone (built-in mic + speakers) (config: echoCancellation).")
    var aec = false

    @Flag(help: "Faster, slightly less accurate transcription results (config: fast).")
    var fast = false

    @Flag(help: "Verbose logging.")
    var verbose = false

    @Flag(help: "Emit newline-delimited JSON events on stdout (segments, status, lifecycle) for a parent process such as the meet TUI; human-readable output moves to stderr.")
    var json = false

    /// Config file merged with CLI overrides.
    func effectiveConfig() throws -> Config {
        var cfg = try Config.load(from: config)
        let cwd = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        if let outDir { cfg.outputDir = resolvePath(outDir, base: cwd) }
        if let summaryDir { cfg.summary.dir = resolvePath(summaryDir, base: cwd) }
        if let summaryPromptFile {
            cfg.summary.prompt = nil
            cfg.summary.promptFile = resolvePath(summaryPromptFile, base: cwd)
        }
        if let summarySystemPromptFile {
            cfg.summary.systemPrompt = nil
            cfg.summary.systemPromptFile = resolvePath(summarySystemPromptFile, base: cwd)
        }
        if let summaryModel { cfg.summary.model = summaryModel }
        if let locale { cfg.locale = locale }
        if aec { cfg.echoCancellation = true }
        if fast { cfg.fast = true }
        return cfg
    }

    func run() async throws {
        signal(SIGPIPE, SIG_IGN)
        if json { Events.enable() }

        let cfg = try effectiveConfig()
        // Fail on a bad prompt file now, not after an hour of recording.
        let prompts = try cfg.summary.resolvePrompts()

        if showConfig {
            log(cfg.configPath.map { "config: \($0)" } ?? "config: (built-in defaults; no config file found)")
            print(try cfg.prettyJSON())
            return
        }
        note("Config: \(cfg.configPath ?? "(built-in defaults; run `meet-rec init` to create one)")")
        Events.emit("config", ["path": cfg.configPath ?? "", "outputDir": expandTilde(cfg.outputDir)])

        // Sources (mic / system / both) and the microphone permission
        guard !(noMic && noSystem) else {
            throw MTError.message("--no-mic and --no-system together leave nothing to record")
        }
        var sources: [Source] = []
        if !noMic {
            if await MicCapture.requestPermission() {
                sources.append(.mic)
            } else if noSystem {
                throw MTError.message("microphone access denied — grant it under System Settings › Privacy & Security › Microphone (or drop --no-system to record system audio instead)")
            } else {
                log("⚠ microphone access denied — recording system audio only. Grant it under System Settings › Privacy & Security › Microphone.")
            }
        }
        if !noSystem { sources.append(.system) }

        // Meeting directory
        let startedAt = Date()
        let root = URL(fileURLWithPath: expandTilde(cfg.outputDir))
        var meetingDir = root.appendingPathComponent(Output.dirFormatter.string(from: startedAt))
        var suffix = 1
        while FileManager.default.fileExists(atPath: meetingDir.path) {
            suffix += 1
            meetingDir = root.appendingPathComponent(Output.dirFormatter.string(from: startedAt) + "-\(suffix)")
        }
        try FileManager.default.createDirectory(at: meetingDir, withIntermediateDirectories: true)

        let session = Session(sources: sources)

        // Speech model
        let requested = Locale(identifier: cfg.locale)
        guard let speechLocale = await SpeechTranscriber.supportedLocale(equivalentTo: requested) else {
            throw MTError.message("locale \(cfg.locale) is not supported for on-device transcription")
        }
        let recorder = Recorder(meetingDir: meetingDir, sources: sources, echoCancellation: cfg.echoCancellation)
        let modules = Dictionary(uniqueKeysWithValues: recorder.sources.map {
            ($0, TrackTranscriber.makeTranscriber(locale: speechLocale, fast: cfg.fast))
        })
        let first = modules[recorder.sources[0]]!
        try await TrackTranscriber.ensureAssets(for: first) { fraction in
            Task { await session.setStatus("Downloading \(speechLocale.identifier) speech model \(Int(fraction * 100))%") }
        }
        await session.setStatus(nil)
        guard let analyzerFormat = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [first]) else {
            throw MTError.message("no compatible audio format for the speech model (assets missing?)")
        }
        if verbose { log("analyzer format: \(analyzerFormat)") }

        // Start capture + transcription
        try recorder.start()
        var transcribers: [TrackTranscriber] = []
        for source in recorder.sources {
            let t = TrackTranscriber(source: source, transcriber: modules[source]!, analyzerFormat: analyzerFormat)
            try await t.start(session: session, audio: recorder.stream(for: source)!)
            transcribers.append(t)
        }

        note("Recording to \(meetingDir.path)")
        note("Sources: \(recorder.sources.map(\.label).joined(separator: ", ")) · locale \(speechLocale.identifier)")
        if let duration { note("Auto-stop after \(duration)s") }
        Events.emit("started", ["meetingDir": meetingDir.path,
                                "sources": recorder.sources.map(\.rawValue),
                                "locale": speechLocale.identifier,
                                "startedAt": ISO8601DateFormatter().string(from: startedAt)])

        // Terminal + controls
        Terminal.enableRaw()
        defer { Terminal.restore() }
        Terminal.onStopSignals { sig in
            if sig == SIGHUP { log("ℹ terminal went away (SIGHUP); finishing the meeting") }
            Task { await session.requestStop() }
        }

        let keysTask = Task {
            for await key in Terminal.keyStream() {
                switch key {
                case 0x20:
                    let paused = await session.togglePause()
                    recorder.setPaused(paused)
                case UInt8(ascii: "q"), UInt8(ascii: "Q"), 0x0A, 0x0D, 0x03, 0x04:
                    await session.requestStop()
                    return
                default:
                    break
                }
            }
        }
        // Once a second: redraw, check source liveness, and periodically checkpoint the
        // transcript so a hard kill mid-meeting doesn't lose hours of text.
        let checkpointEvery = Double(ProcessInfo.processInfo.environment["MT_CHECKPOINT_SECS"] ?? "") ?? 60
        let verbose = self.verbose
        let tickerTask = Task {
            var tick = 0
            var lastCheckpointCount = 0
            var lastCheckpoint = Date()
            var checkpointFailures = 0
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                tick += 1
                await session.draw()
                recorder.checkHealth()

                let now = Date()
                if now.timeIntervalSince(lastCheckpoint) >= checkpointEvery {
                    lastCheckpoint = now
                    let count = await session.segmentCount
                    if count != lastCheckpointCount {
                        lastCheckpointCount = count
                        do {
                            _ = try Output.write(meetingDir: meetingDir,
                                                 startedAt: startedAt,
                                                 endedAt: now,
                                                 durationSecs: await session.elapsed,
                                                 pausedSecs: await session.pausedSeconds,
                                                 locale: speechLocale.identifier,
                                                 sources: recorder.sources,
                                                 echoCancellation: cfg.echoCancellation,
                                                 fast: cfg.fast,
                                                 segments: await session.segments)
                        } catch {
                            checkpointFailures += 1
                            if checkpointFailures == 1 { log("⚠ transcript checkpoint failed: \(error)") }
                        }
                    }
                }
                if verbose, tick % 60 == 0 {
                    let count = await session.segmentCount
                    let audio = recorder.sources.map { "\($0.rawValue) \(Output.humanDuration(recorder.seconds(for: $0)))" }.joined(separator: " ")
                    log("ℹ \(Session.format(await session.elapsed)) rss \(Diagnostics.residentMB()) MB · \(count) segments · audio \(audio) · \(recorder.healthSummary)")
                }
            }
        }
        let durationTask: Task<Void, Never>? = duration.map { secs in
            Task {
                while !Task.isCancelled {
                    try? await Task.sleep(for: .milliseconds(200))
                    if await session.elapsed >= Double(secs) {
                        await session.requestStop()
                        return
                    }
                }
            }
        }

        recorder.startClock()
        await session.start()
        await session.waitUntilStopped()

        tickerTask.cancel()
        durationTask?.cancel()
        keysTask.cancel()

        // Stop capture, drain transcribers
        recorder.stop()
        let endedAt = Date()
        await session.setStatus("Finalizing transcription…")
        for t in transcribers { await t.finish() }
        await session.clearStatus()
        Terminal.restore()

        // One audio file per meeting: sum the per-source tracks (or just rename a lone one).
        let trackURLs = recorder.sources.map { recorder.audioURL(for: $0) }
        let audioURL = meetingDir.appendingPathComponent(Output.audioFileName)
        var mergedAudio: URL?
        var tracksKept = false
        do {
            if recorder.sources.count > 1 {
                note("Merging audio…")
                if keepTracks {
                    try AudioMixer.mix(trackURLs, into: audioURL)
                    tracksKept = true
                } else {
                    try AudioMixer.merge(trackURLs, into: audioURL)
                }
            } else {
                try AudioMixer.merge(trackURLs, into: audioURL)
            }
            mergedAudio = audioURL
        } catch {
            tracksKept = true
            log("⚠ could not merge audio into \(Output.audioFileName): \(error) — keeping the per-source files")
        }

        let elapsed = await session.elapsed
        let pausedSecs = await session.pausedSeconds
        let output = try Output.write(meetingDir: meetingDir,
                                      startedAt: startedAt,
                                      endedAt: endedAt,
                                      durationSecs: elapsed,
                                      pausedSecs: pausedSecs,
                                      locale: speechLocale.identifier,
                                      sources: recorder.sources,
                                      echoCancellation: cfg.echoCancellation,
                                      fast: cfg.fast,
                                      segments: await session.segments)

        let segCount = await session.segments.count
        note("Saved \(output.meetingDir.path)")
        note("  duration \(Output.humanDuration(elapsed)), \(segCount) segment(s)")
        if mergedAudio != nil {
            let audioSecs = recorder.sources.map { recorder.seconds(for: $0) }.max() ?? 0
            note("  \(Output.audioFileName)  \(Output.humanDuration(audioSecs))")
        }
        for source in recorder.sources where recorder.paddedSeconds(for: source) >= 1 {
            note("  (\(source.rawValue) delivered no audio for \(Output.humanDuration(recorder.paddedSeconds(for: source))); filled with silence to stay in sync)")
        }
        if tracksKept {
            for source in recorder.sources {
                note("  \(source.rawValue).m4a  \(Output.humanDuration(recorder.seconds(for: source)))")
            }
        }
        note("  transcript.md, transcript.json, meta.json")
        Events.emit("finished", ["meetingDir": output.meetingDir.path,
                                 "durationSecs": elapsed,
                                 "segmentCount": segCount,
                                 "transcriptPath": output.transcriptMD.path,
                                 "transcriptJsonPath": output.transcriptJSON.path,
                                 "audioPath": mergedAudio?.path ?? ""])

        if !noHooks, !cfg.hooks.onDone.isEmpty {
            let iso = ISO8601DateFormatter()
            let startedISO = iso.string(from: startedAt)
            let durationSecs = String(Int(elapsed.rounded()))
            var env: [String: String] = [
                "MT_MEETING_DIR": meetingDir.path,
                "MT_OUTPUT_DIR": root.path,
                "MT_SUMMARY_DIR": cfg.summaryDir,
                "MT_TRANSCRIPT_PATH": output.transcriptMD.path,
                "MT_TRANSCRIPT_JSON_PATH": output.transcriptJSON.path,
                "MT_META_PATH": output.metaJSON.path,
                "MT_AUDIO_DIR": meetingDir.path,
                "MT_STARTED_AT": startedISO,
                "MT_DURATION_SECS": durationSecs,
                "MT_SEGMENT_COUNT": String(segCount),
            ]
            if let mergedAudio { env["MT_AUDIO_PATH"] = mergedAudio.path }
            if tracksKept {
                for source in recorder.sources {
                    env["MT_AUDIO_\(source.rawValue.uppercased())_PATH"] = recorder.audioURL(for: source).path
                }
            }
            if let p = cfg.configPath { env["MT_CONFIG_PATH"] = p }

            // Values available as {{name}} inside summary prompts and hooks.env.
            let vars: [String: String] = [
                "date": String(startedISO.prefix(10)),
                "startedAt": startedISO,
                "durationSecs": durationSecs,
                "duration": Output.humanDuration(elapsed),
                "segmentCount": String(segCount),
                "meetingDir": meetingDir.path,
                "outputDir": root.path,
                "summaryDir": cfg.summaryDir,
                "transcriptPath": output.transcriptMD.path,
                "transcriptJsonPath": output.transcriptJSON.path,
                "metaPath": output.metaJSON.path,
                "audioPath": mergedAudio?.path ?? "",
                "locale": speechLocale.identifier,
            ]
            if let p = prompts.prompt { env["MT_SUMMARY_PROMPT"] = Placeholders.expand(p, with: vars) }
            if let s = prompts.systemPrompt { env["MT_SUMMARY_SYSTEM_PROMPT"] = Placeholders.expand(s, with: vars) }
            if let m = cfg.summary.model { env["MT_SUMMARY_MODEL"] = m }
            if let c = cfg.summary.claudePath { env["MT_CLAUDE_BIN"] = c }
            for (k, v) in cfg.hooks.env { env[k] = Placeholders.expand(v, with: vars) }

            await Hooks.run(cfg.hooks.onDone, env: env, cwd: meetingDir, stdin: output.transcriptText)
        }
        Darwin.exit(0)
    }
}
