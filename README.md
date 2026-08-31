# meet

A macOS command-line meeting recorder (`meet`). It captures your **microphone** (you and
anyone else in the room) and/or **all system audio** (Zoom, Meet, Teams, browser — anything
playing), transcribes each source **on-device** with Apple's `SpeechAnalyzer` while you
record, then merges everything into one audio file + one transcript on disk and runs your
shell hooks (e.g. have `claude -p` write a summary).

- Pure Swift, no third-party audio drivers (no BlackHole/Loopback), no cloud.
- Requires **macOS 26** (Tahoe) on Apple Silicon and Xcode 26.
- Output per meeting: `audio.m4a` (mono AAC, ~35 MB/hour), `transcript.md`,
  `transcript.json`, `meta.json`.

## Install

```sh
make install          # builds release and symlinks ~/.local/bin/meet
meet --help
```

## Per-project setup

Each project can have its own config — its own summary prompt, storage path, model, hooks:

```sh
cd ~/Work/acme-app
meet init        # writes ./meet.json (meetings go to ./meetings)
$EDITOR meet.json  # tweak summary.prompt, outputDir, …
meet             # record; uses the nearest meet.json up the tree
```

`meet` looks for `meet.json` in the current directory, then each
parent (like `.git`), so it works from any subdirectory of the project. Paths in the file
are relative to the file itself. With no project config it falls back to the global
`~/.config/meet/config.json` (`meet init --global` creates that one,
storing in `~/Meetings`). The chosen config is printed at the start of every recording.

## Usage

```sh
meet                 # mic + system audio (a call, possibly with people in the room too)
meet --no-mic        # system audio only (e.g. just listening in)
meet --no-system     # microphone only (an in-person meeting)
meet --keep-tracks   # also keep the separate mic.m4a / system.m4a next to audio.m4a
meet --aec           # echo cancellation: built-in mic + speakers, no headphones
meet --duration 5    # auto-stop after 5 s (handy for testing)
meet --no-hooks      # record/transcribe but skip onDone hooks
meet --out-dir ~/Work/meetings --summary-dir ~/Work/summaries
meet --config ./team.json --summary-prompt-file ./prompts/standup.md
meet --show-config   # print the effective config (file + flags) and exit
```

While recording the terminal shows a status line and prints each finalized sentence
labeled by the source it came from, `Microphone: …` / `System: …`:

```
● REC 00:12:34 │ mic 12 · system 8 │ 84 MB │ [space] pause  [q] stop
```

(`84 MB` is the process's resident memory, so you can keep an eye on it during long
recordings.)

Keys: `space` pause/resume (paused time is excluded from the files, transcript and timer),
`q` / `enter` / `ctrl-c` stop. `SIGTERM` and `SIGHUP` (closing the terminal window,
dropped SSH) also stop cleanly: audio files are finalized, the transcript is written and
hooks run.

## Long recordings

The recorder is built to run for hours unattended:

- **Bounded memory.** Audio is encoded to disk as it arrives; the only in-memory queues
  (audio → transcriber) are capped at ~30 s per track. If the on-device speech model ever
  falls behind real time, old audio is dropped from the *transcription* queue (logged once,
  then every 1000 drops) rather than growing without limit — the `.m4a` files are unaffected.
- **Transcript checkpoints.** `transcript.md` / `transcript.json` / `meta.json` are rewritten
  every 60 s while segments arrive, so a hard kill (power loss, `kill -9`) loses at most a
  minute of text. The audio files, however, are only finalized on a clean stop.
- **Device changes.** Plugging in headphones, connecting AirPods, or sleep/wake changes the
  default output device; the system-audio tap notices and rebuilds itself (`ℹ system audio
  tap restarted`). The microphone input follows `AVAudioEngine` configuration changes the
  same way. A source that goes silent for 30 s while audio is playing is restarted
  automatically.
- **Quiet on persistent errors.** A failing disk write or format conversion is logged once
  and then every 500th time, not per buffer.
- **`--verbose`** logs a health line every minute: elapsed, resident memory, segment count,
  seconds of audio per track, dropped buffers and tap restarts. Redirect stderr to a file if
  you want a trail for a very long run.

Things to know:

- If your default output is a **Bluetooth headset** and nothing is playing, macOS does not
  run audio IO on it, so the system source receives nothing during silence — that is normal,
  not a fault, and capture resumes as soon as audio plays.
- A Bluetooth headset **microphone** (HFP, 16 kHz) drops its link while the output device
  is being switched; expect a few seconds of silence in the microphone audio around such switches.
