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
# Outside reads select only the files' readers, and a read in test or binary code selects only
# its package, not the package's dependents.
expect 'packages/client/README.md' $'rust=none\nweb=@chelabase/client @chelabase/docs'
expect 'apps/docs/content/docs/architecture/gateway.mdx' $'rust=none\nweb=@chelabase/docs'
expect 'apps/docs/content/docs/reference/access.mdx' $'rust=chelabase-routes\nweb=@chelabase/studio @chelabase/docs'
expect 'packages/client/fixtures/gen/db.ts' $'rust=chelabase-query chelabase-typegen chelabase\nweb=@chelabase/client @chelabase/docs'
expect 'openapi/management.json' $'rust=orchestrator\nweb=@chelabase/management-client @chelabase/studio @chelabase/docs'
expect 'openapi/data.json' $'rust=chelabase-query rest\nweb=@chelabase/studio'
expect 'infra/compose/.env.example' $'rust=realtime chelabase\nweb=@chelabase/studio @chelabase/docs @chelabase/landing'
expect 'infra/compose/nats/nats.conf' $'rust=none\nweb=none'
expect 'scripts/neon-images.sh' $'rust=chelabase-neon chelabase\nweb=none'
[[ "$(CI_AFFECTED_FILES= scripts/ci-affected.sh)" == $'rust=none\nweb=none' ]] || fail "an empty list selects nothing"

# --- 2. Outside-crate reads -----------------------------------------------------------------

