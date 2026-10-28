#!/usr/bin/env bash
# Offline check that the workbench overlay changes nothing (epic selfhost-install, m7): the base
# file plus docker-compose.workbench.yml renders the same stack as the single docker-compose.yml
# did before the split (the file at REV, default the last commit that had only the one file).
#
#   scripts/tests/compose-overlay-equiv.sh [--env-file <stack.env>] [--rev <git rev>]
#
# --env-file defaults to the fixture stack (feat-x); pass a live stack's env/stack.env to compare
# with its real values. Both renders use every profile (sites, runner) and the same environment,
# and the check fails on any difference in:
#   - `docker compose config --format json --no-env-resolution`, keys sorted, with the top-level
#     `x-*` extension fields dropped. Those hold only YAML anchors, which can't cross files, so
#     where they live changes; Compose never applies them to a container. `--no-env-resolution`
#     keeps the env_file lists as lists (compared exactly) instead of inlining the files' values,
#     so a live stack's secrets never reach the output.
#   - `docker compose config --hash '*'`: each service's config hash, the value `up` compares to
#     decide whether a running container must be recreated. Equal hashes mean an existing stack's
#     containers stay as they are.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
env_file="$root/scripts/tests/fixtures/workbench-stack.env"
# The epic branch's last single-file commit (m6 merged): the shape this split must keep.
rev=2a4d327e
while (($#)); do
  case "$1" in
    --env-file) env_file="$2"; shift 2 ;;
    --rev) rev="$2"; shift 2 ;;
    *) echo "usage: $0 [--env-file <stack.env>] [--rev <git rev>]" >&2; exit 2 ;;
  esac
done
fail() {
  echo "compose-overlay-equiv: $*" >&2
  exit 1
}
dir="$root/infra/compose"
base="$dir/docker-compose.yml"
overlay="$dir/docker-compose.workbench.yml"
[[ -f "$overlay" ]] || fail "$overlay is missing"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
git -C "$root" cat-file -e "$rev:infra/compose/docker-compose.yml" 2>/dev/null ||
  fail "git has no infra/compose/docker-compose.yml at $rev (a shallow clone? fetch the history)"
git -C "$root" show "$rev:infra/compose/docker-compose.yml" >"$tmp/single.yml"

project="$(sed -n 's/^CHELABASE_COMPOSE_PROJECT=//p' "$env_file")"
project="${project:-chelabase-feat-x}"
# The project directory is pinned, so the reference file's temp location changes nothing.
compose() {
  env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p "$project" --project-directory "$dir" "$@"
}
render() { # render <-f args…>
  compose "$@" --env-file "$env_file" --profile sites --profile runner config --format json \
    --no-env-resolution | jq -S 'with_entries(select(.key | startswith("x-") | not))'
}
hashes() { # hashes <-f args…>
  compose "$@" --env-file "$env_file" --profile sites --profile runner config --hash '*' | sort
}

render -f "$tmp/single.yml" >"$tmp/single.json" || fail "rendering the single file at $rev failed"
render -f "$base" -f "$overlay" >"$tmp/split.json" || fail "rendering base + overlay failed"
diff -u --label "single ($rev)" --label "base + overlay" "$tmp/single.json" "$tmp/split.json" >&2 ||
  fail "base + overlay renders a different config than the single file at $rev"

hashes -f "$tmp/single.yml" >"$tmp/single.hash" || fail "hashing the single file at $rev failed"
hashes -f "$base" -f "$overlay" >"$tmp/split.hash" || fail "hashing base + overlay failed"
[[ "$(wc -l <"$tmp/split.hash")" -gt 20 ]] || fail "expected a hash per service, got: $(cat "$tmp/split.hash")"
diff -u --label "single ($rev)" --label "base + overlay" "$tmp/single.hash" "$tmp/split.hash" >&2 ||
  fail "a service's config hash differs: up would recreate its container"

echo "compose-overlay-equiv: ok ($(jq '.services | length' "$tmp/split.json") services, $(basename "$env_file"))"
