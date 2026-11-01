#!/usr/bin/env bash
# Offline check (epic selfhost-install m11, AC 4): every app image the self-host install pulls is
# pinned as ghcr.io/chelabase/<image>:<12 hex>@sha256:<64 hex>, in the base compose file (the
# defaults of CHELABASE_IMAGE_REPO / CHELABASE_IMAGE_TAG on each image line) and in the pin block
# of .env.example. With a digests file (`pin <image> <tag> <digest>` per line, as
# `publish-images.sh --push-only` prints them; the `pin ` word is optional), the pin of each image
# it names must equal the pushed tag and digest; images it doesn't name keep their pins. A line
# for an unknown image fails.
#
#   scripts/tests/compose-image-pins.sh [digests-file]
#
# Part of `just check` (offline, with no digests file).
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
compose="$root/infra/compose/docker-compose.yml"
envex="$root/infra/compose/.env.example"
overlays=()
for f in "$root/infra/compose/docker-compose.example.yml"; do
  if [[ -f "$f" ]]; then overlays+=("$f"); fi
done
digests="${1:-}"
images=(gateway orchestrator worker rest auth realtime init studio)
re='^ghcr\.io/chelabase/([a-z]+):([0-9a-f]{12})@sha256:([0-9a-f]{64})$'
fail=0
err() { echo "compose-image-pins: $*" >&2; fail=1; }

declare -A seen_compose seen_env

# Base file: image: ${CHELABASE_IMAGE_REPO:-<repo>}/<image>:${CHELABASE_IMAGE_TAG:-<tag>}
while IFS= read -r line; do
  if [[ "$line" =~ image:\ \$\{CHELABASE_IMAGE_REPO:-([^}]*)\}/([a-z]+):\$\{CHELABASE_IMAGE_TAG:-([^}]*)\}$ ]]; then
    ref="${BASH_REMATCH[1]}/${BASH_REMATCH[2]}:${BASH_REMATCH[3]}"
    name="${BASH_REMATCH[2]}"
    seen_compose[$name]="$ref"
    [[ "$ref" =~ $re ]] || err "docker-compose.yml: $name default is not pinned: $ref"
  fi
done <"$compose"

# .env.example: the lines between the markers, each "# <ref>"
in_block=0
while IFS= read -r line; do
  case "$line" in
    "# BEGIN image pins"*) in_block=1 ;;
    "# END image pins"*) in_block=0 ;;
    *) if [[ $in_block -eq 1 && "$line" =~ ^#\ (ghcr\.io/chelabase/([a-z]+):[^[:space:]]*)$ ]]; then
         ref="${BASH_REMATCH[1]}"
         seen_env[${BASH_REMATCH[2]}]="$ref"
         [[ "$ref" =~ $re ]] || err ".env.example: ${BASH_REMATCH[2]} is not pinned: $ref"
       fi ;;
  esac
done <"$envex"

# Overlays (the example seed) reuse base images: each of their image lines is the base file's pin.
for overlay in "${overlays[@]}"; do
  while IFS= read -r line; do
    if [[ "$line" =~ image:\ \$\{CHELABASE_IMAGE_REPO:-([^}]*)\}/([a-z]+):\$\{CHELABASE_IMAGE_TAG:-([^}]*)\}$ ]]; then
      ref="${BASH_REMATCH[1]}/${BASH_REMATCH[2]}:${BASH_REMATCH[3]}"
      [[ "$ref" == "${seen_compose[${BASH_REMATCH[2]}]:-}" ]] ||
        err "${overlay##*/}: ${BASH_REMATCH[2]} is $ref, not the base pin ${seen_compose[${BASH_REMATCH[2]}]:-none}"
    fi
  done <"$overlay"
done

for i in "${images[@]}"; do
  [[ -n "${seen_compose[$i]:-}" ]] || err "docker-compose.yml: no image line for $i"
  [[ -n "${seen_env[$i]:-}" ]] || err ".env.example: no pin for $i"
  if [[ -n "${seen_compose[$i]:-}" && -n "${seen_env[$i]:-}" && "${seen_compose[$i]}" != "${seen_env[$i]}" ]]; then
    err "$i: compose and .env.example differ (${seen_compose[$i]} vs ${seen_env[$i]})"
  fi
done

if [[ -n "$digests" ]]; then
  [[ -f "$digests" ]] || { echo "compose-image-pins: no digests file $digests" >&2; exit 2; }
  declare -A want
  while read -r i t d x; do
    [[ "$i" == pin ]] && { i="$t"; t="$d"; d="$x"; }
    [[ "$t" =~ ^[0-9a-f]{12}$ && "$d" =~ ^sha256:[0-9a-f]{64}$ ]] || continue
    if [[ " ${images[*]} " == *" $i "* ]]; then want[$i]="ghcr.io/chelabase/$i:$t@$d"; else err "$digests: unknown image $i"; fi
  done <"$digests"
  [[ ${#want[@]} -gt 0 ]] || err "$digests has no pin lines"
  for i in "${!want[@]}"; do
    [[ "${seen_compose[$i]:-}" == "${want[$i]}" ]] || err "$i: compose pin ${seen_compose[$i]:-none} != pushed ${want[$i]}"
    [[ "${seen_env[$i]:-}" == "${want[$i]}" ]] || err "$i: .env.example pin ${seen_env[$i]:-none} != pushed ${want[$i]}"
  done
fi

[[ $fail -eq 0 ]] && echo "compose-image-pins: ok"
exit $fail
