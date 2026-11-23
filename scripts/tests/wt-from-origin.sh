#!/usr/bin/env bash
# wt-add and wt-claim start from origin (after one fetch), not from a stale local main; offline they
# fall back to local refs with a warning. Runs the real recipes in a throwaway repo with a bare origin.
set -euo pipefail
repo="$(git rev-parse --show-toplevel)"
fail() {
  echo "wt-from-origin: $*" >&2
  exit 1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
# pnpm and cargo are not what is tested here.
mkdir "$tmp/bin"
printf '#!/bin/sh\nexit 0\n' > "$tmp/bin/pnpm"
chmod +x "$tmp/bin/pnpm"
export PATH="$tmp/bin:$PATH"

git init -q --bare -b main "$tmp/origin.git"
git init -q -b main "$tmp/work"
cp "$repo/justfile" "$tmp/work/justfile"
cd "$tmp/work"
mkdir .chelabase
printf "/.chelabase\n" > .git/info/exclude
git remote add origin "$tmp/origin.git"
git add justfile
git commit -qm base
git push -q origin main
git fetch -q origin
base="$(git rev-parse HEAD)"

# origin/main moves ahead of the local main (pushed from a second clone).
git clone -q "$tmp/origin.git" "$tmp/other"
git -C "$tmp/other" commit -q --allow-empty -m ahead
git -C "$tmp/other" push -q origin main
ahead="$(git -C "$tmp/other" rev-parse HEAD)"
[ "$(git rev-parse main)" = "$base" ] || fail "setup: local main must be behind"

jf() { just --justfile "$tmp/work/justfile" --working-directory "$tmp/work" "$@"; }

jf wt-add feat-a >/dev/null 2>&1 || fail "wt-add feat-a failed"
[ "$(git rev-parse feat-a)" = "$ahead" ] || fail "wt-add cut feat-a from a stale main"

dir="$(jf wt-claim main fix/b 2>/dev/null | tail -n 1)"
[ "$(git -C "$dir" rev-parse HEAD)" = "$ahead" ] || fail "wt-claim main started from a stale main"
jf wt-release "$(basename "$dir")" >&2 || fail "wt-release failed"

# An existing local branch is checked out as it is, with a warning when it is behind its upstream.
git branch --track old origin/main >/dev/null 2>&1
git update-ref refs/heads/old "$base"
out="$(jf wt-add old 2>&1)" || fail "wt-add of an existing branch failed"
[ "$(git rev-parse old)" = "$base" ] || fail "wt-add moved an existing branch"
[[ "$out" == *"behind"* ]] || fail "wt-add didn't warn that old is behind its upstream"
jf wt-remove old >/dev/null 2>&1 || true

# A branch only on origin is checked out from origin.
git -C "$tmp/other" push -q origin "$ahead:refs/heads/only-remote"
jf wt-add only-remote >/dev/null 2>&1 || fail "wt-add of an origin-only branch failed"
[ "$(git rev-parse only-remote)" = "$ahead" ] || fail "origin-only branch not checked out from origin"

# An explicit SHA is used as is.
dir="$(jf wt-claim "$base" 2>/dev/null | tail -n 1)"
[ "$(git -C "$dir" rev-parse HEAD)" = "$base" ] || fail "wt-claim of a SHA changed it"

# Offline: the fetch fails, the recipes warn and use local refs.
git -C "$tmp/other" commit -q --allow-empty -m ahead2
git -C "$tmp/other" push -q origin main
git remote set-url origin "$tmp/missing.git"
out="$(jf wt-add feat-off 2>&1)" || fail "wt-add must not fail offline"
[[ "$out" == *"local refs"* ]] || fail "wt-add didn't warn about the failed fetch"
[ "$(git rev-parse feat-off)" = "$base" ] || fail "offline wt-add didn't use local main"
out="$(jf wt-claim main fix/off 2>&1)" || fail "wt-claim must not fail offline"
[[ "$out" == *"local refs"* ]] || fail "wt-claim didn't warn about the failed fetch"
echo "wt-from-origin: ok"
