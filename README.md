# meet

**Talk to your repo.** `meet` is a terminal UI you run inside a git checkout and talk into
— alone at your desk, or on a call. It records the **microphone** and/or **all system
audio** (Zoom, Meet, Teams, anything playing), transcribes **on-device** with Apple's
`SpeechAnalyzer`, and every minute or so hands the newest stretch of transcript to a fast,
low-effort headless Claude that looks up what *this* repository says about what is being
discussed — which file does that, what the flag is called, how it behaves today. Those
**facts** land in the Related pane next to the transcript while the conversation goes on.
Press **`a`** (or run `meet ask` in another terminal) for a **Claude Code session that answers
questions over the transcript as it grows** — "what did they decide about the deploy?",
"which files came up?". Press **`x`** to stop the recording: the whole transcript goes to Claude once, which writes
the meeting's **summary** and its **action items**: concrete changes to this repository that
you asked for or decided on, each written up as a fully specified prompt. Select one and
press **Enter**: `meet` creates a git worktree on a new branch, runs a **Claude Code agent**
in it, commits, pushes, and opens a **pull request** with `gh`.

Every session is kept — every transcript line, the summaries, the facts, the action items —
and can be read back later from the same TUI (`S`). An agent that was still running when
you quit resumes on the next launch in that repo; `X` stops one, and Enter resumes it where
it was. An optional *on-done* instruction ("comment a summary on the GitHub issue") reaches
each agent through a Claude Code Stop hook just before it finishes.

While recording — the transcript, and what the repo says about it:

```
 meet my-app (main)                                              ● REC 00:12:34  mic 41 · system 8
╭ Transcript ─────────────────────────╮╭ Related · 5 · looking up #3 ────────────────────────╮
│00:06 mic    the list command just   ││00:00 `todo.sh list` prints plain text through the   │
│             prints plain text, I    ││      awk on line 18; there is no --json flag.       │
│             want a --json flag …    ││      todo.sh:18                                     │
│00:24 mic    adding a todo with      ││      `todo.sh add` takes only $1, so "buy milk"     │
│             spaces is broken …      ││      becomes "buy".                                 │
╰─────────────────────────────────────╯│      todo.sh:9                                      │
╭ Summary · 31 words pending ─────────╮│01:05 The README documents add and list; `done` is   │
│[1] 00:00–01:05 Wants a --json flag  ││      not mentioned.                                 │
│    on list and is annoyed that add  ││      README.md                                      │
│    drops everything after the first ││                                                     │
│    word.                            ││                                                     │
│[2] 01:05–02:10 looking up…          ││                                                     │
╰─────────────────────────────────────╯╰─────────────────────────────────────────────────────╯
 x stop recording  Tab related/items  Enter run  X stop agent  d dismiss  i note  s look up now  space pause  ? help  q quit
```

After `x` — the action items, ready to run:

```
 meet my-app (main)                                                     ■ ended 00:14:02  mic 47 · system 9
╭ Transcript ─────────────────────────╮╭ Action items · 3 · 1 running ──────────────────────╮
│00:06 mic    the list command just   ││○ Add --json flag to `todo.sh list`                  │
│             prints plain text, I    ││● Fix `todo add` to keep text after first word   ● Ba…│
│             want a --json flag …    ││● Document `done` command in README          ✓ PR #12 │
│00:24 mic    adding a todo with      │╰─────────────────────────────────────────────────────╯
│             spaces is broken …      │╭ Prompt · Enter runs it ─────────────────────────────╮
╰─────────────────────────────────────╯│Add --json flag to `todo.sh list`                    │
╭ Summary ────────────────────────────╮│said: "I want a dash dash json flag on list so that  │
│[1] 00:00–01:05 Wants a --json flag  ││other tools can parse it"                            │
│    on list and is annoyed that add  ││                                                     │
│    drops everything after the first ││In todo.sh, the `list` command prints plain text via │
│    word.                            ││the awk on line 18. Add a `--json` flag so that      │
│[2] 01:05–02:10 Decided the README   ││`./todo.sh list --json` prints one JSON object per   │
╰─────────────────────────────────────╯╰─────────────────────────────────────────────────────╯
 Tab related/items  Enter run  X stop agent  d dismiss  l log  o open PR  S sessions  ←/→ session  ? help  q quit
```

