#!/usr/bin/env bash
# FIFO queue on the machine's single dev stack.
#   live.sh run <cmd...>   wait for our turn, hold the stack, run the command
#   live.sh status         who holds the stack and who waits
#   live.sh check          exit 0 if we may use the stack, else print the holder and exit 3
# State lives in $CHELABASE_LIVE_DIR (default: <main checkout>/.chelabase/live):
#   lock (flock), holder (key=value), queue/<epoch-ns>-<pid> (key=value tickets).
set -u

main_checkout() {
  local common
  common="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || return 1
  dirname "$common"
}
if [ -n "${CHELABASE_LIVE_DIR:-}" ]; then
  LIVE_DIR="$CHELABASE_LIVE_DIR"
else
  LIVE_DIR="$(main_checkout)/.chelabase/live" || { echo "live.sh: not in a git checkout" >&2; exit 2; }
fi
QUEUE="$LIVE_DIR/queue"
LOCK="$LIVE_DIR/lock"
HOLDER="$LIVE_DIR/holder"

this_worktree() { git rev-parse --show-toplevel 2>/dev/null || pwd; }
this_branch() { git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?"; }
now_s() { date +%s; }
field() { sed -n "s/^$1=//p" "$2" 2>/dev/null | head -n1; } # field <key> <file>
alive() { [ -n "$1" ] && kill -0 "$1" 2>/dev/null; }
# is_run <pid>: a live live.sh runner. Checks the command line too, so a pid the
# OS reused for another process does not count (kill -0 only where /proc is missing).
is_run() {
  alive "$1" || return 1
  [ -r /proc/self/cmdline ] || return 0
  case "$(tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null)" in *live.sh*) return 0 ;; *) return 1 ;; esac
}

# True when someone currently holds the lock (tested on a throwaway fd).
lock_held() {
  [ -e "$LOCK" ] || return 1
  # shared, non-blocking probe: only fails while an exclusive holder exists
  ( exec 8<"$LOCK"; ! flock -n -s 8 )
}

# Tickets in order, oldest first; drops those whose pid is gone.
live_tickets() {
  local t
  [ -d "$QUEUE" ] || return 0
  for t in $(ls "$QUEUE" 2>/dev/null | sort -t- -k1,1n -k2,2n); do
    if is_run "${t##*-}"; then echo "$t"; else rm -f "$QUEUE/$t"; fi
  done
}

# Only meaningful while the lock is held. A holder whose runner is gone is
# draining: processes of its command's group still run and keep the lock.
describe_holder() {
  local pid
  if [ -s "$HOLDER" ]; then
    pid="$(field pid "$HOLDER")"
    if is_run "$pid"; then
      echo "held by $(field worktree "$HOLDER") [$(field branch "$HOLDER")] running: $(field command "$HOLDER") (pid $pid, since $(field started "$HOLDER"))"
    else
      echo "held by $(field worktree "$HOLDER") [$(field branch "$HOLDER")] draining: $(field command "$HOLDER") (runner pid $pid exited, processes of its command still run; since $(field started "$HOLDER"))"
    fi
  else
    echo "held by an unknown run"
  fi
}

cmd_status() {
  local t n=0 ts
  if lock_held; then describe_holder; else echo "free"; fi
  for t in $(live_tickets); do
    n=$((n + 1))
    ts="${t%%-*}"
    echo "  $n. $(field worktree "$QUEUE/$t") [$(field branch "$QUEUE/$t")] $(field command "$QUEUE/$t") (waiting $(($(now_s) - ts / 1000000000))s)"
  done
}

cmd_check() {
  [ "${CHELABASE_LIVE_HOLDER:-}" = 1 ] && return 0
  lock_held || return 0
  if [ -s "$HOLDER" ] && [ "$(field worktree "$HOLDER")" = "$(this_worktree)" ]; then return 0; fi
  echo "The dev stack is $(describe_holder)." >&2
  echo "Wait for it, or queue your work with: just live <command>" >&2
  return 3
}

