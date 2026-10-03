#!/usr/bin/env bash
# Offline checks for scripts/stack-reset-slots.sh (`just stack-reset-slots`): a fake main
# checkout in a temp dir, a fake docker and a fake just on PATH. Nothing real is touched.
set -uo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
script="$root/scripts/stack-reset-slots.sh"
failures=0
pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1" >&2
  failures=$((failures + 1))
}
check() { # check <name> <command...>
  local name="$1"
  shift
  if "$@"; then pass "$name"; else fail "$name"; fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
bin="$tmp/bin"
mkdir -p "$bin"
# The fake docker keeps state in $FAKE_STATE (see fake_state): slot projects s1 and s2 have
# containers (s1 also a network and an unknown volume); the relay container and image exist;
# the metadata Postgres lists two slot databases. Removals and drops change the state, so a
# second run finds nothing. Every call is logged.
cat >"$bin/docker" <<'EOF'
#!/usr/bin/env bash
echo "docker $*" >>"$FAKE_LOG"
S="$FAKE_STATE"
project() { echo "${1#label=com.docker.compose.project=}"; }
forget() { # forget <kind> <ids...>: drops the ids from every state file of that kind
  local kind="$1" id f
  shift
  for id in "$@"; do
    for f in "$S/$kind".* "$S/relay"; do
      if [ -f "$f" ]; then sed -i "/^$id\$/d" "$f"; fi
    done
  done
}
case "$1 $2" in
  "ps -aq")
    case "$4" in
      label=*) cat "$S/ps.$(project "$4")" 2>/dev/null ;;
      name=*) cat "$S/relay" 2>/dev/null ;;
    esac ;;
  "volume ls") cat "$S/vol.$(project "$5")" 2>/dev/null ;;
  "network ls") cat "$S/net.$(project "$5")" 2>/dev/null ;;
  "rm -f") shift 2; forget ps "$@" ;;
  "volume rm") shift 2; forget vol "$@" ;;
  "network rm") shift 2; forget net "$@" ;;
  "image inspect") test -f "$S/image" ;;
  "image rm") rm -f "$S/image" ;;
  *)
    case "$*" in
      *"SELECT datname"*) cat "$S/dbs" 2>/dev/null ;;
      *"DROP DATABASE IF EXISTS"*)
        db="$(sed -n 's/.*DROP DATABASE IF EXISTS "\([^"]*\)".*/\1/p' <<<"$*")"
        sed -i "/^$db\$/d" "$S/dbs" ;;
    esac ;;
esac
EOF
# A git that always fails (the main checkout can't be found).
mkdir -p "$tmp/nogit-bin"
printf '%s\n' '#!/usr/bin/env bash' 'echo "git $*" >>"$FAKE_LOG"' 'exit 128' >"$tmp/nogit-bin/git"
chmod +x "$tmp/nogit-bin/git"
fake_state() { # the slot leftovers the fake docker reports
  state="$tmp/state"
  rm -rf "$state" && mkdir -p "$state"
  printf '%s\n' c1a c1b >"$state/ps.cheladb-s1"
  echo c2a >"$state/ps.cheladb-s2"
  printf '%s\n' cheladb-s1_nats_data cheladb-s1_valkey_data cheladb-s1_mystery >"$state/vol.cheladb-s1"
  echo cheladb-s2_nats_data >"$state/vol.cheladb-s2"
  echo n1 >"$state/net.cheladb-s1"
  echo relay >"$state/relay"
  : >"$state/image"
  printf '%s\n' cheladb_metadata_s1 cheladb_metadata_s2 >"$state/dbs"
}
cat >"$bin/just" <<'EOF'
#!/usr/bin/env bash
echo "just $*" >>"$FAKE_LOG"
EOF
chmod +x "$bin/docker" "$bin/just"

fake_main() { # a main checkout with every slot leftover, and things that must stay
  main="$tmp/main"
  rm -rf "$main" && mkdir -p "$main/.git"
  fake_state
  mkdir -p "$main/.cheladb/slots" "$main/.cheladb/dev/s1" "$main/.cheladb/dev/s2" \
    "$main/.cheladb/zitadel" "$main/.cheladb/secrets" "$main/.worktrees/.cargo" \
    "$main/.worktrees/target/debug" "$main/.worktrees/a" "$main/.worktrees/b"
  echo 1 >"$main/.cheladb/slots/1.env"
  echo 123 >"$main/.cheladb/dev/pids"
  echo log >"$main/.cheladb/dev/studio.log"
  echo x >"$main/.cheladb/zitadel/studio-s1.env"
  echo x >"$main/.cheladb/zitadel/demo.env"
  echo key >"$main/.cheladb/secrets/secrets.key"
  echo '[build]' >"$main/.worktrees/.cargo/config.toml"
  printf '# Written by just wt-add\nCHELADB_SLOT=1\n' >"$main/.worktrees/a/.env.local"
  printf 'MY_OWN=1\n' >"$main/.worktrees/b/.env.local"
}
reset() { # reset <args...>: runs the script against the fake main; sets status
  : >"$tmp/log"
  status=0
  env -i HOME="$tmp" PATH="$bin:/usr/bin:/bin" FAKE_LOG="$tmp/log" FAKE_STATE="$state" \
    CHELADB_MAIN="$main" "$script" "$@" >"$tmp/out" 2>"$tmp/err" </dev/null || status=$?
}
logged() { grep -qxF -- "$1" "$tmp/log"; }
not_logged() { ! grep -qF -- "$1" "$tmp/log"; }

# A stand-in for a slot's dev app: a sleep whose command line passes the "ours" guard (it contains
# target/debug/), started in its own process group like `just dev` does (setsid). Only these
# fakes are ever signalled: the pids files live in the fake main.
mkdir -p "$tmp/target/debug"
cp "$(command -v sleep)" "$tmp/target/debug/sleep"
fakes=()
spawn_app() { # spawn_app <slot>: starts the fake app, records its pid in the slot's pids file
  setsid "$tmp/target/debug/sleep" 300 >/dev/null 2>&1 </dev/null &
  fakes+=("$!")
  { echo "$!"; echo 999999999; } >"$main/.cheladb/dev/s$1/pids"
  app="$!"
}
reap() { for p in "${fakes[@]+"${fakes[@]}"}"; do kill "$p" 2>/dev/null || true; done; }
trap 'reap; rm -rf "$tmp"' EXIT
alive() { [ -d "/proc/$1" ] && ! grep -q '^State:.*Z' "/proc/$1/status" 2>/dev/null; }
settle() { for _ in 1 2 3 4 5 6 7 8 9 10; do alive "$1" || return 0; sleep 0.1; done; return 1; }

# --- dry run: prints the plan, changes nothing ------------------------------------------------
fake_main
spawn_app 1
reset --dry-run
check "a dry run leaves a slot's app running" alive "$app"
check "a dry run names the app it would stop" grep -q "stop.*$app" "$tmp/out"
check "a dry run exits 0" test "$status" -eq 0
check "a dry run names the slot projects" grep -q 'cheladb-s1' "$tmp/out"
check "a dry run names the slot databases" grep -q 'cheladb_metadata_s2' "$tmp/out"
check "a dry run removes nothing" not_logged ' rm '
check "a dry run drops nothing" not_logged 'DROP DATABASE'
check "a dry run leaves the files" test -d "$main/.cheladb/slots" -a -f "$main/.worktrees/a/.env.local"
check "a dry run doesn't start the stack" not_logged 'deps-up'

# --- no --yes and no answer: refuses ----------------------------------------------------------
fake_main
reset
check "without --yes or a yes, nothing happens" \
  bash -c 'test "$1" -ne 0 && ! grep -qF " rm " "$2" && test -d "$3"' _ "$status" "$tmp/log" "$main/.cheladb/slots"

# --- --yes: does every step ----------------------------------------------------------------------
fake_main
spawn_app 1
reset --yes
check "--yes exits 0" test "$status" -eq 0
check "a slot's running app is stopped" settle "$app"
check "the stop is printed" grep -q "stopping.*$app" "$tmp/out"
check "a stale pid is skipped, not signalled" grep -q "skipping pid 999999999" "$tmp/out"
check "slot 1's containers are removed" logged "docker rm -f c1a c1b"
check "slot 2's containers are removed" logged "docker rm -f c2a"
check "slot 1's network is removed" logged "docker network rm n1"
check "slot volumes for NATS and Valkey are removed" \
  logged "docker volume rm cheladb-s1_nats_data cheladb-s1_valkey_data"
check "an unknown slot volume is kept" not_logged "cheladb-s1_mystery"
check "the relay container is removed" logged "docker rm -f relay"
check "the relay image is removed" logged "docker image rm cheladb/hook-relay:dev"
check "slot databases are dropped" \
  bash -c 'grep -q "DROP DATABASE IF EXISTS \"cheladb_metadata_s1\"" "$1" && grep -q "DROP DATABASE IF EXISTS \"cheladb_metadata_s2\"" "$1"' _ "$tmp/log"
check "cheladb_metadata itself is never dropped" not_logged 'DROP DATABASE IF EXISTS "cheladb_metadata"'
check "the shared volumes are never touched" \
  bash -c '! grep -E "volume rm.*cheladb_(metadata|nats|valkey|zitadel|neon)" "$1"' _ "$tmp/log"
check "the stack comes back up with deps-up" grep -q "^just .*deps-up" "$tmp/log"
check ".cheladb/slots is gone" test ! -e "$main/.cheladb/slots"
check "the slot dev dirs are gone" test ! -e "$main/.cheladb/dev/s1" -a ! -e "$main/.cheladb/dev/s2"
check "the dev pids and logs stay" test -f "$main/.cheladb/dev/pids" -a -f "$main/.cheladb/dev/studio.log"
check "per-slot Studio env files are gone" test ! -e "$main/.cheladb/zitadel/studio-s1.env"
check "demo.env stays" test -f "$main/.cheladb/zitadel/demo.env"
check "secrets.key stays" test -f "$main/.cheladb/secrets/secrets.key"
check "a generated .env.local is gone" test ! -e "$main/.worktrees/a/.env.local"
check "someone's own .env.local stays" test -f "$main/.worktrees/b/.env.local"
check "the shared cargo config is gone" test ! -e "$main/.worktrees/.cargo/config.toml"
check "the shared target dir is gone" test ! -e "$main/.worktrees/target"

# --- idempotent: a second run finds nothing and still succeeds ------------------------------------
reset --yes
check "a second run exits 0" test "$status" -eq 0
check "a second run removes nothing" not_logged ' rm '
check "a second run drops nothing" not_logged 'DROP DATABASE'
check "a second run finds only the kept volume" \
  bash -c 'grep -q "keep unknown volumes \[cheladb-s1_mystery\]" "$1" && ! grep -q "remove containers \[c" "$1"' _ "$tmp/out"

# --- the main checkout can't be found: refuses, changes nothing -------------------------------------
fake_main
: >"$tmp/log"
status=0
# From an empty dir: a script that fell back to "." would find nothing real to remove.
mkdir -p "$tmp/empty"
(cd "$tmp/empty" && env -i HOME="$tmp" PATH="$tmp/nogit-bin:$bin:/usr/bin:/bin" FAKE_LOG="$tmp/log" \
  FAKE_STATE="$state" "$script" --yes >"$tmp/out" 2>"$tmp/err" </dev/null) || status=$?
check "a failing git lookup exits 2" test "$status" -eq 2
check "a failing git lookup says so" grep -q "main checkout" "$tmp/err"
check "a failing git lookup touches nothing" bash -c '! grep -q "^docker" "$1"' _ "$tmp/log"
rm -rf "$main/.git"
reset --yes
check "a main checkout without .git exits 2" test "$status" -eq 2
check "a main checkout without .git is named" grep -qF "$main" "$tmp/err"
check "a main checkout without .git keeps its files" test -d "$main/.cheladb/slots"

# --- a yes on stdin works too --------------------------------------------------------------------
fake_main
: >"$tmp/log"
status=0
echo y | env -i HOME="$tmp" PATH="$bin:/usr/bin:/bin" FAKE_LOG="$tmp/log" FAKE_STATE="$state" \
  CHELADB_MAIN="$main" "$script" >"$tmp/out" 2>&1 || status=$?
check "answering y proceeds" bash -c 'test "$1" -eq 0 && test ! -e "$2"' _ "$status" "$main/.cheladb/slots"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all passed"
