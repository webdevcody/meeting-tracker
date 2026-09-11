#!/bin/bash
# Drives the meet TUI through the live lookup, stopping the recording with x, running /
# stopping / resuming agents, quit-and-auto-resume, session browsing, the live transcript
# files and the question session (a, meet ask) with a fake claude and a fake gh (no money
# spent, no real PRs), on a private tmux server.
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
start_meet() {
  tmux -L $SOCK new-session -d -s meet -x 190 -y 50 -c "$repo" "$W/run-meet.sh $*"
  PANE=$(tmux -L $SOCK display-message -p -t meet '#{pane_id}')
  sleep 0.5
}
alive() { tmux -L $SOCK has-session -t meet 2>/dev/null; }

echo "== 1. first launch: the lookup fills the Related pane, x stops the recording, the items land"
# Slower and with small chunks, so a fact shows while the replay is still going.
start_meet --speed 3 --chunk-max-words 25
wait_for "README.md:1" 40 "a fact from the fake lookup in the Related pane" || exit 1
cap | grep -q "▶ REPLAY" && ok "the replay is still going" || bad "the replay already ended"
keys x
sleep 0.4
wait_for "Stop the recording\\?" 5 "the stop-recording confirm" || exit 1
keys y
wait_for "Fake change 1" 40 "an action item from the fake suggester" || exit 1
wait_for "Fake change 2" 40 "a second action item" || exit 1
cap | grep -q "■ ended" && ok "the header says the recording ended" || bad "header does not say ended"
n_lookups=$(grep -cF '"facts":{"type":"array"' "$W/claude.log")
n_suggests=$(grep -cF '"items":{"type":"array"' "$W/claude.log")
[ "$n_suggests" -eq 1 ] && ok "exactly one action-item call, after the stop" || bad "expected 1 action-item call, saw $n_suggests"
[ "$n_lookups" -ge 1 ] && ok "$n_lookups lookup call(s) while recording" || bad "no lookup calls"
grep -qE -- "--effort low" "$W/claude.log" && ok "the lookup ran with --effort low" || bad "no --effort low in claude args"
keys Tab
sleep 0.4
cap | grep -qE "Related · [0-9]+" && ok "Tab shows the Related pane with its facts" || bad "Tab did not show Related"
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
keys Right
sleep 0.4
if cap | grep -q "session 2 of 2"; then bad "→ did not return to live"; else ok "→ returned to the live meeting"; fi
keys Left
sleep 0.4
wait_for "session 2 of 2" 5 "← steps back to the previous session" || true
keys Escape
sleep 0.4
if cap | grep -q "session 2 of 2"; then bad "Esc did not return to live"; else ok "Esc returned to live"; fi

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
wait_for "○ Fake change" 40 "a new suggested item from this replay" || exit 1
select_runnable || exit 1
keys Enter
wait_for "1 agent running" 20 "an agent is running" || exit 1
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
start_meet --no-resume
wait_for "■ stopped" 20 "with --no-resume the interrupted item shows as stopped" || exit 1
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done

