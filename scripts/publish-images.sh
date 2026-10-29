#!/usr/bin/env bash
# Build, test and publish the release images as ghcr.io/chelabase/<image>:<sha12>: the per-app
# images and Studio (the ones self-host compose pulls by digest). The all-in-one chelabase/server
# is local only. Build once, test those exact images, push them as they are:
#
#   scripts/publish-images.sh              build and tag locally at a clean HEAD; nothing leaves
#                                          this machine
#   SELFHOST_E2E_IMAGES=local CHELABASE_IMAGE_REPO=ghcr.io/chelabase CHELABASE_IMAGE_TAG=<sha12> \
#     just selfhost-e2e                    test them
#   scripts/publish-images.sh --push-only  push the local images without rebuilding. It refuses
#                                          (before any network call) unless the tree is clean and
#                                          every image's org.opencontainers.image.revision label is
#                                          HEAD. Prints one `pin <image> <sha12> sha256:<digest>`
#                                          line per image: feed those to scripts/pin-images.sh.
#   scripts/publish-images.sh --push       build and push in one go (no test in between; not the
#                                          release path)
#
# PUBLISH_IMAGES_REPO replaces ghcr.io/chelabase (a local registry, a test prefix).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

mode=build
case "${1:-}" in
  "") ;;
  --push) mode=push ;;
  --push-only) mode=push-only ;;
  *) echo "usage: $0 [--push | --push-only]" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || { echo "usage: $0 [--push | --push-only]" >&2; exit 2; }

head_full="$(git rev-parse HEAD)"
sha="$(git rev-parse --short=12 HEAD)"
images=(gateway orchestrator worker rest auth realtime init studio)
REPO="${PUBLISH_IMAGES_REPO:-ghcr.io/chelabase}"
# RELEASE adds the OCI index annotations (infra/images/docker-bake.hcl); `--load` with them
# needs the containerd image store.
export REPO TAG="${sha}" REVISION="${head_full}" RELEASE=1
bake=(docker buildx bake -f infra/images/docker-bake.hcl)

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "publish-images: the tree has uncommitted changes; the tag ${sha} wouldn't describe the images" >&2
  exit 1
fi

print_pins() { # print_pins: pin lines from the local images' repo digests, after a push
  local i d
  for i in "${images[@]}"; do
    d="$(docker image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "${REPO}/${i}:${TAG}" |
      sed -n "s#^${REPO}/${i}@##p" | head -n1)"
    [[ "${d}" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "publish-images: no digest for ${i} after the push" >&2; exit 1; }
    echo "pin ${i} ${TAG} ${d}"
  done
}

case "${mode}" in
build)
  "${bake[@]}" --load "${images[@]}"
  echo "built (not pushed):"
  for i in "${images[@]}"; do echo "  ${REPO}/${i}:${TAG}"; done
  ;;
push-only)
  bad=0
  for i in "${images[@]}"; do
    rev="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "${REPO}/${i}:${TAG}" 2>/dev/null)" || rev="<no such local image>"
    if [[ "${rev}" != "${head_full}" ]]; then
      echo "publish-images: refusing to push: ${REPO}/${i}:${TAG} has revision '${rev}', HEAD is ${head_full}" >&2
      bad=1
    fi
  done
  if [[ "${bad}" -ne 0 ]]; then
    echo "publish-images: nothing pushed; build at this HEAD first (scripts/publish-images.sh)" >&2
    exit 1
  fi
  for i in "${images[@]}"; do docker push "${REPO}/${i}:${TAG}"; done
  echo "pushed; pins (scripts/pin-images.sh takes these lines):"
  print_pins
  ;;
push)
  meta="$(mktemp)"
  trap 'rm -f "${meta}"' EXIT
  "${bake[@]}" --push --metadata-file "${meta}" "${images[@]}"
  echo "pushed; pins (scripts/pin-images.sh takes these lines):"
  for i in "${images[@]}"; do
    echo "pin ${i} ${TAG} $(jq -r --arg i "${i}" '.[$i]["containerimage.digest"]' "${meta}")"
  done
  ;;
esac
