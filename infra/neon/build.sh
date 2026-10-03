#!/usr/bin/env bash
# Build Neon images from the chelabase/neon fork at the commit pinned in NEON_REF.
#   build.sh neon          storage image (pageserver, safekeeper, storage_broker,
#                          storage_controller, proxy, ...): cheladb/neon-storage
#   build.sh compute-node  compute image (Postgres 17 + neon extension + compute_ctl), run as a
#                          plain container: cheladb/neon-compute-v17
# VARIANT=dev (default): Postgres 17 only, EXTENSIONS=minimal, tags end in -dev.
# VARIANT=platform: every Postgres version, EXTENSIONS=all, no tag suffix.
# EXTENSIONS overrides the extension set for either variant.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/tags.sh
. "$here/lib/tags.sh"
ref="$neon_ref"
VARIANT="${VARIANT:-dev}"
case "$VARIANT" in
  dev)
    short="$neon_dev_tag"
    storage_args=(--build-arg PG_VERSIONS=v17)
    default_extensions=minimal
    ;;
  platform)
    short="$neon_tag"
    storage_args=()
    default_extensions=all
    ;;
  *)
    echo "VARIANT must be dev or platform, not '$VARIANT'" >&2
    exit 2
    ;;
esac
# Sources are checked out under infra/neon/.cache/ (gitignored).
src="$here/.cache/neon"
min_free_gb=40

usage() {
  echo "usage: [VARIANT=dev|platform] $0 neon|compute-node" >&2
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
    git -C "$src" remote add origin https://github.com/chelabase/neon.git
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
    # The build-tools image is tagged by the hash of its Dockerfile, as the fork's
    # publish-dev.sh and build-tools.yml do.
    build_tools_tag="$(sha256sum "$src/build-tools/Dockerfile" | cut -c1-12)"
    DOCKER_BUILDKIT=1 docker build "$src" \
      --build-arg REPOSITORY=ghcr.io/chelabase \
      --build-arg IMAGE=neon-build-tools \
      --build-arg "TAG=$build_tools_tag" \
      --build-arg GIT_VERSION="$ref" \
      --build-arg BUILD_TAG="$short" \
      "${storage_args[@]}" \
      -t "cheladb/neon-storage:$short"
    echo "built cheladb/neon-storage:$short"
    ;;
  compute-node)
    require_disk
    checkout
    DOCKER_BUILDKIT=1 docker build "$src" \
      -f "$src/compute/compute-node.Dockerfile" \
      --build-arg PG_VERSION=v17 \
      --build-arg BUILD_TAG="$short" \
      --build-arg DEBIAN_VERSION=bookworm \
      --build-arg EXTENSIONS="${EXTENSIONS:-$default_extensions}" \
      -t "cheladb/neon-compute-v17:$short"
    echo "built cheladb/neon-compute-v17:$short"
    ;;
  *) usage ;;
esac
