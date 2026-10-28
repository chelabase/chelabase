#!/usr/bin/env bash
# Write the pushed image pins into the self-host compose defaults and .env.example.
#
#   scripts/pin-images.sh <digests-file>
#
# The digests file has one `<image> <tag> <digest>` line per image, as
# `scripts/publish-images.sh --push-only` prints it (the lines starting with `pin `, minus that
# word, or the whole output: other lines are ignored). Check the result with
# scripts/tests/compose-image-pins.sh <digests-file>.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
compose="$root/infra/compose/docker-compose.yml"
envex="$root/infra/compose/.env.example"
[[ $# -eq 1 && -f "$1" ]] || { echo "usage: $0 <digests-file>" >&2; exit 2; }
images=(gateway orchestrator worker rest auth realtime init studio)
declare -A ref
while read -r a b c d; do
  [[ "$a" == pin ]] && { a="$b"; b="$c"; c="$d"; }
  if [[ " ${images[*]} " == *" $a "* && "$b" =~ ^[0-9a-f]{12}$ && "$c" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    ref[$a]="$b@$c"
  fi
done <"$1"
for i in "${images[@]}"; do [[ -n "${ref[$i]:-}" ]] || { echo "pin-images: no valid line for $i in $1" >&2; exit 1; }; done

for i in "${images[@]}"; do
  sed -i -E "s#^(\s*image: )\\$\{CHELABASE_IMAGE_REPO:-[^}]*\}/$i:\\$\{CHELABASE_IMAGE_TAG:-[^}]*\}\$#\1\${CHELABASE_IMAGE_REPO:-ghcr.io/chelabase}/$i:\${CHELABASE_IMAGE_TAG:-${ref[$i]}}#" "$compose"
done
block="$(for i in "${images[@]}"; do echo "# ghcr.io/chelabase/$i:${ref[$i]}"; done)"
tmp="$(mktemp)"
awk -v block="$block" '
  /^# BEGIN image pins/ { print; print block; skip = 1; next }
  /^# END image pins/ { skip = 0 }
  !skip { print }' "$envex" >"$tmp"
cat "$tmp" >"$envex"
rm -f "$tmp"
echo "pinned ${#images[@]} images"
