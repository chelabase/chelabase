#!/usr/bin/env bash
# Checks that each release image's (the ones named, default all eight) top-level OCI index carries the
# org.opencontainers.image.{source,revision,licenses} annotations (GHCR links a package to its
# repo from them; a label alone is ignored when the tag is an index), and that revision == HEAD.
# Run after `scripts/publish-images.sh` (build mode), on the images it built. Not part of
# `just check`: it needs the images.
#
#   scripts/tests/images-annotations.sh [<image>...]
#   SOURCE="" scripts/publish-images.sh ...   (build with a missing source) -> this fails naming it
#
# Reads the index straight from the local image store: it streams each image through
# `docker save` (layers included, so it takes a while) -> index.json -> index blob, so it sees
# what `docker push` will send. Plain `just images` adds no annotations (only publish-images.sh
# sets RELEASE). REPO/TAG default to what publish-images.sh builds.
#
# Manual check of the push-only path against a scratch registry (never GHCR). Retagging keeps the
# tested index, so nothing is rebuilt:
#   docker run -d -p 5055:5000 --name scratch-registry registry:2
#   scripts/publish-images.sh --service gateway                            # build, --load
#   T=$(git rev-parse --short=12 HEAD)
#   for i in gateway; do
#     docker tag ghcr.io/chelabase/$i:$T localhost:5055/chelabase/$i:$T; done
#   PUBLISH_IMAGES_REPO=localhost:5055/chelabase scripts/publish-images.sh --push-only --service gateway
#   docker buildx imagetools inspect --raw localhost:5055/chelabase/gateway:$T | jq .annotations
#     -> the three keys; the pushed index digest (sha256sum of the raw output) equals
#        `docker image inspect -f '{{.Descriptor.digest}}' ghcr.io/chelabase/gateway:$T`, so the
#        platform manifests are the tested ones
#   docker rm -f scratch-registry
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

REPO="${REPO:-ghcr.io/chelabase}"
TAG="${TAG:-$(git rev-parse --short=12 HEAD)}"
head_full="$(git rev-parse HEAD)"
images=(gateway orchestrator worker rest auth realtime init studio)
[[ $# -eq 0 ]] || images=("$@")
want_source="https://github.com/chelabase/chelabase"
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }
ok() { echo "ok: $*"; }

# Prints the annotations of the image's top-level index as JSON (small blobs only are kept).
index_annotations() {
  docker save "$1" | python3 -I -c '
import sys, tarfile, json
blobs = {}
index = None
with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as t:
    for m in t:
        if not m.isfile(): continue
        if m.name == "index.json":
            index = json.load(t.extractfile(m))
        elif m.name.startswith("blobs/sha256/") and m.size < 1 << 20:
            blobs[m.name.rsplit("/", 1)[1]] = t.extractfile(m).read()
d = index["manifests"][0]["digest"].split(":")[1]
doc = json.loads(blobs[d])
if "manifests" not in doc:
    print("not an index: the top-level descriptor is " + doc.get("mediaType", "a single manifest"))
    sys.exit(3)
print(json.dumps(doc.get("annotations", {})))
'
}

for i in "${images[@]}"; do
  img="${REPO}/${i}:${TAG}"
  docker image inspect "${img}" >/dev/null 2>&1 || { bad "${img} isn't built"; continue; }
  if ! ann="$(index_annotations "${img}")"; then
    bad "${i}: ${ann:-could not read its index} (build with scripts/publish-images.sh on the containerd image store)"
    continue
  fi
  for key in source revision licenses; do
    val="$(jq -r --arg k "org.opencontainers.image.${key}" '.[$k] // empty' <<<"${ann}")"
    if [[ -z "${val}" ]]; then bad "${i}: index annotation org.opencontainers.image.${key} is missing"; continue; fi
    case "${key}" in
      revision) [[ "${val}" == "${head_full}" ]] && ok "${i}: revision ${val}" || bad "${i}: revision '${val}', HEAD is ${head_full}" ;;
      source) [[ "${val}" == "${want_source}" ]] && ok "${i}: source ${val}" || bad "${i}: source '${val}', want ${want_source}" ;;
      licenses) [[ "${val}" == "Apache-2.0" ]] && ok "${i}: licenses ${val}" || bad "${i}: licenses '${val}', want Apache-2.0" ;;
    esac
  done
done

[[ "${fail}" -eq 0 ]] && echo "images-annotations: ok"
exit "${fail}"