- **No drift.** Every source is locked to one recording clock using the capture timestamps
  Core Audio attaches to each buffer. Whenever a source delivers nothing (idle output device,
  tap restart, Bluetooth re-link, slow start-up) that stretch is filled with silence — in the
  audio and in what the transcriber hears — so `audio.m4a` mixes cleanly and transcript lines
  from different sources interleave in the right order. Buffers landing within 250 ms of
  where they belong are left untouched (no clicks), so residual misalignment is bounded by
  that and never accumulates. At stop every track is padded to the same length, and the
  summary line reports how much silence was inserted per source.

## Permissions (first run)

macOS attributes a CLI's permissions to the **terminal app** that launched it.

- **Microphone** — you get the normal "… would like to use your microphone" prompt.
- **System audio** — there is no prompt for many terminals (Ghostty, iTerm, VS Code).
  If the system source stays silent / you see `⚠ no system audio received`, add your terminal under
  **System Settings › Privacy & Security › Screen & System Audio Recording** in the
  **System Audio Recording Only** list (the `+` button), then re-run. Terminal.app usually
  prompts on its own.

## Config

meet reads one JSON config file. The first of these that exists wins:

1. `--config <path>`
2. `$MEET_CONFIG`
3. `meet.json` in the current directory, else the nearest parent directory
4. `~/.config/meet/config.json` (global fallback; the pre-rename
   `~/.config/meeting-tracker/config.json` is still honoured after it)

CLI flags always override the file. `meet --show-config` prints the effective
configuration (and which file it came from) without recording.

`meet init` writes a complete config with every key filled in (the default
Claude prompt + system prompt, storage paths, and the absolute path of the bundled summary
hook):

```sh
meet init                     # ./meet.json, storage ./meetings
meet init --global            # ~/.config/meet/config.json, storage ~/Meetings
meet init --out-dir recordings --model claude-sonnet-5
meet init --prompt-files      # prompt + system prompt as .md files next to the config
meet init team.json --force   # explicit path; overwrite if it exists
```

All keys are optional; see `config.example.json`:

```json
{
  "outputDir": "~/Meetings",
  "locale": "en-US",
  "echoCancellation": false,
  "fast": false,
  "summary": {
    "dir": "~/Meetings/summaries",
    "model": "claude-sonnet-5",
    "prompt": "Summarize this meeting transcript.",
    "systemPrompt": ["line 1", "line 2"],
    "promptFile": "prompts/summary.md",
    "systemPromptFile": "prompts/system.md",
    "claudePath": "/opt/homebrew/bin/claude"
  },
  "hooks": {
    "onDone": ["/path/to/meet/hooks/summarize-transcript.sh"],
    "env": { "MY_VAR": "{{meetingDir}}/notes" }
  }
}
```

| key | CLI flag | meaning |
|---|---|---|
| `outputDir` | `--out-dir` | where each meeting's folder (`YYYY-MM-DD_HH-mm/`) is created. Default `~/Meetings`. |
| `locale` | `--locale` | speech locale. Default `en-US`. |
| `echoCancellation` | `--aec` | echo cancellation on the mic. |
| `fast` | `--fast` | faster, slightly less accurate transcription. |
| `summary.dir` | `--summary-dir` | where the summary hook writes. Default `<outputDir>/summaries`. |
| `summary.prompt` / `summary.promptFile` | `--summary-prompt-file` | the instruction sent to `claude -p`. A string, an array of lines, or a file. |
| `summary.systemPrompt` / `summary.systemPromptFile` | `--summary-system-prompt-file` | text for `claude --append-system-prompt` (output format, file naming, …). |
| `summary.model` | `--summary-model` | `claude --model …`. Default: Claude's own default. |
| `summary.claudePath` | — | the `claude` binary if it's not in `PATH`. |
| `hooks.onDone` | `--no-hooks` to skip | shell commands run after the transcript is written. |
| `hooks.env` | — | extra env vars for every hook. Applied last, so they can override any `MT_*` value. |

Relative paths in the file (`promptFile`, `summary.dir`, …) resolve against the config
file's directory, so a config can live next to its prompt files. `~` is expanded everywhere.

### Prompt placeholders

`summary.prompt`, `summary.systemPrompt` and `hooks.env` values may contain `{{name}}`
placeholders, filled in per meeting:

`{{date}}` (YYYY-MM-DD) · `{{startedAt}}` (ISO-8601) · `{{duration}}` (`12m 34s`) ·
`{{durationSecs}}` · `{{segmentCount}}` · `{{locale}}` · `{{meetingDir}}` · `{{outputDir}}` ·
`{{summaryDir}}` · `{{transcriptPath}}` · `{{transcriptJsonPath}}` · `{{metaPath}}` · `{{audioPath}}`