Two binaries, one tool: `meet` (Rust, the TUI; `crates/meet`) and `meet-rec` (Swift, the
recording + transcription engine; `Sources/meet`). Pure Swift audio — no BlackHole, no
Loopback, no cloud for the audio. Claude is only ever called through the `claude` CLI you
already have. Requires **macOS 26** (Tahoe) on Apple Silicon, Xcode 26, Rust, `claude`
and `gh` on your `PATH`.

## Install

```sh
make install          # builds both binaries, symlinks ~/.local/bin/meet and ~/.local/bin/meet-rec
meet --help
```

## Use it

```sh
cd ~/code/my-app
meet                  # mic + system audio, in this repo
meet --no-system      # microphone only (you, thinking out loud)
meet --no-mic         # system audio only (a call you are listening to)
```

Then talk. The transcript scrolls on the left. When a chunk of talk closes — after a
pause, or ~180 words, or 90 s — a one-line summary appears under it, and the **Related**
pane on the right fills with what the repository says about what was just said: short
facts, each with the file (and line) that shows it, found by a fast `sonnet --effort low`
call with read-only tools. `Tab` flips the right pane to the action-item board and back.

Press **`x`** when you are done talking (it asks first — there is no starting the
recording again). The transcript and audio are saved, the engine's hooks run, and the
whole transcript goes to Claude once: it writes the meeting's summary and its **action
items**, which land at the top of the board, newest first. Each item carries:

- a **title** you can read at a glance (`Add --json flag to `todo.sh list``),
- the **prompt** the agent will get: one complete request in your own words with the real
  file names, what must stay as it is, an example of the expected output, the *why* from
  what you said, and any gap it could not verify marked `(assuming …)` — the same
  prompt-doctor rules nebula's `prompt-daddy` skill applies to a human's prompt,
- `said:` the sentence from the transcript that motivated it, so you can trust where it
  came from.

Items are proposed, never run on their own. Press **Enter** on one and `meet`:

1. creates a branch named from the title and a git worktree for it at
   `<repo>/../<repo-name>-worktrees/<branch>` (nebula's layout, so the two tools' worktrees
   sit side by side),
2. runs `claude -p` in that worktree with full permissions, the prompt, the meeting
   context, and a PR-body template to fill,
3. commits anything the agent left uncommitted, pushes the branch, and opens the pull
   request with `gh pr create` — the title and body the agent wrote, or an honest
   fallback that says the agent did not write one,
4. shows the PR number on the row; `o` opens it in the browser, `l` shows the agent's log.

