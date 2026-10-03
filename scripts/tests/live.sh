#!/usr/bin/env bash
# Offline tests for scripts/live.sh (the FIFO queue on the one dev stack).
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIVE="$HERE/../live.sh"
TOP="$(git -C "$HERE" rev-parse --show-toplevel)"
ROOT="$(mktemp -d)"
FAILS=0
PIDS=()

cleanup() {
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null; done
  rm -rf "$ROOT"
}
trap cleanup EXIT

fresh() { # new isolated live dir
  export CHELADB_LIVE_DIR="$ROOT/$1"
  mkdir -p "$CHELADB_LIVE_DIR"
  unset CHELADB_LIVE_HOLDER LIVE_SWAP_DEV
}
fail() { echo "  FAIL: $*"; FAILS=$((FAILS + 1)); }
wait_for() { # wait_for <seconds> <test-cmd...>
  local n=$(($1 * 10)); shift
  while [ "$n" -gt 0 ]; do "$@" && return 0; sleep 0.1; n=$((n - 1)); done
  return 1
}
holder_exists() { [ -s "$CHELADB_LIVE_DIR/holder" ]; }
no_tickets() { [ -z "$(ls -A "$CHELADB_LIVE_DIR/queue" 2>/dev/null)" ]; }
some_ticket() { [ -n "$(ls -A "$CHELADB_LIVE_DIR/queue" 2>/dev/null)" ]; }
lock_free() { flock -n "$CHELADB_LIVE_DIR/lock" true; }
# hold the lock as another worktree would, without a runner
fake_holder() { # fake_holder <worktree-path>
  mkdir -p "$CHELADB_LIVE_DIR"
  : > "$CHELADB_LIVE_DIR/lock"
  ( exec 8>"$CHELADB_LIVE_DIR/lock"; flock 8; exec sleep 60 ) &
  FAKE_PID=$!; PIDS+=("$FAKE_PID")
  printf 'worktree=%s\nbranch=other\ncommand=fake\npid=%s\nstarted=now\n' "$1" "$FAKE_PID" > "$CHELADB_LIVE_DIR/holder"
}

run_case() {
  echo "== $1"
  local before=$FAILS
  "$1"
  [ "$FAILS" -eq "$before" ] && echo "  ok"
}

two_runs_queue_in_order() {
  fresh order
  local out="$CHELADB_LIVE_DIR/out"
  "$LIVE" run bash -c "echo a-start >> $out; sleep 2; echo a-end >> $out" & PIDS+=($!)
  local a=$!
  wait_for 5 holder_exists || fail "first run never became holder"
  "$LIVE" run bash -c "echo b-start >> $out; echo b-end >> $out" & PIDS+=($!)
  local b=$!
  wait $a; wait $b
  [ "$(tr '\n' ' ' < "$out")" = "a-start a-end b-start b-end " ] || fail "order was: $(tr '\n' ' ' < "$out")"
  no_tickets || fail "tickets left behind"
}

a_crashed_holder_is_cleared() {
  fresh crash
  # runner killed with SIGKILL: its command (an orphan now) still holds the lock
  "$LIVE" run bash -c 'sleep 3; true' & local a=$!; PIDS+=("$a")
  wait_for 5 holder_exists || fail "no holder"
  kill -9 "$a"; wait "$a" 2>/dev/null
  lock_free && fail "lock freed while the orphan still runs"
  "$LIVE" run bash -c "echo proceeded > $CHELADB_LIVE_DIR/out" & local b=$!; PIDS+=("$b")
  sleep 1
  [ -e "$CHELADB_LIVE_DIR/out" ] && fail "next run did not wait for the orphan"
  wait_for 15 test -e "$CHELADB_LIVE_DIR/out" || fail "next run never proceeded"
  wait "$b"
  no_tickets || fail "dead ticket not cleared"
}

sigterm_kills_the_whole_command() {
  fresh term
  "$LIVE" run bash -c 'sleep 42; true' & local a=$!; PIDS+=("$a")
  wait_for 5 holder_exists || fail "no holder"
  wait_for 5 pgrep -fx 'sleep 42' >/dev/null || fail "command did not start"
  kill -TERM "$a"; wait "$a" 2>/dev/null
  pgrep -fx 'sleep 42' >/dev/null && { fail "sleep 42 survived"; pkill -fx 'sleep 42'; }
  wait_for 5 lock_free || fail "lock still held"
}