echo "== 8. the live transcript files, and a: the question session opens in a tmux pane"
# The live files: one per meeting, the transcript appended as the replay went.
n_dirs=$(ls -d "$W"/data/sessions/*/ 2>/dev/null | wc -l | tr -d ' ')
[ "$n_dirs" -ge 4 ] && ok "$n_dirs session directories with live files" || bad "expected a session dir per launch, saw $n_dirs"
live_t=$(ls -t "$W"/data/sessions/*/transcript.md | head -1)
grep -q "^# Transcript · meet in todo" "$live_t" && ok "transcript.md starts with its header" || bad "transcript.md header"
grep -qE "^\[[0-9]{2}:[0-9]{2}\] mic: " "$live_t" && ok "transcript.md carries [mm:ss] mic: lines" || bad "no transcript lines in $live_t"
live_s=$(dirname "$live_t")/summary.md
grep -q "^state: ended" "$live_s" && ok "summary.md says the recording ended" || bad "summary.md state: $(head -3 "$live_s")"
grep -q "## Action items on the board" "$live_s" && ok "summary.md lists the board" || bad "summary.md lacks the board"
# a while recording: meet runs inside tmux here, so the session lands in a pane beside it.
# The pane gets tmux's session environment, not meet's: hand it the fake's log there.
start_meet --speed 3 --chunk-max-words 25
tmux -L $SOCK set-environment -t meet FAKE_CLAUDE_LOG "$W/claude.log"
wait_for "▶ REPLAY" 20 "the replay is going" || exit 1
keys a
wait_for "question session opened in a tmux pane" 15 "the flash says a tmux pane opened" || exit 1
sleep 1
n_panes=$(tmux -L $SOCK list-panes -t meet | wc -l | tr -d ' ')
[ "$n_panes" -eq 2 ] && ok "two panes: meet and the question session" || bad "expected 2 panes, saw $n_panes"
for i in $(seq 1 30); do tmux -L $SOCK capture-pane -p -t "$(other_pane)" 2>/dev/null | grep -q "fake claude · question session" && break; sleep 0.5; done
tmux -L $SOCK capture-pane -p -t "$(other_pane)" 2>/dev/null | grep -q "fake claude · question session" && ok "the fake claude came up in the pane" || { bad "no fake claude in the pane"; tmux -L $SOCK capture-pane -p -t "$(other_pane)"; }
if grep -qE "^ARGS: --append-system-prompt-file .*/ask-system.md --add-dir .*/sessions/.* --name meet · questions --model sonnet" "$W/claude.log"; then ok "claude got the system prompt file, the session dir and the name"; else bad "question session args: $(grep -E '^ARGS: --append' "$W/claude.log" | tail -1)"; fi
grep -q "^INTERACTIVE: Read .*transcript.md and .*summary.md now" "$W/claude.log" && ok "the first prompt asks it to read both files" || bad "no first prompt logged"
sysf=$(ls -t "$W"/data/sessions/*/ask-system.md | head -1)
grep -q "being recorded right now" "$sysf" && ok "ask-system.md says the meeting is live" || bad "ask-system.md: $(head -c 200 "$sysf")"
grep -q "Before answering ANY question, Read" "$sysf" && ok "ask-system.md carries the re-read rule" || bad "no re-read rule"
keys a
wait_for "already open in the tmux pane" 5 "a second a says it is already open" || true
tmux -L $SOCK kill-pane -t "$(other_pane)" 2>/dev/null
keys q; sleep 0.3; keys y
for i in $(seq 1 60); do alive || break; sleep 0.5; done
tmux -L $SOCK kill-server 2>/dev/null

echo "== 9. meet ask from a plain shell picks the newest meeting and execs claude"
MEET_DATA_DIR=$W/data $MEET sessions "$repo" | grep -qE "^    id [0-9a-z]{26}$" && ok "meet sessions prints session ids" || bad "no ids in meet sessions"
( cd "$repo" && MEET_DATA_DIR=$W/data FAKE_CLAUDE_LOG=$W/claude.log $MEET --claude-bin $E2E/fake-claude ask > "$W/ask.out" 2>&1 </dev/null & echo $! > "$W/ask.pid" )
sleep 2
kill "$(cat "$W/ask.pid")" 2>/dev/null
grep -q "meet · questions about the meeting of" "$W/ask.out" && ok "meet ask names the meeting it opens" || bad "meet ask output: $(cat "$W/ask.out")"
grep -q "fake claude · question session · meet · questions" "$W/ask.out" && ok "meet ask exec'd claude with the session name" || bad "claude did not come up: $(cat "$W/ask.out")"
grep -qE "^ARGS: --append-system-prompt-file .*--name meet · questions" "$W/claude.log" && ok "the plain-shell session carried the same context" || bad "no interactive args for meet ask"

echo
echo "passed $pass, failed $fail"
echo "--- meet.err ---"; cat "$W/meet.err"
[ $fail -eq 0 ]
