#!/usr/bin/env bash
# Offline check that the workbench stack doesn't change by accident: base file plus
# docker-compose.workbench.yml must render the stack recorded in
# scripts/tests/fixtures/compose-workbench.json. A change to the workbench shape on purpose
# re-records it in the same commit, and the snapshot's diff shows in review:
#
#   CHELABASE_BLESS=1 scripts/tests/compose-overlay-equiv.sh
#
# (Until 2026-11-06 the reference was the compose files at a pinned commit, REV. A squash merge
# drops a branch's commits, so each merge that moved REV broke this check on main until a
# follow-up moved it again. The snapshot needs no history.)
#
#   scripts/tests/compose-overlay-equiv.sh [--rev <git rev> [--env-file <stack.env>]]
#
# --rev compares with the compose files at that commit instead (a manual check), and --env-file
# (only with --rev) renders both sides with a live stack's env/stack.env instead of the fixture
# stack (feat-x). Every render uses every profile (sites, runner), and the check fails on any
# difference in:
#   - `docker compose config --format json --no-env-resolution`, keys sorted, with the top-level
#     `x-*` extension fields dropped. Those hold only YAML anchors, which can't cross files, so
#     where they live changes; Compose never applies them to a container. `--no-env-resolution`
#     keeps the env_file lists as lists (compared exactly) instead of inlining the files' values,
#     so a stack's secrets never reach the output.
#   - with --rev only, `docker compose config --hash '*'`: each service's config hash, the value
#     `up` compares to decide whether a running container must be recreated. Equal hashes mean an
#     existing stack's containers stay as they are. Compose versions hash the same config
#     differently (v5.1.3 and v5.7.0 do), so the snapshot holds the config alone: both sides of a
#     --rev check are hashed by the same Compose.
# The Neon images render as fixed placeholders (NEON_IMAGE, COMPUTE_NODE_IMAGE), so a Neon pin
# bump (`NEON_REF`) never changes the snapshot. The app images the release pins in the base file
# are all replaced by the workbench image, so those pins don't reach it either.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
env_file="$root/scripts/tests/fixtures/workbench-stack.env"
snap="$root/scripts/tests/fixtures/compose-workbench"
rev=""
custom_env=0
while (($#)); do
  case "$1" in
    --env-file) env_file="$2"; custom_env=1; shift 2 ;;
    --rev) rev="$2"; shift 2 ;;
    *) echo "usage: $0 [--rev <git rev> [--env-file <stack.env>]]" >&2; exit 2 ;;
  esac
done
fail() {
  echo "compose-overlay-equiv: $*" >&2
  exit 1
}
((custom_env == 0 || ${#rev} > 0)) ||
  fail "--env-file needs --rev: the snapshot is the fixture stack's render"
dir="$root/infra/compose"
base="$dir/docker-compose.yml"
overlay="$dir/docker-compose.workbench.yml"
[[ -f "$overlay" ]] || fail "$overlay is missing"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
project="$(sed -n 's/^CHELABASE_COMPOSE_PROJECT=//p' "$env_file")"
project="${project:-chelabase-feat-x}"
# The project directory is pinned, so the reference file's temp location changes nothing.
compose() {
  NEON_IMAGE=neon-storage:placeholder COMPUTE_NODE_IMAGE=neon-compute:placeholder \
    docker compose -p "$project" --project-directory "$dir" "$@"
}
render() { # render <-f args…>
  compose "$@" --env-file "$env_file" --profile sites --profile runner config --format json \
    --no-env-resolution | jq -S 'with_entries(select(.key | startswith("x-") | not))'
}
hashes() { # hashes <-f args…>
  compose "$@" --env-file "$env_file" --profile sites --profile runner config --hash '*' | sort
}

render -f "$base" -f "$overlay" >"$tmp/split.json" || fail "rendering base + overlay failed"

if [[ -n "$rev" ]]; then
  hashes -f "$base" -f "$overlay" >"$tmp/split.hash" || fail "hashing base + overlay failed"
  [[ "$(wc -l <"$tmp/split.hash")" -gt 20 ]] || fail "expected a hash per service, got: $(cat "$tmp/split.hash")"
  git -C "$root" cat-file -e "$rev:infra/compose/docker-compose.yml" 2>/dev/null ||
    fail "git has no infra/compose/docker-compose.yml at $rev (a shallow clone? fetch the history)"
  git -C "$root" show "$rev:infra/compose/docker-compose.yml" >"$tmp/ref-base.yml"
  ref=(-f "$tmp/ref-base.yml")
  if git -C "$root" cat-file -e "$rev:infra/compose/docker-compose.workbench.yml" 2>/dev/null; then
    git -C "$root" show "$rev:infra/compose/docker-compose.workbench.yml" >"$tmp/ref-overlay.yml"
    ref+=(-f "$tmp/ref-overlay.yml")
  fi
  label="at $rev"
  render "${ref[@]}" >"$tmp/ref.json" || fail "rendering the compose files at $rev failed"
  hashes "${ref[@]}" >"$tmp/ref.hash" || fail "hashing the compose files at $rev failed"
else
  label="the snapshot"
  if [[ -n "${CHELABASE_BLESS:-}" ]]; then
    cp "$tmp/split.json" "$snap.json"
    echo "compose-overlay-equiv: recorded $(jq '.services | length' "$snap.json") services in ${snap#"$root/"}.json"
    exit 0
  fi
  [[ -f "$snap.json" ]] || fail "no snapshot at ${snap#"$root/"}.json"
  cp "$snap.json" "$tmp/ref.json"
fi
bless="change it on purpose? re-record: CHELABASE_BLESS=1 $0"
diff -u --label "$label" --label "base + overlay" "$tmp/ref.json" "$tmp/split.json" >&2 ||
  fail "base + overlay renders a different config than $label ($bless)"
if [[ -n "$rev" ]]; then
  diff -u --label "$label" --label "base + overlay" "$tmp/ref.hash" "$tmp/split.hash" >&2 ||
    fail "a service's config hash differs from $label: up would recreate its container"
fi

echo "compose-overlay-equiv: ok ($(jq '.services | length' "$tmp/split.json") services, $(basename "$env_file"))"
