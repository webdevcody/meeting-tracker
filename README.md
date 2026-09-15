# meet

**Talk to your repo.** `meet` is a terminal UI you run inside a git checkout and talk into
— alone at your desk, or on a call. It records the **microphone** and/or **all system
audio** (Zoom, Meet, Teams, anything playing) and transcribes **on-device** with Apple's
`SpeechAnalyzer`; nothing calls Claude while it records.
Press **`a`** and a **Claude Code session opens in the right pane, inside `meet`**, that answers
questions over the transcript as it grows — "what did they decide about the deploy?", "which
files came up?" — while the transcript keeps scrolling beside it. Nothing records until you
press **`r`**; press **`x`** to stop the recording: the whole transcript goes to Claude
once, which writes the meeting's **summary** and its **write-up** — key points, decisions,
open questions.

Every session is kept — every transcript line, the summary and the write-up —
and sits in the **session bar** under the header, newest at the left: `Tab` onto it, `←`/`→`
(or `h`/`l`) walk it, and `a` on any past session opens a Claude Code session to ask about
that meeting. Every recording is its own session — `r` after one ended starts the next; a
session closed again without a word said is not kept.

While recording — just the transcript; no Claude call is made until you stop:

```
 meet my-app (main)                                         ● REC 00:12:34  mic 41 · system 8 
 sessions  ● live │ 09-04 10:00 │ 09-02 15:20 │ 08-29 09:10
╭ Transcript ─────────────────────────╮╭ Meeting summary ────────────────────────────────────╮
│00:06 mic    the list command just   ││the meeting's summary is written when the recording  │
│             prints plain text, I    ││stops (x): the engine runs its onDone hooks — the    │
│             want a --json flag …    ││bundled one asks Claude for a Markdown summary of the│
│00:24 mic    adding a todo with      ││transcript — and meet's own call reads the whole     │
│             spaces is broken …      ││transcript once for the summary and this write-up.   │
│00:41 system one JSON object per     ││Their progress shows here, then the write-up.        │
│             line, or an array?      ││                                                     │
│01:02 mic    one per line, so it     ││                                                     │
│             pipes into jq           ││                                                     │
│01:15 mic    and fix add first,      ││                                                     │
│             that one bites daily    ││                                                     │
│                                     ││                                                     │
╰─────────────────────────────────────╯╰────────────────────────────────── Summary │ Claude ─╯
 x stop recording  D discard  a ask Claude  Tab pane  ↑/↓ scroll  i note  space pause  M/N mute mic/system  , settings  ? help  q quit
```

After `x` — the Summary pane shows what is working on the meeting that just ended (the
engine's `onDone` hooks — the bundled one has Claude write a summary file — and `meet`'s
own Claude call), then the write-up as soon as it lands:

```
 meet my-app (main)                                 ✎ SUMMARIZING 00:14:02  mic 47 · system 9 
 sessions  ■ ended │ 09-04 10:00 │ 09-02 15:20 │ 08-29 09:10
╭ Transcript ─────────────────────────╮╭ Meeting summary · hook running · writing… ──────────╮
│00:06 mic    the list command just   ││✎ meet's summary: Claude is reading the whole        │
│             prints plain text, I    ││transcript…                                          │
│             want a --json flag …    ││⚙ hook 1/1 summarize-transcript.sh: running… 23 s    │
│00:24 mic    adding a todo with      ││    summarize-transcript: summarizing …/transcript.md│
│             spaces is broken …      ││                                                     │
│00:41 system one JSON object per     ││the write-up lands here the moment Claude answers…   │
│             line, or an array?      ││                                                     │
│01:02 mic    one per line, so it     ││                                                     │
│             pipes into jq           ││                                                     │
│01:15 mic    and fix add first,      ││                                                     │
│             that one bites daily    ││                                                     │
│                                     ││                                                     │
╰─────────────────────────────────────╯╰────────────────────────────────── Summary │ Claude ─╯
 hook 1/1 summarize-transcript.sh is running — m shows its output        ⚙ hook summarize-transcript.sh 23s  ✎ writing the summary
```

