#!/usr/bin/env bash
# Chooses the Neon images for `just deps-up`. Prints NEON_IMAGE=… and COMPUTE_NODE_IMAGE=…
# on stdout for the caller to export; messages go to stderr. A local `just neon-build` wins;
# otherwise the digest-pinned GHCR default from the shared Compose file is pulled.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${COMPOSE_FILE:-$ROOT/infra/compose/shared.yml}"
here="$ROOT/infra/neon"
# shellcheck source=../infra/neon/lib/tags.sh
. "$here/lib/tags.sh"

# pinned <VAR>: the default in the Compose file's ${VAR:-…}.
pinned() {
  sed -nE "s/.*\\\$\\{$1:-([^}]+)\\}.*/\\1/p" "$COMPOSE_FILE" | head -n1
}

choose() { # choose <VAR> <image name>
  local var="$1" local_ref="cheladb/$2:$neon_tag" ref
  if docker image inspect "$local_ref" >/dev/null 2>&1; then
    echo "[neon-images] using local build $local_ref" >&2
    echo "$var=$local_ref"
    return
  fi
  ref="$(pinned "$var")"
  if [[ -z "$ref" ]]; then
    echo "[neon-images] no \${$var:-…} default in $COMPOSE_FILE" >&2
    exit 1
  fi
  if ! docker image inspect "$ref" >/dev/null 2>&1 && ! docker pull -q "$ref" >/dev/null; then
    echo "can't pull ${ref%%[:@]*} (docker's error is above). The packages are private: run docker login ghcr.io, or build locally with just neon-build (~40 min, ~40 GB free disk)." >&2
    exit 1
  fi
  echo "$var=$ref"
}

choose NEON_IMAGE neon
choose COMPUTE_NODE_IMAGE compute-node-v17