TICKET=""
CHILD=""      # pid of the running command; also its process-group id (job control)
SIGNALED=0
WATCH=""
LOCKWATCH=""  # pid of the lock-holding watcher (outlives the runner if the group does)
HAVE_LOCK=0
TTY_FG=0      # stdin is a terminal whose foreground group is ours: hand it to the command
cleanup() {
  [ -n "$WATCH" ] && kill "$WATCH" 2>/dev/null
  # a live watcher removes the holder file itself once the group has drained
  [ "$HAVE_LOCK" = 1 ] && [ -z "$LOCKWATCH" ] && rm -f "$HOLDER"
  [ -n "$TICKET" ] && rm -f "$QUEUE/$TICKET"
  # the lock fd (9) closes with the process
}

# INT/TERM/HUP: pass to the command's whole process group, SIGKILL it after 10 s.
# While still queueing there is no command: just leave (the EXIT trap tidies up).
on_signal() {
  SIGNALED=$2
  if [ -z "$CHILD" ]; then exit $((128 + $2)); fi
  kill -s "$1" -- "-$CHILD" 2>/dev/null
  if [ -z "$WATCH" ]; then
    ( sleep 10; kill -s KILL -- "-$CHILD" 2>/dev/null ) 9>&- &
    WATCH=$!
  fi
}

# group_alive <pgid>: any process left in that process group (zombies count).
group_alive() { kill -0 -- "-$1" 2>/dev/null; }

# Terminal hand-over (perl's POSIX module: bash has no tcsetpgrp). SIGTTOU is
# ignored around tcsetpgrp, since a background group may not change the
# foreground group otherwise; the command gets the default SIGTTOU back.
# SIGTSTP is ignored for the command: a Ctrl-Z would stop it while it owns the
# terminal, and the runner (not a job-control shell) could not hand it back.
TTY_TAKE='$SIG{TTOU}="IGNORE"; POSIX::tcsetpgrp(0, getpgrp()); $SIG{TTOU}="DEFAULT"; $SIG{TSTP}="IGNORE";
  exec { $ARGV[0] } @ARGV or do { print STDERR "live.sh: $ARGV[0]: $!\n"; exit 127 }'
tty_detect() {
  [ -t 0 ] && command -v perl >/dev/null 2>&1 &&
    perl -MPOSIX -e 'exit(POSIX::tcgetpgrp(0) == getpgrp() ? 0 : 1)' 2>/dev/null &&
    TTY_FG=1
}
tty_restore() { perl -MPOSIX -e '$SIG{TTOU}="IGNORE"; POSIX::tcsetpgrp(0, getpgrp())' 2>/dev/null; }

# run_child <hold-lock: 1|0> <cmd...>: own process group, stdin kept, default INT.
# The command runs as a background job (so `wait` stays interruptible and the
# INT/TERM/HUP traps can forward signals at once; bash defers traps while a
# foreground job runs). From a terminal (TTY_FG=1) the command first makes its
# group the terminal's foreground group, so it can read the terminal and change
# its settings, and Ctrl-C reaches it straight from the terminal; the runner
# takes the terminal back once the command has exited.
# The command never gets the lock fd. With hold=1 a watcher (below) keeps it
# while any process of the command's group lives.
run_child() {
  local hold="$1" rc; shift
  set -m
  if [ "$TTY_FG" = 1 ]; then
    perl -MPOSIX -e "$TTY_TAKE" -- "$@" 9>&- &
  else
    "$@" 9>&- &
  fi
  CHILD=$!
  set +m
  if [ "$hold" = 1 ]; then
    # The watcher is a separate process in the runner's group (not the command's),
    # so a SIGKILLed runner cannot free the lock while the command's group lives,
    # and a daemon that left the group (setsid, own process group) cannot hold it.
    # It removes the holder file (ours only) before it lets go of the lock, so
    # the file describes the run while the group drains and never outlives it.
    ( while kill -0 -- "-$CHILD" 2>/dev/null; do sleep 0.5 9>&-; done
      [ "$(field pid "$HOLDER")" = "$$" ] && rm -f "$HOLDER"
    ) </dev/null >/dev/null 2>&1 &
    LOCKWATCH=$!
  fi
  while :; do
    wait "$CHILD"; rc=$?
    kill -0 "$CHILD" 2>/dev/null || break # wait was interrupted by a trapped signal
  done
  [ "$TTY_FG" = 1 ] && tty_restore
  [ -n "$WATCH" ] && { kill "$WATCH" 2>/dev/null; WATCH=""; }
  # the group is empty right away in the common case: release without the poll delay
  [ -n "$LOCKWATCH" ] && ! group_alive "$CHILD" && { kill "$LOCKWATCH" 2>/dev/null; LOCKWATCH=""; }
  CHILD=""
  [ "$SIGNALED" -ne 0 ] && return $((128 + SIGNALED))
  return "$rc"
}

