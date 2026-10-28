#!/usr/bin/env bash
# Build and tag the release images as ghcr.io/chelabase/<image>:<git sha>: the per-app images and
# Studio (the ones self-host compose pulls by digest). The all-in-one chelabase/server is local only.
# Pushes only with --push, then prints the digests to pin. Without it, nothing leaves this machine.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

push=0
for a in "$@"; do
  case "$a" in
    --push) push=1 ;;
    *) echo "usage: $0 [--push]" >&2; exit 2 ;;
  esac
done

sha="$(git rev-parse --short=12 HEAD)"
images=(gateway orchestrator worker rest auth realtime init studio)
export REPO=ghcr.io/chelabase TAG="${sha}"
bake=(docker buildx bake -f infra/images/docker-bake.hcl)

if [[ "${push}" -eq 0 ]]; then
  "${bake[@]}" --load "${images[@]}"
  echo "built (not pushed):"
  for i in "${images[@]}"; do echo "  ${REPO}/${i}:${TAG}"; done
  exit 0
fi

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "publish-images: the tree has uncommitted changes; the tag ${sha} wouldn't describe the images" >&2
  exit 1
fi
meta="$(mktemp)"
trap 'rm -f "${meta}"' EXIT
"${bake[@]}" --push --metadata-file "${meta}" "${images[@]}"
echo "pushed; pin these digests:"
for i in "${images[@]}"; do
  d="$(jq -r --arg i "${i}" '.[$i]["containerimage.digest"]' "${meta}")"
  echo "  ${REPO}/${i}@${d}"
done
