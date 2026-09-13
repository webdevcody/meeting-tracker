#!/bin/bash
# Drives the meet TUI through the live lookup, stopping the recording with x, running /
# stopping / resuming agents, quit-and-auto-resume, session browsing, the live transcript
# files, the question session (a, meet ask), the settings modal and discarding a recording
# (D) with a fake claude and a fake gh (no money spent, no real PRs), on a private tmux server.
# `make e2e` runs it; needs tmux and a release build. Run with bash, not zsh.
set -u
# Never inside nebula: with NEBULA_AGENT_ID in the environment, `a` would `nebula spawn` a real
# Claude session instead of the tmux pane this script expects (the tmux server inherits our env).
unset NEBULA_AGENT_ID NEBULA_API_URL NEBULA_API_TOKEN
E2E=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$E2E/../../../.." && pwd)
W=${MEET_E2E_WORK:-$ROOT/target/e2e}
rm -rf "$W"; mkdir -p "$W"
export MEET_DATA_DIR=$W/data
export FAKE_CLAUDE_LOG=$W/claude.log
export FAKE_GH_COUNTER=$W/gh-counter
MEET=$ROOT/target/release/meet
chmod +x "$E2E/fake-claude" "$E2E/fake-gh"

repo=$W/todo
git init -q -b main "$repo"
( cd "$repo" && echo "# todo" > README.md && git add -A && git -c user.email=t@t -c user.name=t commit -q -m init )
git init -q --bare "$W/origin.git"
( cd "$repo" && git remote add origin "$W/origin.git" && git push -q -u origin main )

cat > "$W/run-meet.sh" <<EOF
#!/bin/bash
# A leading "--speed N" picks the replay speed (default 12); the rest goes to meet.
export MEET_DATA_DIR=$W/data FAKE_CLAUDE_LOG=$W/claude.log FAKE_GH_COUNTER=$W/gh-counter
speed=12
if [ "\${1:-}" = "--speed" ]; then speed=\$2; shift 2; fi
exec $MEET --replay $ROOT/crates/meet/testdata/demo-transcript.json --replay-speed \$speed \
  --claude-bin $E2E/fake-claude --gh-bin $E2E/fake-gh \
  --on-done 'Post a one-line summary about {{title}} on branch {{branch}}.' "\$@" $repo 2>>$W/meet.err
EOF
chmod +x "$W/run-meet.sh"

SOCK=meet-e2e
tmux -L $SOCK kill-server 2>/dev/null
pass=0; fail=0
ok()   { echo "  ✓ $1"; pass=$((pass+1)); }
bad()  { echo "  ✗ $1"; fail=$((fail+1)); }
# Every command targets meet's own pane by id, so a pane split off beside it (the question
# session) never takes the keys or the screen captures.
PANE=
cap()  { tmux -L $SOCK capture-pane -p -t "$PANE" 2>/dev/null; }
keys() { tmux -L $SOCK send-keys -t "$PANE" "$@"; }
other_pane() { tmux -L $SOCK list-panes -t meet -F '#{pane_id}' | grep -v "^$PANE\$" | head -1; }
# wait_for <regex> <timeout-secs> [label]
wait_for() {
  local pat=$1 t=$2 label=${3:-$1} i=0
  while [ $i -lt $((t*2)) ]; do
    if cap | grep -qE -- "$pat"; then ok "saw: $label"; return 0; fi
    sleep 0.5; i=$((i+1))
  done
  bad "timed out waiting for: $label"; echo "----- screen -----"; cap; echo "------------------"; return 1
}
# SUGGEST_DELAY=secs holds the fake suggester's answer back, so the "writing…" state shows.
# launch_meet leaves meet idle — nothing records until r; start_meet presses r once it is up.
launch_meet() {
  tmux -L $SOCK new-session -d -s meet -x 190 -y 50 -c "$repo" "env FAKE_SUGGEST_DELAY=${SUGGEST_DELAY:-} $W/run-meet.sh $*"
  PANE=$(tmux -L $SOCK display-message -p -t meet '#{pane_id}')
  sleep 0.5
}
start_meet() {
  launch_meet "$@"
  wait_for "○ idle · r to record" 10 "meet is up and idle" || exit 1
  keys r
}
alive() { tmux -L $SOCK has-session -t meet 2>/dev/null; }

