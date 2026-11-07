#!/usr/bin/env bash
# Offline check (epic selfhost-install m11, AC 4): every app image the self-host install pulls is
# pinned as ghcr.io/chelabase/<image>:<12 hex>@sha256:<64 hex>, in the base compose file (the
# defaults of CHELABASE_IMAGE_REPO / CHELABASE_IMAGE_TAG on each image line) and in the pin block
# of .env.example. With a digests file (`pin <image> <tag> <digest>` per line, as
# `publish-images.sh --push-only` prints them; the `pin ` word is optional), the pin of each image
# it names must equal the pushed tag and digest; images it doesn't name keep their pins. A line
# for an unknown image fails.
#
# An image in `unpublished` below (one added before its first release) may instead have the
# default tag `unpublished` and no .env.example pin, never one without the other; its first
# `pin-images.sh` run pins it like the rest, and that pin PR drops it from the list.
#
#   scripts/tests/compose-image-pins.sh [digests-file]
#
# It also renders the base file with docker-compose.build.yml (the self-hosted build from a
# clone) and checks that every service the base file runs from a ghcr.io/chelabase image gets a
# local chelabase/*:local image there, built or reused: a service the overlay misses would pull
# a release image instead of building the clone.
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
images=(gateway orchestrator worker rest auth realtime storage init studio identity functions functions-runtime service-runner)
# Edge functions (m4b): the functions service and its runtime, until their first pin.
unpublished=(functions functions-runtime service-runner)
re='^ghcr\.io/chelabase/([a-z-]+):([0-9a-f]{12})@sha256:([0-9a-f]{64})$'
fail=0
err() { echo "compose-image-pins: $*" >&2; fail=1; }

declare -A seen_compose seen_env

# Base file: image: ${CHELABASE_IMAGE_REPO:-<repo>}/<image>:${CHELABASE_IMAGE_TAG:-<tag>}
while IFS= read -r line; do
  if [[ "$line" =~ image:\ \$\{CHELABASE_IMAGE_REPO:-([^}]*)\}/([a-z-]+):\$\{CHELABASE_IMAGE_TAG:-([^}]*)\}$ ]]; then
    ref="${BASH_REMATCH[1]}/${BASH_REMATCH[2]}:${BASH_REMATCH[3]}"
    name="${BASH_REMATCH[2]}"
    seen_compose[$name]="$ref"
    if [[ "${BASH_REMATCH[3]}" == unpublished && " ${unpublished[*]} " == *" $name "* ]]; then
      seen_compose[$name]=unpublished
    else
      [[ "$ref" =~ $re ]] || err "docker-compose.yml: $name default is not pinned: $ref"
    fi
  fi
done <"$compose"

# .env.example: the lines between the markers, each "# <ref>"
in_block=0
while IFS= read -r line; do
  case "$line" in
    "# BEGIN image pins"*) in_block=1 ;;
    "# END image pins"*) in_block=0 ;;
    *) if [[ $in_block -eq 1 && "$line" =~ ^#\ (ghcr\.io/chelabase/([a-z-]+):[^[:space:]]*)$ ]]; then
         ref="${BASH_REMATCH[1]}"
         seen_env[${BASH_REMATCH[2]}]="$ref"
         [[ "$ref" =~ $re ]] || err ".env.example: ${BASH_REMATCH[2]} is not pinned: $ref"
       fi ;;
  esac
done <"$envex"

# Overlays (the example seed) reuse base images: each of their image lines is the base file's pin.
for overlay in "${overlays[@]}"; do
  while IFS= read -r line; do
    if [[ "$line" =~ image:\ \$\{CHELABASE_IMAGE_REPO:-([^}]*)\}/([a-z-]+):\$\{CHELABASE_IMAGE_TAG:-([^}]*)\}$ ]]; then
      ref="${BASH_REMATCH[1]}/${BASH_REMATCH[2]}:${BASH_REMATCH[3]}"
      [[ "$ref" == "${seen_compose[${BASH_REMATCH[2]}]:-}" ]] ||
        err "${overlay##*/}: ${BASH_REMATCH[2]} is $ref, not the base pin ${seen_compose[${BASH_REMATCH[2]}]:-none}"
    fi
  done <"$overlay"
done

for i in "${images[@]}"; do
  [[ -n "${seen_compose[$i]:-}" ]] || err "docker-compose.yml: no image line for $i"
  if [[ "${seen_compose[$i]:-}" == unpublished ]]; then
    [[ -z "${seen_env[$i]:-}" ]] || err "$i: compose has no pin (unpublished) but .env.example pins ${seen_env[$i]}"
    continue
  fi
  [[ -n "${seen_env[$i]:-}" ]] || err ".env.example: no pin for $i"
  if [[ -n "${seen_compose[$i]:-}" && -n "${seen_env[$i]:-}" && "${seen_compose[$i]}" != "${seen_env[$i]}" ]]; then
    err "$i: compose and .env.example differ (${seen_compose[$i]} vs ${seen_env[$i]})"
  fi
done

# The build overlay: no app service is left on a release image (the Neon images are pinned
# release images in both).
build="$root/infra/compose/docker-compose.build.yml"
render_images() { # render_images <-f args…>: "service image built" per line
  env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE -u CHELABASE_IMAGE_REPO -u CHELABASE_IMAGE_TAG \
    docker compose -p chelabase-image-pins \
    --project-directory "$root/infra/compose" "$@" --profile services --env-file "$envex" config --format json |
    jq -r '.services | to_entries[]
      | "\(.key) \(.value.image // "-") \(.value.build != null and .value.pull_policy == "build")"'
}
if base_images="$(render_images -f "$compose")" && built_images="$(render_images -f "$compose" -f "$build")"; then
  while read -r svc img _; do
    [[ "$img" == ghcr.io/chelabase/* && "$img" != ghcr.io/chelabase/neon-* ]] || continue
    got="$(awk -v s="$svc" '$1 == s { print $2 }' <<<"$built_images")"
    [[ "$got" =~ ^chelabase/[a-z-]+:local$ ]] ||
      err "docker-compose.build.yml: $svc runs $got, not a local build (base: $img)"
  done <<<"$base_images"
  # Each local image is built by a service of the overlay (the others reuse it).
  for img in $(awk '$2 ~ /^chelabase\/[a-z-]+:local$/ { print $2 }' <<<"$built_images" | sort -u); do
    awk -v i="$img" '$2 == i && $3 == "true"' <<<"$built_images" | grep -q . ||
      err "docker-compose.build.yml: nothing builds $img"
  done
else
  err "rendering the base file with docker-compose.build.yml failed"
fi

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
