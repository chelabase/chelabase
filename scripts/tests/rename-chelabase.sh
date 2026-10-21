#!/usr/bin/env bash
# Offline tests for scripts/rename-chelabase.sh (the Cheladb -> Chelabase sweep).
# Each case builds a throwaway git repo and runs the script there.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../rename-chelabase.sh"
# Only the throwaway fixtures below are ever touched (RT1 final fix round, process failure:
# an inherited GIT_DIR/GIT_WORK_TREE made `git init` re-initialise the caller's repository and
# write core.worktree into its shared config). Every repository-local GIT_* variable goes, no
# user or system config applies, and discovery stops at the fixtures' root
# (scripts/tests/rename-chelabase-isolation.sh checks this).
# shellcheck disable=SC2046 # one variable name per word
unset $(git rev-parse --local-env-vars)
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
ROOT="$(mktemp -d)"
export GIT_CEILING_DIRECTORIES="$ROOT"
FAILS=0
trap 'rm -rf "$ROOT"' EXIT

fail() { echo "  FAIL: $*"; FAILS=$((FAILS + 1)); }
run_case() {
  echo "== $1"
  local before=$FAILS
  "$1"
  [ "$FAILS" -eq "$before" ] && echo "  ok"
}
fixture() { # fixture <name>: new git repo, cwd set to it
  FX="$ROOT/$1"
  mkdir -p "$FX"
  cd "$FX" || exit 1
  git init -q .
}
put() { # put <path> <content>
  mkdir -p "$(dirname "$1")"
  printf '%s\n' "$2" > "$1"
}
commit_all() {
  git add -A
  git -c user.name=t -c user.email=t@t commit -qm fixture
}
run_script() { bash "$SCRIPT" "$@" >"$ROOT/out" 2>&1; }
tree_hash() { find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 sha256sum; }

every_casing_is_mapped() {
  fixture casing
  put a.txt 'ChelaDB
CHELADB_MODE
Cheladb-User
cheladb_admin
@cheladb/client
cheladb.user_id'
  commit_all
  run_script || fail "script failed: $(cat "$ROOT/out")"
  local want='Chelabase
CHELABASE_MODE
Chelabase-User
chelabase_admin
@chelabase/client
chelabase.user_id'
  [ "$(cat a.txt)" = "$want" ] || fail "got: $(cat a.txt)"
}

keep_list_survives() {
  fixture keep
  put k.txt 'chelabase/cheladb-v1
<repo root>
github.com/ChelaDB
mixed cheladb chelabase/cheladb-v1 ChelaDB github.com/ChelaDB/x <repo root>/y'
  commit_all
  run_script || fail "script failed"
  local want='chelabase/cheladb-v1
<repo root>
github.com/ChelaDB
mixed chelabase chelabase/cheladb-v1 Chelabase github.com/ChelaDB/x <repo root>/y'
  [ "$(cat k.txt)" = "$want" ] || fail "got: $(cat k.txt)"
}

excluded_trees_untouched() {
  fixture excl
  local f
  for f in docs/superpowers/specs/a.md docs/superpowers/plans/b.md docs/roadmap/c.md openapi/d.json \
    packages/management-client/src/generated/e.ts apps/studio/src/lib/login-client/generated/f.ts \
    Cargo.lock pnpm-lock.yaml; do
    put "$f" 'cheladb ChelaDB CHELADB'
  done
  put keep.txt 'x'
  commit_all
  local before; before="$(tree_hash)"
  run_script || fail "script failed"
  [ "$(tree_hash)" = "$before" ] || fail "excluded files changed"
}

paths_are_moved() {
  fixture paths
  put crates/cheladb-x/src/lib.rs 'pub fn cheladb() {}'
  put cheladb.toml 'name = "cheladb"'
  commit_all
  run_script || fail "script failed"
  [ -f crates/chelabase-x/src/lib.rs ] || fail "lib.rs not moved"
  [ -f chelabase.toml ] || fail "toml not moved"
  [ ! -e crates/cheladb-x ] || fail "old dir remains"
  [ ! -e cheladb.toml ] || fail "old toml remains"
  [ "$(cat crates/chelabase-x/src/lib.rs)" = 'pub fn chelabase() {}' ] || fail "content not rewritten"
  git status --short | grep -q '^R.*crates/cheladb-x/src/lib.rs -> crates/chelabase-x/src/lib.rs' || fail "no git rename: $(git status --short)"
  git status --short | grep -q '^R.*cheladb.toml -> chelabase.toml' || fail "no git rename for toml"
}

binary_file_untouched() {
  fixture bin
  printf 'cheladb\0cheladb ChelaDB\n' > blob.bin
  put t.txt 'cheladb'
  commit_all
  local want; want="$(sha256sum blob.bin)"
  run_script || fail "script failed"
  [ "$(sha256sum blob.bin)" = "$want" ] || fail "binary changed"
  [ "$(cat t.txt)" = chelabase ] || fail "text not rewritten"
}

second_run_is_a_no_op() {
  fixture twice
  put crates/cheladb-x/lib.rs 'cheladb chelabase/cheladb-v1'
  put cheladb.toml 'ChelaDB'
  commit_all
  run_script || fail "first run failed"
  local status hashes
  status="$(git status --porcelain)"; hashes="$(tree_hash)"
  run_script || fail "second run failed"
  [ "$(git status --porcelain)" = "$status" ] || fail "status changed"
  [ "$(tree_hash)" = "$hashes" ] || fail "hashes changed"
}

dry_run_changes_nothing() {
  fixture dry
  put crates/cheladb-x/lib.rs 'cheladb'
  put docs/n.md 'ChelaDB'
  commit_all
  local before status; before="$(tree_hash)"; status="$(git status --porcelain)"
  run_script --dry-run || fail "dry run failed"
  [ "$(tree_hash)" = "$before" ] || fail "tree changed"
  [ "$(git status --porcelain)" = "$status" ] || fail "status changed"
  grep -q 'docs/n.md' "$ROOT/out" || fail "file not listed"
  grep -q 'crates/chelabase-x/lib.rs' "$ROOT/out" || fail "move not listed"
}

run_case every_casing_is_mapped
run_case keep_list_survives
run_case excluded_trees_untouched
run_case paths_are_moved
run_case binary_file_untouched
run_case second_run_is_a_no_op
run_case dry_run_changes_nothing

[ "$FAILS" -eq 0 ] && echo "all passed" || { echo "$FAILS failure(s)"; exit 1; }
