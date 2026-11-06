#!/usr/bin/env bash
# The inputs of each release image, derived from the build graphs, and which images changed
# since their pins (the 12-hex commit in each pin of infra/compose/docker-compose.yml).
#
#   scripts/image-inputs.sh images                  the nine release images
#   scripts/image-inputs.sh inputs <image>          its input paths, one per line
#   scripts/image-inputs.sh changed [<image>...]    those (default: all) whose inputs differ
#                                                   between their pin commit and HEAD
#   scripts/image-inputs.sh copies [<dockerfile> <image>...]
#                                                   fail when a COPY brings in a path disjoint
#                                                   from the images' inputs (default: the real
#                                                   Dockerfiles; part of `just check`)
#
# Inputs:
#   every image   infra/images/{docker-bake.hcl,server.Dockerfile} and .dockerignore. Not the
#                 release scripts: they select, push and pin images, and publish-images.sh reaches
#                 the build only through bake variables whose effect lives in docker-bake.hcl
#   Rust images   the service's closure of workspace path crates (normal and build deps, from
#                 `cargo metadata`), the manifest of every crate any Rust image builds (one cargo
#                 build unifies features across them), Cargo.toml, Cargo.lock, rust-toolchain*,
#                 .cargo/
#   studio        apps/studio and its workspace: dependencies (pnpm-workspace.yaml globs),
#                 package.json, pnpm-lock.yaml, pnpm-workspace.yaml, studio.Dockerfile, and what
#                 the build reads outside the graph: studio.Caddyfile, openapi/,
#                 infra/compose/.env.example (its pin block left out: pinning rewrites it)
#   identity      services/identity and its workspace: dependencies, package.json, pnpm-lock.yaml,
#                 pnpm-workspace.yaml, identity.Dockerfile
# A pin commit missing from history counts as changed, with a warning.
set -euo pipefail
shopt -s inherit_errexit
root="$(cd "$(dirname "$0")/.." && pwd)"
images=(gateway orchestrator worker rest auth realtime init studio identity)
rust=(gateway orchestrator worker rest auth realtime init)
envex=infra/compose/.env.example
common=(infra/images/docker-bake.hcl infra/images/server.Dockerfile .dockerignore)

die() { echo "image-inputs: $*" >&2; exit 2; }
known() { [[ " ${images[*]} " == *" $1 "* ]]; }

meta="" # cargo metadata, read once by the main part
rust_dirs() { # rust_dirs <package>...: the workspace crate dirs those packages build with
  jq -r --arg root "$root/" --args '
    (.packages | map({key: .name, value: .}) | from_entries) as $pk
    | def deps($n): [$pk[$n].dependencies[] | select(.path != null and (.kind == null or .kind == "build")) | .name];
      def close($seen; $todo):
        if ($todo | length) == 0 then $seen
        elif ($seen | index($todo[0])) then close($seen; $todo[1:])
        else close($seen + [$todo[0]]; $todo[1:] + deps($todo[0])) end;
    close([]; $ARGS.positional)[] | $pk[.].manifest_path | ltrimstr($root) | rtrimstr("/Cargo.toml")' "$@" <<<"$meta"
}

