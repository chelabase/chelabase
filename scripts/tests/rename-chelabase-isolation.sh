#!/usr/bin/env bash
# RT1 final fix round (process failure, T1 coder 2026-10-16): scripts/tests/rename-chelabase.sh
# run with GIT_DIR/GIT_WORK_TREE set (as a worktree tool or a hook does) re-initialised the
# repository those name, writing core.worktree into its shared config. The harness must ignore
# every inherited GIT_* variable and only ever touch its own throwaway repositories.
#
# This runs the harness with GIT_DIR/GIT_WORK_TREE pointing at a throwaway sentinel clone and
# checks the sentinel (its config, its HEAD, its files) is exactly as before.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
FAILS=0
fail() { echo "  FAIL: $*"; FAILS=$((FAILS + 1)); }

origin="$ROOT/origin"
mkdir -p "$origin"
git -C "$origin" init -q
printf 'cheladb sentinel\n' >"$origin/a.txt"
git -C "$origin" add -A
git -C "$origin" -c user.name=t -c user.email=t@t commit -qm sentinel
sentinel="$ROOT/sentinel"
git clone -q "$origin" "$sentinel"
snapshot() {
  git -C "$sentinel" config --file "$sentinel/.git/config" --list | sort
  git -C "$sentinel" rev-parse HEAD
  git -C "$sentinel" status --porcelain
  sha256sum "$sentinel/a.txt"
}
before="$(snapshot)"

echo "== the harness ignores an inherited GIT_DIR/GIT_WORK_TREE"
GIT_DIR="$sentinel/.git" GIT_WORK_TREE="$sentinel" bash "$HERE/rename-chelabase.sh" >"$ROOT/out" 2>&1 ||
  fail "the harness failed with GIT_DIR set: $(tail -n 20 "$ROOT/out")"
after="$(snapshot)"
[ "$after" = "$before" ] || fail "the sentinel repository changed:
--- before
$before
--- after
$after"
git -C "$sentinel" config --file "$sentinel/.git/config" --get core.worktree >/dev/null &&
  fail "core.worktree was written into the sentinel's config"

[ "$FAILS" -eq 0 ] && echo "  ok" && echo "all passed" || { echo "$FAILS failure(s)"; exit 1; }