Several items can run at once; each has its own worktree. A run that stops short — you
pressed `X`, you quit `meet`, it crashed, `gh` was not logged in — keeps its worktree, its
commits and its Claude session. Enter on it again does not re-implement: it resumes the
agent's conversation (`claude --resume`) and tells it to finish, or, when the agent was
already done and only the push or the pull request failed, redoes just that tail. See
[Sessions, stopping and resuming](#sessions-stopping-and-resuming).

### Keys

| key | |
|---|---|
| `x` | stop the recording and stay: the transcript and audio are saved, the engine's hooks run, and the action items are written from everything that was said |
| `Tab` | flip the right pane: related facts / action items (the board takes over on its own when the items land) |
| `j` / `k`, `↓` / `↑` | select an action item |
| `Enter` | run it — worktree, agent, pull request — or resume it where it stopped |
| `X` | stop its agent (SIGTERM, then SIGKILL after 4 s); the worktree, commits and session stay |
| `d` | dismiss it (gone for good) |
| `l` | the agent's log, live while it runs |
| `o` | open its pull request |
| `a` | ask questions: a Claude Code session over the live transcript — beside `meet` in nebula or tmux when it can, else it names the `meet ask` command to run in another terminal |
| `i` | type a note into the transcript (no mic needed) |
| `s` | look up what is pending right now; once the recording has ended, write the action items again |
| `space` | pause / resume the recording (paused time is excluded from the files) |
| `PgUp` / `PgDn`, `J` / `K` | scroll the transcript; `G` follows the newest line again |
| `[` / `]` | scroll the right pane (the facts, or the prompt) |
| `S` | the session list: every meeting held in this repo, newest first |
| `←` / `→` | step to an older / newer session; `Esc` returns to the live meeting |
| `?` | keys |
| `q`, `Ctrl+C` | quit. While recording it asks: `y` stops the recording, saves and quits *without* writing action items; `x` stops the recording and stays instead. Running agents are stopped and resume on the next launch |

### What is kept where

- **Transcript and audio** go where the engine puts them: `meet.json` → `outputDir`, else
  `~/Meetings/<date>_<time>/` (`audio.m4a`, `transcript.md`, `transcript.json`,
  `meta.json`). The engine's `onDone` hooks (the bundled summary hook) still run when the
  meeting ends.
- **Sessions** live in a SQLite database per user,
  `~/Library/Application Support/dev.meet.meet/meet.db` (override with `MEET_DATA_DIR`),
  keyed by repository path: one row per `meet` launch with the summary written when it
  ended, every transcript line as it was heard or typed, the chunks with their summaries
  and the facts looked up for them, and the action items with their branch, worktree,
  Claude session id, pull request and log. Run `meet` again in the same repo and
  the items from earlier sessions are still on the board; `S` opens any earlier session.
  `meet list` prints the items, `meet sessions` the sessions.
- **Agent runs** keep their prompt, a readable log, the raw stream, the PR text and (when
  configured) the on-done prompt and its hook settings under `…/dev.meet.meet/runs/<item-id>/`.
- **Live files** for the question session go under `…/dev.meet.meet/sessions/<session-id>/`:
  `transcript.md` gets one `[mm:ss] source: text` line the moment it is heard (typed notes
  included), `summary.md` is rewritten whenever the recording state, the summaries, the facts or
  the board change, and `ask-system.md` is the system prompt the session was given. Anything
  can read them — `tail -f` works.

There are no projects or workspaces to set up: the repository you run `meet` in is the
project.

### Flags

```
meet [DIR]                       the checkout to work in (default: .)
  --no-mic | --no-system | --aec | --fast | --no-hooks | --config F | --out-dir D
                                 passed through to the engine
  --lookup-model MODEL           the live lookup that fills the Related pane (default: sonnet)
  --lookup-effort LEVEL          its --effort (default: low)
  --lookup-budget USD            cap per lookup call (default: 0.50)
  --no-lookup                    no live lookups; the action items are still written at the end
  --suggest-model MODEL          the action-item writer that runs when the recording stops (default: sonnet)
  --suggest-budget USD           cap for that one call (default: 2.00)
  --agent-model MODEL            the implementing agent (default: claude's default)
  --no-suggest                   transcribe only: no lookups, no action items
  --ask-model MODEL              the question session (a, meet ask) (default: sonnet)
  --ask-effort LEVEL             its --effort (default: claude's own)
  --no-resume                    leave agents that were running when meet last quit stopped
  --on-done TEXT | --on-done-file FILE
                                 one more instruction for every agent as it finishes (config: agent.onDone)
  --quiet-secs 8 --chunk-min-words 40 --chunk-max-words 180 --chunk-max-secs 90
                                 when a chunk closes
  --replay FILE --replay-speed X play a saved transcript.json instead of recording
  --recorder PATH                the meet-rec binary ($MEET_RECORDER)
  --claude-bin, --gh-bin, --remote
meet record …                    run the engine directly (all its flags; see below)
meet init …                      write a meet.json (see Config)
meet list [DIR]                  this repo's action items and pull requests
meet sessions [DIR]              this repo's sessions (with their ids)
meet ask [DIR] [--meeting ID] [--model M] [--effort E]
                                 a Claude Code session to ask about the meeting being recorded
                                 here (else the newest one) — run it in another terminal
```

Try it without a microphone:

```sh
meet --replay crates/meet/testdata/demo-transcript.json --replay-speed 8 .
```

### Asking questions while it records

Press **`a`** while the meeting goes on (or after it ended) and `meet` opens an interactive
Claude Code session in this repository whose job is to answer questions about the meeting:
what was said about X, what was decided, when something came up, how it relates to the code.
It is a normal `claude` run — you type questions, it answers — given a system prompt that
names the live files above and one rule that matters: **re-read `transcript.md` from the top
before every answer**, because it has grown since the last look. It may read the repository
(Glob, Grep, Read) but is told not to edit, build, or commit. Its first turn reads both files
and reports how long the meeting has run and what it has been about. The same session is
`meet ask`, typed into any terminal: it picks the meeting being recorded in this repo (else
the newest one; `--meeting <id>` from `meet sessions` picks another, and a meeting recorded
before these files existed gets them written from the database first) and replaces itself with
`claude --append-system-prompt-file … --add-dir … --name "meet · questions"`.

Where `a` puts the session depends on where `meet` is running:

- **inside a nebula agent session** (`NEBULA_AGENT_ID` is set): `nebula spawn` starts a Claude
  session beside it, in the same worktree, with the whole context as its first prompt — nebula
  takes no system prompt from outside. It shows up in nebula's session list on its own; pick
  it there. nebula only accepts spawn requests from inside an agent session, so this path does
  not fire from a plain nebula terminal; there, run `meet ask` in a second terminal session of
  the same worktree — that is a Claude Code session inside nebula, beside `meet`.
- **inside tmux** (`$TMUX` is set): a pane opens to the right of `meet` running `meet ask`.
- **anywhere else**: the footer (and the log) show the exact `meet ask …` command to run in
  another terminal.

One question session per launch; `a` again says where it is.

### Sessions, stopping and resuming

Each launch of `meet` is a **session**. `S` lists every session held in this repo — when,
how long, how many lines were said, how many action items came of it and what happened to
them, and its summary as a one-line description. Enter on one puts that session on
screen: its transcript and summaries on the left, and on the right only the action items
it produced, still live — an agent started from last week's session and running now
updates there as it works; `Tab` shows the facts that were looked up during it. `←` / `→`
step to an older / newer session without the list; `Esc` returns to the live meeting. The
live meeting's action items still land on the live board while you read.

**Stopping.** `X` on a running item stops its agent: a SIGTERM to the agent's process
group (so the tools it was running go with it), then a SIGKILL if it is still there after
4 s. The item shows `■ stopped`; its worktree, commits and Claude session are untouched, and
`Enter` resumes it.

**Resuming.** Every agent keeps its Claude Code session on disk, and `meet` records the
session id from the agent's stream. Enter on a stopped, failed or interrupted item does
whatever is left:

- the agent was still working → `claude --resume <session>` in the same worktree, with a
  prompt saying how the run ended, that its work is still there (`git status`, `git log`),
  and to carry on and finish — the original task follows for reference. If Claude no longer
  has that session, a new conversation is started over the same worktree with the same
  prompt;
- the agent had finished and only meet's tail (commit → push → `gh pr create`) failed →
  just the tail again;
- the worktree was deleted by hand → a fresh run.

**Quitting with agents running** stops them (SIGTERM, so Claude flushes the session) and
leaves their rows `running`. The next `meet` in the same repo resumes each of them before
anything else and says so; `meet list` marks them `running*` in between. `--no-resume`
leaves them `stopped` instead (Enter still resumes any of them by hand). Agents the user
stopped with `X` are never restarted on their own.

### The on-done step

One more instruction can reach every agent as it is about to finish — post a summary to
the GitHub issue the request came from, append what changed to a changelog, write a note
for the standup. It is configured in `meet.json` (or passed with `--on-done` /
`--on-done-file`):

```json
"agent": {
  "onDone": [
    "Before you finish: find the GitHub issue for \"{{title}}\" with `gh issue list --search`;",
    "if there is one, comment a three-line summary of what you changed on branch {{branch}} with `gh issue comment`.",
    "If there is none, append the same summary to {{repo}}/CHANGES.md instead."
  ]
}
```

`{{title}}`, `{{prompt}}`, `{{why}}`, `{{branch}}`, `{{worktree}}`, `{{repo}}`, `{{repo_name}}`,
`{{base_branch}}`, `{{run_dir}}` and `{{item_id}}` are filled in per run. The agent's
environment carries the same values as `MEET_ITEM_ID`, `MEET_ITEM_TITLE`, `MEET_BRANCH`,
`MEET_WORKTREE`, `MEET_REPO`, `MEET_BASE_BRANCH` and `MEET_RUN_DIR`.

How it works: the run writes the filled-in prompt to `<run dir>/on-done.md` and a settings
file with one Claude Code **`Stop` hook** — `meet hook stop` — which the agent loads with
`claude --settings` (nothing is written into the worktree). When the agent tries to stop,
the hook answers `{"decision": "block", "reason": <prompt>}` and Claude carries on with the
prompt as its next instruction; a marker file makes that happen exactly once per run,
resumes included, and the hook is inert without `MEET_RUN_DIR` in the environment. In the
agent's log this shows up as `── on-done prompt delivered`, and in Claude's own stream as
"Stop hook feedback" (Claude Code also emits a `stop-hook-error` notification for any
blocking Stop hook — that is its name for it, not a failure). The step runs *before* meet
commits, pushes and opens the pull request, so the PR URL is not available to it; what the
agent writes during the step is committed with the rest.

### How Claude is called

Two calls while you talk and after, one agent per item:

- **The lookup**, once per chunk while recording: `claude -p --output-format json
  --json-schema … --effort low` (`--lookup-model`, `sonnet` by default) with read-only
  tools (`Read`, `Glob`, `Grep`), a small spending cap, the summaries so far and the facts
  already shown so it never repeats one. It answers with the chunk's one-line summary and
  zero to four facts, each with the file that shows it. Calls run one at a time, in order.
- **The suggester**, once when the recording stops (or the engine dies): the same shape
  without `--effort`, given the whole transcript, the chunk summaries, the facts and the
  board, so it never proposes what is already there. It answers with the meeting's summary
  and zero to eight action items. `s` after the end runs it again.

The question session (`a`, `meet ask`) is the one interactive call: `claude
--append-system-prompt-file <session dir>/ask-system.md --add-dir <session dir> --name "meet ·
questions"` (`--ask-model`, `sonnet` by default) in the repository, with a first prompt that
reads the live files. Through nebula the same text goes in as the starting prompt instead.

The agent that implements an item runs `claude -p --output-format stream-json
--dangerously-skip-permissions` inside the worktree; `meet` itself does the deterministic
tail (commit, push, `gh pr create`) so a run always ends in a pull request or a clear
error. Both drop `CLAUDECODE` from the environment, so `meet` also works when launched from
inside a Claude Code session.

## The engine: `meet-rec`

`meet-rec` is the recorder the TUI drives (`meet record …` is the same thing). It captures
the microphone and/or system audio, transcribes each source on-device with Apple's
`SpeechAnalyzer` while recording, merges everything into one audio file and one transcript,
and runs your shell hooks. With `--json` it streams events (segments, status, lifecycle)
as newline-delimited JSON on stdout, which is how the TUI reads it.

```sh
meet record                 # mic + system audio, plain terminal recorder
meet record --no-mic        # system audio only
meet record --no-system     # microphone only
meet record --keep-tracks   # also keep mic.m4a / system.m4a next to audio.m4a
meet record --aec           # echo cancellation: built-in mic + speakers, no headphones
meet record --duration 5    # auto-stop after 5 s (handy for testing)
meet record --no-hooks      # record/transcribe but skip onDone hooks
meet record --json          # NDJSON events on stdout (what the TUI uses)
meet record --show-config   # print the effective config (file + flags) and exit
```

While recording directly, the terminal shows a status line and prints each finalized
sentence labeled by the source it came from:

```
● REC 00:12:34 │ mic 12 · system 8 │ 84 MB │ [space] pause  [q] stop
```

Keys: `space` pause/resume, `q` / `enter` / `ctrl-c` stop. `SIGTERM` and `SIGHUP` also stop
cleanly: audio files are finalized, the transcript is written and hooks run.

### Per-project config

```sh
cd ~/Work/acme-app
meet init          # writes ./meet.json (meetings go to ./meetings)
$EDITOR meet.json  # tweak summary.prompt, outputDir, …
meet               # the nearest meet.json up the tree is used
```

With no project config the engine falls back to `~/.config/meet/config.json`
(`meet init --global`, storing in `~/Meetings`).

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

The engine reads one JSON config file (the TUI passes `--config` and `--out-dir` through). The first of these that exists wins:

1. `--config <path>`
2. `$MEET_CONFIG`
3. `meet.json` in the current directory, else the nearest parent directory
4. `~/.config/meet/config.json` (global fallback; the pre-rename
   `~/.config/meeting-tracker/config.json` is still honoured after it)

CLI flags always override the file. `meet record --show-config` prints the effective
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
  },
  "agent": {
    "model": "default",
    "onDone": "Comment a short summary of what you changed on the GitHub issue for \"{{title}}\" with gh.",
    "onDoneFile": "agent-on-done.md"
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
| `agent.model` | `--agent-model` | the TUI's implementing agent (`claude --model …`). `"default"` or unset: Claude's own. |
| `agent.onDone` / `agent.onDoneFile` | `--on-done` / `--on-done-file` | the [on-done step](#the-on-done-step): one more instruction for each agent as it finishes. A string, an array of lines, or a file. Empty: no such step. |

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
crates/meet/src/        the `meet` TUI (Rust)
  main.rs              CLI flags; `record` / `init` pass through to the engine; `list`, `sessions`, `hook stop`
  event_loop.rs        keys, recorder / suggester / runner events, the one-second tick
  app.rs               TUI state and its transitions
  ui.rs                header, transcript, summaries, action items, prompt pane, overlays
  recorder.rs          `meet-rec record --json` as a child process; transcript replay
  chunker.rs           when a stretch of transcript becomes a chunk to summarize
  suggest.rs           the summarizer / action-item writer (prompt-doctor rules) via `claude -p`
  runner.rs            worktree → agent → commit → push → `gh pr create`; the resume plan
  claude.rs            headless Claude: structured one-shots, streaming agents (resume, stop)
  hook.rs              the on-done step: the Stop hook settings and `meet hook stop`
  config.rs            the `agent` block of meet.json, with the engine's lookup order
  stream_json.rs       `--output-format stream-json` parsing
  git.rs               git / gh, shelled out; nebula's worktree layout
  branch_name.rs       action-item title → branch slug
  pr_body.rs           the PR body template the agent fills, and the fallback
  store.rs             SQLite: sessions with their transcript lines, chunks, action items
  paths.rs             the per-user data directory
  when.rs              local date/time and duration formatting
  wrap.rs, theme.rs
crates/meet/testdata/  demo-transcript.json for `meet --replay`; e2e/ drives the TUI in tmux
                       against a fake claude and gh (`make e2e`)
Sources/meet/           the `meet-rec` engine (Swift)
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
  Events.swift         `--json` event stream for the TUI
hooks/                 example hook scripts
```