echo "== 1. first launch: idle until r; then the lookup fills the Related pane, x stops the recording, the items land"
# Slower and with small chunks, so a fact shows while the replay is still going.
SUGGEST_DELAY=4 launch_meet --speed 3 --chunk-max-words 25
wait_for "○ idle · r to record" 10 "the header says meet is idle and how to start" || exit 1
cap | grep -q "press r to start recording" && ok "the transcript pane says how to start" || bad "no start hint in the transcript pane"
cap | tail -1 | grep -q "^ r record " && ok "the footer offers r" || bad "footer: $(cap | tail -1)"
cap | grep -qE "^ sessions +○ idle" && ok "the session bar's live tab is idle" || bad "session bar: $(cap | sed -n 2p)"
sleep 2
cap | grep -q "REPLAY" && bad "the replay started on its own" || ok "nothing recorded on its own"
keys x
sleep 0.4
cap | grep -q "nothing is recording — r starts the recording" && ok "x while idle says r starts it" || bad "flash after x while idle: $(cap | tail -1)"
keys r
wait_for "▶ REPLAY" 10 "r starts the replay" || exit 1
wait_for "README.md:1" 40 "a fact from the fake lookup in the Related pane" || exit 1
wait_for "in [0-9.]+k? · out [0-9.]+k? · [$][0-9]+\\.[0-9]+ *$" 10 "the footer's bottom right counts the lookup's tokens and cost" || exit 1
cap | grep -q "▶ REPLAY" && ok "the replay is still going" || bad "the replay already ended"
cap | grep -qE "Related · \[[0-9]+\] [0-9]+:[0-9]+–[0-9]+:[0-9]+" && ok "the Related pane names the summary it shows" || bad "the Related pane title names no summary: $(cap | grep -o 'Related[^│]*' | head -1)"
cap | grep -qE "⚠ Contradiction [0-9]+" && ok "a contradiction from the fake lookup" || bad "no contradiction shown"
cap | grep -qE "\? Question [0-9]+" && ok "a question from the fake lookup" || bad "no question shown"
cap | grep -q "▸\[" && ok "the shown summary is marked on the left" || bad "no summary marker on the left"
keys k
sleep 0.4
cap | grep -q "G to follow" && ok "k picks an earlier summary and the pane stops following" || bad "no follow tag after k"
keys G
sleep 0.4
cap | grep -q "G to follow" && bad "G did not resume following" || ok "G follows the newest summary again"
keys x
sleep 0.4
wait_for "Stop the recording\\?" 5 "the stop-recording confirm" || exit 1
keys y
# The Summary pane takes the right side while the meeting is summarized: the header says so,
# the pane and the footer show meet's call at work (the fake holds its answer back 4 s), then
# the write-up lands in the pane and the flash points at the board.
wait_for "Meeting summary · writing…" 10 "the Summary pane takes the right pane and says the summary is being written" || exit 1
cap | grep -q "✎ SUMMARIZING" && ok "the header says the meeting is being summarized" || bad "header: $(cap | sed -n 1p)"
cap | grep -q "Claude is reading the whole transcript" && ok "the pane says what meet's call is doing" || bad "no call status in the pane"
cap | grep -q "✎ writing the summary" && ok "the footer's right end shows the call too" || bad "footer: $(cap | tail -1)"
cap | grep -q "engine hooks" && bad "a replay has no engine, so no hook line is expected" || ok "no hook line without an engine"
wait_for "Key points" 40 "the write-up lands in the Summary pane" || exit 1
cap | grep -q "• Ship changes 1 and 2" && ok "its decisions read as bullets" || bad "no decisions bullet"
cap | grep -q "✓ meet's summary and action items: written" && ok "the pane says meet's call is done" || bad "no done line in the pane"
cap | grep -qE "^ meet .*■ ended" && ok "the header says the recording ended" || bad "header does not say ended: $(cap | sed -n 1p)"
cap | grep -q "summary written · 2 action items — Tab shows the board" && ok "the flash counts the items and points at the board" || bad "flash: $(cap | tail -1)"
keys m
sleep 0.3
cap | grep -q "Meeting summary" && ok "m keeps the summary pane" || bad "m lost the summary pane"
n_lookups=$(grep -cF '"facts":{"type":"array"' "$W/claude.log")
n_suggests=$(grep -cF '"items":{"type":"array"' "$W/claude.log")
[ "$n_suggests" -eq 1 ] && ok "exactly one action-item call, after the stop" || bad "expected 1 action-item call, saw $n_suggests"
[ "$n_lookups" -ge 1 ] && ok "$n_lookups lookup call(s) while recording" || bad "no lookup calls"
grep -qE -- "--effort low" "$W/claude.log" && ok "the lookup ran with --effort low" || bad "no --effort low in claude args"
cap | grep -qE "^ sessions +■ ended" && ok "the session bar under the header: this launch's tab, ended" || bad "session bar: $(cap | sed -n 2p)"
keys Tab
sleep 0.4
cap | grep -qE "^ sessions +\[■ ended\]" && ok "Tab puts the keys on the session bar: its tab is bracketed" || bad "Tab did not focus the session bar: $(cap | sed -n 2p)"
keys Tab
sleep 0.4
cap | grep -qE "Related · \[[0-9]+\]" && ok "Tab moves on to the Related pane on the newest summary" || bad "Tab did not show Related"
cap | grep -qE "^ sessions +\[" && bad "the bar kept the keys" || ok "and the bar let go of the keys"
keys Tab
sleep 0.4
cap | grep -q "Action items" && ok "Tab returns to the action items" || bad "Tab did not return to the items"

