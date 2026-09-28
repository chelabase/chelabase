#!/usr/bin/env bash
# Build Neon images from source at the commit pinned in NEON_REF.
#   build.sh neon          storage image (pageserver, safekeeper, storage_broker,
#                          storage_controller, proxy, ...)
#   build.sh compute-node  compute image (Postgres 17 + neon extension + compute_ctl), run as a
#                          plain container; EXTENSIONS=minimal by default
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/tags.sh
. "$here/lib/tags.sh"
ref="$neon_ref"
short="$neon_tag"
# Sources are checked out under infra/neon/.cache/ (gitignored).
src="$here/.cache/neon"
min_free_gb=40

usage() {
  echo "usage: $0 neon|compute-node" >&2
  exit 2
}

require_disk() {
  free_gb=$(df --output=avail -BG "$here" | tail -1 | tr -dc '0-9')
  if [ "$free_gb" -lt "$min_free_gb" ]; then
    echo "need at least ${min_free_gb}GB free disk, have ${free_gb}GB" >&2
    exit 1
  fi
}

checkout() {
  if [ ! -d "$src/.git" ]; then
    mkdir -p "$src"
    git -C "$src" init -q
    git -C "$src" remote add origin https://github.com/neondatabase/neon.git
  fi
  if [ "$(git -C "$src" rev-parse HEAD 2>/dev/null || true)" != "$ref" ]; then
    git -C "$src" fetch --depth 1 origin "$ref"
    git -C "$src" checkout -q --force "$ref"
  fi
  git -C "$src" submodule update --init --depth 1 \
    vendor/postgres-v14 vendor/postgres-v15 vendor/postgres-v16 vendor/postgres-v17
}

case "${1:-}" in
  neon)
    require_disk
    checkout
    DOCKER_BUILDKIT=1 docker build "$src" \
      --build-arg GIT_VERSION="$ref" \
      --build-arg BUILD_TAG="$short" \
      -t "cheladb/neon:$short"
    echo "built cheladb/neon:$short"
    ;;
  compute-node)
    require_disk
    checkout
    DOCKER_BUILDKIT=1 docker build "$src" \
      -f "$src/compute/compute-node.Dockerfile" \
      --build-arg PG_VERSION=v17 \
      --build-arg BUILD_TAG="$short" \
      --build-arg EXTENSIONS="${EXTENSIONS:-minimal}" \
      -t "cheladb/compute-node-v17:$short"
    echo "built cheladb/compute-node-v17:$short"
    ;;
  *) usage ;;
esac
