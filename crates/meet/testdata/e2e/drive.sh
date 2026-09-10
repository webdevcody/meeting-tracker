#!/bin/bash
# Drives the meet TUI through stop / resume / quit-and-auto-resume / session browsing with
# a fake claude and a fake gh (no money spent, no real PRs), on a private tmux server.
# `make e2e` runs it; needs tmux and a release build. Run with bash, not zsh.
set -u
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
export MEET_DATA_DIR=$W/data FAKE_CLAUDE_LOG=$W/claude.log FAKE_GH_COUNTER=$W/gh-counter
exec $MEET --replay $ROOT/crates/meet/testdata/demo-transcript.json --replay-speed 12 \
  --claude-bin $E2E/fake-claude --gh-bin $E2E/fake-gh \
  --on-done 'Post a one-line summary about {{title}} on branch {{branch}}.' "\$@" $repo 2>>$W/meet.err
EOF
chmod +x "$W/run-meet.sh"

SOCK=meet-e2e
tmux -L $SOCK kill-server 2>/dev/null
pass=0; fail=0
ok()   { echo "  ✓ $1"; pass=$((pass+1)); }
bad()  { echo "  ✗ $1"; fail=$((fail+1)); }
cap()  { tmux -L $SOCK capture-pane -p -t meet 2>/dev/null; }
keys() { tmux -L $SOCK send-keys -t meet "$@"; }
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
  sleep 0.5
}
alive() { tmux -L $SOCK has-session -t meet 2>/dev/null; }

echo "== 1. first launch: replay produces action items"
start_meet
wait_for "Fake change 1" 40 "an action item from the fake summarizer" || exit 1
wait_for "Fake change 2" 40 "a second action item" || exit 1

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
tmux -L $SOCK kill-server 2>/dev/null

echo
echo "passed $pass, failed $fail"
echo "--- meet.err ---"; cat "$W/meet.err"
[ $fail -eq 0 ]