stdin_reaches_the_command() {
  fresh stdin
  echo hello | "$LIVE" run bash -c 'read x; [ "$x" = hello ]' || fail "stdin not passed through"
}

check_free_lock_with_stale_holder_passes() {
  fresh stale
  printf 'worktree=/nonexistent/x\nbranch=b\ncommand=c\npid=999999\nstarted=n\n' > "$CHELADB_LIVE_DIR/holder"
  (cd "$TOP" && "$LIVE" check) || fail "free lock with stale holder should pass"
}

check_held_without_holder_file_exits_3() {
  fresh nohold
  fake_holder /x; rm -f "$CHELADB_LIVE_DIR/holder"
  (cd "$TOP" && "$LIVE" check >/dev/null 2>&1); local rc=$?
  [ "$rc" -eq 3 ] || fail "expected 3, got $rc"
  kill -9 "$FAKE_PID" 2>/dev/null
}

paths_with_spaces_work() {
  fresh "dir with spaces"
  local out="$CHELADB_LIVE_DIR/out file"
  "$LIVE" run bash -c 'echo "$1" > "$2"' _ "a b" "$out" || fail "run failed"
  [ "$(cat "$out")" = "a b" ] || fail "argument with space mangled"
}

swap_failure_aborts_the_run() {
  fresh swapfail
  LIVE_SWAP_DEV="exit 5" "$LIVE" run bash -c "echo ran > $CHELADB_LIVE_DIR/out" 2>/dev/null; local rc=$?
  [ "$rc" -eq 5 ] || fail "expected 5, got $rc"
  [ -e "$CHELADB_LIVE_DIR/out" ] && fail "command ran after a failed swap"
  lock_free || fail "lock held"
}

swap_hook_does_not_see_holder_flag_and_newlines_are_stripped() {
  fresh flag
  local out="$CHELADB_LIVE_DIR/out"
  CHELADB_LIVE_HOLDER=1 LIVE_SWAP_DEV="echo \"[\${CHELADB_LIVE_HOLDER:-}]\" > $out" "$LIVE" run bash -c "printf '%s\n' \"\$CHELADB_LIVE_HOLDER\" >> $out; sleep 1" &
  local a=$!; PIDS+=("$a")
  wait_for 5 holder_exists || fail "no holder"
  [ "$(wc -l < "$CHELADB_LIVE_DIR/holder")" -eq 5 ] || fail "holder file has stray lines"
  wait "$a"
  [ "$(tr '\n' ' ' < "$out")" = "[] 1 " ] || fail "got: $(tr '\n' ' ' < "$out")"
  fresh nl
  "$LIVE" run bash -c 'sleep 1
true' & local b=$!; PIDS+=("$b")
  wait_for 5 holder_exists || fail "no holder"
  [ "$(wc -l < "$CHELADB_LIVE_DIR/holder")" -eq 5 ] || fail "newline in command leaked into holder"
  wait "$b"
}

a_detached_daemon_does_not_hold_the_lock() {
  fresh daemon
  local pidf="$CHELADB_LIVE_DIR/daemon.pid"
  "$LIVE" run bash -c "setsid bash -c 'echo \$\$ > $pidf; exec sleep 40' >/dev/null 2>&1 & while [ ! -s $pidf ]; do sleep 0.1; done" || fail "run failed"
  PIDS+=("$(cat "$pidf" 2>/dev/null)")
  kill -0 "$(cat "$pidf")" 2>/dev/null || fail "daemon did not survive the run"
  wait_for 3 lock_free || fail "daemon holds the lock after the run"
  timeout 5 "$LIVE" run bash -c "echo second > $CHELADB_LIVE_DIR/out" || fail "next run did not get the lock right away"
  [ -e "$CHELADB_LIVE_DIR/out" ] || fail "next run never ran"
  kill -9 "$(cat "$pidf")" 2>/dev/null
}

