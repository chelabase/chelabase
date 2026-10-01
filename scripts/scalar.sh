#!/usr/bin/env bash
# The Scalar bundle the orchestrator serves at /v1/docs comes from the same
# pnpm package Studio uses (@scalar/api-reference, a root devDependency).
#   scripts/scalar.sh sync   copy the installed bundle + write VERSION
#   scripts/scalar.sh check  fail when the committed copy or the pins drift
set -euo pipefail
cd "$(dirname "$0")/.."

pkg=$(realpath node_modules/@scalar/api-reference 2>/dev/null) ||
  { echo "@scalar/api-reference is not installed: run pnpm install" >&2; exit 1; }
src="$pkg/dist/browser/standalone.js"
dest=services/orchestrator/assets/scalar
version=$(node -p "require('$pkg/package.json').version")
sum=$(sha256sum "$src" | cut -d' ' -f1)

case "${1:-}" in
sync)
  mkdir -p "$dest"
  cp "$src" "$dest/standalone.js"
  printf '@scalar/api-reference %s\nsha256 %s\n' "$version" "$sum" > "$dest/VERSION"
  echo "scalar bundle synced: @scalar/api-reference $version"
  ;;
check)
  want=$(printf '@scalar/api-reference %s\nsha256 %s\n' "$version" "$sum")
  if [[ "$(cat "$dest/VERSION" 2>/dev/null)" != "$want" ]] || ! cmp -s "$src" "$dest/standalone.js"; then
    echo "scalar bundle is stale: $dest does not match the installed @scalar/api-reference $version; run just scalar-sync and commit" >&2
    exit 1
  fi
  react_pkg=$(realpath apps/studio/node_modules/@scalar/api-reference-react)
  react_dep=$(node -p "require('$react_pkg/package.json').dependencies['@scalar/api-reference']")
  if [[ "$react_dep" != "$version" ]]; then
    echo "version mismatch: @scalar/api-reference-react depends on @scalar/api-reference $react_dep but $version is installed; align the pins" >&2
    exit 1
  fi
  ;;
*)
  echo "usage: $0 sync|check" >&2
  exit 2
  ;;
esac
