#!/usr/bin/env bash
# `just deny` refuses chrono, chrono-tz and humantime (deny.toml `[bans]`), named directly or
# pulled in through another dependency. Each case is a scratch workspace that holds the real
# deny.toml and one crate adding the banned dependency; the real `deny` recipe runs there and
# must fail naming the crate as banned. (An advisory failure alone doesn't count: chrono and
# humantime also have open advisories, so only the ban message proves the ban.)
#
#   bash scripts/tests/deny-bans.sh   (needs cargo-deny; `just deny-install`)
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
root="$PWD"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A workspace with one member `app`, depending on $1 (a TOML dependency line), and with the
# extra member crates given as `name=dependency-line` pairs after it.
scratch() {
  local dir="$1" dep="$2"
  shift 2
  mkdir -p "$dir/app/src"
  cp "$root/deny.toml" "$dir/deny.toml"
  local members='"app"'
  for extra in "$@"; do
    local name="${extra%%=*}"
    members+=", \"$name\""
    mkdir -p "$dir/$name/src"
    : >"$dir/$name/src/lib.rs"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2024"\npublish = false\n\n[dependencies]\n%s\n' \
      "$name" "${extra#*=}" >"$dir/$name/Cargo.toml"
  done
  printf '[workspace]\nresolver = "3"\nmembers = [%s]\n' "$members" >"$dir/Cargo.toml"
  : >"$dir/app/src/lib.rs"
  printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2024"\npublish = false\n\n[dependencies]\n%s\n' \
    "$dep" >"$dir/app/Cargo.toml"
}

bad=0
# Runs the real `deny` recipe in $1 and expects it to fail with a ban on crate $2.
expect_banned() {
  local dir="$1" crate="$2" out
  if out="$(just --justfile "$root/justfile" --working-directory "$dir" deny 2>&1)"; then
    echo "deny-bans: just deny passed with $crate in the tree" >&2
    bad=1
  elif ! grep -qE "crate '$crate = [^']*' is explicitly banned" <<<"$out"; then
    echo "deny-bans: just deny failed, but not with a ban on $crate:" >&2
    grep -E '^(error|warning)\[' <<<"$out" >&2 || echo "$out" >&2
    bad=1
  fi
}

# chrono added directly.
scratch "$tmp/direct" 'chrono = "=0.4.45"'
expect_banned "$tmp/direct" chrono

# humantime pulled in through a dependency (a path crate of the scratch workspace).
scratch "$tmp/transitive" 'puller = { path = "../puller" }' 'puller=humantime = "=2.4.0"'
expect_banned "$tmp/transitive" humantime

# chrono-tz (which also pulls chrono).
scratch "$tmp/tz" 'chrono-tz = "=0.10.4"'
expect_banned "$tmp/tz" chrono-tz

[[ $bad -eq 0 ]] && echo "deny-bans: ok"
exit $bad