a_killed_runner_keeps_the_lock_while_the_command_runs() {
  fresh killrun
  local pidf="$CHELADB_LIVE_DIR/daemon.pid"
  "$LIVE" run bash -c "setsid bash -c 'echo \$\$ > $pidf; exec sleep 40' >/dev/null 2>&1 & sleep 3" & local a=$!; PIDS+=("$a")
  wait_for 5 test -s "$pidf" || fail "daemon never started"
  PIDS+=("$(cat "$pidf" 2>/dev/null)")
  kill -9 "$a"; wait "$a" 2>/dev/null
  sleep 1
  lock_free && fail "lock freed while the command still runs"
  wait_for 8 lock_free || fail "lock never freed after the command exited (detached daemon still alive)"
  kill -0 "$(cat "$pidf")" 2>/dev/null || fail "daemon should still be alive"
  kill -9 "$(cat "$pidf")" 2>/dev/null
}

ctrl_c_releases() {
  fresh sigint
  set -m # job control, so the background runner does not start with SIGINT ignored
  "$LIVE" run sleep 60 & local a=$!; PIDS+=("$a")
  set +m
  wait_for 5 holder_exists || fail "no holder"
  kill -INT "$a"; wait "$a" 2>/dev/null
  wait_for 5 lock_free || fail "lock still held"
  holder_exists && fail "holder file left"
  no_tickets || fail "ticket left"
}

status_shows_holder_and_queue() {
  fresh status
  "$LIVE" status | grep -qi free || fail "empty status should say free"
  "$LIVE" run sleep 4 & local a=$!; PIDS+=("$a")
  wait_for 5 holder_exists || fail "no holder"
  "$LIVE" run echo queued & local b=$!; PIDS+=("$b")
  wait_for 5 some_ticket || fail "second ticket missing"
  local s; s="$("$LIVE" status)"
  echo "$s" | grep -q "sleep 4" || fail "holder command missing: $s"
  echo "$s" | grep -q "echo queued" || fail "queued command missing: $s"
  wait "$a"; wait "$b"
}

check_exits_3_for_another_worktree() {
  fresh other
  fake_holder /nonexistent/other-worktree
  (cd "$TOP" && "$LIVE" check >"$ROOT/check.out" 2>&1); local rc=$?
  [ "$rc" -eq 3 ] || fail "expected 3, got $rc"
  grep -q other-worktree "$ROOT/check.out" || fail "holder not printed"
  kill -9 "$FAKE_PID" 2>/dev/null
}

check_passes_inside_a_live_run() {
  fresh inside
  fake_holder /nonexistent/other-worktree
  (cd "$TOP" && CHELADB_LIVE_HOLDER=1 "$LIVE" check) || fail "check should pass with CHELADB_LIVE_HOLDER=1"
  kill -9 "$FAKE_PID" 2>/dev/null
  fresh inside2
  (cd "$TOP" && "$LIVE" run bash -c "\"$LIVE\" check") || fail "check inside a run failed"
  (cd "$TOP" && "$LIVE" check) || fail "check on a free stack failed"
}

same_worktree_check_passes() {
  fresh same
  fake_holder "$TOP"
  (cd "$TOP" && "$LIVE" check) || fail "same worktree should pass"
  kill -9 "$FAKE_PID" 2>/dev/null
}

swap_hook_runs_before_the_command() {
  fresh swap
  local out="$CHELADB_LIVE_DIR/out"
  LIVE_SWAP_DEV="echo swap >> $out" "$LIVE" run bash -c "echo cmd >> $out"
  [ "$(tr '\n' ' ' < "$out")" = "swap cmd " ] || fail "got: $(tr '\n' ' ' < "$out")"
}

exit_status_is_propagated() {
  fresh status7
  "$LIVE" run bash -c 'exit 7'; local rc=$?
  [ "$rc" -eq 7 ] || fail "expected 7, got $rc"
  no_tickets || fail "ticket left"
  lock_free || fail "lock held"
}

