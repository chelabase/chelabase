#!/usr/bin/env bash
# #126: the retired identity provider is gone from the live tree. Nothing tracked names it, except
# history (applied migrations, which are never edited, the CHANGELOG, docs/roadmap and
# docs/superpowers) and lines that name this guard itself (the justfile runs it). The word is built from parts so this file doesn't name it.
#
# TEMPORARY (m14 removes this list): the docs site, the e2e scripts with their shared libraries,
# and THIRD_PARTY_NOTICES are cleaned in #127 (m14).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
word="zit""adel"
mapfile -t files < <(git ls-files -- . \
  ':!crates/chelabase-metadata/migrations/**' ':!CHANGELOG.md' ':!docs/roadmap/**' \
  ':!docs/superpowers/**' \
  ':!apps/docs/**' ':!scripts/*-e2e.sh' ':!scripts/lib/**' ':!THIRD_PARTY_NOTICES*')
# Regular files only (a tracked symlink to a directory isn't one). grep's own status is ignored: a
# failure to read a file shows on stderr, and only the filtered hits decide.
mapfile -t files < <(for f in "${files[@]}"; do [[ -f $f && ! -L $f ]] && echo "$f"; done)
hits="$(grep -nIi -- "$word" "${files[@]}" | grep -v "no-${word}\.sh" || true)"
if [[ -n $hits ]]; then
  echo "no-${word}: the retired identity provider is still named here:" >&2
  echo "$hits" >&2
  echo "no-${word}: $(printf '%s\n' "$hits" | wc -l) line(s)" >&2
  exit 1
fi
echo "no-${word}: ok"