# Caveat: the command runs without the lock fd (a daemon it starts would
# otherwise keep the lock forever). Instead a watcher process holds the lock
# while any process in the command's process group is alive, so the stack stays
# locked until the command and its group have exited, even if this runner is
# SIGKILLed. A daemon that left the group (setsid, its own process group) does
# not hold the lock. The watcher polls every 0.5 s and exits once the group is
# gone. LIVE_SWAP_DEV runs with no watcher (it starts long-lived daemons on
# purpose).
# The holder file stays while the group drains (status shows it as draining)
# and the watcher removes it before letting go of the lock. Our queue ticket
# goes as soon as we hold the lock. From a terminal the command gets the
# terminal (see run_child); without perl it runs as before, unable to read it.
cmd_run() {
  [ $# -gt 0 ] || { echo "usage: live.sh run <command...>" >&2; return 2; }
  mkdir -p "$QUEUE"
  : >> "$LOCK"
  local wt br started cmdline
  wt="$(this_worktree)"; br="$(this_branch)"; started="$(date -Is)"
  cmdline="$*"; cmdline="${cmdline//$'\n'/ }"
  TICKET="$(date +%s%N)-$$"
  printf 'worktree=%s\nbranch=%s\ncommand=%s\nstarted=%s\n' "$wt" "$br" "$cmdline" "$started" > "$QUEUE/$TICKET"
  trap cleanup EXIT
  trap 'on_signal INT 2' INT
  trap 'on_signal TERM 15' TERM
  trap 'on_signal HUP 1' HUP

  exec 9>"$LOCK"
  local waited=0 pos first
  while :; do
    first="$(live_tickets | head -n1)"
    if [ "$first" = "$TICKET" ] && flock -n 9; then break; fi
    if [ $((waited % 60)) -eq 0 ]; then
      pos="$(live_tickets | grep -n -x -- "$TICKET" | cut -d: -f1)"
      if lock_held; then echo "live: waiting (position ${pos:-?}); stack $(describe_holder)" >&2
      else echo "live: waiting (position ${pos:-?}) for the queue ahead of us" >&2; fi
    fi
    sleep 1 & wait $!
    waited=$((waited + 1))
  done
  HAVE_LOCK=1
  # the lock keeps the order now: our ticket would only show us as waiting
  rm -f "$QUEUE/$TICKET"; TICKET=""
  printf 'worktree=%s\nbranch=%s\ncommand=%s\npid=%s\nstarted=%s\n' "$wt" "$br" "$cmdline" "$$" "$(date -Is)" > "$HOLDER"

  tty_detect
  local rc=0
  if [ -n "${LIVE_SWAP_DEV:-}" ]; then
    run_child 0 env -u CHELABASE_LIVE_HOLDER bash -c "$LIVE_SWAP_DEV"; rc=$?
    [ "$rc" -eq 0 ] || { echo "live: LIVE_SWAP_DEV failed ($rc)" >&2; return "$rc"; }
  fi
  export CHELABASE_LIVE_HOLDER=1
  run_child 1 "$@"
}

case "${1:-}" in
  run) shift; cmd_run "$@"; exit $? ;;
  status) cmd_status ;;
  check) cmd_check; exit $? ;;
  *) echo "usage: live.sh run <cmd...> | status | check" >&2; exit 2 ;;
esac
