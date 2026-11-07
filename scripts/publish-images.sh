#!/usr/bin/env bash
# Build, test and publish the release images as ghcr.io/chelabase/<image>:<sha12>: the per-app
# images (storage among them), Studio and the identity service (the ones self-host compose pulls
# by digest). The all-in-one chelabase/server is local only. Build once, test those exact images, push them as they are. Every run names its
# images: --service <image> (repeatable) takes exactly those, even unchanged ones (with a note);
# --all takes the images whose inputs changed since their pins (scripts/image-inputs.sh) and
# builds nothing when none did. With neither it refuses and prints the changed set.
#
#   scripts/publish-images.sh --all | --service <image>...
#                                          build and tag locally at a clean HEAD; nothing leaves
#                                          this machine
#   SELFHOST_E2E_IMAGES=local CHELABASE_IMAGE_REPO=ghcr.io/chelabase CHELABASE_IMAGE_TAG_<IMAGE>=<sha12> \
#     just selfhost-e2e                    test them (images without a tag stay at their pins)
#   scripts/publish-images.sh --push-only <same selection>
#                                          push the local images without rebuilding. It refuses
#                                          (before any push) unless HEAD is on origin/main, the
#                                          tree is clean and each selected image's
#                                          org.opencontainers.image.revision label is HEAD. Prints one `pin <image> <sha12>
#                                          sha256:<digest>` line per pushed image: feed those to
#                                          scripts/pin-images.sh.
#   scripts/publish-images.sh --push <selection>
#                                          build and push in one go (no test in between; not the
#                                          release path)
#
# PUBLISH_IMAGES_REPO replaces ghcr.io/chelabase (a local registry, a test prefix).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

mapfile -t all < <(scripts/image-inputs.sh images)
usage() {
  echo "usage: $0 (--all | --service <image>...) [--push | --push-only]" >&2
  echo "  images: ${all[*]}" >&2
  exit 2
}
mode=build
pick_all=""
images=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --push | --push-only) [[ "${mode}" == build ]] || usage; mode="${1#--}" ;;
    --all) pick_all=1 ;;
    --service)
      [[ $# -ge 2 ]] || usage
      [[ " ${all[*]} " == *" $2 "* ]] || { echo "publish-images: unknown image '$2'" >&2; usage; }
      [[ " ${images[*]} " == *" $2 "* ]] || images+=("$2")
      shift ;;
    *) usage ;;
  esac
  shift
done
[[ -n "${pick_all}" && ${#images[@]} -gt 0 ]] && { echo "publish-images: --all and --service exclude each other" >&2; usage; }
if [[ -z "${pick_all}" && ${#images[@]} -eq 0 ]]; then
  changed="$(scripts/image-inputs.sh changed | paste -sd ' ')"
  echo "publish-images: name the images; changed since their pins: ${changed:-none}" >&2
  usage
fi

head_full="$(git rev-parse HEAD)"
sha="$(git rev-parse --short=12 HEAD)"
REPO="${PUBLISH_IMAGES_REPO:-ghcr.io/chelabase}"
# RELEASE adds the OCI index annotations (infra/images/docker-bake.hcl); `--load` with them
# needs the containerd image store.
export REPO TAG="${sha}" REVISION="${head_full}" RELEASE=1
bake=(docker buildx bake -f infra/images/docker-bake.hcl)

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "publish-images: the tree has uncommitted changes; the tag ${sha} wouldn't describe the images" >&2
  exit 1
fi

# A public push is from a commit on origin/main. A squash merge drops a branch's commits, so
# images built on a branch would carry a revision, and get pins, that no history has: merge
# first, then build, test, push and pin from main.
if [[ "${mode}" != build && -z "${PUBLISH_IMAGES_REPO:-}" ]]; then
  git fetch -q origin main ||
    { echo "publish-images: refusing to push: can't fetch origin/main to check HEAD is on it" >&2; exit 1; }
  if ! git merge-base --is-ancestor "${head_full}" origin/main; then
    echo "publish-images: refusing to push: HEAD ${sha} is not on origin/main; merge first, then publish from main" >&2
    exit 1
  fi
fi

changed="$(scripts/image-inputs.sh changed)"
if [[ -n "${pick_all}" ]]; then
  mapfile -t images <<<"${changed}"
  [[ -n "${changed}" ]] || { echo "publish-images: nothing changed since the pins; nothing to ${mode/-only/}"; exit 0; }
  echo "publish-images: changed since their pins: ${images[*]}"
else
  for i in "${images[@]}"; do
    grep -qx "${i}" <<<"${changed}" || echo "publish-images: note: ${i} is unchanged since its pin; taking it anyway" >&2
  done
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