```
 meet my-app (main)                                       ■ ended 00:14:02  mic 47 · system 9 
 sessions  ■ ended │ 09-04 10:00 │ 09-02 15:20 │ 08-29 09:10
╭ Transcript ─────────────────────────╮╭ Meeting summary ────────────────────────────────────╮
│00:06 mic    the list command just   ││✓ meet's summary: written                            │
│             prints plain text, I    ││✓ hook 1/1 summarize-transcript.sh: done in 41 s     │
│             want a --json flag …    ││    wrote …/summaries/2026-09-11_json-flag.md        │
│00:24 mic    adding a todo with      ││                                                     │
│             spaces is broken …      ││Summary                                              │
│00:41 system one JSON object per     ││A short working session on todo.sh: a --json flag    │
│             line, or an array?      ││for list, and a bug in add that drops words.         │
│01:02 mic    one per line, so it     ││                                                     │
│             pipes into jq           ││Decisions                                            │
│01:15 mic    and fix add first,      ││• output flags go on the subcommand                  │
│             that one bites daily    ││                                                     │
│                                     ││── 2026-09-11_json-flag.md · written by summarize-t… │
╰─────────────────────────────────────╯╰────────────────────────────────── Summary │ Claude ─╯
 summary written                                                              in 62k · out 4.1k · $0.38
```

