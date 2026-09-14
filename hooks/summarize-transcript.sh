#!/bin/sh
# meet onDone hook: run Claude Code headless to write a Markdown summary of the
# transcript into a summaries directory, named with a descriptive title.
#
# meet runs this with cwd = the meeting directory, the transcript on stdin,
# and env vars MT_MEETING_DIR, MT_TRANSCRIPT_PATH, MT_STARTED_AT, MT_DURATION_SECS,
# MT_SEGMENT_COUNT (and more; see README).
#
# Everything below is configurable from the `summary` block of the config file, which
# meet exports to this script as environment variables:
#
#   MT_SUMMARY_DIR            summary.dir          where summaries go (default <outputDir>/summaries)
#   MT_SUMMARY_PROMPT         summary.prompt       the instruction sent to Claude
#   MT_SUMMARY_SYSTEM_PROMPT  summary.systemPrompt text for `claude --append-system-prompt`
#   MT_SUMMARY_MODEL          summary.model        `claude --model …`
#   MT_CLAUDE_BIN             summary.claudePath   claude command (default: `claude`)
#
# Claude starts the way it would at your terminal prompt: through your login, interactive
# shell ($SHELL -l -i -c), so ~/.zprofile and ~/.zshrc load and an alias or function named
# `claude` wins over the binary on PATH — as nebula and meet itself start it.
#
# The transcript itself and a short metadata block (date, duration, paths) are always
# appended after MT_SUMMARY_PROMPT, so the prompt only needs to say what to do with them.
set -u

TRANSCRIPT="${MT_TRANSCRIPT_PATH:?MT_TRANSCRIPT_PATH is not set — run this via meet}"
MEETING_DIR="${MT_MEETING_DIR:-$(dirname "$TRANSCRIPT")}"
SUMMARY_DIR="${MT_SUMMARY_DIR:-${MT_OUTPUT_DIR:-$HOME/Meetings}/summaries}"
CLAUDE_BIN="${MT_CLAUDE_BIN:-claude}"
LOG="$MEETING_DIR/summary.log"
DATE="$(printf '%s' "${MT_STARTED_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}" | cut -c1-10)"

DEFAULT_PROMPT='Summarize this meeting transcript.'
DEFAULT_SYSTEM_PROMPT="You are running headless as a meet hook; do not ask questions. Read the transcript in the prompt and write ONE Markdown file into the summary directory using the Write tool. File name: <meeting date>_<descriptive-kebab-case-title>.md (e.g. 2026-08-28_billing-migration-release-plan.md) — the title must describe what the meeting was actually about, 3-7 words, lowercase, hyphens only, no other punctuation. File contents: a '# ' heading with the descriptive title in Title Case, a line with the date and duration, then sections: '## Summary' (2-5 sentences), '## Key Points' (bullets), '## Decisions' (bullets, or 'None recorded'), '## Action Items' (bullets with owner if stated, or 'None recorded'). Speakers are labeled by audio source: Microphone (this Mac's microphone — the recording user and anyone else in the room) and System (audio this Mac played — remote call participants, videos); either may contain several people. Do not invent facts that are not in the transcript. When done, print only the absolute path of the file you wrote."

PROMPT="${MT_SUMMARY_PROMPT:-$DEFAULT_PROMPT}"
SYSTEM_PROMPT="${MT_SUMMARY_SYSTEM_PROMPT:-$DEFAULT_SYSTEM_PROMPT}"

# Run "$@" (a command name, then its arguments) in the user's login, interactive shell, so
# the shell resolves the name the way a typed command is resolved: rc files loaded, alias or
# function first. perl's setsid keeps that interactive shell off the terminal meet draws on
# (zsh -i with a terminal takes it over as the foreground). Without perl or $SHELL, or when
# setsid is refused, the command runs directly.
login_shell() {
  if [ -n "${SHELL:-}" ] && command -v perl >/dev/null 2>&1; then
    perl -MPOSIX -e '
      my ($sh, $cmd, @args) = @ARGV;
      if (defined POSIX::setsid()) {
        my $word = $cmd =~ m{\A[A-Za-z0-9_./-]+\z} ? $cmd : q{"$0"};
        exec $sh, "-l", "-i", "-c", "$word \"\$@\"", $cmd, @args;
      }
      exec $cmd, @args;
      die "exec $cmd: $!\n";' "$SHELL" "$@"
  else
    "$@"
  fi
}

if ! login_shell command -v "$CLAUDE_BIN" >/dev/null 2>&1 \
  && ! command -v "$CLAUDE_BIN" >/dev/null 2>&1; then
  echo "summarize-transcript: '$CLAUDE_BIN' not found in your login shell (set summary.claudePath in the config or add it to PATH)" >&2
  exit 127
fi
if [ "${MT_SEGMENT_COUNT:-1}" = "0" ]; then
  echo "summarize-transcript: transcript has no speech; skipping" >&2
  exit 0
fi

mkdir -p "$SUMMARY_DIR" || exit 1

# Allow launching from inside another Claude Code session (e.g. a hook test).
unset CLAUDECODE

# Optional extra claude flags.
set --
if [ -n "${MT_SUMMARY_MODEL:-}" ]; then
  set -- "$@" --model "$MT_SUMMARY_MODEL"
fi

echo "summarize-transcript: summarizing $TRANSCRIPT → $SUMMARY_DIR/${MT_SUMMARY_MODEL:+ (model $MT_SUMMARY_MODEL)}"

STATUS_FILE="$MEETING_DIR/.hook-status"
{
  printf '%s\n\n' "$PROMPT"
  printf 'Meeting date: %s\nDuration: %s seconds\nTranscript file: %s\nSummary directory: %s\n\n' \
    "$DATE" "${MT_DURATION_SECS:-unknown}" "$TRANSCRIPT" "$SUMMARY_DIR"
  printf -- '--- TRANSCRIPT ---\n'
  cat "$TRANSCRIPT"
} | {
  login_shell "$CLAUDE_BIN" -p \
    --output-format text \
    --allowedTools "Write" "Read" \
    --append-system-prompt "$SYSTEM_PROMPT" \
    "$@" \
    2>&1
  echo "$?" > "$STATUS_FILE"
} | tee "$LOG"

status=$(cat "$STATUS_FILE" 2>/dev/null || echo 1)
rm -f "$STATUS_FILE"

# The last non-empty line of Claude's output should be the summary path; verify it exists.
SUMMARY_PATH="$(grep -v '^[[:space:]]*$' "$LOG" | tail -n 1 | tr -d '`' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
if [ "$status" -eq 0 ] && [ -f "$SUMMARY_PATH" ]; then
  echo "summarize-transcript: wrote $SUMMARY_PATH"
  exit 0
fi
if [ "$status" -ne 0 ]; then
  echo "summarize-transcript: claude exited $status (see $LOG)" >&2
  exit "$status"
fi
NEWEST="$(ls -t "$SUMMARY_DIR"/"$DATE"_*.md 2>/dev/null | head -n 1)"
if [ -n "$NEWEST" ] && [ "$(find "$NEWEST" -newer "$TRANSCRIPT" 2>/dev/null)" ]; then
  echo "summarize-transcript: wrote $NEWEST"
  exit 0
fi
echo "summarize-transcript: no summary file was written (see $LOG)" >&2
exit 1
