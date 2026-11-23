#!/usr/bin/env bash
# A slot claimed from a branch that has a worktree records that worktree's stack in
# `.workbench-stack`, whether or not the stack is registered yet: a worktree directory named
# epic-<slug> owns stack epic-<slug>. Runs the real recipes in a throwaway repo with a bare origin.
set -euo pipefail
repo="$(git rev-parse --show-toplevel)"
fail() {
  echo "wt-slot-stack: $*" >&2
  exit 1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
# pnpm and cargo are not what is tested here; a cargo call would mean a cold CLI build in a slot.
mkdir "$tmp/bin"
printf '#!/bin/sh\nexit 0\n' > "$tmp/bin/pnpm"
printf '#!/bin/sh\necho "cargo must not run here" >&2\nexit 1\n' > "$tmp/bin/cargo"
chmod +x "$tmp/bin/pnpm" "$tmp/bin/cargo"
export PATH="$tmp/bin:$PATH"

git init -q --bare -b main "$tmp/origin.git"
git init -q -b main "$tmp/work"
cp "$repo/justfile" "$tmp/work/justfile"
cd "$tmp/work"
mkdir -p .chelabase/stacks
printf '/.chelabase\n/.workbench-stack\n' > .gitignore
git remote add origin "$tmp/origin.git"
git add justfile .gitignore
git commit -qm base
git push -q origin main
git fetch -q origin

jf() { just --justfile "$tmp/work/justfile" --working-directory "$tmp/work" "$@"; }
registry() {
  local entries="" slug
  for slug in "$@"; do
    entries+="${entries:+,}{\"slug\":\"$slug\",\"checkout\":\"$tmp/work/.worktrees/$slug\",\"running_since\":null}"
  done
  printf '{"stacks":[%s]}\n' "$entries" > .chelabase/stacks/registry.json
}
# An epic as /epic makes it: a branch on origin and its worktree.
epic() {
  git push -q origin "main:refs/heads/epic/$1"
  jf wt-add "epic/$1" >/dev/null 2>&1 || fail "wt-add epic/$1 failed"
}
recorded() { tr -d '[:space:]' < "$1/.workbench-stack"; }
claim() { jf wt-claim "$@" 2>"$tmp/err" | tail -n 1; }
release() { jf wt-release "$(basename "$1")" >/dev/null 2>&1 || fail "wt-release $(basename "$1") failed: $(cat "$tmp/err")"; }

registry
epic x

# The epic's stack isn't up yet: the slot still records the stack it will use.
for ref in epic/x origin/epic/x; do
  dir="$(claim "$ref" "fix/a-${ref//\//-}")" || fail "wt-claim $ref failed: $(cat "$tmp/err")"
  [ -f "$dir/.workbench-stack" ] || fail "wt-claim $ref wrote no .workbench-stack"
  [ "$(recorded "$dir")" = epic-x ] ||
    fail "wt-claim $ref (stack not registered) recorded \"$(recorded "$dir")\", want epic-x"
  release "$dir"
done

# Registered: the same slug.
registry epic-x
dir="$(claim epic/x fix/b)" || fail "wt-claim epic/x failed: $(cat "$tmp/err")"
[ "$(recorded "$dir")" = epic-x ] || fail "wt-claim epic/x (registered) recorded \"$(recorded "$dir")\""
release "$dir"

# From main: no record, the slot gets its own stack.
dir="$(claim main fix/c)" || fail "wt-claim main failed: $(cat "$tmp/err")"
[ ! -e "$dir/.workbench-stack" ] || fail "wt-claim main left a .workbench-stack: \"$(recorded "$dir")\""
release "$dir"

# The same slot reused for another epic records the new epic's stack, not the old one's.
epic y
dir="$(claim epic/x fix/d)" || fail "wt-claim epic/x failed: $(cat "$tmp/err")"
[ "$(recorded "$dir")" = epic-x ] || fail "setup: wt-claim epic/x recorded \"$(recorded "$dir")\""
release "$dir"
again="$(claim epic/y fix/e)" || fail "wt-claim epic/y failed: $(cat "$tmp/err")"
[ "$again" = "$dir" ] || fail "setup: the slot wasn't reused ($again, $dir)"
[ "$(recorded "$again")" = epic-y ] ||
  fail "a slot reused for epic/y recorded \"$(recorded "$again")\", want epic-y"
release "$again"

# Only a directory name that already is a slug is taken as is; the slug is never recomputed here,
# so a name the CLI would change (case, `_`, `.`, over 40 characters) is left to the registry.
for b in members-and-roles-with-a-very-long-branch-name-t3 Feat_X.Y; do
  git push -q origin "main:refs/heads/$b"
  jf wt-add "$b" >/dev/null 2>&1 || fail "wt-add $b failed"
  dir="$(claim "$b" "fix/f-$b")" || fail "wt-claim $b failed: $(cat "$tmp/err")"
  [ -z "$(recorded "$dir")" ] || fail "wt-claim $b guessed stack \"$(recorded "$dir")\""
  release "$dir"
done

# A branch held by a slot: the slot's directory name says nothing about the branch's stack.
held="$(claim epic/x fix/h)" || fail "wt-claim epic/x failed: $(cat "$tmp/err")"
printf '\n' > "$held/.workbench-stack"
dir="$(claim fix/h)" || fail "wt-claim fix/h failed: $(cat "$tmp/err")"
[ "$dir" != "$held" ] || fail "setup: fix/h claimed the slot that holds it"
[ -z "$(recorded "$dir")" ] || fail "a slot based on a slot-held branch recorded \"$(recorded "$dir")\""
release "$dir"
echo "wt-slot-stack: ok"