# I1: from a terminal the command is the foreground job: it can read the
# terminal and change its settings (no SIGTTIN/SIGTTOU stop), and the terminal
# comes back to the caller afterwards.
a_command_run_from_a_terminal_can_use_it() {
  fresh tty
  local out="$CHELADB_LIVE_DIR/out" rc
  command -v script >/dev/null || { fail "script (util-linux) not installed"; return; }
  ( sleep 1; printf 'typed\n'; sleep 1; printf 'later\n'; sleep 1 ) |
    timeout 10 script -qec "\"$LIVE\" run bash -c 'read -r x; stty -echo; stty echo; echo \"got:\$x\" > \"$out\"'; read -r y; echo \"after:\$y\" >> \"$out\"" /dev/null >/dev/null 2>&1
  rc=$?
  [ "$rc" -eq 124 ] && fail "the run hung on the terminal"
  [ "$(tr '\n' ' ' < "$out" 2>/dev/null)" = "got:typed after:later " ] || fail "got: $(tr '\n' ' ' < "$out" 2>/dev/null)"
  pkill -9 -f "^bash .*/live\\.sh run bash -c read -r x" 2>/dev/null
  pkill -9 -fx "bash -c read -r x; stty -echo.*" 2>/dev/null
  wait_for 5 lock_free || fail "lock still held"
}

# I1: Ctrl-C typed on the terminal stops the command and releases the stack.
ctrl_c_on_the_terminal_releases() {
  fresh ttyint
  local rc
  ( sleep 1.5; printf '\003'; sleep 1 ) | timeout 10 script -qec "\"$LIVE\" run sleep 31" /dev/null >/dev/null 2>&1
  rc=$?
  [ "$rc" -eq 124 ] && fail "Ctrl-C did not end the run"
  pgrep -fx 'sleep 31' >/dev/null && { fail "sleep 31 survived"; pkill -fx 'sleep 31'; }
  wait_for 5 lock_free || fail "lock still held"
  holder_exists && fail "holder file left"
}

# I1: from a terminal, TERM to the runner still reaches the command at once,
# and exit statuses (including a missing command) come through.
a_terminal_run_forwards_term_and_exit_status() {
  fresh ttyterm
  ( sleep 6 ) | timeout 10 script -qec "\"$LIVE\" run sleep 33" /dev/null >/dev/null 2>&1 & local s=$!; PIDS+=("$s")
  wait_for 5 pgrep -fx 'sleep 33' >/dev/null || fail "command did not start"
  kill -TERM "$(ps -o ppid= -p "$(pgrep -fx 'sleep 33')" | tr -d ' ')" # the runner
  wait_for 2 eval '! pgrep -fx "sleep 33" >/dev/null' || { fail "TERM not forwarded"; pkill -9 -fx 'sleep 33'; }
  wait_for 5 lock_free || fail "lock still held"
  wait "$s" 2>/dev/null
  local rc
  timeout 10 script -qec "\"$LIVE\" run bash -c 'exit 7'" /dev/null </dev/null >/dev/null 2>&1; rc=$?
  [ "$rc" -eq 7 ] || fail "expected 7, got $rc"
  timeout 10 script -qec "\"$LIVE\" run no-such-command-xyz" /dev/null </dev/null >/dev/null 2>&1; rc=$?
  [ "$rc" -eq 127 ] || fail "expected 127, got $rc"
}

# I1: Ctrl-Z must not leave the terminal with a stopped command the runner
# cannot hand back: the command ignores it, and a later Ctrl-C still ends it.
ctrl_z_on_the_terminal_does_not_stop_the_run() {
  fresh ttytstp
  local rc
  ( sleep 1.5; printf '\032'; sleep 0.5; printf '\003'; sleep 1 ) | timeout 8 script -qec "\"$LIVE\" run sleep 35" /dev/null >/dev/null 2>&1
  rc=$?
  [ "$rc" -eq 124 ] && fail "the run hung after Ctrl-Z"
  pgrep -fx 'sleep 35' >/dev/null && { fail "sleep 35 survived"; pkill -9 -fx 'sleep 35'; }
  pkill -9 -fx "bash .*/live\\.sh run sleep 35" 2>/dev/null
  wait_for 5 lock_free || fail "lock still held"
}

