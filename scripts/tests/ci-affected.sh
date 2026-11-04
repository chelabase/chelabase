#!/usr/bin/env bash
# Tests of scripts/ci-affected.sh, the change detector that picks what the ci workflow checks.
#  1. The classification on synthetic file lists (CI_AFFECTED_FILES).
#  2. A guard: every path outside its own directory that a Rust package's source names
#     (`../`-relative literals, and repo-root-relative ones such as "openapi/data.json") must,
#     when changed, select that package (or run every Rust check). A miss means the detector's
#     rust_reads table is missing a row. Paths built at run time aren't seen; keep them literal.
# Part of `just check-scripts`.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
root="$(pwd -P)"

failed=0
fail() {
  echo "ci-affected: $*" >&2
  failed=1
}

classify() { CI_AFFECTED_FILES="$1" scripts/ci-affected.sh; }

# --- 1. Synthetic file lists ----------------------------------------------------------------

expect() { # <files> <expected output>
  local got
  got="$(classify "$1")" || {
    fail "detector failed on: $1"
    return
  }
  [[ "$got" == "$2" ]] || fail "for $(tr '\n' ' ' <<<"$1")expected
$2
got
$got"
}
expect_has() { # <files> <output line prefix (rust=|web=)> <word>...
  local got line w
  got="$(classify "$1")" || {
    fail "detector failed on: $1"
    return
  }
  line="$(grep "^$2" <<<"$got")"
  for w in "${@:3}"; do
    [[ "$line" == "$2full" || " ${line#"$2"} " == *" $w "* ]] || fail "for $1: $2 lacks $w: $line"
  done
}

expect $'README.md\ndocs/roadmap/x.md\n.github/workflows/live.yml\n.claude/settings.json' $'rust=none\nweb=none'
expect 'scripts/tests/entry-urls.sh' $'rust=none\nweb=none'
expect 'tools/bench-data-api/src/main.rs' $'rust=bench-data-api\nweb=none'
expect 'services/gateway/src/lib.rs' $'rust=gateway\nweb=none'
# A core crate selects most of the workspace; every leaf of the graph's roots selects it all.
expect_has 'crates/chelabase-types/src/lib.rs' rust= chelabase-types orchestrator gateway chelabase
expect $'crates/chelabase-types/src/lib.rs\ncrates/chelabase-http/src/lib.rs\ncrates/chelabase-secrets/src/lib.rs\ncrates/chelabase-typegen/src/lib.rs' $'rust=full\nweb=none'
expect 'Cargo.lock' $'rust=full\nweb=none'
expect 'pnpm-lock.yaml' $'rust=none\nweb=full'
expect 'brand-new-dir/file.txt' $'rust=full\nweb=full'
expect 'crates/brand-new-crate/src/lib.rs' $'rust=full\nweb=full'
expect 'packages/design/tokens.css' $'rust=none\nweb=@chelabase/studio @chelabase/design @chelabase/docs @chelabase/landing'
expect 'packages/neon-e2e/check.mjs' $'rust=none\nweb=none'
expect_has 'openapi/management.json' rust= orchestrator
expect_has 'openapi/management.json' web= @chelabase/management-client @chelabase/studio
expect_has 'packages/client/fixtures/gen/db.ts' rust= chelabase-query chelabase-typegen chelabase
# A file only a dependency reads selects its dependents too.
expect_has 'crates/chelabase-routes/src/lib.rs' rust= chelabase-routes gateway orchestrator
[[ "$(CI_AFFECTED_FILES= scripts/ci-affected.sh)" == $'rust=none\nweb=none' ]] || fail "an empty list selects nothing"

# --- 2. Outside-crate reads -----------------------------------------------------------------

# Literals that name no file a test reads (a ticket string, made-up script names in CLI tests).
ignore=(
  "realtime ../../rt:ticket:x"
  "chelabase scripts/s.sh"
  "chelabase scripts/x.sh"
)

mapfile -t topdirs < <(git ls-files | grep / | cut -d/ -f1 | sort -u)
mapfile -t topfiles < <(git ls-files | grep -v / | sort -u)

# The repo-relative paths (inside the repo, existing or in an existing directory) <literal> in
# <file> of the package at <dir> may name.
candidates() { # <dir> <file> <literal>
  local dir="$1" file="$2" lit="$3" base p
  if [[ "$lit" == *../* ]]; then
    lit="${lit#/}"
    # include_str!/#[path] resolve against the file's directory, everything else (cargo's cwd
    # for tests, CARGO_MANIFEST_DIR) against the package's.
    for base in "$(dirname "$file")" "$dir"; do
      p="$(realpath -m --relative-to="$root" "$root/$base/$lit")"
      # Not outside the repo, and not the repo root or a directory above the package
      # ("../.." from tests/ is the root joined later: the root-relative literals cover that).
      [[ "$p" != . && "$p" != ../* && "$p" != .. && "$dir" != "$p"/* ]] || continue
      [[ -e "$p" || -d "$(dirname "$p")" ]] && echo "$p"
    done
  else
    p="${lit%%[ \`;:\\]*}"
    p="${p%/}"
    [[ -e "$p" || -d "$(dirname "$p")" ]] && echo "$p"
  fi
  return 0
}

declare -A verdict=()
covered() { # <package> <path>: does a change to <path> select <package>?
  local pkg="$1" p="$2" rep out
  rep="$p"
  if [[ -d "$p" ]]; then rep="$(git ls-files -- "$p" | head -n1)"; fi
  [[ -n "$rep" ]] || rep="$p/x"
  if [[ -z "${verdict[$rep]+x}" ]]; then
    verdict[$rep]="$(classify "$rep" | sed -n 's/^rust=//p')"
  fi
  out="${verdict[$rep]}"
  [[ "$out" == full || " $out " == *" $pkg "* ]]
}

checked=0
while read -r pkg dir; do
  while IFS= read -r hit; do
    file="${hit%%:*}"
    lit="${hit#*:}"
    lit="${lit#\"}"
    lit="${lit%\"}"
    case "$lit" in
      *../*) ;;
      *)
        top="${lit%%/*}"
        if [[ "$lit" == */* ]]; then
          [[ " ${topdirs[*]} " == *" $top "* ]] || continue
        else
          [[ " ${topfiles[*]} " == *" $lit "* ]] || continue
        fi
        ;;
    esac
    [[ " ${ignore[*]/%/|} " != *" $pkg $lit|"* ]] || continue
    mapfile -t paths < <(candidates "$dir" "$file" "$lit")
    ((${#paths[@]})) || continue
    inside=0
    for p in "${paths[@]}"; do [[ "$p" == "$dir" || "$p" == "$dir"/* ]] && inside=1; done
    ((inside == 0)) || continue
    for p in "${paths[@]}"; do
      checked=$((checked + 1))
      covered "$pkg" "$p" || fail "$file reads $p (\"$lit\"), but changing it doesn't select $pkg: add a rust_reads row"
    done
  done < <(git ls-files -- "$dir/*.rs" | xargs -r grep -oHE '"[^"]*"')
done < <(cargo metadata --no-deps --format-version 1 |
  jq -r '.workspace_root as $r | .workspace_members as $w | .packages[] | select(.id as $i | $w | index($i))
    | "\(.name) \(.manifest_path | ltrimstr($r + "/") | rtrimstr("/Cargo.toml"))"')

((checked > 10)) || fail "found only $checked outside-crate references: the scan is broken"
((failed == 0)) || exit 1
echo "ci-affected: ok ($checked outside-crate references covered)"
