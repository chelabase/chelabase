#!/usr/bin/env bash
# What a change affects, for .github/workflows/ci.yml. Prints two GitHub step outputs:
#   rust=none|full|<cargo packages>     the argument to `just check-rust`
#   web=none|full|<pnpm packages>       the argument to `just check-web`
# Usage: scripts/ci-affected.sh <base-ref>                  classifies the files <base-ref>...HEAD changes
#        CI_AFFECTED_FILES=$'a\nb' scripts/ci-affected.sh   classifies the given paths (tests)
# `just check-scripts` always runs, so paths only the shell tests read need no rule here.
# Tested, and its rust_reads table checked against the Rust sources, by scripts/tests/ci-affected.sh.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# --- Rules ----------------------------------------------------------------------------------
# Bash patterns against repo-relative paths; `*` also matches `/`. A file under a Cargo
# workspace member's directory selects that member, and one under a pnpm workspace package's
# directory selects that package, without a rule. Selections then grow to every workspace
# package that depends (directly or not, dev-dependencies too) on a selected one.
# A path no rule matches runs every Rust and web check.

# A change here runs every Rust check.
rust_full=(Cargo.toml Cargo.lock rust-toolchain.toml clippy.toml deny.toml '.cargo/*' '.config/*'
  justfile .prototools .github/workflows/ci.yml scripts/ci-affected.sh)

# A change here runs every web check.
web_full=(package.json pnpm-lock.yaml pnpm-workspace.yaml .npmrc 'tsconfig*.json' 'biome.json*'
  '.eslintrc*' 'eslint.config.*' '.prettierrc*' 'prettier.config.*'
  justfile .prototools .github/workflows/ci.yml scripts/ci-affected.sh)

# Files outside a package's directory that it reads: pattern, mode, packages. Mode `self` selects
# just the packages: the read is in their tests or a binary, which no dependent builds. Mode
# `closure` also selects their dependents: the read is in library code. Name the files read, not
# their directory. scripts/tests/ci-affected.sh checks every row against the Rust sources.
rust_reads=(
  'openapi/management.json                                   self     orchestrator'
  'openapi/data.json                                         self     rest chelabase-query'
  'openapi/login.json                                        self     auth'
  'apps/studio/package.json                                  self     orchestrator'
  'apps/studio/src/lib/role-rules.fixtures.json              self     orchestrator'
  'apps/docs/content/docs/reference/access.mdx               self     chelabase-routes'
  'apps/docs/content/docs/reference/errors.mdx               self     chelabase-routes'
  'apps/docs/content/docs/using/self-hosted-vs-platform.mdx  self     chelabase-routes'
  'packages/client/fixtures/*                                self     chelabase-query chelabase-typegen chelabase'
  'infra/compose/.env.example                                self     realtime chelabase'
  'infra/compose/docker-compose.yml                          self     chelabase-init init chelabase'
  'infra/compose/docker-compose.workbench.yml                self     chelabase-init chelabase'
  'infra/compose/docker-compose.example.yml                  self     init'
  'infra/workbench/*                                         self     chelabase'
  'infra/functions-runtime/*                                 self     chelabase'
  'infra/images/functions-runtime.Dockerfile                 self     chelabase'
  'infra/neon/NEON_REF                                       closure  bench-data-api'
  'scripts/compute-network-e2e.sh                            self     orchestrator'
  'scripts/neon-images.sh                                    self     chelabase-neon chelabase'
  'scripts/neon-e2e.sh                                       self     chelabase'
  'services/identity/openapi.json                            self     chelabase-identity'
)
# The same for the web packages (their tests and builds read these; not checked by a guard).
web_reads=(
  'openapi/management.json                                   @chelabase/management-client @chelabase/studio @chelabase/docs'
  'openapi/data.json                                         @chelabase/studio @chelabase/docs'
  'openapi/login.json                                        @chelabase/studio @chelabase/docs'
  'infra/compose/.env.example                                @chelabase/studio @chelabase/docs @chelabase/landing'
  'apps/docs/content/docs/reference/access.mdx               @chelabase/studio'
  'crates/chelabase-query/tests/fixtures/text_keys.json      @chelabase/studio'
)

# Paths no Rust or web check reads.
no_checks=('*.md' 'docs/*' '.github/*' '.claude/*' '.cursor/*' .mcp.json 'LICENSE*' .gitignore
  .dockerignore 'infra/images/*' 'infra/neon/*' 'infra/compose/*' 'infra/workbench/*' 'scripts/*')

# The packages `just check-web` checks, in its order.
web_checked=(@chelabase/management-client @chelabase/studio @chelabase/client @chelabase/design
  @chelabase/docs @chelabase/landing)

# --- Workspaces -----------------------------------------------------------------------------

meta="$(cargo metadata --no-deps --format-version 1)"
# {name: [workspace dependencies]} and "dir name" lines, for Cargo and pnpm.
rust_graph="$(jq -c '.workspace_members as $w | [.packages[] | select(.id as $i | $w | index($i))]
  | (map(.name)) as $n | map({key: .name, value: [.dependencies[].name | select(. as $d | $n | index($d))]})
  | from_entries' <<<"$meta")"