# Minor 1: the holder's own ticket leaves the queue once it has the lock.
the_holder_is_not_listed_as_waiting() {
  fresh holdticket
  local err="$CHELADB_LIVE_DIR/waiter.err"
  "$LIVE" run sleep 3 & local a=$!; PIDS+=("$a")
  wait_for 5 holder_exists || fail "no holder"
  "$LIVE" run echo queued 2>"$err" & local b=$!; PIDS+=("$b")
  wait_for 5 grep -q position "$err" || fail "waiter never reported"
  grep -q "position 1" "$err" || fail "waiter position: $(cat "$err")"
  local s; s="$("$LIVE" status)"
  [ "$(echo "$s" | grep -cE '^  [0-9]+\. ')" -eq 1 ] || fail "expected one waiter: $s"
  echo "$s" | grep -qE '^  1\. .*echo queued' || fail "waiter not first: $s"
  echo "$s" | head -n1 | grep -q "sleep 3" || fail "holder missing: $s"
  wait "$a"; wait "$b"
}

# Minor 2: while the command's group drains after the runner is gone, the
# holder stays described; the holder file never outlives the lock.
a_draining_group_keeps_its_holder_description() {
  fresh drain
  "$LIVE" run bash -c 'sleep 2.7 >/dev/null 2>&1 &' || fail "run failed"
  lock_free && fail "lock freed while the group drains"
  local s; s="$("$LIVE" status)"
  echo "$s" | grep -q unknown && fail "status says unknown: $s"
  echo "$s" | grep -q "sleep 2.7" || fail "holder command missing: $s"
  echo "$s" | grep -q draining || fail "not marked draining: $s"
  wait_for 8 lock_free || fail "lock never freed"
  holder_exists && fail "holder file outlived the lock"
  fresh drain9
  "$LIVE" run bash -c 'sleep 2.8' & local a=$!; PIDS+=("$a")
  wait_for 5 holder_exists || fail "no holder"
  wait_for 5 pgrep -fx 'sleep 2.8' >/dev/null || fail "command did not start"
  kill -9 "$a"; wait "$a" 2>/dev/null
  s="$("$LIVE" status)"
  echo "$s" | grep -q draining || fail "killed runner not marked draining: $s"
  wait_for 8 lock_free || fail "lock never freed"
  holder_exists && fail "holder file outlived the lock (killed runner)"
}

# Minor 3: a ticket whose pid now belongs to some other process is dropped.
a_reused_ticket_pid_does_not_block_the_queue() {
  fresh reuse
  sleep 30 & local p=$!; PIDS+=("$p")
  mkdir -p "$CHELADB_LIVE_DIR/queue"
  printf 'worktree=/x\nbranch=b\ncommand=dead\nstarted=n\n' > "$CHELADB_LIVE_DIR/queue/1-$p"
  timeout 5 "$LIVE" run true 2>/dev/null || fail "blocked by a ticket whose pid is not a live.sh run"
  [ -e "$CHELADB_LIVE_DIR/queue/1-$p" ] && fail "foreign ticket left"
  kill -9 "$p" 2>/dev/null
}

for c in two_runs_queue_in_order a_crashed_holder_is_cleared ctrl_c_releases \
  status_shows_holder_and_queue check_exits_3_for_another_worktree \
  check_passes_inside_a_live_run same_worktree_check_passes \
  swap_hook_runs_before_the_command exit_status_is_propagated \
  sigterm_kills_the_whole_command stdin_reaches_the_command \
  check_free_lock_with_stale_holder_passes check_held_without_holder_file_exits_3 \
  paths_with_spaces_work swap_failure_aborts_the_run \
  swap_hook_does_not_see_holder_flag_and_newlines_are_stripped \
  a_detached_daemon_does_not_hold_the_lock a_killed_runner_keeps_the_lock_while_the_command_runs \
  a_command_run_from_a_terminal_can_use_it ctrl_c_on_the_terminal_releases \
  a_terminal_run_forwards_term_and_exit_status ctrl_z_on_the_terminal_does_not_stop_the_run \
  the_holder_is_not_listed_as_waiting a_draining_group_keeps_its_holder_description \
  a_reused_ticket_pid_does_not_block_the_queue; do
  run_case "$c"
done

if [ "$FAILS" -gt 0 ]; then echo "$FAILS failure(s)"; exit 1; fi
echo "all passed"