# Literals that name no file a test reads (a ticket string, made-up script names in CLI tests).
ignore=(
  "realtime ../../rt:ticket:x"
  "chelabase scripts/s.sh"
  "chelabase scripts/x.sh"
  # migrate.rs's tests build apps/studio in a temporary checkout.
  "chelabase apps/studio"
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

# Where in <package> the reference at <file>:<line> sits:
#   test  tests/, benches/, examples/, a #[cfg(test)] item or a module declared under #[cfg(test)]
#   bin   a package without a library, src/main.rs or src/bin/
#   lib   anything else: library code, which every dependent builds
mapfile -t libs < <(cargo metadata --no-deps --format-version 1 |
  jq -r '.packages[] | select(any(.targets[].kind[]; . == "lib" or . == "proc-macro")) | .name')
kind() { # <package> <dir> <file> <line>
  local pkg="$1" dir="$2" file="$3" line="$4" rel base stem
  [[ " ${libs[*]} " == *" $pkg "* ]] || { echo bin; return; }
  rel="${file#"$dir"/}"
  case "$rel" in
    tests/* | benches/* | examples/*) echo test; return ;;
    src/main.rs | src/bin/*) echo bin; return ;;
  esac
  # A module file whose `mod` (or `#[path]`) declaration is under #[cfg(test)].
  base="$(basename "$file")"
  stem="${base%.rs}"
  [[ "$stem" != mod ]] || stem="$(basename "$(dirname "$file")")"
  if git ls-files -- "$dir/*.rs" | xargs grep -hE -B3 "^\s*(pub(\([a-z]+\))? )?mod $stem;|#\[path = \"([^\"]*/)?$base\"\]" |
    grep -q 'cfg(test)'; then
    echo test
    return
  fi
  # Inside a #[cfg(test)] item: from the attribute to the closing brace at its indentation.
  if awk -v want="$line" '
    !inside && /^[ \t]*#\[cfg\(test\)\]/ { match($0, /^[ \t]*/); ind = substr($0, 1, RLENGTH); pending = 1; next }
    pending && /^[ \t]*#\[/ { next }
    pending { pending = 0; if ($0 ~ /\{[ \t]*$/) { inside = 1; start = NR } }
    inside { if (NR == want) found = 1; if (NR > start && $0 == ind "}") inside = 0 }
    END { exit !found }' "$file"; then
    echo test
    return
  fi
  echo lib
}

# The detector's rust_reads table: "pattern self|closure package..." rows.
eval "$(sed -n '/^rust_reads=(/,/^)/p' scripts/ci-affected.sh)"
declare -A used=()
# Check <package>'s <kind> reference to <path> against the table; prints the matching row index.
check_row() { # <package> <kind> <path> <where>
  local pkg="$1" kind="$2" p="$3" at="$4" i d pat mode pkgs ok=0
  # Another member's files select it and its dependents; a full trigger selects everything.
  for d in "${member_dirs[@]}"; do [[ "$p" != "$d"/* ]] || return 0; done
  [[ -d "$p" || "$(rust_of "$p")" != full ]] || return 0
  for i in "${!rust_reads[@]}"; do
    read -r pat mode pkgs <<<"${rust_reads[$i]}"
    # A directory the code joins file names onto at run time: rows name the files under it.
    if [[ -d "$p" ]]; then
      [[ "$pat" == "$p"/* ]] || continue
    else
      # shellcheck disable=SC2053 # the row's pattern
      [[ "$p" == $pat ]] || continue
    fi
    [[ " $pkgs " == *" $pkg "* ]] || continue
    used["$i $pkg"]=1
    if [[ "$kind" == lib && "$mode" != closure ]]; then
      fail "$at: $pkg's library reads $p, but row '$pat' is 'self': its dependents build that code, make it 'closure'"
    fi
    ok=1
  done
  ((ok)) || fail "$at reads $p ($kind), but no rust_reads row matching it names $pkg"
}

mapfile -t member_dirs < <(cargo metadata --no-deps --format-version 1 |
  jq -r '.workspace_root as $r | .workspace_members as $w | .packages[] | select(.id as $i | $w | index($i))
    | .manifest_path | ltrimstr($r + "/") | rtrimstr("/Cargo.toml")')

declare -A verdict=()
rust_of() { # <path>: the detector's rust= output for a change to <path>
  if [[ -z "${verdict[$1]+x}" ]]; then verdict[$1]="$(classify "$1" | sed -n 's/^rust=//p')"; fi
  echo "${verdict[$1]}"
}
covered() { # <package> <path>: does a change to <path> select <package>? (files only)
  local out
  [[ ! -d "$2" ]] || return 0
  out="$(rust_of "$2")"
  [[ "$out" == full || " $out " == *" $1 "* ]]
}

checked=0
while read -r pkg dir; do
  while IFS= read -r hit; do
    file="${hit%%:*}"
    hit="${hit#*:}"
    line="${hit%%:*}"
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
    k="$(kind "$pkg" "$dir" "$file" "$line")"
    for p in "${paths[@]}"; do
      # A git-ignored file (a local env file) is never in a diff.
      ! git check-ignore -q -- "$p" || continue
      checked=$((checked + 1))
      if [[ -n "${CI_AFFECTED_LIST:-}" ]]; then
        echo "$pkg $k $p $file:$line"
        continue
      fi
      covered "$pkg" "$p" || fail "$file:$line reads $p (\"$lit\"), but changing it doesn't select $pkg"
      check_row "$pkg" "$k" "$p" "$file:$line"
    done
  done < <(git ls-files -- "$dir/*.rs" | xargs -r grep -onHE '"[^"]*"')
done < <(cargo metadata --no-deps --format-version 1 |
  jq -r '.workspace_root as $r | .workspace_members as $w | .packages[] | select(.id as $i | $w | index($i))
    | "\(.name) \(.manifest_path | ltrimstr($r + "/") | rtrimstr("/Cargo.toml"))"')

[[ -z "${CI_AFFECTED_LIST:-}" ]] || exit 0
# Every row's package still reads something the row matches: rows don't outlive their reads.
for i in "${!rust_reads[@]}"; do
  read -r pat mode pkgs <<<"${rust_reads[$i]}"
  for pkg in $pkgs; do
    [[ -n "${used["$i $pkg"]+x}" ]] || fail "rust_reads row '$pat' names $pkg, but $pkg reads nothing it matches: narrow or drop it"
  done
done
((checked > 10)) || fail "found only $checked outside-crate references: the scan is broken"
((failed == 0)) || exit 1
echo "ci-affected: ok ($checked outside-crate references covered)"