echo "== 2. run the selected item, stop it with X"
keys Enter
wait_for "● Read" 20 "the agent is working (activity)" || exit 1
wait_for "session +fake-session-" 10 "the session id is shown in the detail pane"
keys X
sleep 0.4
wait_for "Stop the agent" 5 "the stop confirm" || exit 1
keys y
wait_for "■ stopped" 15 "the item is stopped" || exit 1
cap | grep -qE "Enter resumes" && ok "the detail pane offers to resume" || bad "no resume hint"

echo "== 3. Enter resumes it with --resume; the fake finishes and a PR opens"
keys Enter
wait_for "PR #1" 30 "the pull request opened after the resume" || exit 1
if grep -qE "^ARGS: .*--resume fake-session-" "$W/claude.log"; then ok "claude was started with --resume <sid>"; else bad "no --resume in claude args"; fi
if grep -qE "^ARGS: .*--settings .*/settings.json" "$W/claude.log"; then ok "claude got --settings with the hook file"; else bad "no --settings"; fi
run_dir=$(ls -d "$W"/data/runs/* | head -1)
if [ -f "$run_dir/on-done.md" ] && grep -q "Fake change" "$run_dir/on-done.md"; then ok "on-done.md written with placeholders filled: $(cat "$run_dir/on-done.md")"; else bad "on-done.md missing"; fi
if grep -q '"Stop"' "$run_dir/settings.json" && grep -q "hook stop" "$run_dir/settings.json"; then ok "settings.json carries the Stop hook"; else bad "settings.json wrong"; fi
if grep -q "^MEET_RUN_DIR=" "$W/claude.log"; then ok "MEET_RUN_DIR exported to the agent"; else bad "MEET_RUN_DIR missing"; fi
if grep -q "resuming session fake-session-" "$run_dir/agent.log"; then ok "agent.log notes the resume"; else bad "agent.log lacks the resume note"; fi

# Move the selection onto an item whose detail pane says "Enter runs it".
select_runnable() {
  for i in 1 2 3 4 5 6; do cap | grep -q "Prompt · Enter runs it" && return 0; keys k; sleep 0.3; done
  for i in 1 2 3 4 5 6 7 8; do cap | grep -q "Prompt · Enter runs it" && return 0; keys j; sleep 0.3; done
  bad "no runnable item on screen"; cap; return 1
}

echo "== 4. run a second item, quit while it runs"
select_runnable || exit 1
keys Enter
wait_for "1 agent running" 20 "a second agent is running" || exit 1
keys q
sleep 0.4
wait_for "Quit\\?" 5 "the quit confirm" || exit 1
cap | grep -q "resumes" && ok "the quit confirm says the agent resumes next time" || bad "quit confirm copy"
keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
alive && { bad "meet did not exit"; tmux -L $SOCK kill-server; exit 1; } || ok "meet exited"
if MEET_DATA_DIR=$W/data $MEET list "$repo" | grep -q "running\*"; then ok "meet list shows the interrupted item as running*"; else bad "meet list lacks running*"; MEET_DATA_DIR=$W/data $MEET list "$repo"; fi
grep -q "still running — stopped" "$W/meet.err" && ok "exit message names the stopped agent" || bad "no exit message: $(tail -3 "$W/meet.err")"

echo "== 5. second launch: the interrupted agent resumes on its own and finishes"
start_meet
# The replay ends on its own and the Summary pane takes over: Tab round to the board.
wait_for "Key points" 40 "this replay's write-up in the Summary pane" || exit 1
keys Tab; sleep 0.3; keys Tab; sleep 0.3; keys Tab; sleep 0.4
wait_for "PR #2" 40 "the resumed agent's pull request" || exit 1
if ls "$W"/data/runs/*/agent.log | xargs grep -l "resuming session fake-session-" | wc -l | grep -qE "[2-9]"; then ok "the second run's agent.log notes its resume too"; else bad "no second resume note"; fi
n_resume=$(grep -cE "^ARGS: .*--resume" "$W/claude.log")
[ "$n_resume" -ge 2 ] && ok "two --resume launches so far" || bad "expected 2 --resume launches, saw $n_resume"

echo "== 6. sessions: the list, viewing the previous one, stepping back"
wait_for "Fake change" 40 "items exist" || exit 1
keys S
sleep 0.4
wait_for "Sessions · 2 in todo" 5 "the session list with both sessions" || exit 1
cap | grep -qE "● live" && ok "the live row" || bad "no live row"
cap | grep -qE "[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2} · .*lines · .*items" && ok "the previous session row with counts" || bad "previous session row"
keys j
sleep 0.2
keys Enter
wait_for "session 2 of 2" 5 "the header shows the viewed session" || exit 1
cap | grep -q "from this session" && ok "the board is narrowed to that session's items" || bad "board not narrowed"
cap | grep -qE "PR #1" && ok "the earlier session's done item is there with its PR" || bad "PR #1 missing in session view"
cap | grep -qE "^ sessions +. [a-z]+ +│ *[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}" && ok "the session bar: the live tab at the left, the earlier session's date to its right" || bad "session bar: $(cap | sed -n 2p)"
keys Left
sleep 0.4
if cap | grep -q "session 2 of 2"; then bad "← did not return to live"; else ok "← (newer) returned to the live meeting"; fi
keys Right
sleep 0.4
wait_for "session 2 of 2" 5 "→ (older) steps back to the previous session" || true
keys Escape
sleep 0.4
if cap | grep -q "session 2 of 2"; then bad "Esc did not return to live"; else ok "Esc returned to live"; fi
# The bar itself: Tab until it has the keys (the shown tab is bracketed), l steps to the
# older session, a asks Claude about that past session, and the session keeps its Claude.
for i in 1 2 3; do cap | grep -qE "^ sessions +\[" && break; keys Tab; sleep 0.4; done
cap | grep -qE "^ sessions +\[" && ok "Tab put the keys on the session bar" || bad "no bracketed tab: $(cap | sed -n 2p)"
keys l
wait_for "session 2 of 2" 5 "l on the bar steps to the older session" || exit 1
cap | grep -qE "\[[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}\]" && ok "its tab is the bracketed one now" || bad "bar after l: $(cap | sed -n 2p)"
keys a
wait_for "Claude Code · [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2} · has the keys" 10 "a on a past session: Claude Code about that session, with the keys" || exit 1
wait_for "fake claude · question session" 15 "the fake claude came up for the past session" || exit 1
past_dir=$(dirname "$(ls -t "$W"/data/sessions/*/ask-system.md | head -1)")
grep -qF -- "--append-system-prompt-file $past_dir/ask-system.md --add-dir $past_dir " "$W/claude.log" && ok "the question session was pointed at the past session's files" || bad "past-session args: $(grep -E '^ARGS: --append' "$W/claude.log" | tail -1) (dir $past_dir)"
grep -q "A meeting was recorded;" "$past_dir/ask-system.md" && ok "its ask-system.md says the meeting ended" || bad "ask-system.md in $past_dir"
grep -q "^state: ended" "$past_dir/summary.md" && ok "its summary.md was written from the database" || bad "summary.md in $past_dir: $(head -3 "$past_dir/summary.md")"
keys C-q
wait_for "keys back to meet" 5 "Ctrl+q hands the keys back" || exit 1
keys Escape
sleep 0.4
if cap | grep -q "session 2 of 2"; then bad "Esc did not return to live"; else ok "Esc returned to live"; fi
cap | grep -q "Claude Code · questions" && bad "the live meeting shows the past session's Claude" || ok "the live meeting has no Claude Code of its own"
keys Right
sleep 0.4
# Tab round the right panes until the past session's own Claude Code comes back.
for i in 1 2 3 4 5; do cap | grep -qE "Claude Code · [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}" && break; keys Tab; sleep 0.4; done
cap | grep -qE "Claude Code · [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2} · a gives it the keys" && ok "back on the past session, Tab reaches its own Claude Code again" || bad "screen after → Tab: $(cap | sed -n 3p)"
keys Escape
sleep 0.4

echo "== 7. --no-resume path and the sessions CLI"
keys q
sleep 0.3
keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
MEET_DATA_DIR=$W/data $MEET sessions "$repo" > "$W/sessions.txt" 2>&1
grep -qE "2 session\(s\)" "$W/sessions.txt" && ok "meet sessions lists 2 sessions" || { bad "meet sessions output"; cat "$W/sessions.txt"; }
grep -qE "[0-9]+ lines" "$W/sessions.txt" && ok "sessions carry line counts" || bad "no line counts"

# Leave an item running, then start with --no-resume: it must become 'stopped'.
start_meet
wait_for "Key points" 40 "this replay's write-up" || exit 1
keys Tab; sleep 0.3; keys Tab; sleep 0.3; keys Tab; sleep 0.4
wait_for "○ Fake change" 40 "a new suggested item from this replay" || exit 1
select_runnable || exit 1
keys Enter
wait_for "1 agent running" 20 "an agent is running" || exit 1
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
start_meet --no-resume
wait_for "Key points" 40 "this replay's write-up" || exit 1
keys Tab; sleep 0.3; keys Tab; sleep 0.3; keys Tab; sleep 0.4
wait_for "■ stopped" 20 "with --no-resume the interrupted item shows as stopped" || exit 1
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done

echo "== 8. the live transcript files, and a: Claude Code on the embedded terminal in the right pane"
# The live files: one per meeting, the transcript appended as the replay went.
n_dirs=$(ls -d "$W"/data/sessions/*/ 2>/dev/null | wc -l | tr -d ' ')
[ "$n_dirs" -ge 4 ] && ok "$n_dirs session directories with live files" || bad "expected a session dir per launch, saw $n_dirs"
live_t=$(ls -t "$W"/data/sessions/*/transcript.md | head -1)
grep -q "^# Transcript · meet in todo" "$live_t" && ok "transcript.md starts with its header" || bad "transcript.md header"
grep -qE "^\[[0-9]{2}:[0-9]{2}\] mic: " "$live_t" && ok "transcript.md carries [mm:ss] mic: lines" || bad "no transcript lines in $live_t"
live_s=$(dirname "$live_t")/summary.md
grep -q "^state: ended" "$live_s" && ok "summary.md says the recording ended" || bad "summary.md state: $(head -3 "$live_s")"
grep -q "## Action items on the board" "$live_s" && ok "summary.md lists the board" || bad "summary.md lacks the board"
grep -q "^### Write-up" "$live_s" && grep -q "^## Key points" "$live_s" && ok "summary.md carries the write-up under the meeting summary" || bad "summary.md lacks the write-up"
# a while recording: Claude Code (the fake) comes up on meet's embedded terminal in the right
# pane and gets the keys; typed lines reach it and its answers land on screen; Ctrl+q hands
# the keys back; Tab cycles the pane; q quits and takes the child down.
start_meet --speed 3 --chunk-max-words 25
wait_for "▶ REPLAY" 20 "the replay is going" || exit 1
keys a
wait_for "Claude Code · questions · has the keys" 10 "the right pane is Claude Code with the keys" || exit 1
wait_for "fake claude · question session · meet · questions" 15 "the fake claude painted its banner in the pane" || exit 1
n_panes=$(tmux -L $SOCK list-panes -t meet | wc -l | tr -d ' ')
[ "$n_panes" -eq 1 ] && ok "still one tmux pane: the terminal is inside meet" || bad "expected 1 pane, saw $n_panes"
keys -l "what did they decide"
keys Enter
wait_for "you asked: what did they decide" 10 "typed keys reached the fake claude and its answer is on screen" || exit 1
keys q
sleep 0.4
cap | grep -q "Quit?" && bad "q reached meet while Claude had the keys" || ok "q went to Claude, not to meet"
if grep -qE "^ARGS: --append-system-prompt-file .*/ask-system.md --add-dir .*/sessions/.* --name meet · questions --model sonnet" "$W/claude.log"; then ok "claude got the system prompt file, the session dir and the name"; else bad "question session args: $(grep -E '^ARGS: --append' "$W/claude.log" | tail -1)"; fi
grep -q "^INTERACTIVE: Read .*transcript.md and .*summary.md now" "$W/claude.log" && ok "the first prompt asks it to read both files" || bad "no first prompt logged"
sysf=$(ls -t "$W"/data/sessions/*/ask-system.md | head -1)
grep -q "being recorded right now" "$sysf" && ok "ask-system.md says the meeting is live" || bad "ask-system.md: $(head -c 200 "$sysf")"
grep -q "Before answering ANY question, Read" "$sysf" && ok "ask-system.md carries the re-read rule" || bad "no re-read rule"
keys C-q
wait_for "keys back to meet" 5 "Ctrl+q hands the keys back" || exit 1
wait_for "a gives it the keys" 5 "the pane title says a refocuses it" || true
keys Tab
sleep 0.4
cap | grep -qE "^ sessions +\[(▶|■) " && ok "Tab moves the keys from Claude Code to the session bar" || bad "Tab did not focus the bar: $(cap | sed -n 2p)"
keys Tab
sleep 0.4
cap | grep -qE "╭ Related " && ok "Tab cycles on to the Related pane" || bad "Tab did not leave the bar for Related"
for i in 1 2 3 4 5; do cap | grep -q "Claude Code · questions" && break; keys Tab; sleep 0.4; done
cap | grep -q "Claude Code · questions" && ok "Tab cycles back round to Claude Code" || bad "Tab did not return to Claude Code"
keys a
sleep 0.4
cap | grep -q "has the keys" && ok "a hands the keys back to Claude" || bad "a did not refocus"
# The q sent earlier is still pending on the fake's input line: Ctrl+u clears it first.
keys C-u
keys -l "exit"
keys Enter
wait_for "Claude Code · questions · exited" 10 "the pane says the session exited" || exit 1
keys q
sleep 0.4
wait_for "Quit\?" 5 "q reaches meet again once the session is gone" || exit 1
keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
alive && { bad "meet did not exit"; tmux -L $SOCK kill-server; exit 1; } || ok "meet exited"
tmux -L $SOCK kill-server 2>/dev/null

echo "== 9. meet ask from a plain shell picks the newest meeting and execs claude"
MEET_DATA_DIR=$W/data $MEET sessions "$repo" | grep -qE "^    id [0-9a-z]{26}$" && ok "meet sessions prints session ids" || bad "no ids in meet sessions"
( cd "$repo" && MEET_DATA_DIR=$W/data FAKE_CLAUDE_LOG=$W/claude.log $MEET --claude-bin $E2E/fake-claude ask > "$W/ask.out" 2>&1 </dev/null & echo $! > "$W/ask.pid" )
sleep 2
kill "$(cat "$W/ask.pid")" 2>/dev/null
grep -q "meet · questions about the meeting of" "$W/ask.out" && ok "meet ask names the meeting it opens" || bad "meet ask output: $(cat "$W/ask.out")"
grep -q "fake claude · question session · meet · questions" "$W/ask.out" && ok "meet ask exec'd claude with the session name" || bad "claude did not come up: $(cat "$W/ask.out")"
grep -qE "^ARGS: --append-system-prompt-file .*--name meet · questions" "$W/claude.log" && ok "the plain-shell session carried the same context" || bad "no interactive args for meet ask"

echo "== 10. the settings modal: a change lands in settings.json and the next lookup runs with it"
start_meet --speed 3 --chunk-max-words 25
wait_for "▶ REPLAY" 20 "the replay is going" || exit 1
keys ,
wait_for "Settings" 5 "the settings modal" || exit 1
cap | grep -q "\[sonnet\]" && ok "the lookup model row shows its default" || bad "no [sonnet] row"
cap | grep -qF "data/settings.json" && ok "the modal names the settings file" || bad "the settings file is not named"
keys l
wait_for "saved · lookup model opus" 5 "the notice says the change was saved" || exit 1
grep -q '"model": "opus"' "$W/data/settings.json" && ok "settings.json carries the new lookup model" || bad "settings.json: $(cat "$W/data/settings.json" 2>&1)"
keys j
sleep 0.3
keys l
wait_for "saved · lookup effort medium" 5 "the effort row cycles too" || exit 1
keys Escape
sleep 0.4
cap | grep -q " Settings " && bad "Esc did not close the settings" || ok "Esc closed the settings"
for i in $(seq 1 80); do grep -qE -- "--model opus --effort medium" "$W/claude.log" && break; sleep 0.5; done
grep -qE -- "--model opus --effort medium" "$W/claude.log" && ok "the next lookup ran with --model opus --effort medium" || bad "no lookup with the new model and effort"
keys ,
sleep 0.4
keys R
wait_for "Reset the settings\\?" 5 "the reset confirm" || exit 1
keys y
wait_for "back to its default" 5 "the notice says everything is back to its default" || exit 1
grep -q '"model": "sonnet"' "$W/data/settings.json" && ok "settings.json is back to sonnet" || bad "settings.json after the reset: $(cat "$W/data/settings.json")"
keys Escape
sleep 0.3
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
alive && { bad "meet did not exit"; tmux -L $SOCK kill-server; exit 1; } || ok "meet exited"
tmux -L $SOCK kill-server 2>/dev/null
# A flag passed on the command line wins over the file for that launch, and the modal says so.
start_meet --lookup-model haiku
wait_for "REPLAY|ended" 20 "meet is up" || exit 1
keys ,
wait_for "lookup-model haiku this launch" 5 "the modal names the flag that overrides the row" || exit 1
keys Escape
sleep 0.3
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
tmux -L $SOCK kill-server 2>/dev/null

echo "== 11. D discards the recording: nothing is kept, nothing is summarized, meet quits"
count_sessions() { MEET_DATA_DIR=$W/data $MEET sessions "$repo" | grep -cE "^    id [0-9a-z]{26}$"; }
n_sessions_before=$(count_sessions)
n_dirs_before=$(ls -d "$W"/data/sessions/*/ 2>/dev/null | wc -l | tr -d ' ')
n_suggests_before=$(grep -cF '"items":{"type":"array"' "$W/claude.log")
start_meet --speed 3 --chunk-max-words 25
wait_for "▶ REPLAY" 20 "the replay is going" || exit 1
cap | tail -1 | grep -q "D discard" && ok "the footer offers D discard" || bad "footer: $(cap | tail -1)"
keys D
wait_for "Discard the recording\\?" 5 "the discard confirm" || exit 1
cap | grep -q "no summary or action items" && ok "it says nothing is written" || bad "discard confirm copy"
keys n
sleep 0.4
cap | grep -q "Discard the recording?" && bad "n did not close the discard confirm" || ok "n keeps the recording"
cap | grep -q "▶ REPLAY" && ok "the replay is still going" || bad "the replay stopped: $(cap | sed -n 1p)"
keys q
wait_for "Quit\\?" 5 "the quit confirm" || exit 1
cap | grep -q "discard the recording and quit" && ok "the quit confirm offers d to discard" || bad "quit confirm lacks discard: $(cap | grep -i discard)"
keys d
wait_for "Discard the recording\\?" 5 "d in the quit confirm asks to discard" || exit 1
keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
alive && { bad "meet did not exit"; tmux -L $SOCK kill-server; exit 1; } || ok "meet exited"
grep -q "recording discarded" "$W/meet.err" && ok "the exit message says the recording was discarded" || bad "no discard message: $(tail -3 "$W/meet.err")"
n_sessions_after=$(count_sessions)
[ "$n_sessions_after" -eq "$n_sessions_before" ] && ok "the discarded session is not in meet sessions (still $n_sessions_after)" || bad "sessions before $n_sessions_before, after $n_sessions_after"
n_dirs_after=$(ls -d "$W"/data/sessions/*/ 2>/dev/null | wc -l | tr -d ' ')
[ "$n_dirs_after" -eq "$n_dirs_before" ] && ok "its live files are gone" || bad "session dirs before $n_dirs_before, after $n_dirs_after"
n_suggests_after=$(grep -cF '"items":{"type":"array"' "$W/claude.log")
[ "$n_suggests_after" -eq "$n_suggests_before" ] && ok "no action-item call was made" || bad "an action-item call ran after the discard"
tmux -L $SOCK kill-server 2>/dev/null

echo "== 12. r after a recording ended starts the next one as a new session"
before=$(MEET_DATA_DIR=$W/data $MEET sessions "$repo" | grep -oE "[0-9]+ session" | grep -oE "[0-9]+")
start_meet
wait_for "Key points" 40 "the first recording's write-up" || exit 1
cap | grep -qE "^ sessions +■ ended │" && ok "the bar: this session, ended, at the left" || bad "bar: $(cap | sed -n 2p)"
# The "summary written" flash holds the footer for a few seconds first.
for i in $(seq 1 20); do cap | tail -1 | grep -q "^ r record " && break; sleep 0.5; done
cap | tail -1 | grep -q "^ r record " && ok "the footer offers r again once the recording ended" || bad "footer: $(cap | tail -1)"
keys r
wait_for "▶ REPLAY" 20 "r starts the next recording" || exit 1
cap | grep -qE "^ sessions +▶ live │ [0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}" && ok "the bar: the new live session at the left, the ended one next to it" || bad "bar after r: $(cap | sed -n 2p)"
cap | grep -q "Key points" && bad "the ended meeting's write-up is still on screen" || ok "the Summary pane started over"
cap | grep -q "Related" && ok "the Related pane is back for the new recording" || bad "no Related pane after r"
wait_for "Key points" 60 "the second recording's write-up" || exit 1
keys Right
wait_for "session 2 of" 5 "→ steps to the session that ended before r" || exit 1
keys m
sleep 0.4
cap | grep -q "Key points" && ok "its write-up is kept under its own session" || bad "the ended session's write-up is gone"
keys Escape
sleep 0.4
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
alive && { bad "meet did not exit"; tmux -L $SOCK kill-server; exit 1; } || ok "meet exited"
after=$(MEET_DATA_DIR=$W/data $MEET sessions "$repo" | grep -oE "[0-9]+ session" | grep -oE "[0-9]+")
[ "$after" -eq $((before + 2)) ] && ok "two sessions were kept from one launch ($before → $after)" || bad "sessions before $before, after $after"
n_suggests=$(grep -cF '"items":{"type":"array"' "$W/claude.log")
tmux -L $SOCK kill-server 2>/dev/null

echo
echo "passed $pass, failed $fail"
echo "--- meet.err ---"; cat "$W/meet.err"
[ $fail -eq 0 ]