pnpm_dirs() { # pnpm_dirs <dir>: <dir> and the workspace packages it depends on, transitively
  local -A dir_of
  local g d n deps todo=("$1") seen=" "
  while read -r g; do
    for d in $root/$g; do [[ -f "$d/package.json" ]] && dir_of[$(jq -r .name "$d/package.json")]="${d#"$root/"}"; done
  done < <(sed -nE '/^packages:/,/^[^ ]/ s/^\s+-\s+["'\'']?([^"'\'']+)["'\'']?\s*$/\1/p' "$root/pnpm-workspace.yaml")
  while [[ ${#todo[@]} -gt 0 ]]; do
    d="${todo[0]}"
    todo=("${todo[@]:1}")
    [[ "$seen" == *" $d "* ]] && continue
    seen+="$d "
    echo "$d"
    deps="$(jq -r '[.dependencies, .devDependencies, .optionalDependencies, .peerDependencies]
      | map(select(. != null) | to_entries[] | select(.value | startswith("workspace:")) | .key) | .[]' "$root/$d/package.json")"
    for n in $deps; do
      [[ -n "${dir_of[$n]:-}" ]] || die "$d: workspace dependency $n is not in the workspace"
      todo+=("${dir_of[$n]}")
    done
  done
}

inputs() { # inputs <image>
  printf '%s\n' "${common[@]}"
  case "$1" in
    studio)
      pnpm_dirs apps/studio
      printf '%s\n' package.json pnpm-lock.yaml pnpm-workspace.yaml infra/images/studio.Dockerfile \
        infra/images/studio.Caddyfile openapi "$envex" ;;
    identity)
      pnpm_dirs services/identity
      printf '%s\n' package.json pnpm-lock.yaml pnpm-workspace.yaml infra/images/identity.Dockerfile ;;
    *)
      rust_dirs "$1"
      rust_dirs "${rust[@]}" | sed 's#$#/Cargo.toml#'
      printf '%s\n' Cargo.toml Cargo.lock rust-toolchain rust-toolchain.toml .cargo ;;
  esac
}

pin_of() { # pin_of <image>: the commit in its compose pin
  sed -nE "s#^\s*image: \\$\{CHELABASE_IMAGE_REPO:-[^}]*\}/$1:\\$\{CHELABASE_IMAGE_TAG:-([0-9a-f]{12})@.*#\1#p" \
    "$root/infra/compose/docker-compose.yml" | head -n1
}

no_pins() { git -C "$root" show "$1:$envex" 2>/dev/null | sed '/^# BEGIN image pins/,/^# END image pins/d'; }

differs() { # differs <pin> <inputs>: true when the inputs differ between <pin> and HEAD
  local p paths=() env=""
  while read -r p; do [[ "$p" == "$envex" ]] && env=1 || paths+=("$p"); done <<<"$2"
  git -C "$root" diff --quiet "$1" HEAD -- "${paths[@]}" || return 0
  [[ -n "$env" ]] && ! cmp -s <(no_pins "$1") <(no_pins HEAD)
}

copies() { # copies <dockerfile> <image>...: every COPY source must be, be under or contain an input
  local df="$1" i s src fail=0
  shift
  local ins
  ins="$(for i in "$@"; do inputs "$i"; done | sort -u)"
  # Join continued lines, keep COPY/ADD without --from, drop the flags and the destination.
  while read -r -a src; do
    for s in "${src[@]}"; do
      s="${s#./}"
      s="${s%/}"
      [[ -z "$s" ]] && s=.
      [[ "$s" == . ]] && continue
      # covered: an input itself, under one, or containing one
      if ! awk -v s="$s" '$0 == s || index(s, $0 "/") == 1 || index($0, s "/") == 1 { f = 1 } END { exit !f }' <<<"$ins"; then
        echo "image-inputs: $df: COPY $s is outside the inputs of $* (scripts/image-inputs.sh)" >&2
        fail=1
      fi
    done
  done < <(sed -e ':a' -e '/\\$/{N;s/\\\n//;ba}' "$df" |
    awk 'toupper($1) == "COPY" || toupper($1) == "ADD" {
      out = ""; n = 0
      for (k = 2; k <= NF; k++) { if ($k ~ /^--from=/) next; if ($k ~ /^--/) continue; a[++n] = $k }
      for (k = 1; k < n; k++) out = out (k > 1 ? " " : "") a[k]
      print out }')
  return $fail
}

cmd="${1:-}"
[[ $# -gt 0 ]] && shift
[[ "$cmd" == images ]] || meta="$(cargo metadata --format-version 1 --no-deps --offline --manifest-path "$root/Cargo.toml")"
case "$cmd" in
  images) printf '%s\n' "${images[@]}" ;;
  inputs)
    [[ $# -eq 1 ]] && known "$1" || die "usage: $0 inputs <image> (one of: ${images[*]})"
    inputs "$1" | sort -u ;;
  changed)
    [[ $# -gt 0 ]] || set -- "${images[@]}"
    for i in "$@"; do known "$i" || die "unknown image $i (one of: ${images[*]})"; done
    for i in "$@"; do
      pin="$(pin_of "$i")"
      if [[ -z "$pin" ]] || ! git -C "$root" rev-parse -q --verify "$pin^{commit}" >/dev/null; then
        echo "image-inputs: warning: $i: pin commit ${pin:-<none>} is not in this repository's history; counting it as changed" >&2
        echo "$i"
        continue
      fi
      ins="$(inputs "$i" | sort -u)"
      if differs "$pin" "$ins"; then echo "$i"; fi
    done ;;
  copies)
    if [[ $# -gt 0 ]]; then
      [[ $# -ge 2 && -f "$1" ]] || die "usage: $0 copies [<dockerfile> <image>...]"
      for i in "${@:2}"; do known "$i" || die "unknown image $i"; done
      copies "$@"
    else
      rc=0
      copies "$root/infra/images/server.Dockerfile" "${rust[@]}" || rc=1
      copies "$root/infra/images/studio.Dockerfile" studio || rc=1
      copies "$root/infra/images/identity.Dockerfile" identity || rc=1
      [[ $rc -eq 0 ]] && echo "image-inputs: every Dockerfile COPY is within its images' inputs"
      exit $rc
    fi ;;
  *) die "usage: $0 images | inputs <image> | changed [<image>...] | copies [<dockerfile> <image>...]" ;;
esac
