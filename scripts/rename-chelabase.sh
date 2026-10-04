#!/usr/bin/env bash
# One-shot, idempotent product rename sweep: Cheladb -> Chelabase.
# Run from a repo root: rewrites tracked text files, then `git mv`s tracked paths. Never commits.
#   scripts/rename-chelabase.sh [--dry-run]
set -euo pipefail

DRY=0
case "${1:-}" in
  "") ;;
  --dry-run) DRY=1 ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

# Keep list (masked first): the archived repo, this checkout's path, the GitHub org name.
MAP='s/cheladb-v1/\x01A\x01/g; s/cheladb-scaffold/\x01B\x01/g; s{github\.com/ChelaDB}{\x01C\x01}g;
     s/ChelaDB/Chelabase/g; s/CHELADB/CHELABASE/g; s/Cheladb/Chelabase/g; s/cheladb/chelabase/g;
     s/\x01A\x01/cheladb-v1/g; s/\x01B\x01/cheladb-scaffold/g; s/\x01C\x01/github.com\/ChelaDB/g'

excluded() { # excluded <path>
  case "$1" in
    docs/superpowers/* | docs/roadmap/* | openapi/* | \
    packages/management-client/src/generated/* | apps/studio/src/lib/login-client/generated/* | \
    Cargo.lock | pnpm-lock.yaml | \
    scripts/rename-chelabase.sh | scripts/tests/rename-chelabase.sh) return 0 ;;
  esac
  return 1
}

git rev-parse --show-toplevel >/dev/null
cd "$(git rev-parse --show-toplevel)"

mapfile -d '' ALL < <(git ls-files -z)

# 1. contents
CHANGE=()
for f in "${ALL[@]}"; do
  excluded "$f" && continue
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  grep -Iq . -- "$f" 2>/dev/null || continue # binary or empty
  grep -qi cheladb -- "$f" || continue
  CHANGE+=("$f")
done

for f in "${CHANGE[@]}"; do
  if [ "$DRY" -eq 1 ]; then
    echo "rewrite: $f"
  else
    perl -pi -e "$MAP" -- "$f"
  fi
done

# 2. paths
for f in "${ALL[@]}"; do
  excluded "$f" && continue
  case "$f" in *[Cc][Hh][Ee][Ll][Aa][Dd][Bb]*) ;; *) continue ;; esac
  new="$(printf '%s' "$f" | perl -pe "$MAP")"
  [ "$new" != "$f" ] || continue
  if [ "$DRY" -eq 1 ]; then
    echo "move: $f -> $new"
  else
    mkdir -p -- "$(dirname -- "$new")"
    git mv -- "$f" "$new"
    rmdir -p -- "$(dirname -- "$f")" 2>/dev/null || true # drop emptied old dirs
  fi
done