mapfile -t rust_dirs < <(jq -r '.workspace_root as $r | .workspace_members as $w | .packages[]
  | select(.id as $i | $w | index($i)) | "\(.manifest_path | ltrimstr($r + "/") | rtrimstr("/Cargo.toml")) \(.name)"' <<<"$meta")

mapfile -t web_globs < <(sed -n '/^packages:/,/^[^ ]/s/^ *- *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' pnpm-workspace.yaml)
web_dirs=()
web_graph='{}'
for g in "${web_globs[@]}"; do
  while IFS= read -r d; do
    [[ -f "$d/package.json" ]] || continue
    name="$(jq -r .name "$d/package.json")"
    web_dirs+=("$d $name")
    web_graph="$(jq -c --arg n "$name" --slurpfile p "$d/package.json" '. + {($n): [($p[0].dependencies // {}), ($p[0].devDependencies // {})
      | to_entries[] | select(.value | tostring | startswith("workspace:")) | .key]}' <<<"$web_graph")"
  done < <(compgen -G "$g" || true)
done

# Every rule's package must exist.
for row in "${rust_reads[@]}"; do
  read -r _ mode pkgs <<<"$row"
  [[ "$mode" == self || "$mode" == closure ]] || {
    echo "ci-affected: rust_reads row '$row' has mode $mode, not self or closure" >&2
    exit 1
  }
  for p in $pkgs; do jq -e --arg p "$p" 'has($p)' <<<"$rust_graph" >/dev/null || {
    echo "ci-affected: rust_reads names $p, not a workspace member" >&2
    exit 1
  }; done
done
for row in "${web_reads[@]}"; do
  read -r _ pkgs <<<"$row"
  for p in $pkgs; do jq -e --arg p "$p" 'has($p)' <<<"$web_graph" >/dev/null || {
    echo "ci-affected: web_reads names $p, not a pnpm workspace package" >&2
    exit 1
  }; done
done

# --- Classify -------------------------------------------------------------------------------

if [[ -n "${CI_AFFECTED_FILES+x}" ]]; then
  files="$CI_AFFECTED_FILES"
elif [[ $# -eq 1 ]]; then
  files="$(git diff --name-only --no-renames "$1...HEAD")"
else
  echo "usage: scripts/ci-affected.sh <base-ref>" >&2
  exit 2
fi

matches() { # <path> <pattern>...
  local f="$1" pat
  shift
  for pat in "$@"; do [[ "$f" == $pat ]] && return 0; done # shellcheck disable=SC2053
  return 1
}

rust_all=0 web_all=0
rust_seeds=() rust_self=() web_seeds=()
while IFS= read -r f; do
  [[ -n "$f" ]] || continue
  hit=0
  if matches "$f" "${rust_full[@]}"; then rust_all=1 hit=1; fi
  if matches "$f" "${web_full[@]}"; then web_all=1 hit=1; fi
  for row in "${rust_dirs[@]}"; do
    read -r d n <<<"$row"
    if [[ "$f" == "$d"/* ]]; then rust_seeds+=("$n") hit=1; fi
  done
  for row in "${web_dirs[@]}"; do
    read -r d n <<<"$row"
    if [[ "$f" == "$d"/* ]]; then web_seeds+=("$n") hit=1; fi
  done
  for row in "${rust_reads[@]}"; do
    read -r pat mode pkgs <<<"$row"
    matches "$f" "$pat" || continue
    hit=1
    # shellcheck disable=SC2206
    if [[ "$mode" == closure ]]; then rust_seeds+=($pkgs); else rust_self+=($pkgs); fi
  done
  for row in "${web_reads[@]}"; do
    read -r pat pkgs <<<"$row"
    # shellcheck disable=SC2206
    if matches "$f" "$pat"; then web_seeds+=($pkgs) hit=1; fi
  done
  if matches "$f" "${no_checks[@]}"; then hit=1; fi
  if ((hit == 0)); then rust_all=1 web_all=1; fi
done <<<"$files"

# The seeds and every package that depends on one, in the graph's order.
closure() { # <graph> <seed>...
  jq -r '. as $g
    | def grow: . as $s
        | [$g | keys_unsorted[] | select(. as $n | any($s[]; . == $n) or any($g[$n][]; . as $d | any($s[]; . == $d)))] as $t
        | if ($t | length) == ($s | length) then $t else ($t | grow) end;
    $ARGS.positional | unique | grow | .[]' --args "${@:2}" <<<"$1"
}

rust_sel=()
if ((rust_all == 0 && ${#rust_seeds[@]} + ${#rust_self[@]})); then
  # The closure of the seeds, plus the `self` packages alone, in the graph's order.
  mapfile -t rust_sel < <(jq -r --argjson self "$(jq -nc '$ARGS.positional' --args "${rust_self[@]}")" \
    --argjson sel "$(jq -nc '$ARGS.positional' --args $( ((${#rust_seeds[@]})) && closure "$rust_graph" "${rust_seeds[@]}"))" \
    'keys_unsorted[] | select(. as $n | any($self[], $sel[]; . == $n))' <<<"$rust_graph")
fi
if ((rust_all == 1 || ${#rust_sel[@]} == $(jq length <<<"$rust_graph"))); then
  echo "rust=full"
elif ((${#rust_sel[@]} == 0)); then
  echo "rust=none"
else
  echo "rust=${rust_sel[*]}"
fi

web_sel=()
if ((web_all == 0 && ${#web_seeds[@]})); then
  mapfile -t reached < <(closure "$web_graph" "${web_seeds[@]}")
  for p in "${web_checked[@]}"; do [[ " ${reached[*]} " == *" $p "* ]] && web_sel+=("$p"); done
fi
if ((web_all == 1 || ${#web_sel[@]} == ${#web_checked[@]})); then
  echo "web=full"
elif ((${#web_sel[@]} == 0)); then
  echo "web=none"
else
  echo "web=${web_sel[*]}"
fi