Two binaries, one tool: `meet` (Rust, the TUI; `crates/meet`) and `meet-rec` (Swift, the
recording + transcription engine; `Sources/meet`). Pure Swift audio — no BlackHole, no
Loopback, no cloud for the audio. Claude is only ever called through the `claude` CLI you
already have, started through your login shell the way your terminal starts it (see
[How Claude is called](#how-claude-is-called)). Requires **macOS 26** (Tahoe) on Apple Silicon, Xcode 26, Rust, and `claude`
on your `PATH`.

## Install

```sh
make install          # builds both binaries, symlinks ~/.local/bin/meet and ~/.local/bin/meet-rec
meet --help
```

Working from the checkout, `make dev` builds the latest code and launches it in an isolated
instance: its own data directory under `~/.meet-dev/`, seeded from your real sessions the first
time, so nothing you try lands in the real database. `make cycle` runs `make install` first, so
the `meet` on your `PATH` is the new build too. A `meet` that is already running keeps the code
it started with: quit it (`q`) and start it again. `make help` lists the rest (`make e2e`,
`make test`, `make dev-reset`).

## Use it

```sh
cd ~/code/my-app
meet                  # mic + system audio, in this repo
meet --no-system      # microphone only (you, thinking out loud)
meet --no-mic         # system audio only (a call you are listening to)
```

Then talk. The transcript scrolls on the left, and nothing calls Claude while you record:
the right pane is the **Meeting summary**, which says what happens when you stop. `Tab`
moves the keys on to the Claude Code session (once `a` opened one) and the session bar.

The bottom right of the footer keeps a running total of what the summary writer spends —
`in 45k · out 3.4k · $0.31`: the tokens its calls read (fresh and from the prompt cache)
and wrote, and the dollars `claude` reported. The question session (`a`) is interactive
and is not counted.

Press **`x`** when you are done talking (it asks first). The transcript and audio are
saved, and two things start at once, both visible: the header switches to `✎ SUMMARIZING`
and the right pane becomes the **Meeting
summary** pane, which lists what is at work and how far along it is.

- The engine runs its **`onDone` hooks** (`meet.json` → `hooks.onDone`), one after another.
  The bundled `hooks/summarize-transcript.sh` runs Claude on the transcript and writes a
  Markdown summary file; a hook of your own — a repo-specific skill, say — runs the same
  way. Each hook shows as it starts (`⚙ hook 1/1 summarize-transcript.sh: running… 23 s`,
  with the last lines it printed under it), the footer counts its seconds, and a flash says
  when it ended and how (`hook summarize-transcript.sh wrote …/2026-09-11_json-flag.md in
  41 s`; a non-zero exit is a warning). When a hook's last lines name a Markdown or text
  file it wrote, that file is shown at the bottom of the pane, so a summary a hook writes
  is readable in `meet` too.
- `meet`'s own Claude call reads the whole transcript once and answers with the meeting's
  **summary** (a few sentences — the session's one-line description) and its **write-up**
  (Markdown: summary, key points, decisions, open questions). The write-up lands in the pane
  the moment it is back. It is meet's own summary, independent of whatever the hooks write,
  and it is kept with the session: `m` on a past session reads it back.

Press **`D`** instead to throw the recording away — a false start, a call that turned out
to be nothing. It asks first; then the recording stops and nothing is kept: the engine
deletes the meeting directory (audio and transcript) and runs no `onDone` hook, `meet`
writes no summary, drops the session from the session bar along with its
live files, and quits. `q` while recording offers the same under `d`.

Press **`r`** for the next meeting: the ended session slides along the session bar and a
new one opens — the transcript, the clock and the summary pane start over,
while the Claude Code sessions stay. A summary call or a
hook still working on the ended meeting finishes in the background and lands on it.

Quitting while either is still running asks first — the engine, and any hook it is running,
goes down with `meet`.

### Keys

| key | |
|---|---|
| `r` | start recording — nothing records until you do; after a recording ended, start the next one as a new session on the bar |
| `x` | stop the recording and stay: the transcript and audio are saved, the engine's hooks run, and the summary and the write-up are written from everything that was said — the Summary pane shows all of it as it happens |
| `D` | discard the recording and quit (asks first): it stops, the engine deletes its audio and transcript and runs no hook, nothing is summarized, and the session is dropped as if it never happened |
| `m` | the Meeting summary pane: while the meeting is being summarized, the hooks and meet's summary call at work; then the write-up, and the file a hook wrote. On a past session, its write-up |
| `Tab` | cycle the focus: the meeting summary / the Claude Code session once one is open / the session bar (`Shift+Tab` goes backwards) |
| `j` / `k`, `↓` / `↑` | scroll the Summary pane |
| `a` | ask Claude Code about the session on screen — the live meeting, or the past session the bar is on: a session starts in the right pane (an embedded terminal) and takes the keys; `Ctrl+q` hands them back to `meet`, `a` again returns them. Each session keeps its own |
| `i` | type a note into the transcript (no mic needed) |
| `s` | once the recording has ended, write the summary again |
| `space` | pause / resume the recording (paused time is excluded from the files) |
| `M` / `N` | mute / unmute the microphone / the system audio: while muted, that source is recorded and transcribed as silence (the other keeps going, the files stay in sync) and the header marks it `⊘ mic`; clicking `mic` / `system` in the header does the same |
| `PgUp` / `PgDn`, `J` / `K` | scroll the transcript; `G` follows the newest line again |
| `[` / `]` | scroll the right pane (the summary) |
| `←` / `→` | walk the session bar: `→` an older session, `←` a newer one (the live meeting is at the left end); `Esc` returns to the live meeting |
| `h` / `l` | the same while the bar has the keys (`Tab` onto it); `Enter` or `Esc` hand them back to the panes |
| `S` | the session list: every meeting held in this repo, newest first, with details |
| `,` | [settings](#settings): which Claude model and effort the summary and the question session run with — a change is saved and live at once |
| `?` | keys |
| `q`, `Ctrl+C` | quit. While recording it asks: `y` stops the recording, saves and quits *without* writing the summary; `x` stops the recording and stays instead; `d` discards the recording (as `D`) |

### Mouse

What the keys reach, the mouse reaches too, the way it does in nebula:

- **Click** a pane to give it the keys — the Claude Code pane hands them to Claude, as `a`
  does. A tab on the right pane's border (`Summary │ Claude`) shows that
  pane; a session on the session bar shows that session; `mic` / `system` in the header
  mutes that source. In an
  overlay, a row of the session list opens it and a row of the settings picks it (a second
  click cycles it); a click outside the overlay closes it, as `Esc` would.
- **Scroll** with the wheel over the transcript or the right pane; over the session list
  or the settings it steps the selection.
- **Drag** over the transcript to select its text. The highlight follows the pointer, the
  transcript scrolls along past its top or bottom edge, and the text is copied to the
  clipboard when the button comes up: as it was said, without the wrap's line breaks or the
  indentation under the clock. A **double-click** copies the word under the pointer. The
  highlight stays until the next click. Over ssh, or with no clipboard tool here (`pbcopy`,
  `wl-copy`, `xclip`, `xsel`), the copy goes through the terminal instead (OSC 52), which
  some terminals ignore.
- **Drag** the border between the transcript and the right pane to resize them. It carries
  a short thick grip in its middle, and the pointer turns into resize arrows over it in
  terminals that support that. The share is kept in `layout.json` in
  the data directory, so the next launch opens the same way (delete the file for the
  defaults). Hold `⇧` (`⌥` in some terminals) to select through your terminal instead,
  anywhere on the screen.

### What is kept where

- **Transcript and audio** go where the engine puts them: `meet.json` → `outputDir`, else
  `~/Meetings/<date>_<time>/` (`audio.m4a`, `transcript.md`, `transcript.json`,
  `meta.json`). The engine's `onDone` hooks (the bundled summary hook) still run when the
  meeting ends, and write wherever they write (the bundled one: `summary.dir`, else
  `<outputDir>/summaries/`); `meet` shows their progress and, when they name the file
  they wrote, the file.
- **Sessions** live in a SQLite database per user,
  `~/Library/Application Support/dev.meet.meet/meet.db` (override with `MEET_DATA_DIR`),
  keyed by repository path: one row per `meet` launch with the summary and the write-up
  written when it ended, and every transcript line as it was heard or typed. Run `meet`
  again in the same repo and every earlier
  session is a tab in the session bar (`S` lists them with details). A launch closed again without a
  word said leaves no session behind. `meet sessions` lists the
  sessions.
- **Live files** for the question session go under `…/dev.meet.meet/sessions/<session-id>/`:
  `transcript.md` gets one `[mm:ss] source: text` line the moment it is heard (typed notes
  included), `summary.md` is rewritten whenever the recording state or the summary
  changes, and `ask-system.md` is the system prompt the session was given. Anything
  can read them — `tail -f` works.

There are no projects or workspaces to set up: the repository you run `meet` in is the
project.

### Flags

```
meet [DIR]                       the checkout to work in (default: .)
  --no-mic | --no-system | --aec | --fast | --no-hooks | --config F | --out-dir D
                                 passed through to the engine
  --suggest-model M --suggest-effort L
                                 the summary writer that runs when the recording stops
                                 (default: the settings, else sonnet)
  --suggest-budget USD           cap for that one call (default: 2.00)
  --no-suggest                   transcribe only: no summary
  --ask-model M --ask-effort L   the question session (a, meet ask) (default: the settings, else sonnet)
  --replay FILE --replay-speed X play a saved transcript.json instead of recording
  --recorder PATH                the meet-rec binary ($MEET_RECORDER)
  --claude-bin
meet record …                    run the engine directly (all its flags; see below)
meet init …                      write a meet.json (see Config)
meet sessions [DIR]              this repo's sessions (with their ids)
meet ask [DIR] [--meeting ID] [--model M] [--effort E]
                                 the question session in this terminal instead of meet's pane:
                                 the meeting being recorded here, else the newest one
```

Try it without a microphone:

```sh
meet --replay crates/meet/testdata/demo-transcript.json --replay-speed 8 .
```

### Settings

`,` opens the settings: which Claude model and `--effort` each of the two features runs
with — the summary writer and the question session. It is laid out like nebula's: a row per value, `↑`/`↓` to move, `←`/`→` (or
`Enter`) to step through the choices, `1`–`2` to jump to a feature, `R` to put everything
back to its default, `Esc` to close. The choices are claude's aliases (`haiku`, `sonnet`,
`opus`, `fable`) and the current model ids, and the effort levels `low` to `max`;
`default` passes no flag, so claude picks. A change is written to
`~/Library/Application Support/dev.meet.meet/settings.json` the moment it is made and is
live at once: the next summary, the next question session. Hand-edit the
file if you like; a model that is not in the list is kept and passed through as it is.

The defaults are what `meet` always ran with: the summary and the question session on
`sonnet`. A `--suggest-model`, `--ask-effort`, … flag overrides the
setting for one launch; the modal says so beside the row.

### Asking questions while it records

Press **`a`** while the meeting goes on (or after it ended) and Claude Code starts in the right
pane, on a terminal embedded in `meet` — the transcript keeps scrolling on the left. It is a
normal interactive `claude` run in this repository whose job is to answer questions about the
meeting: what was said about X, what was decided, when something came up, how it relates to
the code. Its system prompt names the live files above and carries one rule that matters:
**re-read `transcript.md` from the top before every answer**, because it has grown since the
last look. It may read the repository (Glob, Grep, Read) but is told not to edit, build, or
commit. Its first turn reads both files and reports how long the meeting has run and what it
has been about.

While the pane has the keys, every key goes to Claude Code — type a question, Enter sends
it, Esc interrupts it, Ctrl+C is its own. **`Ctrl+q`** (or `Ctrl+]`) hands the keys back to
`meet`; the session keeps running and painting, `Tab` cycles the focus through the
summary, the session and the session bar, and **`a`** gives it the keys again. If Claude Code
exits (`/exit`, Ctrl+D), the pane keeps its last screen and the next `a` starts a fresh
session. Quitting `meet` ends the session. Keys reach it in the conventional xterm
encoding — no kitty keyboard protocol — so Shift+Enter is not a newline there; type `\` and
Enter, as in any plain terminal. The pane is the right column, so give `meet` a wide
terminal.

**`a` on a past session** — walk the session bar to it — asks about that meeting instead: its
transcript and summary are written from the database first (the definitive record), the
session is told the meeting has ended, and it answers from what was said then. Each
session keeps its own Claude Code; the tab the bar is on decides which one the right pane
shows, and all of them end when `meet` quits.

The same session is `meet ask`, typed into any terminal: it picks the meeting being recorded
in this repo (else the newest one; `--meeting <id>` from `meet sessions` picks another, and a
meeting recorded before these files existed gets them written from the database first) and
replaces itself with `claude --append-system-prompt-file … --add-dir … --name "meet ·
questions"`. In nebula that is a second terminal session of the same worktree, beside `meet`
(nebula takes no session requests from outside its own agent sessions, which is why `a` runs
the terminal itself).

### Sessions

Each launch of `meet` is a **session**, and every session held in this repo is a tab in the
**session bar** under the header: the live meeting at the left end, then every earlier one,
newest first, each named by when it started. `→` steps to an older session and `←` to a
newer one; `Tab` puts the keys on the bar itself (its tab is bracketed), where `h` / `l` do
the same and `Enter` or `Esc` hand the keys back; `Esc` returns to the live meeting. When
there are more tabs than fit, the bar scrolls to keep the shown one in view and counts
what is hidden past each end. A past session on screen shows its transcript
on the left and its write-up on the right, and `a` opens a Claude Code session to ask
about it. `S` lists the sessions
with details — when, how long, how many lines were said, and the summary as a one-line
description — and Enter on one opens it. Every recording is its own session: the launch
opens one, `r` starts recording into it, and `r` after that recording ended closes it and opens the
next; close `meet` without a word said and the open session is dropped again (as is any a
crash left empty), so the bar holds only meetings that happened.

### How Claude is called

One headless call, after you talk — none while you record:

- **The summary writer**, once when the recording stops (or the engine dies): `claude -p
  --output-format json --json-schema …` with no tools, its model and effort from the
  [settings](#settings) (`sonnet`, claude's own effort by default), given the whole
  transcript. It answers with the meeting's summary and its write-up (`notes`: Markdown with
  summary, key points, decisions and open questions, what the Summary pane shows). `s` after
  the end runs it again.

The engine's `onDone` hooks are not Claude calls `meet` makes: the engine runs them
(`meet.json` → `hooks.onDone`) and reports each one starting and ending, and `meet` shows
that. The bundled hook is itself one more `claude -p` — see [Config](#config).

The question session (`a`, `meet ask`) is the one interactive call: `claude
--append-system-prompt-file <session dir>/ask-system.md --add-dir <session dir> --name "meet ·
questions"` (its model and effort from the settings, `sonnet` by default) in the repository, with a first prompt that
reads the live files. Under `a` it runs on a pseudo-terminal `meet` owns (portable-pty), its
output parsed by a vt100 screen that the right pane paints.

The headless calls drop `CLAUDECODE` from the environment, so `meet` also works when
launched from inside a Claude Code session.

Every one of these starts through your login shell, the way nebula starts its sessions:
`$SHELL -l -i -c 'claude …'`. `-l` and `-i` load `~/.zprofile` and `~/.zshrc` (or bash's
files), so `claude` sees your terminal's `PATH`; and the command word goes in bare, so an
alias or function named `claude` wins over the binary, exactly as at a prompt. A work setup
that picks an account, backend or subscription per directory applies here too, run from the
directory the call runs in: the repository. `--claude-bin`
(`$MEET_CLAUDE_BIN`) names another command, resolved the same way. The headless calls run
in a session of their own, off `meet`'s terminal. A stop, a quit, and closing the question
pane signal every process group under the shell, not just the shell's own: bash runs
`claude` as a job in a group of its own and ignores SIGTERM itself. `git` is run
directly, as nebula runs it.

## The engine: `meet-rec`

`meet-rec` is the recorder the TUI drives (`meet record …` is the same thing). It captures
the microphone and/or system audio, transcribes each source on-device with Apple's
`SpeechAnalyzer` while recording, merges everything into one audio file and one transcript,
and runs your shell hooks. With `--json` it streams events (segments, status, lifecycle,
and after `finished` a `hooks` event with how many `onDone` hooks follow, then a `hook`
pair — `phase` `start` / `end`, with the command, the exit `status` and the `secs` it took
— around each one) as newline-delimited JSON on stdout, which is how the TUI reads it;
what a hook prints goes to stderr.

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
● REC 00:12:34 │ mic 12 · system 8 │ 84 MB │ [space] pause  [m] mute mic  [n] mute system  [q] stop  [D] discard
```

Keys: `space` pause/resume, `m` / `n` mute/unmute the microphone / the system audio (a
muted source is recorded and transcribed as silence, so the tracks stay in sync; the
status line marks it `⊘ mic`; in `--json` mode each flip is a `muted` event), `q` /
`enter` / `ctrl-c` stop, `D` discard (the meeting
directory is deleted — no audio, no transcript — and no hook runs; in `--json` mode a
`discarded` event replaces `finished`). `SIGTERM` and `SIGHUP` also stop
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
| `summary.claudePath` | — | the `claude` command to run (default `claude`, resolved by your login shell). |
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
appended after the prompt. It starts `claude` through your login shell as `meet` does (via
perl's `setsid`, so the interactive shell stays off the terminal), so your rc files and any
`claude` alias or function apply. Needs `claude` there (or `summary.claudePath`); Claude's
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
  main.rs              CLI flags; `record` / `init` pass through to the engine; `sessions`, `ask`
  event_loop.rs        keys, recorder / suggester events, the one-second tick
  app.rs               TUI state and its transitions
  ui.rs                header, transcript, the right pane, overlays
  layout.rs            the seam between the columns (dragged with the mouse; layout.json) and where the last frame drew what, for the clicks
  selection.rs         selecting transcript text with the mouse, and the copy: pbcopy (or wl-copy, xclip, xsel), else OSC 52
  recorder.rs          `meet-rec record --json` as a child process; transcript replay
  suggest.rs           the summary writer via `claude -p`
  claude.rs            headless Claude: structured one-shots
  shell.rs             every `claude` launch goes through the login shell (rc files, aliases), as in nebula
  settings.rs          which Claude model and effort each feature runs with; the settings file (`,`)
  stream_json.rs       the tokens and cost claude reports, and excerpts
  git.rs               git, shelled out
  store.rs             SQLite: sessions with their transcript lines and summaries
  paths.rs             the per-user data directory
  when.rs              the recording clock, local date/time and duration formatting
  wrap.rs, theme.rs
crates/meet/testdata/  demo-transcript.json for `meet --replay`; e2e/ drives the TUI in tmux
                       against a fake claude (`make e2e`)
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