### Hooks

Each `onDone` command runs sequentially via `/bin/sh -c` after the transcript is written,
with cwd = the meeting directory, the Markdown transcript on **stdin**, and these env vars:

| var | value |
|---|---|
| `MT_MEETING_DIR` | `~/Meetings/2026-08-28_19-15` |
| `MT_OUTPUT_DIR` | the configured `outputDir` |
| `MT_TRANSCRIPT_PATH` | `…/transcript.md` |
| `MT_TRANSCRIPT_JSON_PATH` | `…/transcript.json` |
| `MT_META_PATH` | `…/meta.json` |
| `MT_AUDIO_PATH` | `…/audio.m4a`, the merged recording |
| `MT_AUDIO_MIC_PATH` / `MT_AUDIO_SYSTEM_PATH` | the per-source `.m4a` files; only present with `--keep-tracks` (or if merging failed) |
| `MT_STARTED_AT` | ISO-8601 start time |
| `MT_DURATION_SECS` | recorded seconds (excludes pauses) |
| `MT_SEGMENT_COUNT` | number of transcribed segments |
| `MT_CONFIG_PATH` | the config file that was loaded (absent when using defaults) |
| `MT_SUMMARY_DIR` | `summary.dir` (default `<outputDir>/summaries`) |
| `MT_SUMMARY_PROMPT` | `summary.prompt` / `promptFile`, placeholders expanded (absent if unset) |
| `MT_SUMMARY_SYSTEM_PROMPT` | `summary.systemPrompt` / `systemPromptFile`, placeholders expanded (absent if unset) |
| `MT_SUMMARY_MODEL` | `summary.model` (absent if unset) |
| `MT_CLAUDE_BIN` | `summary.claudePath` (absent if unset) |
| anything in `hooks.env` | as configured |

A failing hook is logged and the next hook still runs.

`hooks/summarize-transcript.sh` pipes the transcript into `claude -p`, which writes a
Markdown summary to `MT_SUMMARY_DIR/<date>_<descriptive-title>.md`, e.g.
`~/Meetings/summaries/2026-08-28_billing-migration-release-plan.md`. The prompt, system
prompt, model and directory come from the `summary` config block above (the script has
built-in defaults matching `config.example.json` for anything unset). The transcript and a
short metadata block (date, duration, transcript path, summary directory) are always
appended after the prompt. Needs `claude` in `PATH` (or `summary.claudePath`); Claude's
output is also saved to `<meeting dir>/summary.log`.

## Output format

Each source is transcribed on its own, and the results are interleaved by time into one
transcript. Lines are labeled by **audio source**, not by person: `Microphone` is whatever
the mic heard (you, plus anyone in the room with you), `System` is whatever the Mac played
(everyone on the call). Either label can therefore cover several people — there is no
per-voice diarization.

`transcript.md` — no timestamps, consecutive lines from the same source merged:

```markdown
# Meeting 2026-08-28 19:15
Duration: 12m 34s

**System:** So the plan for the release is …

**Microphone:** Agreed, I'll take the migration.
```

`transcript.json` keeps the raw segments (`source`, `text`, `start`, `end`) sorted by
`start`; `meta.json` records locale, sources, the audio file name, paused seconds and flags.

`audio.m4a` is the sum of the per-source tracks (they share a timeline — see *No drift*
above — including pauses), hard-clipped at full scale. With a single source it is simply
that source's recording.

## Layout

```
Sources/meet/
  App.swift            CLI flags, wiring, main loop
  Config.swift         config file lookup, defaults, summary/hook settings
  Init.swift           `meet init` config generator
  Diagnostics.swift    resident-memory readout, rate-limited logging
  Session.swift        actor: pause/stop state, segments, status line
  Recorder.swift       owns capture sources + writers, pause gate, fan-out
  Mixer.swift          sums the per-source tracks into audio.m4a
  SystemAudioTap.swift Core Audio process tap (system audio)
  MicCapture.swift     AVAudioEngine input
  TrackWriter.swift    AAC .m4a writer
  BufferConverter.swift sample-rate/format conversion
  Transcriber.swift    SpeechAnalyzer + SpeechTranscriber per source
  Terminal.swift       raw mode, keys, signals
  Output.swift         transcript.md / transcript.json / meta.json
  Hooks.swift          onDone runner
hooks/                 example hook scripts
```
