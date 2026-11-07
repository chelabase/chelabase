#!/usr/bin/env bash
# Write the pushed image pins into the self-host compose defaults (and the overlays that reuse them)
# and .env.example.
#
#   scripts/pin-images.sh <digests-file>
#
# The digests file has one `<image> <tag> <digest>` line per pushed image, as
# `scripts/publish-images.sh --push-only` prints it (the lines starting with `pin `, minus that
# word, or the whole output: other lines are ignored). Only those images' pins change; the rest
# stay as they are. A line for an unknown image, or a malformed `pin` line, fails before any
# file is touched. An image that isn't published yet (compose default tag `unpublished`, no
# .env.example pin) gets its first pin from a line for it, and stays as it is otherwise. Check the
# result with scripts/tests/compose-image-pins.sh <digests-file>.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
compose="$root/infra/compose/docker-compose.yml"
envex="$root/infra/compose/.env.example"
# Overlays that reuse base images (the example seed) get the same pins.
overlays=()
for f in "$root/infra/compose/docker-compose.example.yml"; do
  if [[ -f "$f" ]]; then overlays+=("$f"); fi
done
[[ $# -eq 1 && -f "$1" ]] || { echo "usage: $0 <digests-file>" >&2; exit 2; }
images=(gateway orchestrator worker rest auth realtime storage init studio identity)
declare -A ref
pin_re='^[0-9a-f]{12}$'
digest_re='^sha256:[0-9a-f]{64}$'
while read -r a b c d; do
  if [[ "$a" == pin ]]; then
    a="$b"; b="$c"; c="$d"
  elif ! [[ "$b" =~ $pin_re && "$c" =~ $digest_re ]]; then
    continue # not a pin line
  fi
  [[ " ${images[*]} " == *" $a "* ]] || { echo "pin-images: unknown image '$a' in $1" >&2; exit 1; }
  [[ "$b" =~ $pin_re && "$c" =~ $digest_re ]] || { echo "pin-images: malformed line for $a in $1" >&2; exit 1; }
  ref[$a]="$b@$c"
done <"$1"
[[ ${#ref[@]} -gt 0 ]] || { echo "pin-images: no pin lines in $1" >&2; exit 1; }

# The current .env.example pins, so the block keeps the images that aren't being repinned.
declare -A old
while read -r line; do
  [[ "$line" =~ ^#\ ghcr\.io/chelabase/([a-z]+):(.*)$ ]] && old[${BASH_REMATCH[1]}]="${BASH_REMATCH[2]}"
done < <(sed -n '/^# BEGIN image pins/,/^# END image pins/p' "$envex")
unpublished() { grep -qE "^\s*image: \\\$\{CHELABASE_IMAGE_REPO:-[^}]*\}/$1:\\\$\{CHELABASE_IMAGE_TAG:-unpublished\}\$" "$compose"; }
for i in "${images[@]}"; do
  [[ -n "${ref[$i]:-}${old[$i]:-}" ]] || unpublished "$i" ||
    { echo "pin-images: .env.example has no pin for $i" >&2; exit 1; }
done

pinned=()
for i in "${images[@]}"; do
  [[ -n "${ref[$i]:-}" ]] || continue
  pinned+=("$i")
  sed -i -E "s#^(\s*image: )\\$\{CHELABASE_IMAGE_REPO:-[^}]*\}/$i:\\$\{CHELABASE_IMAGE_TAG:-[^}]*\}\$#\1\${CHELABASE_IMAGE_REPO:-ghcr.io/chelabase}/$i:\${CHELABASE_IMAGE_TAG:-${ref[$i]}}#" "$compose" "${overlays[@]}"
done
block="$(for i in "${images[@]}"; do
  [[ -n "${ref[$i]:-}${old[$i]:-}" ]] && echo "# ghcr.io/chelabase/$i:${ref[$i]:-${old[$i]}}"
done)"
tmp="$(mktemp)"
awk -v block="$block" '
  /^# BEGIN image pins/ { print; print block; skip = 1; next }
  /^# END image pins/ { skip = 0 }
  !skip { print }' "$envex" >"$tmp"
cat "$tmp" >"$envex"
rm -f "$tmp"
echo "pinned ${pinned[*]}"
